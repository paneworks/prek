//! Offline downstream routing with explicit simulated readiness and writes.
use super::configured::{
    Budget, MAX_OUTCOMES, MAX_SEATS, MAX_WORK, Seat, approved, invalid, source, work,
};
use super::{MAX_LINE_BYTES, MAX_RECORDING_BYTES, MAX_RECORDS, Record as Input};
use crate::{
    config::Config,
    context::{Authority, Binding, Snapshot, State},
    gestures::Mapping,
    input::{RawEvent, Source},
    input_events::ResetReason,
    lua::Definition,
    protocol::{Capability, Epoch, Fence},
    router::Scheduled,
    routing::Routing,
    rules::Event,
    transport::{ConsumerEvidence, Descriptor, Readiness},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    sync::Arc,
    time::{Duration, Instant},
};

pub const FORMAT_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "routing", rename_all = "snake_case", deny_unknown_fields)]
pub enum Record {
    Header {
        version: u32,
    },
    Input {
        now: Duration,
        input: Box<Input>,
    },
    Attach {
        at: Duration,
        descriptor: Box<Descriptor>,
        consumer: ConsumerEvidence,
        feedback: bool,
    },
    Actions {
        at: Duration,
        seat: String,
        generation: u64,
        capabilities: BTreeSet<Capability>,
    },
    Poll {
        at: Duration,
    },
    Write {
        at: Duration,
        source: Source,
        ticket: u64,
    },
    Retire {
        at: Duration,
        source: Source,
    },
    Expected {
        outcomes: Vec<Outcome>,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum Outcome {
    Offered {
        at: Duration,
        scheduled: Box<Scheduled>,
    },
    Delivered {
        at: Duration,
        source: Source,
        ticket: u64,
    },
    Dispatch {
        at: Duration,
        event: Box<Event>,
    },
    Observation {
        event: Box<Event>,
    },
    Recovery {
        at: Duration,
        source: Source,
        neutral_events: Vec<RawEvent>,
        destroy_virtual_and_release_grab: bool,
    },
}

/// Recorded assertions create no live authority, kernel I/O or effects.
pub struct Routed {
    config: Config,
    definition: Option<Arc<Definition>>,
    budget: Budget,
    baseline: Option<super::Replay>,
    seats: BTreeMap<String, Seat<Routing>>,
    mappings: BTreeMap<Source, Mapping>,
    attached: BTreeSet<Source>,
    admitted: BTreeSet<Source>,
    in_flight: BTreeMap<Source, Scheduled>,
    outcomes: Vec<Outcome>,
    expected: Option<Vec<Outcome>>,
    header: bool,
    input_seen: bool,
    at: Duration,
    now: Duration,
    expires: Instant,
    records: usize,
    bytes: usize,
    outcome_bytes: usize,
}
impl Routed {
    pub fn new(config: Config, definition: Option<Definition>) -> io::Result<Self> {
        Self::with_budget(config, definition.map(Arc::new), Budget::default())
    }
    pub(crate) fn with_budget(
        config: Config,
        definition: Option<Arc<Definition>>,
        budget: Budget,
    ) -> io::Result<Self> {
        approved(&config, definition.as_deref())?;
        crate::worker::preflight(&config)?;
        Routing::validate_profile(&config)?;
        Ok(Self {
            config,
            definition,
            budget,
            baseline: Some(super::Replay::default()),
            seats: BTreeMap::new(),
            mappings: BTreeMap::new(),
            attached: BTreeSet::new(),
            admitted: BTreeSet::new(),
            in_flight: BTreeMap::new(),
            outcomes: vec![],
            expected: None,
            header: false,
            input_seen: false,
            at: Duration::ZERO,
            now: Duration::ZERO,
            expires: Instant::now(),
            records: 0,
            bytes: 0,
            outcome_bytes: 2,
        })
    }
    pub fn outcomes(&self) -> &[Outcome] {
        &self.outcomes
    }
    fn close(&mut self) {
        self.baseline = None;
        self.seats.clear();
        self.attached.clear();
        self.in_flight.clear();
        self.mappings.clear();
        self.admitted.clear();
    }
    pub fn push(&mut self, record: Record) -> io::Result<()> {
        if self.baseline.is_none() {
            return Err(invalid("routing replay is closed"));
        }
        self.expires = Instant::now() + MAX_WORK;
        let result = self.push_inner(record).and_then(|()| work(self.expires));
        if result.is_err() {
            self.close();
        }
        result
    }
    fn push_inner(&mut self, record: Record) -> io::Result<()> {
        if self.expected.is_some() {
            return Err(invalid("routing expectations must be the final record"));
        }
        let bytes = crate::lua::bounded_json(&record, MAX_LINE_BYTES)?.len();
        if self.records >= MAX_RECORDS || bytes > MAX_RECORDING_BYTES.saturating_sub(self.bytes) {
            return Err(invalid("routing replay recording limit exceeded"));
        }
        if !self.header
            && !matches!(
                record,
                Record::Header {
                    version: FORMAT_VERSION
                }
            )
        {
            return Err(invalid(
                "routing recording must start with a supported header",
            ));
        }
        match record {
            Record::Header {
                version: FORMAT_VERSION,
            } if !self.header => {
                self.baseline.as_mut().unwrap().push(Input::Header {
                    version: super::FORMAT_VERSION,
                })?;
                self.header = true;
            }
            Record::Header { .. } => {
                return Err(invalid("duplicate or unsupported routing header"));
            }
            Record::Input { now, input } => {
                self.wall(now)?;
                self.input(*input)?;
            }
            Record::Attach {
                at,
                descriptor,
                consumer,
                feedback,
            } => {
                self.wall(at)?;
                self.attach(*descriptor, consumer, feedback)?;
            }
            Record::Actions {
                at,
                seat,
                generation,
                capabilities,
            } => {
                self.wall(at)?;
                let seat = self
                    .seats
                    .get_mut(&seat)
                    .ok_or_else(|| invalid("actions require recorded context"))?;
                if generation != seat.fence.session {
                    return Err(invalid(
                        "action availability has a stale session generation",
                    ));
                }
                seat.engine.actions_available(generation, capabilities, at);
            }
            Record::Poll { at } => {
                self.wall(at)?;
                self.settle()?;
                self.poll()?;
            }
            Record::Write { at, source, ticket } => {
                self.wall(at)?;
                self.settle()?;
                if self
                    .in_flight
                    .get(&source)
                    .is_none_or(|frame| frame.ticket != ticket)
                {
                    return Err(invalid("write acknowledgement has no exact offered frame"));
                }
                self.seats
                    .get_mut(&source.seat)
                    .ok_or_else(|| invalid("write has no owning seat"))?
                    .engine
                    .delivered_after_input(&source, ticket, at)?;
                self.in_flight.remove(&source);
                self.append(Outcome::Delivered { at, source, ticket })?;
            }
            Record::Retire { at, source } => {
                self.wall(at)?;
                if !self.attached.contains(&source) {
                    return Err(invalid("retirement requires an attached source"));
                }
                self.seats
                    .get_mut(&source.seat)
                    .unwrap()
                    .engine
                    .recover(&source);
            }
            Record::Expected { outcomes } => {
                if !self.input_seen || outcomes.len() > MAX_OUTCOMES {
                    return Err(invalid(
                        "routing expectations require bounded registered input",
                    ));
                }
                self.expected = Some(outcomes);
            }
        }
        if self.expected.is_none() {
            self.settle()?;
        }
        self.records += 1;
        self.bytes += bytes;
        Ok(())
    }
    fn wall(&mut self, now: Duration) -> io::Result<()> {
        if now < self.now || now > Duration::from_nanos(i64::MAX as u64) - Duration::from_secs(3600)
        {
            return Err(invalid(
                "routing replay wall clock regressed or exceeded its range",
            ));
        }
        self.now = now;
        for seat in self.seats.values_mut() {
            seat.engine.authority_mut().tick(now);
        }
        Ok(())
    }
    fn input(&mut self, record: Input) -> io::Result<()> {
        let at = match &record {
            Input::Frame { frame } => frame.at,
            Input::Context { at, .. }
            | Input::Tick { at }
            | Input::Mapping { at, .. }
            | Input::Reconcile { at, .. }
            | Input::Remove { at, .. } => *at,
            _ => self.at,
        };
        if at < self.at || at > self.now {
            return Err(invalid(
                "routing replay input watermark regressed or exceeds wall time",
            ));
        }
        self.at = at;
        if let Input::Context {
            binding, snapshot, ..
        } = record
        {
            self.context(binding, *snapshot)?;
            self.baseline.as_mut().unwrap().push(Input::Tick { at })?;
            self.baseline.as_mut().unwrap().observations.clear();
            return Ok(());
        }
        if matches!(
            record,
            Input::Header { .. } | Input::Expected { .. } | Input::ExpectedRules { .. }
        ) {
            return Err(invalid(
                "routing input cannot contain a header or recognition expectations",
            ));
        }
        self.baseline.as_mut().unwrap().push(record.clone())?;
        self.baseline.as_mut().unwrap().observations.clear();
        self.input_seen |= matches!(record, Input::Device { .. });
        if let Input::Device { device, .. } = &record {
            let previous: Vec<_> = self
                .admitted
                .iter()
                .filter(|old| {
                    old.device == device.source.device
                        && self
                            .seats
                            .get(&old.seat)
                            .is_some_and(|seat| seat.engine.has_input_source(old))
                })
                .cloned()
                .collect();
            for old in previous {
                let seat = self.seats.get_mut(&old.seat).unwrap();
                seat.engine.recover(&old);
                seat.engine
                    .push_at(Input::Remove { source: old, at }, self.now)?;
            }
            self.mappings
                .retain(|old, _| old.device != device.source.device);
        }
        if let Input::Mapping {
            source, mapping, ..
        } = &record
        {
            self.mappings.insert(source.clone(), mapping.clone());
        }
        for (name, seat) in &mut self.seats {
            if source(&record).is_some_and(|source| &source.seat != name) {
                continue;
            }
            let selected = source(&record).is_none_or(|source| seat.engine.has_source(source))
                || matches!(&record, Input::Remove { source, .. } if seat.engine.has_input_source(source));
            if !selected {
                continue;
            }
            let removed = if let Input::Remove { source, .. } = &record {
                seat.engine
                    .input_reset(source, self.now, ResetReason::Removed)
            } else {
                None
            };
            match &record {
                Input::Device { .. } => {}
                Input::Absolute { .. } => {
                    return Err(invalid("absolute state must precede routing attachment"));
                }
                _ => seat.engine.push_at(record.clone(), self.now)?,
            }
            seat.metadata(&self.config, removed.into_iter().collect(), self.now)?;
        }
        if let Input::Remove { source, .. } = record {
            self.mappings.remove(&source);
        }
        Ok(())
    }
    fn context(&mut self, binding: Binding, snapshot: Snapshot) -> io::Result<()> {
        if !self.config.policy.allowed_seats.contains(&binding.seat) {
            return Err(invalid("recorded routing context exceeds seat policy"));
        }
        let authority =
            Authority::recorded(binding.clone(), snapshot.clone(), self.now).map_err(invalid)?;
        let name = binding.seat.clone();
        let replace = if let Some(old) = self.seats.get(&name) {
            let mut previous = old.engine.authority().clone();
            previous.tick(self.now);
            if snapshot.generation < old.fence.session
                || snapshot.generation == old.fence.session
                    && (binding != old.binding
                        || snapshot.revision <= old.snapshot.revision
                        || previous.generation() != old.fence.session)
            {
                return Err(invalid(
                    "recorded routing context generation, binding or revision regressed",
                ));
            }
            snapshot.generation != old.fence.session
        } else {
            true
        };
        if replace {
            if let Some(mut old) = self.seats.remove(&name) {
                old.expires = self.expires;
                old.retire(self.now)?;
                for source in self.attached.iter().filter(|source| source.seat == name) {
                    old.engine.recover(source);
                }
                self.collect_seat(&mut old)?;
            } else if self.seats.len() >= MAX_SEATS {
                return Err(invalid("offline routing seat limit exceeded"));
            }
            let fence = Fence {
                broker: Epoch([0x5052454b, 0x5245504c4159]),
                configuration: 1,
                session: snapshot.generation,
            };
            let engine = Routing::with_recognition(self.config.clone(), fence)?;
            self.seats.insert(
                name.clone(),
                Seat::new(
                    engine,
                    binding.clone(),
                    snapshot.clone(),
                    fence,
                    self.now,
                    self.expires,
                ),
            );
        }
        let seat = self.seats.get_mut(&name).unwrap();
        *seat.engine.authority_mut() = authority;
        seat.binding = binding;
        seat.snapshot = snapshot;
        seat.engine.push_at(Input::Tick { at: self.at }, self.now)?;
        seat.engine.recognition_ready(
            seat.engine.authority().state(self.now) == State::Desktop
                && !seat.snapshot.secure_overlay,
            self.now,
        )?;
        Ok(())
    }
    fn attach(
        &mut self,
        descriptor: Descriptor,
        consumer: ConsumerEvidence,
        feedback: bool,
    ) -> io::Result<()> {
        if self.now != self.at {
            return Err(invalid(
                "routing attachment requires a current drained input watermark",
            ));
        }
        descriptor.validate().map_err(invalid)?;
        let source = descriptor.source.clone();
        let (device, absolute) = self.baseline.as_ref().unwrap().route_seed(&source)?;
        if !self.seats.contains_key(&source.seat)
            || self.admitted.contains(&source)
            || self.admitted.len() >= 1024
        {
            return Err(invalid(
                "routing attachment has no exact fresh source/context",
            ));
        }
        let seat = self.seats.get_mut(&source.seat).unwrap();
        if seat.engine.authority().state(self.now) != State::Desktop || seat.snapshot.secure_overlay
        {
            return Err(invalid(
                "routing attachment requires current recorded desktop context",
            ));
        }
        let mut readiness = Readiness::new(descriptor.clone(), consumer.consumer.clone(), self.now)
            .map_err(invalid)?;
        readiness.metadata(&descriptor, self.now).map_err(invalid)?;
        readiness.consumer(&consumer, self.now).map_err(invalid)?;
        readiness.feedback(feedback, self.now).map_err(invalid)?;
        readiness.neutral(true, self.now).map_err(invalid)?;
        let ready = readiness.finish(self.now).map_err(invalid)?;
        if !ready.matches(device) {
            return Err(invalid(
                "routing descriptor differs from the recorded source",
            ));
        }
        seat.engine.attach_with_state(
            device.clone(),
            &ready,
            self.mappings.get(&source).cloned(),
            Some(absolute.clone()),
            self.now,
        )?;
        seat.metadata(&self.config, seat.engine.input_states(self.now), self.now)?;
        self.attached.insert(source.clone());
        self.admitted.insert(source);
        Ok(())
    }
    fn settle(&mut self) -> io::Result<()> {
        let revoked: Vec<_> = self
            .attached
            .iter()
            .filter(|source| {
                self.seats.get(&source.seat).is_some_and(|seat| {
                    seat.engine.authority().state(self.now) != State::Desktop
                        || seat.snapshot.secure_overlay
                })
            })
            .cloned()
            .collect();
        for source in revoked {
            self.seats
                .get_mut(&source.seat)
                .unwrap()
                .engine
                .recover(&source);
        }
        for seat in self.seats.values_mut() {
            seat.expires = self.expires;
            seat.settle_clocked(
                &self.config,
                self.definition.as_deref(),
                &self.budget,
                self.at,
                self.now,
            )?;
        }
        let names: Vec<_> = self.seats.keys().cloned().collect();
        for name in names {
            let mut seat = self.seats.remove(&name).unwrap();
            self.collect_seat(&mut seat)?;
            self.seats.insert(name, seat);
        }
        Ok(())
    }
    fn collect_seat(&mut self, seat: &mut Seat<Routing>) -> io::Result<()> {
        for event in seat.engine.take_events() {
            self.append(Outcome::Observation {
                event: Box::new(event),
            })?;
        }
        for recovery in seat.engine.take_recoveries() {
            self.attached.remove(&recovery.source);
            self.in_flight.remove(&recovery.source);
            self.append(Outcome::Recovery {
                at: self.now,
                source: recovery.source,
                neutral_events: recovery.neutral_events,
                destroy_virtual_and_release_grab: recovery.destroy_virtual_and_release_grab,
            })?;
        }
        Ok(())
    }
    fn poll(&mut self) -> io::Result<()> {
        let sources: Vec<_> = self.attached.iter().cloned().collect();
        for source in sources {
            let offered = self
                .seats
                .get_mut(&source.seat)
                .unwrap()
                .engine
                .due_after_input(&source, self.now)?
                .cloned();
            if let Some(scheduled) = offered {
                if let Some(previous) = self.in_flight.get(&source) {
                    if previous != &scheduled {
                        return Err(invalid(
                            "routing output changed before its write acknowledgement",
                        ));
                    }
                } else {
                    self.in_flight.insert(source, scheduled.clone());
                    self.append(Outcome::Offered {
                        at: self.now,
                        scheduled: Box::new(scheduled),
                    })?;
                }
            }
        }
        let dispatches: Vec<_> = self
            .seats
            .values_mut()
            .flat_map(|seat| seat.engine.take_dispatches(self.now))
            .collect();
        for event in dispatches {
            self.append(Outcome::Dispatch {
                at: self.now,
                event: Box::new(event),
            })?;
        }
        Ok(())
    }
    fn append(&mut self, outcome: Outcome) -> io::Result<()> {
        if self.outcomes.len() >= MAX_OUTCOMES {
            return Err(invalid("routing replay outcome limit exceeded"));
        }
        self.outcome_bytes += crate::lua::bounded_json(
            &outcome,
            MAX_RECORDING_BYTES.saturating_sub(self.outcome_bytes),
        )?
        .len()
            + 1;
        if self.outcome_bytes > MAX_RECORDING_BYTES {
            return Err(invalid("routing replay outcome byte limit exceeded"));
        }
        self.outcomes.push(outcome);
        Ok(())
    }
    pub fn check(&self) -> io::Result<()> {
        let expected = self
            .expected
            .as_ref()
            .ok_or_else(|| invalid("routing recording has no expected downstream outcomes"))?;
        if super::similar_geometry(
            &serde_json::to_value(expected)?,
            &serde_json::to_value(&self.outcomes)?,
        ) {
            Ok(())
        } else {
            Err(invalid(format!(
                "routing outcome mismatch: expected {} outcomes, received {}",
                expected.len(),
                self.outcomes.len()
            )))
        }
    }
    pub fn finish(&mut self, check: bool) -> io::Result<()> {
        if self.baseline.is_none() {
            return Err(invalid("routing replay is closed"));
        }
        self.expires = Instant::now() + MAX_WORK;
        let result = (|| {
            if !self.header
                || !self.input_seen
                || self.admitted.is_empty()
                || !self.attached.is_empty()
                || !self.in_flight.is_empty()
            {
                return Err(invalid(
                    "routing replay must register input and explicitly retire every transport",
                ));
            }
            for seat in self.seats.values_mut() {
                seat.expires = self.expires;
                seat.retire(self.now)?;
            }
            self.settle()?;
            if check {
                self.check()?;
            }
            work(self.expires)
        })();
        self.close();
        result
    }
    pub fn read(&mut self, input: &mut impl io::BufRead) -> io::Result<()> {
        let result = super::visit_records(input, |record| self.push(record));
        if result.is_err() {
            self.close();
        }
        result
    }
}

#[cfg(test)]
pub(crate) mod tests;
