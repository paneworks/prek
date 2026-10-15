//! Configured routing in the sole descriptor-owning supervised worker.
//!
//! This integration is under construction and is not wired into the CLI. The
//! broker continues to refuse route mode until handoff, recovery, and action
//! execution have been validated together.
use crate::{
    config::{Config, Mode},
    configuration::{Assembly, Command},
    input::{Device, Frame, Source},
    ipc::{Channel, monotonic},
    kernel::{Flush, Registry, Virtual},
    linux::{self, InputSink, ReaderBuffer, ReaderDisposition},
    protocol::{self, Fence},
    replay::Record,
    routing::Routing,
    rules::Engine,
    supervisor::Heartbeat,
    transport::{Descriptor, KernelIdentity, Readiness},
    worker::{Message, Report, Subscription, Subscriptions},
};
use evdev::raw_stream::RawDevice;
use nix::poll::{PollFd, PollFlags};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    io,
    os::fd::AsFd,
    time::Duration,
};

struct Seat {
    subscription: Subscription,
    routing: Routing,
    expires: Duration,
    reported: bool,
}
struct Output {
    fence: Fence,
    virtual_device: Virtual,
    state_probe: Option<linux::StateProbe>,
    readiness: Option<Readiness>,
    consumer: Option<crate::transport::ConsumerEvidence>,
    offered: Duration,
    started: Duration,
    in_flight: Option<u64>,
    sequence: u64,
}
pub struct Stream {
    channel: Channel,
    registry: Registry,
    config: Config,
    baseline: Engine,
    generation: u64,
    revision: u64,
    seats: BTreeMap<String, Seat>,
    outputs: BTreeMap<Source, Output>,
    grabbed: BTreeSet<Source>,
    releases: BTreeMap<Source, (Fence, crate::supervisor::WorkerIdentity)>,
    assembly: Assembly,
    messages: Outbox,
}
#[derive(Default)]
struct Outbox {
    messages: VecDeque<(Message, usize)>,
    bytes: usize,
}
impl Outbox {
    fn push(&mut self, message: Message) -> io::Result<()> {
        let bytes = protocol::encode(&protocol::Packet {
            version: 1,
            sequence: u64::MAX,
            at: Duration::MAX,
            message: &message,
        })?
        .len();
        if self.messages.len() >= protocol::MAX_QUEUED_PACKETS
            || self.bytes + bytes > protocol::MAX_QUEUED_BYTES
        {
            return Err(invalid("routing outcome queue overflow"));
        }
        if matches!(message, Message::Configuration { .. }) {
            if self
                .messages
                .iter()
                .any(|(message, _)| matches!(message, Message::Configuration { .. }))
            {
                return Err(invalid("configuration acknowledgements are not drained"));
            }
            self.messages.push_front((message, bytes));
        } else {
            self.messages.push_back((message, bytes));
        }
        self.bytes += bytes;
        Ok(())
    }
    fn discard_seat(&mut self, seat: &str) {
        self.messages.retain(|(message, bytes)| {
            let discard = match message {
                Message::Outcome { report } => report.subscription().seat == seat,
                Message::Transport { subscription, .. } => subscription.seat == seat,
                Message::Configuration { .. } => false,
                Message::RulesChanged {
                    seat: changed_seat, ..
                } => changed_seat == seat,
                Message::Released { .. } => false,
            };
            if discard {
                self.bytes -= bytes;
            }
            !discard
        });
    }
    fn flush(&mut self, channel: &mut Channel) -> io::Result<()> {
        for _ in 0..4 {
            let Some((message, _)) = self.messages.front() else {
                break;
            };
            if !channel.send(message, monotonic()?)? {
                break;
            }
            let (_, bytes) = self.messages.pop_front().unwrap();
            self.bytes -= bytes;
        }
        Ok(())
    }
}
fn identity(subscription: &Subscription) -> Subscription {
    Subscription {
        seat: subscription.seat.clone(),
        fence: subscription.fence,
        context: None,
        capabilities: BTreeSet::new(),
        consumer: None,
    }
}
fn invalid(reason: &str) -> io::Error {
    protocol::invalid(reason)
}
impl Stream {
    pub fn new(config: Config, channel: Channel) -> io::Result<Self> {
        if config.mode != Mode::Route {
            return Err(invalid("routing worker requires route configuration"));
        }
        Routing::new(config.clone())?;
        let mut baseline = config.clone();
        for rule in &mut baseline.rules {
            rule.enabled = false;
        }
        let mut baseline = Engine::new(baseline)?;
        baseline.push(Record::Header { version: 1 })?;
        let mut messages = Outbox::default();
        messages.push(Message::Configuration {
            generation: 1,
            error: None,
        })?;
        Ok(Self {
            channel,
            registry: Registry::system()?,
            config,
            baseline,
            generation: 1,
            revision: 0,
            seats: BTreeMap::new(),
            outputs: BTreeMap::new(),
            grabbed: BTreeSet::new(),
            releases: BTreeMap::new(),
            assembly: Assembly::default(),
            messages,
        })
    }
    fn queue(&mut self, message: Message) -> io::Result<()> {
        self.messages.push(message)
    }
    fn retire(&mut self, recovery: crate::router::Recovery, now: Duration) {
        if let Some(mut output) = self.outputs.remove(&recovery.source) {
            let frame = Frame {
                source: recovery.source,
                at: now,
                sequence: output.sequence.saturating_add(1),
                events: recovery.neutral_events,
            };
            // Complete an already started frame before attempting neutral.
            // All operations are nonblocking; destruction remains mandatory
            // when a partial/faulted write prevents a complete neutral frame.
            if matches!(output.virtual_device.flush(), Ok(Flush::Complete)) {
                let _ = output.virtual_device.enqueue(&frame);
                let _ = output.virtual_device.flush();
            }
        }
    }
    fn teardown(&mut self, seat: &str, now: Duration) {
        if let Some(mut owner) = self.seats.remove(seat) {
            let sources: Vec<_> = self
                .outputs
                .keys()
                .filter(|source| source.seat == seat)
                .cloned()
                .collect();
            for source in &sources {
                owner.routing.recover(source);
            }
            for recovery in owner.routing.take_recoveries() {
                self.retire(recovery, now);
            }
        }
        self.outputs.retain(|source, _| source.seat != seat);
        self.messages.discard_seat(seat);
    }
    fn subscriptions(&mut self, update: Subscriptions, now: Duration) -> io::Result<()> {
        if update.revision <= self.revision || update.seats.len() > crate::service::MAX_CLIENTS {
            return Err(invalid("invalid routing subscription revision"));
        }
        let mut prepared = BTreeMap::new();
        let mut seen = BTreeSet::new();
        for subscription in &update.seats {
            subscription.fence.validate()?;
            if !seen.insert(subscription.seat.clone())
                || subscription.fence.configuration != self.generation
                || !self
                    .config
                    .policy
                    .allowed_seats
                    .contains(&subscription.seat)
            {
                return Err(invalid(
                    "routing subscription differs from configured policy",
                ));
            }
            let context = subscription
                .context
                .as_ref()
                .ok_or_else(|| invalid("routing needs positive live context"))?;
            if context.binding.seat != subscription.seat
                || context.snapshot.generation != subscription.fence.session
                || subscription.consumer.is_none()
            {
                return Err(invalid("routing context/consumer binding differs"));
            }
            if let Some(seat) = self.seats.get(&subscription.seat)
                && seat.subscription.fence == subscription.fence
            {
                let mut authority = seat.routing.authority().clone();
                if seat
                    .subscription
                    .context
                    .as_ref()
                    .is_none_or(|old| old.snapshot.revision < context.snapshot.revision)
                {
                    authority
                        .update(context.snapshot.clone(), now)
                        .map_err(io::Error::other)?;
                }
                if authority.generation() != subscription.fence.session {
                    return Err(invalid("routing context expired"));
                }
                prepared.insert(subscription.seat.clone(), authority);
            } else {
                let mut authority = crate::context::Authority::default();
                authority
                    .adopt(context.binding.clone(), subscription.fence.session)
                    .map_err(io::Error::other)?;
                authority
                    .update(context.snapshot.clone(), now)
                    .map_err(io::Error::other)?;
                prepared.insert(subscription.seat.clone(), authority);
            }
        }
        let removed: Vec<_> = self
            .seats
            .iter()
            .filter(|(name, seat)| {
                !update.seats.iter().any(|subscription| {
                    &subscription.seat == *name && subscription.fence == seat.subscription.fence
                })
            })
            .map(|(name, _)| name.clone())
            .collect();
        for seat in removed {
            self.teardown(&seat, now);
        }
        for subscription in update.seats {
            if !self.seats.contains_key(&subscription.seat) {
                let mut routing = Routing::new(self.config.clone())?;
                routing.configuration_generation(self.generation)?;
                self.seats.insert(
                    subscription.seat.clone(),
                    Seat {
                        subscription: subscription.clone(),
                        routing,
                        expires: now + protocol::MAX_PACKET_AGE,
                        reported: false,
                    },
                );
            }
            let seat = self.seats.get_mut(&subscription.seat).unwrap();
            *seat.routing.authority_mut() = prepared.remove(&subscription.seat).unwrap();
            seat.routing.actions_available(
                subscription.fence.session,
                subscription.capabilities.clone(),
                now,
            );
            seat.subscription = subscription;
            seat.expires = now + protocol::MAX_PACKET_AGE;
        }
        self.revision = update.revision;
        Ok(())
    }
    fn collect(&mut self, name: &str, now: Duration) -> io::Result<()> {
        let Some(seat) = self.seats.get_mut(name) else {
            return Ok(());
        };
        let subscription = identity(&seat.subscription);
        let diagnostics = seat.routing.take_events();
        let dispatches = seat.routing.take_dispatches(now);
        let recoveries = seat.routing.take_recoveries();
        for recovery in recoveries {
            self.retire(recovery, now);
        }
        for event in diagnostics {
            self.queue(Message::Outcome {
                report: Report::Observation {
                    subscription: subscription.clone(),
                    event: Box::new(event),
                },
            })?;
        }
        for event in dispatches {
            self.queue(Message::Outcome {
                report: Report::Dispatch {
                    subscription: subscription.clone(),
                    event: Box::new(event),
                },
            })?;
        }
        Ok(())
    }
}
impl InputSink for Stream {
    fn reader_released(&mut self, source: &Source) -> io::Result<()> {
        if self.outputs.contains_key(source) {
            return Err(invalid("original closed before virtual retirement"));
        }
        let Some((fence, worker)) = self.releases.remove(source) else {
            return Ok(());
        };
        self.queue(Message::Released {
            worker,
            fence,
            source: source.clone(),
        })?;
        self.messages.flush(&mut self.channel)
    }
    fn reader(
        &mut self,
        raw: &mut RawDevice,
        device: &Device,
        buffer: ReaderBuffer,
        health: Option<&Heartbeat>,
    ) -> io::Result<ReaderDisposition> {
        let now = monotonic()?;
        if (self.grabbed.contains(&device.source) || self.releases.contains_key(&device.source))
            && !self.outputs.contains_key(&device.source)
        {
            // The old routing engine retires this source on Remove before a
            // reopened reader can attach with a fresh connection generation.
            return Ok(ReaderDisposition::Reopen);
        }
        let eligible = self.seats.get(&device.source.seat).is_some_and(|seat| {
            now < seat.expires
                && seat.routing.authority().state(now) == crate::context::State::Desktop
        });
        if !eligible {
            if self.grabbed.contains(&device.source) {
                if let Some(seat) = self.seats.get_mut(&device.source.seat) {
                    seat.routing.recover(&device.source);
                }
                self.collect(&device.source.seat, now)?;
                self.outputs.remove(&device.source);
                return Ok(ReaderDisposition::Reopen);
            }
            self.outputs.remove(&device.source);
            return Ok(ReaderDisposition::Keep);
        }
        if self.grabbed.contains(&device.source) {
            return Ok(ReaderDisposition::Keep);
        }
        if !self.outputs.contains_key(&device.source) {
            let health = health.ok_or_else(|| invalid("routing requires supervisor ownership"))?;
            let id = raw.input_id();
            let descriptor = Descriptor::mirror(
                device,
                KernelIdentity {
                    bus: id.bus_type().0,
                    vendor: id.vendor(),
                    product: id.product(),
                    version: id.version(),
                },
                None,
                &BTreeMap::new(),
                BTreeMap::new(),
            )
            .map_err(invalid)?;
            if descriptor.feedback_required() {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "routing requires a verified physical feedback bridge",
                ));
            }
            let state_probe = linux::StateProbe::new(raw)?;
            let output = Virtual::create(
                self.registry
                    .prepare_for_worker(descriptor.clone(), health)?,
            )?;
            let subscription = self.seats[&device.source.seat].subscription.clone();
            if self.releases.len() >= 4096 {
                return Err(invalid("too many pending reader releases"));
            }
            self.releases.insert(
                device.source.clone(),
                (subscription.fence, health.worker_identity()),
            );
            let readiness = Readiness::new(descriptor, subscription.consumer.clone().unwrap(), now)
                .map_err(invalid)?;
            self.outputs.insert(
                device.source.clone(),
                Output {
                    fence: subscription.fence,
                    virtual_device: output,
                    state_probe: Some(state_probe),
                    readiness: Some(readiness),
                    consumer: None,
                    offered: Duration::ZERO,
                    started: now,
                    in_flight: None,
                    sequence: 0,
                },
            );
        }
        let output = self.outputs.get_mut(&device.source).unwrap();
        if now.saturating_sub(output.started) >= Duration::from_secs(5) {
            return Err(invalid("virtual device readiness timed out before grab"));
        }
        if now >= output.offered + Duration::from_millis(100) {
            output.offered = now;
            let descriptor = Box::new(output.virtual_device.descriptor().clone());
            let subscription = identity(&self.seats[&device.source.seat].subscription);
            self.queue(Message::Transport {
                subscription,
                descriptor,
            })?;
        }
        let output = self.outputs.get_mut(&device.source).unwrap();
        let Some(evidence) = output.consumer.as_ref() else {
            return Ok(ReaderDisposition::Keep);
        };
        if now < evidence.at || now - evidence.at > Duration::from_millis(250) {
            return Ok(ReaderDisposition::Keep);
        }
        if !output.virtual_device.processed_metadata()? {
            return Ok(ReaderDisposition::Keep);
        }
        // Complete frames awaiting the watermark and partial assembler state
        // belong to observation. Let the sole loop deliver them before grab.
        if !buffer.is_drained() {
            return Ok(ReaderDisposition::Keep);
        }
        if !output.state_probe.as_ref().unwrap().is_neutral(device)? {
            return Ok(ReaderDisposition::Keep);
        }
        let gate = output.readiness.as_mut().unwrap();
        gate.metadata(output.virtual_device.descriptor(), now)
            .map_err(invalid)?;
        gate.consumer(evidence, now).map_err(invalid)?;
        gate.neutral(true, now).map_err(invalid)?;
        let ready = output
            .readiness
            .take()
            .unwrap()
            .finish(now)
            .map_err(invalid)?;
        raw.grab()?;
        // A handoff cannot convert already held input into a new gesture.
        if !output.state_probe.as_ref().unwrap().is_neutral(device)? {
            self.outputs.remove(&device.source);
            raw.ungrab()?;
            return Ok(ReaderDisposition::Keep);
        }
        // TODO: this drain cannot distinguish pre-grab backlog from events
        // arriving after grab. The software-buffer gate above does not solve
        // that kernel race. CLI routing must remain disabled until consumer
        // acknowledgement establishes the complete ownership boundary.
        let mut discarded = 0usize;
        loop {
            let fetched = raw.fetch_events().map(|events| events.count());
            match fetched {
                Ok(count) => {
                    if count == 0 {
                        break;
                    }
                    discarded += count;
                    if discarded > 65_536 {
                        let _ = raw.ungrab();
                        return Err(invalid("input handoff backlog exceeds bounded drain"));
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => {
                    let _ = raw.ungrab();
                    return Err(error);
                }
            }
        }
        let seat = self.seats.get_mut(&device.source.seat).unwrap();
        if let Err(error) = seat.routing.attach(device.clone(), &ready, None, now) {
            let _ = raw.ungrab();
            return Err(error);
        }
        output.state_probe = None;
        self.grabbed.insert(device.source.clone());
        if !seat.reported {
            seat.reported = true;
            let subscription = identity(&seat.subscription);
            self.queue(Message::Outcome {
                report: Report::Ready { subscription },
            })?;
        }
        Ok(ReaderDisposition::Keep)
    }
    fn push(&mut self, record: Record) -> io::Result<()> {
        if matches!(record, Record::Header { .. }) {
            return Ok(());
        }
        self.baseline.push(record.clone())?;
        self.baseline.take_events();
        let source = match &record {
            Record::Frame { frame } => Some(&frame.source),
            Record::Remove { source, .. }
            | Record::Mapping { source, .. }
            | Record::Reconcile { source, .. } => Some(source),
            _ => None,
        };
        let names: Vec<_> = self
            .seats
            .keys()
            .filter(|name| source.is_none_or(|source| &source.seat == *name))
            .cloned()
            .collect();
        for name in names {
            let seat = self.seats.get_mut(&name).unwrap();
            let invalidated = match &record {
                Record::Remove { source, .. }
                | Record::Reconcile { source, .. }
                | Record::Mapping { source, .. } => Some(source.clone()),
                _ => None,
            };
            if let Some(source) = invalidated {
                let subscription = identity(&seat.subscription);
                self.queue(Message::Outcome {
                    report: Report::SourceInvalidated {
                        subscription,
                        source,
                    },
                })?;
            }
            let seat = self.seats.get_mut(&name).unwrap();
            if matches!(record, Record::Tick { .. })
                || source.is_some_and(|source| self.grabbed.contains(source))
            {
                seat.routing.push(record.clone())?;
                self.collect(&name, monotonic()?)?;
            }
        }
        if let Record::Remove { source, .. } = record {
            self.outputs.remove(&source);
            self.grabbed.remove(&source);
        }
        Ok(())
    }
    fn service(&mut self) -> io::Result<()> {
        let now = monotonic()?;
        if self
            .assembly
            .deadline()
            .is_some_and(|deadline| now >= deadline)
        {
            return Err(invalid("configuration snapshot transfer expired"));
        }
        let expired: Vec<_> = self
            .seats
            .iter()
            .filter(|(_, seat)| {
                now >= seat.expires
                    || seat.routing.authority().state(now) != crate::context::State::Desktop
            })
            .map(|(name, seat)| (name.clone(), identity(&seat.subscription)))
            .collect();
        for (name, subscription) in expired {
            self.teardown(&name, now);
            self.queue(Message::Outcome {
                report: Report::ContextLost { subscription },
            })?;
        }
        for _ in 0..8 {
            let Some(packet) = self.channel.receive::<Command>()? else {
                break;
            };
            match packet.message {
                Command::Subscriptions { update } => {
                    let identities: Vec<_> = update.seats.iter().map(identity).collect();
                    if let Err(error) = self.subscriptions(update, now) {
                        if error
                            .get_ref()
                            .and_then(|error| error.downcast_ref::<crate::context::Rejection>())
                            .is_some_and(|rejection| {
                                matches!(
                                    rejection,
                                    crate::context::Rejection::StaleSnapshot
                                        | crate::context::Rejection::StaleGeneration
                                )
                            })
                        {
                            for subscription in identities {
                                self.teardown(&subscription.seat, now);
                                self.queue(Message::Outcome {
                                    report: Report::ContextLost { subscription },
                                })?;
                            }
                        } else {
                            return Err(error);
                        }
                    }
                }
                Command::Transport {
                    fence,
                    source,
                    evidence,
                } => {
                    if let Some(output) = self.outputs.get_mut(&source)
                        && output.fence == fence
                    {
                        output.consumer = Some(evidence);
                    }
                }
                command => {
                    if let Some(installation) = self.assembly.accept(command, now)? {
                        let result = installation.config.and_then(|config| {
                            if config.mode != Mode::Route
                                || self.generation.checked_add(1) != Some(installation.generation)
                            {
                                return Err(invalid("routing reload mode/generation differs"));
                            }
                            let mut baseline = config.clone();
                            for rule in &mut baseline.rules {
                                rule.enabled = false;
                            }
                            let baseline = self.baseline.prepare_replacement(baseline, now)?;
                            Routing::new(config.clone())?;
                            let names: Vec<_> = self.seats.keys().cloned().collect();
                            for name in names {
                                self.teardown(&name, now);
                            }
                            self.config = config;
                            self.baseline = baseline;
                            self.generation = installation.generation;
                            Ok(())
                        });
                        self.queue(Message::Configuration {
                            generation: installation.generation,
                            error: result.err().map(|error| error.to_string()),
                        })?;
                    }
                }
            }
        }
        Ok(())
    }
    fn flush(&mut self) -> io::Result<()> {
        let now = monotonic()?;
        let sources: Vec<_> = self
            .outputs
            .keys()
            .filter(|source| self.grabbed.contains(*source))
            .cloned()
            .collect();
        for source in sources {
            let Some(seat) = self.seats.get_mut(&source.seat) else {
                continue;
            };
            let output = self.outputs.get_mut(&source).unwrap();
            if output.in_flight.is_none()
                && let Some(scheduled) = seat.routing.due(&source, now)?
            {
                output.sequence = scheduled.frame.sequence;
                output.virtual_device.enqueue(&scheduled.frame)?;
                output.in_flight = Some(scheduled.ticket);
            }
            if output.in_flight.is_some() && output.virtual_device.flush()? == Flush::Complete {
                let ticket = output.in_flight.take().unwrap();
                seat.routing.delivered(&source, ticket, monotonic()?)?;
            }
        }
        self.messages.flush(&mut self.channel)
    }
    fn deadline(&self) -> Option<Duration> {
        self.seats
            .values()
            .flat_map(|seat| {
                seat.routing.deadline().into_iter().chain([
                    seat.expires,
                    seat.subscription
                        .context
                        .as_ref()
                        .unwrap()
                        .snapshot
                        .captured_at
                        + crate::context::SNAPSHOT_MAX_AGE
                        + Duration::from_nanos(1),
                ])
            })
            .chain(self.assembly.deadline())
            .chain(
                self.outputs
                    .values()
                    .filter(|output| output.readiness.is_some())
                    .map(|output| output.offered + Duration::from_millis(100)),
            )
            .min()
    }
    fn poll_sources(&self) -> Vec<PollFd<'_>> {
        let mut sources = vec![PollFd::new(
            self.channel.as_fd(),
            PollFlags::POLLIN
                | if self.messages.messages.is_empty() {
                    PollFlags::empty()
                } else {
                    PollFlags::POLLOUT
                },
        )];
        sources.extend(self.outputs.values().filter_map(|output| {
            output
                .in_flight
                .map(|_| PollFd::new(output.virtual_device.as_fd(), PollFlags::POLLOUT))
        }));
        sources
    }
    fn device_policy(&self) -> Option<(&Config, u64)> {
        Some((&self.config, self.generation))
    }
    fn shutdown(&mut self) {
        let now = monotonic().unwrap_or(Duration::ZERO);
        let names: Vec<_> = self.seats.keys().cloned().collect();
        for name in names {
            self.teardown(&name, now);
        }
        self.outputs.clear();
    }
}
impl Drop for Stream {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ended(seat: &str, reason_bytes: usize) -> Message {
        Message::Outcome {
            report: Report::Ended {
                subscription: Subscription {
                    seat: seat.into(),
                    fence: Fence {
                        broker: protocol::Epoch([1, 2]),
                        configuration: 1,
                        session: 1,
                    },
                    context: None,
                    capabilities: BTreeSet::new(),
                    consumer: None,
                },
                reason: "x".repeat(reason_bytes),
            },
        }
    }

    #[test]
    fn slow_private_consumers_cannot_exceed_aggregate_byte_or_packet_limits() {
        let mut bytes = Outbox::default();
        for _ in 0..10 {
            bytes.push(ended("seat0", 48_000)).unwrap();
        }
        let before = bytes.bytes;
        assert!(bytes.push(ended("seat0", 48_000)).is_err());
        assert_eq!(bytes.messages.len(), 10);
        assert_eq!(bytes.bytes, before);

        let mut packets = Outbox::default();
        for _ in 0..protocol::MAX_QUEUED_PACKETS {
            packets.push(ended("seat0", 1)).unwrap();
        }
        assert!(packets.push(ended("seat0", 1)).is_err());
        assert_eq!(packets.messages.len(), protocol::MAX_QUEUED_PACKETS);
    }

    #[test]
    fn teardown_reclaims_queue_budget_and_configuration_acknowledgements_go_first() {
        let mut queue = Outbox::default();
        queue.push(ended("seat0", 48_000)).unwrap();
        queue.push(ended("seat1", 48_000)).unwrap();
        queue.discard_seat("seat0");
        assert_eq!(queue.bytes, queue.messages[0].1);
        queue
            .push(Message::Configuration {
                generation: 2,
                error: None,
            })
            .unwrap();
        assert!(matches!(
            queue.messages[0].0,
            Message::Configuration { generation: 2, .. }
        ));
        assert!(
            queue
                .push(Message::Configuration {
                    generation: 3,
                    error: None,
                })
                .is_err()
        );
        queue.discard_seat("seat1");
        assert_eq!(queue.messages.len(), 1);
        assert_eq!(queue.bytes, queue.messages[0].1);
    }

    #[test]
    fn seat_replacement_preserves_original_descriptor_release_acknowledgements() {
        let mut queue = Outbox::default();
        let worker = serde_json::from_value(serde_json::json!({
            "pid":std::process::id(),"birth":1,"epoch":1
        }))
        .unwrap();
        let fence = Fence {
            broker: protocol::Epoch([1, 2]),
            configuration: 1,
            session: 1,
        };
        let source = Source {
            device: 7,
            generation: 2,
            seat: "seat0".into(),
        };
        queue.push(ended("seat0", 1)).unwrap();
        queue
            .push(Message::Released {
                worker,
                fence,
                source: source.clone(),
            })
            .unwrap();
        queue.discard_seat("seat0");
        assert_eq!(queue.messages.len(), 1);
        assert_eq!(queue.bytes, queue.messages[0].1);
        let (mut sender, mut receiver) = Channel::pair().unwrap();
        queue.flush(&mut sender).unwrap();
        assert_eq!(queue.bytes, 0);
        let message = receiver.receive::<Message>().unwrap().unwrap().message;
        assert!(
            matches!(message,Message::Released {worker:w,fence:f,source:s} if w==worker && f==fence && s==source)
        );
    }
}
