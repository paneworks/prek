//! Bounded, route-scoped execution on one seat's generated keyboard.

use super::{
    Owner, SourceKey,
    output::{Claim, Output, Progress, Sink},
};
use crate::{
    config::Action,
    input::{EV_KEY, Frame, InputPosition, Source},
    protocol::Fence,
    router::InputSpan,
    routing::{KeyWork, Routing},
    rules::{Event, Stage},
};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fmt, io,
    time::Duration,
};

const MAX_PENDING: usize = 64;
const MAX_BINDINGS: usize = 4096;
const MAX_BYTES: usize = 512 * 1024;
const MAX_AGE: Duration = Duration::from_secs(1);
const DRAIN: Duration = Duration::from_millis(250);

mod software;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Gesture {
    source: Source,
    rule: String,
    id: u64,
}

/// A particular configured remap on this output incarnation.
#[derive(Clone, Debug)]
pub struct Remap {
    sink: Source,
    fence: Fence,
    gesture: Gesture,
}

pub enum Submission {
    Ignored,
    Queued,
    Remap(Remap),
}

struct Binding {
    work: KeyWork,
    claim: Option<Claim>,
    ended: bool,
    cancelled: bool,
    bytes: usize,
}
enum Operation {
    Software(software::Work),
    Begin(Gesture),
    End(Gesture, Box<KeyWork>),
    Tap(Box<KeyWork>),
    Repeat {
        members: Vec<Gesture>,
        inputs: BTreeMap<Source, u64>,
        emitted: BTreeSet<u16>,
        position: Option<InputPosition>,
        inclusive: bool,
    },
}
struct Pending {
    operation: Operation,
    position: Option<InputPosition>,
    received: Duration,
    bytes: usize,
}
enum FlightKind {
    Software {
        producer: [u64; 2],
        id: u64,
        release: bool,
    },
    Begin,
    End(Gesture),
    Tap(Box<KeyWork>, Claim),
    Repeat,
}
struct Flight {
    position: Option<InputPosition>,
    kind: FlightKind,
    inputs: BTreeMap<Source, u64>,
    deadline: Duration,
    bytes: usize,
}

pub struct StartError<S: Sink> {
    pub error: io::Error,
    pub output: Output<S>,
}
impl<S: Sink> fmt::Debug for StartError<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StartError")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

pub struct Seat<S: Sink> {
    fence: Fence,
    output: Output<S>,
    bindings: BTreeMap<Gesture, Binding>,
    pending: VecDeque<Pending>,
    bytes: usize,
    flight: Option<Flight>,
    started_position: Option<InputPosition>,
    serial: u64,
    repeats: BTreeMap<SourceKey, (u64, usize)>,
    paused: bool,
    retiring: Option<Duration>,
    draining: Option<Duration>,
    failed: bool,
    at: Duration,
    software: BTreeMap<[u64; 2], software::Producer>,
}

impl<S: Sink> fmt::Debug for Seat<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Seat")
            .field("fence", &self.fence)
            .field("pending", &self.pending.len())
            .field("bindings", &self.bindings.len())
            .field("failed", &self.failed())
            .finish_non_exhaustive()
    }
}

impl<S: Sink> Seat<S> {
    #[cfg(feature = "native-test-harness")]
    #[doc(hidden)]
    pub fn fixture_producer_request(
        &mut self,
        routing: &Routing,
        request: &crate::keys::producer::Request,
        at: Duration,
    ) -> io::Result<bool> {
        self.producer_request(routing, request, at)
    }
    pub fn new(fence: Fence, output: Output<S>) -> Result<Self, Box<StartError<S>>> {
        if let Err(error) = fence.validate().and_then(|()| {
            if output.idle() && !output.retiring() && output.configuration_live(fence.configuration)
            {
                Ok(())
            } else {
                Err(invalid("key worker requires an idle live output"))
            }
        }) {
            return Err(Box::new(StartError { error, output }));
        }
        let serial = output.serial(fence.configuration);
        let at = output.watermark();
        Ok(Self {
            fence,
            output,
            bindings: BTreeMap::new(),
            pending: VecDeque::new(),
            bytes: 0,
            flight: None,
            started_position: None,
            serial,
            repeats: BTreeMap::new(),
            paused: false,
            retiring: None,
            draining: None,
            failed: false,
            at,
            software: BTreeMap::new(),
        })
    }

    pub fn enqueue(&mut self, work: KeyWork, at: Duration) -> io::Result<Submission> {
        self.live(at)?;
        if self.paused || self.retiring.is_some() {
            return Err(denied("key worker is not accepting work"));
        }
        if work.fence() != self.fence || work.source().seat != self.output.source().seat {
            return Err(denied("key work belongs to another coordinator or seat"));
        }
        if work
            .inputs()
            .keys()
            .any(|source| self.output.source_retired(source))
        {
            return Err(denied("key work refers to a retired source incarnation"));
        }
        self.check_position(work.scheduling_position())?;
        let (gesture, stage) = identity(work.event())?;
        let bytes = work.encoded_bytes()?;
        match work.event() {
            Event::Matched {
                action: Action::Remap { key },
                evidence: crate::rules::Evidence::Key { down },
                ..
            } if (*down && stage == Stage::Begin) || (!*down && stage == Stage::End) => {
                if work.remap_key().is_none() || !self.output.supports_key(key.0) {
                    return Err(invalid(
                        "remap requires a supported target and exact key trigger",
                    ));
                }
                if *down {
                    if self.bindings.contains_key(&gesture) {
                        return Err(invalid("remap begin is already accepted"));
                    }
                    if self.bindings.len() + self.software_binding_count() >= MAX_BINDINGS {
                        return Err(invalid("remap binding limit exceeded"));
                    }
                    self.capacity(bytes, 1)?;
                    self.bytes += bytes;
                    self.bindings.insert(
                        gesture.clone(),
                        Binding {
                            work,
                            claim: None,
                            ended: false,
                            cancelled: false,
                            bytes,
                        },
                    );
                    self.queue(Operation::Begin(gesture.clone()), 0, at);
                    Ok(Submission::Remap(Remap {
                        sink: self.output.source().clone(),
                        fence: self.fence,
                        gesture,
                    }))
                } else {
                    let Some(binding) = self.bindings.get(&gesture) else {
                        return Ok(Submission::Ignored);
                    };
                    if binding.ended || binding.cancelled {
                        return Ok(Submission::Ignored);
                    }
                    self.capacity(bytes, 1)?;
                    self.bindings.get_mut(&gesture).unwrap().ended = true;
                    self.queue(Operation::End(gesture, Box::new(work)), bytes, at);
                    Ok(Submission::Queued)
                }
            }
            Event::Matched {
                action: Action::Keys { keys },
                ..
            } if matches!(stage, Stage::Begin | Stage::Discrete) => {
                if keys.is_empty()
                    || keys.len() > 32
                    || keys.iter().any(|key| !self.output.supports_key(key.0))
                {
                    return Err(invalid("key work has an unsupported chord"));
                }
                self.capacity(bytes, 1)?;
                self.queue(Operation::Tap(Box::new(work)), bytes, at);
                Ok(Submission::Queued)
            }
            Event::Matched {
                stage: Stage::Cancel,
                ..
            } => {
                self.cancel_gesture(&gesture, at)?;
                Ok(Submission::Ignored)
            }
            _ => Ok(Submission::Ignored),
        }
    }

    /// Admit scoped work and original repeats in input order.
    pub fn admit(
        &mut self,
        routing: &Routing,
        work: Vec<KeyWork>,
        input: Option<&Frame>,
        at: Duration,
    ) -> io::Result<Vec<Submission>> {
        enum Admission {
            Work(Box<KeyWork>),
            Repeat(usize),
        }
        let mut jobs = Vec::new();
        for work in work {
            let position = work
                .position()
                .or(work.scheduling_position())
                .ok_or_else(|| invalid("key work has no original input position"))?;
            jobs.push((position, Admission::Work(Box::new(work))));
        }
        if let Some(frame) = input {
            for (index, _) in frame
                .events
                .iter()
                .enumerate()
                .filter(|(_, event)| event.kind == EV_KEY && event.value == 2)
            {
                let position = routing
                    .key_input_position(frame, index)
                    .ok_or_else(|| invalid("repeat input has no canonical coordinator position"))?;
                jobs.push((position, Admission::Repeat(index)));
            }
        }
        jobs.sort_by_key(|(position, _)| *position);
        let mut submitted = Vec::new();
        for (_, job) in jobs {
            match job {
                Admission::Work(work) => submitted.push(self.enqueue(*work, at)?),
                Admission::Repeat(index) => {
                    let frame = input.unwrap();
                    let handles = self.repeat_handles(&frame.source, frame.events[index].code);
                    if !handles.is_empty() {
                        self.repeat_positioned(routing, &handles, frame, index, at)?;
                    }
                }
            }
        }
        Ok(submitted)
    }

    /// List live remap instances for one physical source key.
    pub(crate) fn repeat_handles(&self, source: &Source, code: u16) -> Vec<Remap> {
        self.bindings
            .iter()
            .filter(|(_, binding)| {
                !binding.ended
                    && !binding.cancelled
                    && binding
                        .work
                        .remap_key()
                        .is_some_and(|key| key.source == *source && key.code == code)
            })
            .map(|(gesture, _)| Remap {
                sink: self.output.source().clone(),
                fence: self.fence,
                gesture: gesture.clone(),
            })
            .collect()
    }

    /// Forward one owned source repeat to each distinct output code once.
    pub fn repeat(
        &mut self,
        handles: &[Remap],
        frame: &Frame,
        index: usize,
        at: Duration,
    ) -> io::Result<()> {
        self.repeat_inner(handles, frame, index, at, None)
    }

    pub fn repeat_positioned(
        &mut self,
        routing: &Routing,
        handles: &[Remap],
        frame: &Frame,
        index: usize,
        at: Duration,
    ) -> io::Result<()> {
        if !routing.key_lane(self.fence, at) {
            return Err(denied("repeat coordinator is unavailable"));
        }
        let position = routing
            .key_repeat_position(frame, index)
            .ok_or_else(|| invalid("repeat has no original coordinator input"))?;
        self.repeat_inner(handles, frame, index, at, Some(position))
    }

    fn repeat_inner(
        &mut self,
        handles: &[Remap],
        frame: &Frame,
        index: usize,
        at: Duration,
        position: Option<InputPosition>,
    ) -> io::Result<()> {
        self.live(at)?;
        if self.paused || self.retiring.is_some() {
            return Err(denied("key worker is not accepting repeats"));
        }
        let event = frame
            .events
            .get(index)
            .filter(|event| event.kind == EV_KEY && event.value == 2)
            .ok_or_else(|| invalid("repeat requires its exact input event"))?;
        let key = SourceKey {
            source: frame.source.clone(),
            code: event.code,
        };
        if self
            .repeats
            .get(&key)
            .is_some_and(|position| (frame.sequence, index) <= *position)
        {
            return Err(invalid("source repeat was already accepted or reordered"));
        }
        let mut members = BTreeSet::new();
        let mut inclusive = false;
        for handle in handles {
            if handle.sink != *self.output.source() || handle.fence != self.fence {
                return Err(denied("repeat handle belongs to another output"));
            }
            let binding = self
                .bindings
                .get(&handle.gesture)
                .filter(|binding| {
                    !binding.ended
                        && !binding.cancelled
                        && binding.work.remap_key().as_ref() == Some(&key)
                        && binding
                            .work
                            .inputs()
                            .get(&frame.source)
                            .is_none_or(|sequence| *sequence <= frame.sequence)
                })
                .ok_or_else(|| denied("repeat handle has no live matching press"))?;
            inclusive |= binding.work.observe();
            if !members.insert(handle.gesture.clone()) {
                return Err(invalid("duplicate repeat handle"));
            }
        }
        if members.is_empty() {
            return Err(invalid("repeat requires at least one active remap"));
        }
        let bytes = members.len().saturating_mul(512);
        self.check_position(position)?;
        self.capacity(bytes, 1)?;
        self.repeats.insert(key, (frame.sequence, index));
        self.queue(
            Operation::Repeat {
                members: members.into_iter().collect(),
                inputs: BTreeMap::from([(frame.source.clone(), frame.sequence)]),
                emitted: BTreeSet::new(),
                position,
                inclusive,
            },
            bytes,
            at,
        );
        Ok(())
    }

    pub fn cancel(&mut self, fence: Fence, event: &Event, at: Duration) -> io::Result<Progress> {
        if fence != self.fence {
            return Err(denied("cancellation belongs to another coordinator"));
        }
        let gesture = match event {
            Event::Cancelled {
                source, rule, id, ..
            } => Gesture {
                source: source.clone(),
                rule: rule.clone(),
                id: *id,
            },
            _ => return Err(invalid("expected a gesture cancellation")),
        };
        self.cancel_gesture(&gesture, at)
    }

    fn cancel_gesture(&mut self, gesture: &Gesture, at: Duration) -> io::Result<Progress> {
        let at = at.max(self.at);
        self.at = at;
        let mut cleanup = false;
        self.remove_pending(|operation| operation.gesture().as_ref() == Some(gesture));
        if let Some(binding) = self.bindings.get_mut(gesture) {
            binding.cancelled = true;
            if let Some(claim) = &binding.claim {
                cleanup = true;
                self.output.cancel(claim, at)?;
            }
        }
        if let Some(Flight {
            kind: FlightKind::Tap(work, _),
            ..
        }) = &self.flight
            && identity(work.event())?.0 == *gesture
        {
            cleanup = true;
            let claim = self.flight_claim().unwrap();
            self.output.cancel(&claim, at)?;
        }
        self.prune();
        if cleanup {
            self.mark_draining(at);
        }
        Ok(if self.settled() {
            Progress::Complete
        } else {
            Progress::Pending
        })
    }

    pub fn retire_source(&mut self, source: &Source, at: Duration) -> io::Result<Progress> {
        let at = at.max(self.at);
        if source.seat != self.output.source().seat {
            return Err(invalid("retired source belongs to another seat"));
        }
        if source.generation == 0 {
            return Err(invalid("retired source generation is zero"));
        }
        self.at = at;
        let affected = self
            .bindings
            .iter()
            .filter(|(_, binding)| binding.work.affects(source))
            .map(|(gesture, _)| gesture.clone())
            .collect::<Vec<_>>();
        for gesture in affected {
            self.cancel_gesture(&gesture, at)?;
        }
        self.remove_pending(
            |operation| matches!(operation, Operation::Tap(work) if work.affects(source)),
        );
        if let Some(Flight {
            kind: FlightKind::Tap(work, claim),
            ..
        }) = &self.flight
            && work.affects(source)
        {
            self.output.cancel(claim, at)?;
            self.mark_draining(at);
        }
        self.output.retire_source(source, at)?;
        if self.output.retiring() {
            return self.retire(at);
        }
        self.prune();
        Ok(if self.settled() {
            Progress::Complete
        } else {
            Progress::Pending
        })
    }

    pub fn pause(&mut self, routing: &mut Routing, at: Duration) -> io::Result<Progress> {
        if !routing.key_coordinator(self.fence) {
            return Err(denied("pause belongs to another coordinator"));
        }
        routing.revoke_key_work();
        self.paused = true;
        self.clear(at, false)
    }
    pub fn resume(&mut self, routing: &Routing, at: Duration) -> io::Result<()> {
        self.live(at)?;
        if self.retiring.is_some()
            || !self.settled()
            || !self.output.idle()
            || !routing.key_lane(self.fence, at)
        {
            return Err(denied(
                "key worker requires fresh authority and drained output before resume",
            ));
        }
        self.paused = false;
        self.at = at;
        Ok(())
    }
    pub fn retire(&mut self, at: Duration) -> io::Result<Progress> {
        let at = at.max(self.at);
        self.retiring.get_or_insert(at.saturating_add(DRAIN));
        self.clear(at, true)
    }
    pub(crate) fn retirement_deadline(&self) -> Option<Duration> {
        self.retiring
    }
    fn clear(&mut self, at: Duration, retire: bool) -> io::Result<Progress> {
        let at = at.max(self.at);
        self.at = at;
        self.remove_pending(|_| true);
        self.retire_software(at)?;
        for binding in self.bindings.values_mut() {
            binding.cancelled = true;
        }
        let result = if retire {
            self.output.retire(at)
        } else {
            self.output.cancel_all(at)
        }?;
        self.prune();
        self.mark_draining(at);
        Ok(result)
    }

    pub fn drain(&mut self, at: Duration) -> io::Result<Progress> {
        if !self.paused && self.retiring.is_none() {
            return Err(invalid("active key work requires route validation"));
        }
        let result = self.drain_inner(at);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn drain_inner(&mut self, at: Duration) -> io::Result<Progress> {
        self.live(at)?;
        self.at = at;
        if self
            .retiring
            .into_iter()
            .chain(self.draining)
            .any(|deadline| at >= deadline)
            && !self.output.settled()
        {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "generated keyboard cleanup did not drain",
            ));
        }
        self.output.flush()?;
        self.prune();
        Ok(if self.settled() {
            Progress::Complete
        } else {
            Progress::Pending
        })
    }

    pub fn poll(&mut self, routing: &Routing, at: Duration) -> io::Result<Progress> {
        let result = self.poll_inner(routing, at);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn poll_inner(&mut self, routing: &Routing, at: Duration) -> io::Result<Progress> {
        self.live(at)?;
        self.at = at;
        self.expire_software(routing, at)?;
        if self.output.retiring() && self.retiring.is_none() {
            self.paused = true;
            self.retire(at)?;
        }
        if self.retiring.is_some_and(|deadline| at >= deadline) && !self.settled() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "generated keyboard cleanup did not drain",
            ));
        }
        if self.retiring.is_none() && !routing.key_lane(self.fence, at) {
            self.paused = true;
            self.clear(at, false)?;
        }
        let invalid: Vec<_> = self
            .bindings
            .iter()
            .filter(|(_, binding)| !binding.cancelled && !binding.work.available(routing, at))
            .map(|(gesture, _)| gesture.clone())
            .collect();
        for gesture in invalid {
            self.cancel_gesture(&gesture, at)?;
        }
        if let Some(Flight {
            kind: FlightKind::Tap(work, _),
            ..
        }) = &self.flight
            && !work.available(routing, at)
        {
            let claim = self.flight_claim().unwrap();
            self.output.cancel(&claim, at)?;
            self.mark_draining(at);
        }
        self.remove_pending(
            |operation| matches!(operation, Operation::Tap(work) if !work.available(routing,at)),
        );
        if self.draining.is_some_and(|deadline| at >= deadline) && !self.output.settled()
            || self.retiring.is_none()
                && self
                    .flight
                    .as_ref()
                    .is_some_and(|flight| at >= flight.deadline)
            || self
                .pending
                .iter()
                .any(|pending| at.saturating_sub(pending.received) >= MAX_AGE)
        {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "key output work or cleanup exceeded its deadline",
            ));
        }
        self.output.flush()?;
        self.prune();
        if !self.output.settled() {
            return Ok(Progress::Pending);
        }
        if self.paused || self.retiring.is_some() {
            return Ok(if self.settled() {
                Progress::Complete
            } else {
                Progress::Pending
            });
        }
        for _ in 0..8 {
            let Some(front) = self.pending.front() else {
                return Ok(Progress::Complete);
            };
            if at.saturating_sub(front.received) >= MAX_AGE {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "queued key work expired",
                ));
            }
            if !self.prefix_ready(&front.operation, routing) {
                return Ok(Progress::Pending);
            }
            let pending = self.pending.pop_front().unwrap();
            self.bytes -= pending.bytes;
            self.check_position(pending.position)?;
            if pending.position.is_some() {
                self.started_position = pending.position;
            }
            if let Some(operation) = self.execute(pending.operation, routing, at)? {
                self.bytes += pending.bytes;
                self.pending.push_front(Pending {
                    operation,
                    position: pending.position,
                    received: pending.received,
                    bytes: pending.bytes,
                });
            } else if let Some(Flight {
                kind: FlightKind::Tap(_, _) | FlightKind::Software { .. },
                bytes,
                ..
            }) = &mut self.flight
            {
                *bytes = pending.bytes;
                self.bytes += pending.bytes;
            }
            self.prune();
            if !self.output.settled() {
                return Ok(Progress::Pending);
            }
        }
        Ok(if self.pending.is_empty() {
            Progress::Complete
        } else {
            Progress::Pending
        })
    }

    fn execute(
        &mut self,
        operation: Operation,
        routing: &Routing,
        at: Duration,
    ) -> io::Result<Option<Operation>> {
        match operation {
            Operation::Software(work) => self.execute_software(work, at)?,
            Operation::Begin(gesture) => {
                let Some(binding) = self
                    .bindings
                    .get(&gesture)
                    .filter(|binding| !binding.cancelled && binding.work.available(routing, at))
                else {
                    return Ok(None);
                };
                let key = binding.work.remap_key().unwrap();
                let Action::Remap { key: target } = action(binding.work.event()) else {
                    unreachable!()
                };
                let target = target.0;
                let inputs = binding.work.inputs().clone();
                let position = binding.work.scheduling_position();
                let serial = self.next_serial()?;
                let claim = Claim {
                    owner: Owner::Remap {
                        key: key.clone(),
                        generation: self.fence.configuration,
                        id: serial,
                    },
                    origin: Some(key.source),
                    configuration: self.fence.configuration,
                    serial,
                };
                self.output.hold(claim.clone(), &[target], at)?;
                self.bindings.get_mut(&gesture).unwrap().claim = Some(claim);
                if !self.output.settled() {
                    self.flight = Some(Flight {
                        position,
                        kind: FlightKind::Begin,
                        inputs,
                        deadline: at.saturating_add(MAX_AGE),
                        bytes: 0,
                    });
                }
            }
            Operation::End(gesture, work) => {
                if let Some(claim) = self
                    .bindings
                    .get(&gesture)
                    .and_then(|binding| binding.claim.as_ref())
                {
                    self.output.release(claim, at)?;
                    if !self.output.settled() {
                        self.flight = Some(Flight {
                            position: work.scheduling_position(),
                            kind: FlightKind::End(gesture),
                            inputs: work.inputs().clone(),
                            deadline: at.saturating_add(MAX_AGE),
                            bytes: 0,
                        });
                    } else {
                        self.remove_binding(&gesture);
                    }
                } else {
                    self.remove_binding(&gesture);
                }
            }
            Operation::Tap(work) => {
                if !work.available(routing, at) {
                    return Ok(None);
                }
                let Action::Keys { keys } = action(work.event()) else {
                    unreachable!()
                };
                let keys = keys.iter().map(|key| key.0).collect::<Vec<_>>();
                let serial = self.next_serial()?;
                let claim = Claim {
                    owner: Owner::Synthetic {
                        seat: self.output.source().seat.clone(),
                        generation: self.fence.configuration,
                        id: serial,
                    },
                    origin: Some(work.source().clone()),
                    configuration: self.fence.configuration,
                    serial,
                };
                self.output.tap(claim.clone(), &keys, at)?;
                if !self.output.settled() {
                    self.flight = Some(Flight {
                        position: work.scheduling_position(),
                        inputs: work.inputs().clone(),
                        kind: FlightKind::Tap(work, claim),
                        deadline: at.saturating_add(MAX_AGE),
                        bytes: 0,
                    });
                }
            }
            Operation::Repeat {
                members,
                inputs,
                mut emitted,
                position,
                inclusive,
            } => {
                let claim = members
                    .iter()
                    .filter_map(|member| self.bindings.get(member))
                    .filter(|binding| !binding.cancelled && binding.work.available(routing, at))
                    .find_map(|binding| {
                        let Action::Remap { key } = action(binding.work.event()) else {
                            return None;
                        };
                        (!emitted.contains(&key.0))
                            .then(|| binding.claim.clone().map(|claim| (key.0, claim)))
                            .flatten()
                    });
                if let Some((code, claim)) = claim {
                    self.output.repeat(&claim, at)?;
                    emitted.insert(code);
                    if !self.output.settled() {
                        self.flight = Some(Flight {
                            position,
                            kind: FlightKind::Repeat,
                            inputs: inputs.clone(),
                            deadline: at.saturating_add(MAX_AGE),
                            bytes: 0,
                        });
                    }
                    return Ok(Some(Operation::Repeat {
                        members,
                        inputs,
                        emitted,
                        position,
                        inclusive,
                    }));
                }
            }
        }
        Ok(None)
    }

    pub fn blocks_raw(&self, source: &Source, sequence: u64) -> bool {
        if source.seat != self.output.source().seat {
            return false;
        }
        if self.software_cleanup_pending() {
            return true;
        }
        self.flight.as_ref().is_some_and(|flight| {
            if matches!(flight.kind, FlightKind::Software { .. }) {
                sequence > flight.inputs.get(source).copied().unwrap_or(0)
            } else {
                flight
                    .inputs
                    .get(source)
                    .is_some_and(|bound| sequence >= *bound)
            }
        }) || self
            .bindings
            .values()
            .any(|binding| binding.cancelled && binding.work.affects(source))
            || self.pending.iter().any(|pending| match &pending.operation {
                Operation::Software(work) => {
                    sequence > work.inputs.get(source).copied().unwrap_or(0)
                }
                Operation::Begin(gesture) => self
                    .bindings
                    .get(gesture)
                    .is_some_and(|binding| binding.work.blocks_raw(source, sequence)),
                Operation::End(_, work) | Operation::Tap(work) => work.blocks_raw(source, sequence),
                Operation::Repeat { inputs, .. } => {
                    inputs.get(source).is_some_and(|bound| sequence >= *bound)
                }
            })
    }
    /// Retain key-domain ordering and seat-wide neutral cleanup for matching routes.
    pub fn blocks_ordered_span(&self, routing: &Routing, source: &Source, span: InputSpan) -> bool {
        if source.seat != self.output.source().seat {
            return false;
        }
        if !routing.key_coordinator(self.fence) {
            return true;
        }
        (routing.key_orders_source(source)
            || self.draining.is_some()
            || self.software_cleanup_pending()
            || self.bindings.values().any(|binding| binding.cancelled))
            && self.blocks_raw_span(source, span)
    }

    #[cfg(feature = "native-test-harness")]
    pub(crate) fn fixture_settled(&self) -> bool {
        self.pending.is_empty() && self.flight.is_none() && self.draining.is_none()
    }
    pub fn blocks_raw_span(&self, source: &Source, span: InputSpan) -> bool {
        if source.seat != self.output.source().seat {
            return false;
        }
        if self.draining.is_some()
            || self.software_cleanup_pending()
            || self.bindings.values().any(|binding| binding.cancelled)
        {
            return true;
        }
        self.flight.as_ref().is_some_and(|flight| {
            flight.position.map_or_else(
                || {
                    flight
                        .inputs
                        .get(source)
                        .is_some_and(|sequence| span.sequence >= *sequence)
                },
                |position| span.last > position,
            )
        }) || self.pending.iter().any(|pending| match &pending.operation {
            Operation::Software(work) => span.last > work.position,
            Operation::Begin(gesture) => self
                .bindings
                .get(gesture)
                .is_some_and(|binding| Self::work_blocks_span(&binding.work, source, span)),
            Operation::End(_, work) | Operation::Tap(work) => {
                Self::work_blocks_span(work, source, span)
            }
            Operation::Repeat {
                position, inputs, ..
            } => position.map_or_else(
                || {
                    inputs
                        .get(source)
                        .is_some_and(|sequence| span.sequence >= *sequence)
                },
                |position| span.last > position,
            ),
        })
    }
    fn work_blocks_span(work: &KeyWork, source: &Source, span: InputSpan) -> bool {
        if work.scheduling_position().is_some() {
            work.blocks_raw_span(span)
        } else {
            work.blocks_raw(source, span.sequence)
        }
    }
    fn prefix_ready(&self, operation: &Operation, routing: &Routing) -> bool {
        match operation {
            Operation::Software(work) => {
                routing.key_position_prefix_ready(work.position, true, &self.output.source().seat)
            }
            Operation::Begin(gesture) => self
                .bindings
                .get(gesture)
                .is_none_or(|binding| binding.work.input_prefix_ready(routing)),
            Operation::End(_, work) | Operation::Tap(work) => work.input_prefix_ready(routing),
            Operation::Repeat {
                inputs,
                position,
                inclusive,
                ..
            } => position.map_or_else(
                || routing.key_prefix_ready(inputs, false),
                |position| {
                    routing.key_position_prefix_ready(
                        position,
                        *inclusive,
                        &self.output.source().seat,
                    )
                },
            ),
        }
    }
    fn flight_claim(&self) -> Option<Claim> {
        match &self.flight.as_ref()?.kind {
            FlightKind::Tap(_, claim) => Some(claim.clone()),
            _ => None,
        }
    }
    fn prune(&mut self) {
        if self.output.settled()
            && let Some(flight) = self.flight.take()
        {
            self.bytes -= flight.bytes;
            if let FlightKind::End(gesture) = flight.kind {
                self.remove_binding(&gesture);
            } else if let FlightKind::Software {
                producer,
                id,
                release: true,
            } = flight.kind
            {
                self.remove_software_press(producer, id);
            }
        }
        let removed: Vec<_> = self
            .bindings
            .iter()
            .filter(|(_, binding)| {
                binding.cancelled
                    && binding
                        .claim
                        .as_ref()
                        .is_none_or(|claim| !self.output.contains_claim(claim))
            })
            .map(|(gesture, _)| gesture.clone())
            .collect();
        for gesture in removed {
            self.remove_binding(&gesture);
        }
        self.prune_software();
        self.repeats.retain(|key, _| {
            self.bindings
                .values()
                .any(|binding| binding.work.remap_key().as_ref() == Some(key))
        });
        if self.output.settled()
            && !self.software_cleanup_pending()
            && self.bindings.values().all(|binding| !binding.cancelled)
        {
            self.draining = None;
        }
    }
    fn mark_draining(&mut self, at: Duration) {
        if !self.output.settled()
            || self.software_cleanup_pending()
            || self.bindings.values().any(|binding| binding.cancelled)
        {
            self.draining.get_or_insert(at.saturating_add(DRAIN));
        }
    }
    fn remove_binding(&mut self, gesture: &Gesture) {
        if let Some(binding) = self.bindings.remove(gesture) {
            self.bytes -= binding.bytes;
        }
    }
    fn remove_pending(&mut self, mut remove: impl FnMut(&Operation) -> bool) {
        self.pending.retain(|pending| {
            if remove(&pending.operation) {
                self.bytes -= pending.bytes;
                false
            } else {
                true
            }
        });
    }
    fn queue(&mut self, operation: Operation, bytes: usize, at: Duration) {
        let position = match &operation {
            Operation::Software(work) => Some(work.position),
            Operation::Begin(gesture) => self
                .bindings
                .get(gesture)
                .and_then(|binding| binding.work.scheduling_position()),
            Operation::End(_, work) | Operation::Tap(work) => work.scheduling_position(),
            Operation::Repeat { position, .. } => *position,
        };
        let index = position.map_or(self.pending.len(), |position| {
            self.pending
                .iter()
                .rposition(|pending| pending.position.is_none_or(|prior| prior <= position))
                .map_or(0, |index| index + 1)
        });
        self.pending.insert(
            index,
            Pending {
                operation,
                position,
                received: at,
                bytes,
            },
        );
        self.bytes += bytes;
        self.at = at;
    }
    fn check_position(&self, position: Option<InputPosition>) -> io::Result<()> {
        if position
            .zip(self.started_position)
            .is_some_and(|(position, started)| position < started)
        {
            return Err(invalid("key work precedes an already started operation"));
        }
        Ok(())
    }
    fn capacity(&self, bytes: usize, count: usize) -> io::Result<()> {
        if self.pending.len().saturating_add(count) > MAX_PENDING
            || self.bytes.saturating_add(bytes) > MAX_BYTES
        {
            return Err(invalid("key worker queue limit exceeded"));
        }
        Ok(())
    }
    fn next_serial(&mut self) -> io::Result<u64> {
        self.serial = self
            .serial
            .checked_add(1)
            .ok_or_else(|| invalid("key output submission serial exhausted"))?;
        Ok(self.serial)
    }
    fn live(&self, at: Duration) -> io::Result<()> {
        if self.output.failed() || self.failed && self.retiring.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "key output failed; recovery required",
            ));
        }
        if at < self.at {
            return Err(invalid("key worker time moved backwards"));
        }
        Ok(())
    }
    pub fn settled(&self) -> bool {
        self.pending.is_empty()
            && self.flight.is_none()
            && self.bindings.values().all(|binding| !binding.cancelled)
            && !self.software_cleanup_pending()
            && self.output.settled()
    }
    pub fn down(&self, code: u16) -> bool {
        self.output.down(code)
    }
    pub fn failed(&self) -> bool {
        self.failed || self.output.failed()
    }
    pub fn uncertain_frame(&self) -> Option<&Frame> {
        self.output.uncertain_frame()
    }
    pub fn paused(&self) -> bool {
        self.paused
    }
    pub fn source(&self) -> &Source {
        self.output.source()
    }
    #[cfg(target_os = "linux")]
    pub fn poll_fd(&self) -> Option<std::os::fd::BorrowedFd<'_>>
    where
        S: std::os::fd::AsFd,
    {
        if self.output.settled() {
            None
        } else {
            self.output.poll_fd()
        }
    }
    pub fn deadline(&self) -> Option<Duration> {
        if self.settled() {
            return self.software_deadline();
        }
        if self.paused || self.retiring.is_some() {
            return self.retiring.into_iter().chain(self.draining).min();
        }
        self.retiring
            .into_iter()
            .chain(self.draining)
            .chain(self.software_deadline())
            .chain(self.flight.as_ref().map(|flight| flight.deadline))
            .chain(
                self.pending
                    .iter()
                    .map(|pending| pending.received.saturating_add(MAX_AGE)),
            )
            .min()
    }
    pub fn into_output(self) -> Result<Output<S>, Box<Self>> {
        if !self.failed && self.settled() && self.bindings.is_empty() && self.software.is_empty() {
            Ok(self.output)
        } else {
            Err(Box::new(self))
        }
    }
}
impl Operation {
    fn gesture(&self) -> Option<Gesture> {
        match self {
            Self::Begin(gesture) | Self::End(gesture, _) => Some(gesture.clone()),
            Self::Tap(work) => identity(work.event()).ok().map(|(gesture, _)| gesture),
            Self::Repeat { .. } | Self::Software(_) => None,
        }
    }
}
fn identity(event: &Event) -> io::Result<(Gesture, Stage)> {
    if let Event::Matched {
        source,
        rule,
        id,
        stage,
        ..
    } = event
    {
        Ok((
            Gesture {
                source: source.clone(),
                rule: rule.clone(),
                id: *id,
            },
            *stage,
        ))
    } else {
        Err(invalid("key work is not a match"))
    }
}
fn action(event: &Event) -> &Action {
    match event {
        Event::Matched { action, .. } => action,
        _ => unreachable!(),
    }
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
fn denied(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

#[cfg(test)]
pub(crate) mod tests;
