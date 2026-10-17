//! Recorded context and source-bound callbacks on an isolated virtual timeline.
use super::{MAX_LINE_BYTES, MAX_RECORDING_BYTES, MAX_RECORDS, Record};
use crate::{
    config::Config,
    context::{Authority, Binding, Snapshot, State},
    context_transfer::View,
    input::Source,
    input_events::ResetReason,
    lua::{CapabilityView, Definition, Runtime},
    protocol::{Capability, Epoch, Fence},
    recognition::{Metadata, metadata_selectors},
    rules::{Engine, Event},
    worker::ProvidedContext,
};
use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    io,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

pub(super) const MAX_SEATS: usize = 16;
const MAX_VMS: usize = 4;
pub(super) const MAX_OUTCOMES: usize = 16_384;
const MAX_CALLBACKS: usize = 4096;
pub(super) const MAX_WORK: Duration = Duration::from_millis(250);

#[derive(Clone, Default)]
pub(crate) struct Budget(Rc<Cell<usize>>);
struct Lease(Budget);
impl Budget {
    fn acquire(&self) -> io::Result<Lease> {
        if self.0.get() >= MAX_VMS {
            return Err(invalid("offline callback VM limit exceeded"));
        }
        self.0.set(self.0.get() + 1);
        Ok(Lease(self.clone()))
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.0.0.set(self.0.0.get() - 1);
    }
}
struct Callbacks {
    runtime: Runtime,
    _lease: Lease,
}
pub(super) trait Coordinator {
    fn authority(&self) -> &Authority;
    fn take_recognition_cancellations(&mut self) -> Vec<crate::recognition::Cancellation>;
    fn recognition_ready(&mut self, ready: bool, at: Duration) -> io::Result<()>;
    fn recognition_requests(
        &mut self,
        at: Duration,
        now: Duration,
    ) -> io::Result<Vec<crate::recognition::Ticket>>;
    fn recognition_candidate_live(&self, id: u64) -> bool;
    fn recognition_response(
        &mut self,
        proposal: crate::recognition::Proposal,
        at: Duration,
        now: Duration,
    ) -> io::Result<()>;
    fn input_states(&self, at: Duration) -> Vec<crate::input_events::Event>;
}
impl Coordinator for Engine {
    fn authority(&self) -> &Authority {
        self.authority()
    }
    fn take_recognition_cancellations(&mut self) -> Vec<crate::recognition::Cancellation> {
        self.take_recognition_cancellations()
    }
    fn recognition_ready(&mut self, ready: bool, at: Duration) -> io::Result<()> {
        self.recognition_ready(ready, at)
    }
    fn recognition_requests(
        &mut self,
        at: Duration,
        _now: Duration,
    ) -> io::Result<Vec<crate::recognition::Ticket>> {
        self.recognition_requests(at)
    }
    fn recognition_candidate_live(&self, id: u64) -> bool {
        self.recognition_candidate_live(id)
    }
    fn recognition_response(
        &mut self,
        proposal: crate::recognition::Proposal,
        _at: Duration,
        now: Duration,
    ) -> io::Result<()> {
        self.recognition_response(proposal, now)
    }
    fn input_states(&self, at: Duration) -> Vec<crate::input_events::Event> {
        self.input_states(at)
    }
}
impl Coordinator for crate::routing::Routing {
    fn authority(&self) -> &Authority {
        self.authority()
    }
    fn take_recognition_cancellations(&mut self) -> Vec<crate::recognition::Cancellation> {
        self.take_recognition_cancellations()
    }
    fn recognition_ready(&mut self, ready: bool, at: Duration) -> io::Result<()> {
        self.recognition_ready(ready, at)
    }
    fn recognition_requests(
        &mut self,
        at: Duration,
        now: Duration,
    ) -> io::Result<Vec<crate::recognition::Ticket>> {
        self.recognition_requests_after_input(at, now)
    }
    fn recognition_candidate_live(&self, id: u64) -> bool {
        self.recognition_candidate_live(id)
    }
    fn recognition_response(
        &mut self,
        proposal: crate::recognition::Proposal,
        at: Duration,
        now: Duration,
    ) -> io::Result<()> {
        self.recognition_response_after_input(proposal, at, now)
    }
    fn input_states(&self, at: Duration) -> Vec<crate::input_events::Event> {
        self.input_states(at)
    }
}
pub(super) struct Seat<E = Engine> {
    pub(super) engine: E,
    callbacks: Option<Callbacks>,
    pub(super) binding: Binding,
    pub(super) snapshot: Snapshot,
    pub(super) fence: Fence,
    at: Duration,
    pub(super) expires: Instant,
}
impl<E: Coordinator> Seat<E> {
    pub(super) fn new(
        engine: E,
        binding: Binding,
        snapshot: Snapshot,
        fence: Fence,
        at: Duration,
        expires: Instant,
    ) -> Self {
        Self {
            engine,
            callbacks: None,
            binding,
            snapshot,
            fence,
            at,
            expires,
        }
    }
    fn view(&self, at: Duration) -> View {
        let state = self.engine.authority().state(at);
        View {
            fence: self.fence,
            sampled_at: at,
            state,
            context: (state == self.snapshot.state).then(|| {
                Box::new(ProvidedContext {
                    binding: self.binding.clone(),
                    snapshot: self.snapshot.clone(),
                })
            }),
            morf: vec![],
            morf_rendered: vec![],
            error: None,
        }
    }
    pub(super) fn clock(&mut self, at: Duration) -> io::Result<()> {
        let view = self.view(at);
        if let Some(callbacks) = &mut self.callbacks {
            callbacks.runtime.set_context(view, at).map_err(invalid)?;
            callbacks
                .runtime
                .set_capabilities(
                    CapabilityView {
                        fence: self.fence,
                        sampled_at: at,
                        negotiated: BTreeSet::from([
                            Capability::CustomRecognizers,
                            Capability::LuaEvents,
                            Capability::Observations,
                            Capability::Context,
                        ]),
                        ready: BTreeSet::new(),
                    },
                    at,
                )
                .map_err(invalid)?;
        }
        self.at = at;
        Ok(())
    }
    fn cleanup(&mut self) -> io::Result<()> {
        let cancellations = self.engine.take_recognition_cancellations();
        if let Some(callbacks) = &mut self.callbacks {
            for cancellation in cancellations {
                work(self.expires)?;
                callbacks
                    .runtime
                    .cancel_recognition(&cancellation)
                    .map_err(invalid)?;
            }
        }
        Ok(())
    }
    pub(super) fn retire(&mut self, at: Duration) -> io::Result<()> {
        self.engine.recognition_ready(false, at)?;
        self.cleanup()
    }
    fn settle(
        &mut self,
        config: &Config,
        definition: Option<&Definition>,
        budget: &Budget,
        at: Duration,
    ) -> io::Result<()> {
        self.settle_clocked(config, definition, budget, at, at)
    }
    pub(super) fn settle_clocked(
        &mut self,
        config: &Config,
        definition: Option<&Definition>,
        budget: &Budget,
        at: Duration,
        now: Duration,
    ) -> io::Result<()> {
        self.clock(now)?;
        for _ in 0..MAX_CALLBACKS {
            work(self.expires)?;
            self.cleanup()?;
            let tickets = self.engine.recognition_requests(at, now)?;
            if tickets.is_empty() {
                return self.cleanup();
            }
            if self.callbacks.is_none() {
                let lease = budget.acquire()?;
                let expires = self.expires;
                let mut runtime = definition
                    .ok_or_else(|| invalid("custom replay requires its original Lua source"))?
                    .instantiate_progress(&mut || work(expires))
                    .map_err(invalid)?;
                runtime.offline(config.clone()).map_err(invalid)?;
                self.callbacks = Some(Callbacks {
                    runtime,
                    _lease: lease,
                });
                self.clock(now)?;
                self.metadata(config, self.engine.input_states(now), now)?;
            }
            for ticket in tickets {
                work(self.expires)?;
                self.cleanup()?;
                if !self.engine.recognition_candidate_live(ticket.candidate) {
                    continue;
                }
                let result = self
                    .callbacks
                    .as_mut()
                    .unwrap()
                    .runtime
                    .recognize(&ticket)
                    .map_err(invalid)?;
                if let Some(proposal) = result.proposal {
                    self.engine.recognition_response(proposal, at, now)?;
                }
                self.cleanup()?;
            }
        }
        Err(invalid("offline callback transition limit exceeded"))
    }
    pub(super) fn metadata(
        &mut self,
        config: &Config,
        inputs: Vec<crate::input_events::Event>,
        at: Duration,
    ) -> io::Result<()> {
        if self.engine.authority().state(at) != State::Desktop || self.snapshot.secure_overlay {
            return Ok(());
        }
        let Some(callbacks) = &mut self.callbacks else {
            return Ok(());
        };
        let scope = metadata_selectors(config);
        for input in inputs {
            if let Some(metadata) = Metadata::project(input, &scope)? {
                callbacks
                    .runtime
                    .recognizer_metadata(self.fence, &metadata)
                    .map_err(invalid)?;
            }
        }
        Ok(())
    }
}

/// Pure configured recognition. Recorded context is simulation data, not live
/// provider authentication, input ownership or an action execution grant.
pub struct Configured {
    config: Config,
    definition: Option<Arc<Definition>>,
    budget: Budget,
    baseline: Option<Engine>,
    seats: BTreeMap<String, Seat>,
    events: Vec<Event>,
    expected: Option<Vec<Event>>,
    at: Duration,
    header: bool,
    records: usize,
    bytes: usize,
    outcome_bytes: usize,
    input_seen: bool,
    expires: Instant,
}
impl Configured {
    pub fn new(config: Config, definition: Option<Definition>) -> io::Result<Self> {
        Self::with_budget(config, definition.map(Arc::new), Budget::default())
    }
    pub(crate) fn with_budget(
        config: Config,
        definition: Option<Arc<Definition>>,
        budget: Budget,
    ) -> io::Result<Self> {
        approved(&config, definition.as_deref())?;
        let baseline = Engine::new(crate::worker::preflight_profile(&config))?;
        Ok(Self {
            config,
            definition,
            budget,
            baseline: Some(baseline),
            seats: BTreeMap::new(),
            events: vec![],
            expected: None,
            at: Duration::ZERO,
            header: false,
            records: 0,
            bytes: 0,
            outcome_bytes: 2,
            input_seen: false,
            expires: Instant::now(),
        })
    }
    pub fn events(&self) -> &[Event] {
        &self.events
    }
    pub fn push(&mut self, record: Record) -> io::Result<()> {
        if self.baseline.is_none() {
            return Err(invalid("configured replay is closed"));
        }
        self.expires = Instant::now() + MAX_WORK;
        let result = self.push_inner(record).and_then(|()| work(self.expires));
        if result.is_err() {
            self.baseline = None;
            self.seats.clear();
        }
        result
    }
    fn push_inner(&mut self, record: Record) -> io::Result<()> {
        if self.expected.is_some() {
            return Err(invalid("expected rule outcomes must be the final record"));
        }
        let bytes = crate::lua::bounded_json(&record, MAX_LINE_BYTES)?.len();
        if self.records >= MAX_RECORDS || bytes > MAX_RECORDING_BYTES.saturating_sub(self.bytes) {
            return Err(invalid("configured replay recording limit exceeded"));
        }
        if !self.header && !matches!(record, Record::Header { .. }) {
            return Err(invalid("recording must start with a version header"));
        }
        match record {
            Record::ExpectedRules { events } => {
                if !self.input_seen {
                    return Err(invalid("rule expectations require registered input"));
                }
                if events.len() > MAX_OUTCOMES {
                    return Err(invalid("too many expected rule outcomes"));
                }
                self.expected = Some(events);
            }
            Record::Context {
                at,
                binding,
                snapshot,
            } => self.context(at, binding, *snapshot)?,
            Record::Expected { .. } => {
                return Err(invalid(
                    "generic expectations cannot check configured replay",
                ));
            }
            record => {
                let at = match &record {
                    Record::Frame { frame } => frame.at,
                    Record::Tick { at }
                    | Record::Mapping { at, .. }
                    | Record::Reconcile { at, .. }
                    | Record::Remove { at, .. } => *at,
                    _ => self.at,
                };
                self.time(at)?;
                self.baseline.as_mut().unwrap().push(record.clone())?;
                self.input_seen |= matches!(record, Record::Device { .. });
                self.header = true;
                for (name, seat) in &mut self.seats {
                    seat.expires = self.expires;
                    if source(&record).is_some_and(|source| &source.seat != name)
                        || matches!(record, Record::Header { .. })
                    {
                        continue;
                    }
                    let before = match &record {
                        Record::Remove { source, .. } => {
                            seat.engine.input_reset(source, at, ResetReason::Removed)
                        }
                        Record::Device { device, .. } => seat
                            .engine
                            .input_states(at)
                            .into_iter()
                            .find(|input| {
                                input.source().device == device.source.device
                                    && input.source() != &device.source
                            })
                            .and_then(|input| {
                                seat.engine
                                    .input_reset(input.source(), at, ResetReason::Removed)
                            }),
                        _ => None,
                    };
                    seat.engine.push(record.clone())?;
                    seat.clock(at)?;
                    let mut inputs: Vec<_> = before.into_iter().collect();
                    let reset = match &record {
                        Record::Device { device, .. } => {
                            Some((&device.source, ResetReason::Attached))
                        }
                        Record::Mapping { source, .. } => {
                            Some((source, ResetReason::MappingChanged))
                        }
                        Record::Reconcile { source, .. } => Some((source, ResetReason::Reconciled)),
                        _ => None,
                    };
                    if let Some((source, reason)) = reset {
                        inputs.extend(seat.engine.input_reset(source, at, reason));
                    }
                    seat.metadata(&self.config, inputs, at)?;
                    seat.settle(&self.config, self.definition.as_deref(), &self.budget, at)?;
                }
            }
        }
        self.collect()?;
        self.records += 1;
        self.bytes += bytes;
        Ok(())
    }
    fn time(&mut self, at: Duration) -> io::Result<()> {
        let maximum = Duration::from_nanos(i64::MAX as u64) - Duration::from_secs(3600);
        if at > maximum {
            return Err(invalid(
                "configured replay timestamp exceeds bounded clock range",
            ));
        }
        if at < self.at {
            return Err(invalid("configured replay clock moved backwards"));
        }
        self.at = at;
        Ok(())
    }
    fn context(&mut self, at: Duration, binding: Binding, snapshot: Snapshot) -> io::Result<()> {
        self.time(at)?;
        if !self.config.policy.allowed_seats.contains(&binding.seat) {
            return Err(invalid("recorded context seat exceeds policy"));
        }
        let authority =
            Authority::recorded(binding.clone(), snapshot.clone(), at).map_err(invalid)?;
        let name = binding.seat.clone();
        let replace = if let Some(old) = self.seats.get(&name) {
            let mut authority = old.engine.authority().clone();
            authority.tick(at);
            if snapshot.generation < old.fence.session
                || snapshot.generation == old.fence.session
                    && (binding != old.binding
                        || snapshot.revision <= old.snapshot.revision
                        || authority.generation() != old.fence.session)
            {
                return Err(invalid(
                    "recorded context generation, binding or revision regressed",
                ));
            }
            snapshot.generation != old.fence.session
        } else {
            true
        };
        if replace {
            if let Some(mut old) = self.seats.remove(&name) {
                old.expires = self.expires;
                old.retire(at)?;
                self.append(old.engine.take_events())?;
            } else if self.seats.len() >= MAX_SEATS {
                return Err(invalid("offline context seat limit exceeded"));
            }
            let fence = Fence {
                broker: Epoch([0x5052454b, 0x5245504c4159]),
                configuration: 1,
                session: snapshot.generation,
            };
            let engine = self.baseline.as_ref().unwrap().fork_observer(
                self.config.clone(),
                &name,
                fence,
                at,
            )?;
            self.seats.insert(
                name.clone(),
                Seat {
                    engine,
                    callbacks: None,
                    binding: binding.clone(),
                    snapshot: snapshot.clone(),
                    fence,
                    at,
                    expires: self.expires,
                },
            );
        }
        let seat = self.seats.get_mut(&name).unwrap();
        seat.expires = self.expires;
        *seat.engine.authority_mut() = authority;
        seat.binding = binding;
        seat.snapshot = snapshot;
        seat.clock(at)?;
        seat.engine.recognition_ready(
            seat.engine.authority().state(at) == State::Desktop && !seat.snapshot.secure_overlay,
            at,
        )?;
        seat.engine.push(Record::Tick { at })?;
        seat.settle(&self.config, self.definition.as_deref(), &self.budget, at)?;
        self.baseline.as_mut().unwrap().push(Record::Tick { at })?;
        Ok(())
    }
    fn append(&mut self, events: Vec<Event>) -> io::Result<()> {
        if self.events.len() + events.len() > MAX_OUTCOMES {
            return Err(invalid("offline replay outcome limit exceeded"));
        }
        self.outcome_bytes += crate::lua::bounded_json(
            &events,
            MAX_RECORDING_BYTES.saturating_sub(self.outcome_bytes),
        )?
        .len();
        self.events.extend(events);
        Ok(())
    }
    fn collect(&mut self) -> io::Result<()> {
        let ordinary = self
            .baseline
            .as_mut()
            .unwrap()
            .take_events()
            .into_iter()
            .filter(|event| {
                let source = match event {
                    Event::Matched { source, .. }
                    | Event::Skipped { source, .. }
                    | Event::Cancelled { source, .. } => source,
                };
                !self.seats.contains_key(&source.seat)
            })
            .collect();
        self.append(ordinary)?;
        let events = self
            .seats
            .values_mut()
            .flat_map(|seat| seat.engine.take_events())
            .collect();
        self.append(events)
    }
    pub fn check(&self) -> io::Result<()> {
        let expected = self
            .expected
            .as_ref()
            .ok_or_else(|| invalid("recording has no expected rule outcomes"))?;
        if super::similar_geometry(
            &serde_json::to_value(expected)?,
            &serde_json::to_value(&self.events)?,
        ) {
            Ok(())
        } else {
            Err(invalid(format!(
                "rule outcome mismatch: expected {} events, received {}",
                expected.len(),
                self.events.len()
            )))
        }
    }
    pub fn finish(&mut self, check: bool) -> io::Result<()> {
        if self.baseline.is_none() {
            return Err(invalid("configured replay is closed"));
        }
        self.expires = Instant::now() + MAX_WORK;
        let result = (|| {
            if self.records == 0 {
                return Err(invalid("empty configured replay"));
            }
            if check {
                self.check()?;
            }
            for seat in self.seats.values_mut() {
                seat.expires = self.expires;
                seat.retire(self.at)?;
            }
            Ok(())
        })()
        .and_then(|()| work(self.expires));
        self.baseline = None;
        self.seats.clear();
        result
    }
}
pub(super) fn source(record: &Record) -> Option<&Source> {
    match record {
        Record::Device { device, .. } => Some(&device.source),
        Record::Frame { frame } => Some(&frame.source),
        Record::Absolute { source, .. }
        | Record::Mapping { source, .. }
        | Record::Reconcile { source, .. }
        | Record::Remove { source, .. } => Some(source),
        _ => None,
    }
}
pub(super) fn approved(config: &Config, definition: Option<&Definition>) -> io::Result<()> {
    config.validate().map_err(invalid)?;
    let declarations = config.recognizer_definitions();
    if !declarations.is_empty() {
        // SAFETY: read-only effective credential query.
        if unsafe { nix::libc::geteuid() } == 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "offline Lua callbacks cannot run as root",
            ));
        }
        let source =
            definition.ok_or_else(|| invalid("custom replay requires its original Lua source"))?;
        source.validate().map_err(invalid)?;
        if declarations.iter().any(|declaration| {
            !source
                .recognizers
                .iter()
                .any(|binding| &binding.definition == declaration)
        }) {
            return Err(invalid(
                "offline recognizer differs from approved source declaration",
            ));
        }
    }
    Ok(())
}
pub(super) fn invalid(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}
pub(super) fn work(expires: Instant) -> io::Result<()> {
    if Instant::now() >= expires {
        Err(invalid("offline replay work time limit exceeded"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests;
