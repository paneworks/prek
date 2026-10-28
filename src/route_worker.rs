//! Configured routing in the sole descriptor-owning supervised worker.
//!
//! `prek broker --mode route` runs it as `route-worker`. Physical takeover is
//! admitted only from the bound Wayland compositor, at a neutral idle cut; every other source
//! is refused before any virtual mirror exists.
use crate::{
    config::{Config, Mode},
    configuration::{Assembly, Command},
    input::{Device, Frame, Source},
    ipc::{Channel, monotonic},
    kernel::{Flush, Publication, Registry, Virtual},
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

mod key_control;
mod keys;
mod latency;
#[cfg(feature = "native-test-harness")]
#[doc(hidden)]
pub mod native_fixture;
mod recognizers;
mod takeover;

struct Seat {
    subscription: Subscription,
    routing: Routing,
    expires: Duration,
    started: Duration,
    recognizer_ready: Option<bool>,
    input_enabled: bool,
    input_started: Duration,
    metadata_enabled: bool,
    metadata_started: Duration,
    keys: Option<keys::Slot>,
}
struct Output {
    fence: Fence,
    virtual_device: Virtual,
    state_probe: Option<linux::StateProbe>,
    feedback: Option<linux::feedback::LedFeedback>,
    readiness: Option<Readiness>,
    consumer: Option<crate::transport::ConsumerEvidence>,
    offered: Duration,
    started: Duration,
    in_flight: Option<u64>,
    queued: Option<(Duration, Duration)>,
    sequence: u64,
}
#[cfg(feature = "native-test-harness")]
impl Drop for Output {
    fn drop(&mut self) {
        eprintln!(
            "routing worker: dropping output {}\n{}",
            self.virtual_device.descriptor().marker,
            std::backtrace::Backtrace::force_capture()
        );
    }
}
struct NativePreparation {
    descriptor: Descriptor,
    readiness: Readiness,
}
impl NativePreparation {
    fn new(
        publication: &Publication,
        consumer: crate::transport::Consumer,
        now: Duration,
    ) -> io::Result<Self> {
        let descriptor = publication.descriptor().clone();
        let readiness = Readiness::new(descriptor.clone(), consumer, now).map_err(invalid)?;
        Ok(Self {
            descriptor,
            readiness,
        })
    }
}
fn consumer_boundary(
    gate: &mut Readiness,
    descriptor: &Descriptor,
    evidence: &crate::transport::ConsumerEvidence,
    feedback: bool,
    now: Duration,
) -> io::Result<()> {
    gate.metadata(descriptor, now).map_err(invalid)?;
    gate.consumer(evidence, now).map_err(invalid)?;
    gate.feedback(feedback, now).map_err(invalid)?;
    native_boundary(&evidence.consumer)
}
/// Admits physical takeover only from the bound Wayland compositor, through the neutral idle
/// cut in `takeover`; other consumers have no prefix/drain/neutral/rollback boundary.
fn native_boundary(consumer: &crate::transport::Consumer) -> io::Result<()> {
    let kind = match consumer {
        crate::transport::Consumer::Compositor { .. } => return Ok(()),
        crate::transport::Consumer::ConsoleKeyboard { .. } => "console keyboard",
        crate::transport::Consumer::IsolatedReceiver { .. } => "isolated receiver",
    };
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        format!(
            "native {kind} consumer prefix/drain/neutral/rollback boundary is not implemented; leave source ungrabbed"
        ),
    ))
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
    releases: BTreeSet<Source>,
    assembly: Assembly,
    subscriptions: crate::subscription_transfer::Assembly,
    messages: Outbox,
    pending: VecDeque<Pending>,
    pending_bytes: usize,
    closing_keys: Vec<keys::Slot>,
    closing_outputs: Vec<(crate::router::Recovery, Output)>,
    key_holds: BTreeMap<String, Duration>,
    refused: BTreeSet<Source>,
    committed: BTreeMap<Source, takeover::Committed>,
    forwarding: BTreeMap<Source, latency::Forwarding>,
    key_actions: BTreeMap<String, u64>,
    last_input: BTreeMap<Source, Duration>,
    key_generation: u64,
    key_failure: Option<io::Error>,
}
struct Pending {
    seat: String,
    fence: Fence,
    received_at: Duration,
    bytes: usize,
    operation: Operation,
}
enum Operation {
    Recognition(crate::recognition::Proposal),
    Rules {
        id: u64,
        transaction: crate::rule_transactions::Transaction,
    },
}
#[derive(Default)]
struct Outbox {
    messages: VecDeque<(Message, usize)>,
    bytes: usize,
}
impl Outbox {
    fn push(&mut self, message: Message) -> io::Result<()> {
        self.push_reserved(message, 0, 0)
    }
    fn push_reserved(
        &mut self,
        message: Message,
        packets: usize,
        reserved: usize,
    ) -> io::Result<()> {
        if let Message::Outcome {
            report:
                Report::RecognitionCancelled {
                    subscription,
                    cancellation,
                },
        } = &message
        {
            self.retain(|message| {
                !matches!(message, Message::Outcome {
                report: Report::Recognition { subscription: owner, ticket }
            } if owner.same_registration(subscription) && ticket.candidate == cancellation.candidate
                && ticket.revision == cancellation.revision && ticket.id <= cancellation.through)
            });
        }
        let bytes = protocol::encode(&protocol::Packet {
            version: 1,
            sequence: u64::MAX,
            at: Duration::MAX,
            message: &message,
        })?
        .len();
        if self.messages.len() + packets >= protocol::MAX_QUEUED_PACKETS
            || self.bytes + bytes + reserved > protocol::MAX_QUEUED_BYTES
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
        } else if matches!(
            message,
            Message::RecognitionResult { .. } | Message::RulesChanged { .. }
        ) {
            let index = self
                .messages
                .iter()
                .position(|(message, _)| {
                    !matches!(
                        message,
                        Message::Configuration { .. }
                            | Message::RecognitionResult { .. }
                            | Message::RulesChanged { .. }
                    )
                })
                .unwrap_or(self.messages.len());
            self.messages.insert(index, (message, bytes));
        } else {
            self.messages.push_back((message, bytes));
        }
        self.bytes += bytes;
        Ok(())
    }
    fn discard_seat(&mut self, seat: &str) {
        self.retain(|message| {
            let discard = match message {
                Message::Outcome { report } => report.subscription().seat == seat,
                Message::Transport { subscription, .. } => subscription.seat == seat,
                Message::Configuration { .. } => false,
                Message::RulesChanged {
                    seat: changed_seat, ..
                }
                | Message::RecognitionResult {
                    seat: changed_seat, ..
                } => changed_seat == seat,
                Message::KeyDrain { .. } => false,
                Message::SoftwareResult { .. } => false,
            };
            !discard
        });
    }
    fn retain(&mut self, mut keep: impl FnMut(&Message) -> bool) {
        self.messages.retain(|(message, bytes)| {
            let retained = keep(message);
            if !retained {
                self.bytes -= bytes;
            }
            retained
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
    subscription.identity()
}
fn invalid(reason: &str) -> io::Error {
    protocol::invalid(reason)
}
impl Stream {
    fn offer_transport(&mut self, source: &Source, now: Duration) -> io::Result<()> {
        let Some(output) = self.outputs.get_mut(source) else {
            return Ok(());
        };
        if now < output.offered + Duration::from_millis(100) {
            return Ok(());
        }
        output.offered = now;
        let descriptor = Box::new(output.virtual_device.descriptor().clone());
        let subscription = identity(&self.seats[&source.seat].subscription);
        self.queue(Message::Transport {
            subscription,
            descriptor,
        })
    }

    pub fn new(config: Config, channel: Channel) -> io::Result<Self> {
        Self::with_registry(config, channel, Registry::system()?)
    }
    fn with_registry(config: Config, channel: Channel, registry: Registry) -> io::Result<Self> {
        if config.mode != Mode::Route {
            return Err(invalid("routing worker requires route configuration"));
        }
        crate::worker::preflight(&config)?;
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
            registry,
            config,
            baseline,
            generation: 1,
            revision: 0,
            seats: BTreeMap::new(),
            outputs: BTreeMap::new(),
            grabbed: BTreeSet::new(),
            releases: BTreeSet::new(),
            assembly: Assembly::default(),
            subscriptions: crate::subscription_transfer::Assembly::default(),
            messages,
            pending: VecDeque::new(),
            pending_bytes: 0,
            closing_keys: Vec::new(),
            closing_outputs: Vec::new(),
            key_holds: BTreeMap::new(),
            refused: BTreeSet::new(),
            committed: BTreeMap::new(),
            forwarding: BTreeMap::new(),
            key_actions: BTreeMap::new(),
            last_input: BTreeMap::new(),
            key_generation: 0,
            key_failure: None,
        })
    }
    fn queue(&mut self, message: Message) -> io::Result<()> {
        self.messages
            .push_reserved(message, self.pending.len(), self.pending_bytes)
    }
    /// Logs and drops the forwarding delays of devices that no longer have an output.
    fn settle_forwarding(&mut self) {
        let outputs = &self.outputs;
        self.forwarding.retain(|source, forwarding| {
            outputs.contains_key(source) || {
                eprintln!("{}", forwarding.summary());
                false
            }
        });
    }
    /// Logs the forwarding delays of every device taken over since the last
    /// report and how often each key-action rule queued its keys.
    pub fn report(&mut self) {
        for forwarding in std::mem::take(&mut self.forwarding).into_values() {
            eprintln!("{}", forwarding.summary());
        }
        for (rule, count) in std::mem::take(&mut self.key_actions) {
            eprintln!("routing worker: rule {rule} queued its keys {count} times");
        }
    }
    fn retire(&mut self, recovery: crate::router::Recovery, now: Duration) {
        if let Some(output) = self.outputs.remove(&recovery.source) {
            eprintln!(
                "routing worker: retired output for device {} (recovered at {})",
                recovery.source.device,
                recovery
                    .origin
                    .map_or_else(|| "unknown".into(), |origin| origin.to_string())
            );
            Self::retire_output(recovery, output, now);
        }
    }
    fn retire_output(recovery: crate::router::Recovery, mut output: Output, now: Duration) {
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
    fn teardown(&mut self, seat: &str, now: Duration) {
        if let Some(mut owner) = self.seats.remove(seat) {
            if let Some(keys) = owner.keys.take()
                && let Err(error) = self.close_keys(keys, now)
            {
                self.key_failure.get_or_insert(error);
            }
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
                if self.key_drain_pending(seat) {
                    if let Some(output) = self.outputs.remove(&recovery.source) {
                        self.closing_outputs.push((recovery, output));
                    }
                } else {
                    self.retire(recovery, now);
                }
            }
        }
        self.outputs.retain(|source, _| source.seat != seat);
        self.messages.discard_seat(seat);
        self.pending.retain(|pending| {
            if pending.seat == seat {
                self.pending_bytes -= pending.bytes;
                false
            } else {
                true
            }
        });
    }
    fn restrict_scopes(
        &mut self,
        scopes: &[crate::subscription_transfer::Scope],
        now: Duration,
    ) -> io::Result<()> {
        crate::subscription_transfer::validate_scopes(scopes)?;
        for scope in scopes {
            if scope.fence.configuration != self.generation
                || !self.config.policy.allowed_seats.contains(&scope.seat)
            {
                return Err(invalid("routing scope differs from configured policy"));
            }
            if let Some(seat) = self.seats.get(&scope.seat) {
                scope.validate_transition(&seat.subscription)?;
            }
        }
        let removed: Vec<_> = self
            .seats
            .iter()
            .filter(|(_, seat)| !scopes.iter().any(|scope| scope.retains(&seat.subscription)))
            .map(|(name, _)| name.clone())
            .collect();
        for name in removed {
            self.teardown(&name, now);
        }
        for scope in scopes {
            if let Some(seat) = self.seats.get_mut(&scope.seat) {
                seat.subscription
                    .capabilities
                    .retain(|capability| scope.capabilities.contains(capability));
                seat.routing
                    .restrict_actions(&seat.subscription.capabilities);
                self.sync_seat(&scope.seat, now)?;
                self.collect(&scope.seat, now, None)?;
            }
        }
        self.prune_reports(now)?;
        Ok(())
    }
    fn expire_subscriptions(&mut self, now: Duration) -> io::Result<()> {
        if let Some(scopes) = self.subscriptions.expire(now) {
            for scope in scopes {
                eprintln!(
                    "routing worker: seat {} lost its context: subscription transfer expired",
                    scope.seat
                );
                self.teardown(&scope.seat, now);
                self.queue(Message::Outcome {
                    report: Report::ContextLost {
                        subscription: scope.identity(),
                    },
                })?;
            }
        }
        Ok(())
    }
    fn apply_subscriptions(&mut self, update: Subscriptions, now: Duration) -> io::Result<()> {
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
                    eprintln!(
                        "routing worker: seat {} lost its context: {error}",
                        subscription.seat
                    );
                    self.teardown(&subscription.seat, now);
                    self.queue(Message::Outcome {
                        report: Report::ContextLost { subscription },
                    })?;
                }
            } else {
                return Err(error);
            }
        }
        Ok(())
    }
    fn transfer_subscriptions(&mut self, command: Command, now: Duration) -> io::Result<()> {
        self.expire_subscriptions(now)?;
        if let Command::SubscriptionsBegin {
            revision, scopes, ..
        } = &command
            && !scopes.is_empty()
            && scopes
                .iter()
                .all(|scope| scope.fence.configuration < self.generation)
        {
            return self.subscriptions.retire(*revision);
        }
        let deadline = self.subscriptions.deadline();
        match self.subscriptions.accept(command, now)? {
            Some(crate::subscription_transfer::Update::Scope(scopes)) => {
                self.restrict_scopes(&scopes, now)?
            }
            Some(crate::subscription_transfer::Update::Complete(update)) => {
                let now = monotonic()?;
                if deadline.is_some_and(|deadline| now >= deadline) {
                    for subscription in update.seats {
                        eprintln!(
                            "routing worker: seat {} lost its context: subscription transfer completed late",
                            subscription.seat
                        );
                        self.teardown(&subscription.seat, now);
                        self.queue(Message::Outcome {
                            report: Report::ContextLost {
                                subscription: subscription.identity(),
                            },
                        })?;
                    }
                } else {
                    self.apply_subscriptions(update, now)?;
                }
            }
            None => {}
        }
        Ok(())
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
                crate::subscription_transfer::Scope::from_subscription(subscription)
                    .validate_transition(&seat.subscription)?;
                if seat.subscription.context.as_ref().is_some_and(|old| {
                    context.snapshot.revision < old.snapshot.revision
                        || (context.snapshot.revision == old.snapshot.revision && context != old)
                }) {
                    return Err(invalid("routing context revision regressed or changed"));
                }
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
                } else {
                    authority.tick(now);
                }
                if authority.generation() != subscription.fence.session {
                    return Err(io::Error::other(crate::context::Rejection::StaleSnapshot));
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
                let mut routing =
                    Routing::with_input_projection(self.config.clone(), subscription.fence)?;
                routing.configuration_generation(self.generation)?;
                routing.recognition_ready(false, now)?;
                self.seats.insert(
                    subscription.seat.clone(),
                    Seat {
                        subscription: subscription.clone(),
                        routing,
                        expires: now + protocol::LEASE,
                        started: now,
                        recognizer_ready: None,
                        input_enabled: false,
                        input_started: now,
                        metadata_enabled: false,
                        metadata_started: now,
                        keys: None,
                    },
                );
                self.queue(Message::Outcome {
                    report: Report::Ready {
                        subscription: subscription.identity(),
                    },
                })?;
            }
            let seat = self.seats.get_mut(&subscription.seat).unwrap();
            *seat.routing.authority_mut() = prepared.remove(&subscription.seat).unwrap();
            seat.routing.sync_authority(now)?;
            let mut capabilities = subscription.capabilities.clone();
            if !seat.keys.as_ref().is_some_and(|keys| keys.available(now)) {
                capabilities.remove(&protocol::Capability::SyntheticInput);
            }
            seat.routing
                .actions_available(subscription.fence.session, capabilities, now);
            seat.subscription = subscription;
            seat.expires = now + protocol::LEASE;
            let name = seat.subscription.seat.clone();
            self.sync_seat(&name, now)?;
            self.collect(&name, now, None)?;
        }
        self.prune_reports(now)?;
        self.revision = update.revision;
        Ok(())
    }
    /// Creates and publishes a source's mirror with its readiness probes.
    fn prepare_output(
        &mut self,
        raw: &mut RawDevice,
        device: &Device,
        health: &Heartbeat,
        now: Duration,
    ) -> io::Result<()> {
        let subscription = self.seats[&device.source.seat].subscription.clone();
        let consumer = subscription
            .consumer
            .clone()
            .ok_or_else(|| invalid("takeover has no bound consumer"))?;
        let id = raw.input_id();
        let identity = KernelIdentity {
            bus: id.bus_type().0,
            vendor: id.vendor(),
            product: id.product(),
            version: id.version(),
        };
        let compositor = matches!(consumer, crate::transport::Consumer::Compositor { .. });
        let descriptor = if compositor && device.capabilities.contains_key(&20) {
            Descriptor::mirror_with_compositor_repeat(device, identity, &BTreeMap::new())
        } else {
            Descriptor::mirror(device, identity, None, &BTreeMap::new())
        }
        .map_err(invalid)?;
        let publication = self.registry.prepare_for_worker(descriptor, health)?;
        let preparation = NativePreparation::new(&publication, consumer, now)?;
        let feedback = preparation
            .descriptor
            .capabilities
            .contains_key(&17)
            .then(|| linux::feedback::LedFeedback::new(raw, &preparation.descriptor))
            .transpose()?;
        let state_probe = linux::StateProbe::new(raw)?;
        let output = Virtual::create(publication)?;
        if self.releases.len() >= 4096 {
            return Err(invalid("too many pending reader releases"));
        }
        self.releases.insert(device.source.clone());
        self.outputs.insert(
            device.source.clone(),
            Output {
                fence: subscription.fence,
                virtual_device: output,
                state_probe: Some(state_probe),
                feedback,
                readiness: Some(preparation.readiness),
                consumer: None,
                offered: Duration::ZERO,
                started: now,
                in_flight: None,
                queued: None,
                sequence: 0,
            },
        );
        Ok(())
    }
    /// Keeps a source with its original consumer, before any mirror exists,
    /// while its seat has no consumer or the consumer refuses takeover.
    fn refuse_takeover(&mut self, device: &Device) -> bool {
        if self.refused.contains(&device.source) {
            return true;
        }
        let Some(consumer) = self
            .seats
            .get(&device.source.seat)
            .and_then(|seat| seat.subscription.consumer.as_ref())
        else {
            return true;
        };
        let Err(error) = native_boundary(consumer) else {
            return false;
        };
        self.refuse_output(device, &error);
        true
    }
    fn collect(&mut self, name: &str, now: Duration, input_at: Option<Duration>) -> io::Result<()> {
        self.collect_input(name, now, input_at, None)
    }
    fn collect_input(
        &mut self,
        name: &str,
        now: Duration,
        input_at: Option<Duration>,
        input: Option<&crate::input::Frame>,
    ) -> io::Result<()> {
        let Some(seat) = self.seats.get_mut(name) else {
            return Ok(());
        };
        let subscription = identity(&seat.subscription);
        let tickets = if seat.recognizer_ready == Some(true) {
            input_at
                .map(|at| seat.routing.recognition_requests_after_input(at, now))
                .transpose()?
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let cancellations = seat.routing.take_recognition_cancellations();
        let diagnostics = seat.routing.take_events();
        let dispatches = seat.routing.take_scoped_dispatches(now);
        let recoveries = seat.routing.take_recoveries();
        for recovery in recoveries {
            let source = recovery.source.clone();
            if let Some(keys) = self.seats.get_mut(name).and_then(|seat| seat.keys.as_mut()) {
                keys.retire_source(&source, now)?;
            }
            self.retire(recovery, now);
            self.queue(Message::Outcome {
                report: Report::SourceInvalidated {
                    subscription: subscription.clone(),
                    source,
                },
            })?;
        }
        for cancellation in cancellations {
            self.queue(Message::Outcome {
                report: Report::RecognitionCancelled {
                    subscription: subscription.clone(),
                    cancellation,
                },
            })?;
        }
        for ticket in tickets {
            self.queue(Message::Outcome {
                report: Report::Recognition {
                    subscription: subscription.clone(),
                    ticket: Box::new(ticket),
                },
            })?;
        }
        for event in diagnostics {
            if matches!(event, crate::rules::Event::Cancelled { .. })
                && let Some(keys) = self.seats.get_mut(name).and_then(|seat| seat.keys.as_mut())
            {
                keys.cancel(&event, now)?;
            }
            self.queue(Message::Outcome {
                report: Report::Observation {
                    subscription: subscription.clone(),
                    event: Box::new(event),
                },
            })?;
        }
        let mut key_work = Vec::new();
        for dispatch in dispatches {
            if let crate::rules::Event::Matched {
                rule,
                action: crate::config::Action::Keys { .. },
                stage: crate::rules::Stage::Discrete | crate::rules::Stage::Begin,
                ..
            } = &dispatch.event
            {
                *self.key_actions.entry(rule.clone()).or_default() += 1;
            }
            if matches!(
                &dispatch.event,
                crate::rules::Event::Matched {
                    action: crate::config::Action::Keys { .. }
                        | crate::config::Action::Remap { .. },
                    ..
                }
            ) {
                let work = dispatch
                    .into_key_work()
                    .map_err(|_| invalid("native key dispatch lost its original scope"))?;
                key_work.push(work);
                continue;
            }
            self.queue(Message::Outcome {
                report: Report::Dispatch {
                    subscription: subscription.clone(),
                    event: Box::new(dispatch.event),
                },
            })?;
        }
        let seat = self.seats.get_mut(name).unwrap();
        if let Some(keys) = seat.keys.as_mut() {
            keys.admit(&seat.routing, key_work, input, now)?;
        } else if !key_work.is_empty() {
            return Err(invalid("native key dispatch has no generated output"));
        }
        Ok(())
    }
}
impl InputSink for Stream {
    fn service_owned(&mut self, health: Option<&Heartbeat>) -> io::Result<()> {
        self.service()?;
        self.service_keys(health)
    }
    fn reader_released(&mut self, source: &Source) -> io::Result<()> {
        if self.outputs.contains_key(source) || self.key_drain_pending(&source.seat) {
            return Err(invalid("original closed before virtual retirement"));
        }
        self.releases.remove(source);
        Ok(())
    }
    fn reader(
        &mut self,
        raw: &mut RawDevice,
        device: &Device,
        buffer: ReaderBuffer,
        health: Option<&Heartbeat>,
    ) -> io::Result<ReaderDisposition> {
        let now = monotonic()?;
        if self.key_drain_pending(&device.source.seat) {
            return Ok(ReaderDisposition::Keep);
        }
        if (self.grabbed.contains(&device.source) || self.releases.contains(&device.source))
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
                self.collect(&device.source.seat, now, None)?;
                self.outputs.remove(&device.source);
                return Ok(ReaderDisposition::Reopen);
            }
            self.outputs.remove(&device.source);
            return Ok(ReaderDisposition::Keep);
        }
        if self.grabbed.contains(&device.source) {
            if !self.outputs.contains_key(&device.source) {
                return Ok(ReaderDisposition::Reopen);
            }
            return Ok(ReaderDisposition::Keep);
        }
        if !self.outputs.contains_key(&device.source) {
            if self.refuse_takeover(device) {
                return Ok(ReaderDisposition::Keep);
            }
            let health = health.ok_or_else(|| invalid("routing requires supervisor ownership"))?;
            if let Err(error) = self.prepare_output(raw, device, health, now) {
                self.refuse_output(device, &error);
                return Ok(ReaderDisposition::Keep);
            }
        }
        let output = self.outputs.get_mut(&device.source).unwrap();
        if now.saturating_sub(output.started) >= Duration::from_secs(5) {
            self.refuse_output(device, &"virtual device readiness timed out before grab");
            return Ok(ReaderDisposition::Keep);
        }
        self.offer_transport(&device.source, now)?;
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
        if output
            .feedback
            .as_ref()
            .is_some_and(|feedback| !feedback.ready(now))
        {
            return Ok(ReaderDisposition::Keep);
        }
        // Pending complete and partial frames remain on the observation path.
        if !buffer.is_drained() {
            return Ok(ReaderDisposition::Keep);
        }
        if !output.state_probe.as_ref().unwrap().is_neutral(device)? {
            return Ok(ReaderDisposition::Keep);
        }
        if let Err(error) = consumer_boundary(
            output.readiness.as_mut().unwrap(),
            output.virtual_device.descriptor(),
            evidence,
            output.feedback.is_some(),
            now,
        ) {
            self.refuse_output(device, &error);
            return Ok(ReaderDisposition::Keep);
        }
        let quiet_since = self
            .last_input
            .get(&device.source)
            .map_or(output.started, |at| (*at).max(output.started));
        if now.saturating_sub(quiet_since) < takeover::QUIET {
            return Ok(ReaderDisposition::Keep);
        }
        match takeover::Kernel::new(raw) {
            Ok(mut cutover) => self.take_over(device, &mut cutover),
            Err(error) => {
                self.refuse_output(device, &error);
                Ok(ReaderDisposition::Keep)
            }
        }
    }
    fn reader_cutover(&mut self, source: &Source, _through: u64, _at: Duration) -> io::Result<()> {
        self.attach_takeover(source)
    }
    fn push(&mut self, record: Record) -> io::Result<()> {
        let now = monotonic()?;
        if matches!(record, Record::Header { .. }) {
            return Ok(());
        }
        let names: Vec<_> = self.seats.keys().cloned().collect();
        for name in names {
            self.sync_seat(&name, now)?;
        }
        let inputs = self.baseline_inputs(&record, now)?;
        self.input_reports(&record, inputs, now)?;
        if let Record::Tick { at } = record {
            self.apply_pending(at, monotonic()?)?;
        }
        if let Record::Frame { frame } = &record {
            self.last_input.insert(frame.source.clone(), frame.at);
        }
        if let Record::Remove { source, .. } = record {
            self.outputs.remove(&source);
            self.grabbed.remove(&source);
            self.refused.remove(&source);
            self.committed.remove(&source);
            self.last_input.remove(&source);
        }
        Ok(())
    }
    fn service(&mut self) -> io::Result<()> {
        let now = monotonic()?;
        self.expire_subscriptions(now)?;
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
            .map(|(name, seat)| {
                let reason = if now >= seat.expires {
                    format!("lease expired {} ms ago", (now - seat.expires).as_millis())
                } else {
                    format!("state {:?}", seat.routing.authority().state(now))
                };
                (name.clone(), identity(&seat.subscription), reason)
            })
            .collect();
        for (name, subscription, reason) in expired {
            eprintln!("routing worker: seat {name} lost its context: {reason}");
            self.teardown(&name, now);
            self.queue(Message::Outcome {
                report: Report::ContextLost { subscription },
            })?;
        }
        for _ in 0..8 {
            let Some(packet) = self.channel.receive::<Command>()? else {
                break;
            };
            let now = monotonic()?;
            match packet.message {
                Command::Software { request } => {
                    let accepted = if let Some(seat) = self.seats.get_mut(&request.source.seat) {
                        let lease_valid = match request.operation {
                            crate::keys::producer::Operation::Open { expires }
                            | crate::keys::producer::Operation::Renew { expires } => {
                                expires <= seat.expires
                            }
                            _ => true,
                        };
                        if let Some(keys) = &mut seat.keys
                            && lease_valid
                        {
                            keys.producer_request(&seat.routing, &request, now)?
                        } else {
                            false
                        }
                    } else {
                        false
                    };
                    self.queue(Message::SoftwareResult {
                        fence: request.fence,
                        producer: request.producer,
                        sequence: request.sequence,
                        accepted,
                    })?;
                }
                command @ (Command::Recognition { .. } | Command::Rules { .. }) => {
                    self.enqueue(command, now)?
                }
                Command::Subscriptions { update } => {
                    if !update.seats.is_empty()
                        && update
                            .seats
                            .iter()
                            .all(|subscription| subscription.fence.configuration < self.generation)
                    {
                        self.subscriptions.retire(update.revision)?;
                        continue;
                    }
                    self.subscriptions.direct(update.revision)?;
                    self.apply_subscriptions(update, monotonic()?)?;
                }
                command @ (Command::SubscriptionsBegin { .. }
                | Command::SubscriptionsChunk { .. }
                | Command::SubscriptionsCommit { .. }) => {
                    self.transfer_subscriptions(command, monotonic()?)?
                }
                Command::Transport {
                    fence,
                    source,
                    marker,
                    evidence,
                } => {
                    if let Some(keys) = self
                        .seats
                        .get_mut(&source.seat)
                        .and_then(|seat| seat.keys.as_mut())
                        && keys.matches(fence, &source, &marker)
                    {
                        keys.observe(fence, &source, &marker, &evidence, monotonic()?)?;
                        continue;
                    }
                    if let Some(output) = self.outputs.get_mut(&source)
                        && output.fence == fence
                        && output.virtual_device.descriptor().marker == marker
                    {
                        output.consumer = Some(evidence);
                    }
                }
                Command::KeyDrained { request, at } => {
                    self.acknowledge_key_drain(&request, at, monotonic()?);
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
                            crate::worker::preflight(&config)?;
                            let names: Vec<_> = self.seats.keys().cloned().collect();
                            for name in names {
                                self.teardown(&name, now);
                            }
                            self.config = config;
                            self.baseline = baseline;
                            self.generation = installation.generation;
                            self.subscriptions.cancel();
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
        self.prune_reports(now)?;
        Ok(())
    }
    fn flush(&mut self) -> io::Result<()> {
        let now = monotonic()?;
        self.refresh_keys(now)?;
        self.expire_subscriptions(now)?;
        self.prune_reports(now)?;
        for (source, output) in &mut self.outputs {
            if let Some(feedback) = &mut output.feedback {
                feedback.service(
                    &mut output.virtual_device,
                    self.grabbed.contains(source),
                    now,
                )?;
            }
        }
        let sources: Vec<_> = self
            .outputs
            .keys()
            .filter(|source| self.grabbed.contains(*source))
            .cloned()
            .collect();
        for source in sources {
            if self
                .closing_keys
                .iter()
                .any(|keys| keys.seat() == source.seat && !keys.closed())
            {
                continue;
            }
            let Some(seat) = self.seats.get_mut(&source.seat) else {
                continue;
            };
            let output = self.outputs.get_mut(&source).unwrap();
            if output.in_flight.is_none()
                && let Some(scheduled) = seat.routing.due_after_input(&source, now)?.cloned()
            {
                if seat.keys.as_ref().is_some_and(|keys| {
                    keys.blocks_input(
                        &seat.routing,
                        &source,
                        scheduled.frame.sequence,
                        seat.routing.input_span(&source, scheduled.ticket),
                    )
                }) {
                    continue;
                }
                output.sequence = scheduled.frame.sequence;
                let frame = if let Some(feedback) = &mut output.feedback {
                    feedback.prepare_forward(&scheduled.frame)?
                } else {
                    std::borrow::Cow::Borrowed(&scheduled.frame)
                };
                output.virtual_device.enqueue(&frame)?;
                output.in_flight = Some(scheduled.ticket);
                output.queued = Some((scheduled.frame.at, scheduled.due));
            }
            if output.in_flight.is_some() && output.virtual_device.flush()? == Flush::Complete {
                let ticket = output.in_flight.take().unwrap();
                let written = monotonic()?;
                if let Some((at, due)) = output.queued.take()
                    && let Some(forwarding) = self.forwarding.get_mut(&source)
                {
                    forwarding.record(at, due, written);
                }
                seat.routing
                    .delivered_after_input(&source, ticket, written)?;
            }
        }
        self.messages.flush(&mut self.channel)
    }
    fn deadline(&self) -> Option<Duration> {
        self.seats
            .values()
            .flat_map(|seat| {
                seat.routing
                    .deadline_with_barriers(|source, scheduled, span| {
                        self.outputs
                            .get(source)
                            .is_some_and(|output| output.in_flight.is_some())
                            || self
                                .closing_keys
                                .iter()
                                .any(|keys| keys.seat() == source.seat && !keys.closed())
                            || seat.keys.as_ref().is_some_and(|keys| {
                                keys.blocks_input(
                                    &seat.routing,
                                    source,
                                    scheduled.frame.sequence,
                                    span,
                                )
                            })
                    })
                    .into_iter()
                    .chain([
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
                self.seats
                    .values()
                    .filter_map(|seat| seat.keys.as_ref().and_then(keys::Slot::deadline)),
            )
            .chain(self.closing_keys.iter().filter_map(keys::Slot::deadline))
            .chain(self.subscriptions.deadline())
            .chain(self.pending.front().map(|pending| pending.received_at))
            .chain(
                self.outputs
                    .values()
                    .filter(|output| output.readiness.is_some())
                    .map(|output| output.offered + Duration::from_millis(100)),
            )
            .chain(
                self.outputs.values().filter_map(|output| {
                    output.feedback.as_ref().map(|feedback| feedback.deadline())
                }),
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
        for output in self.outputs.values() {
            let mut flags = PollFlags::empty();
            if output.in_flight.is_some() {
                flags |= PollFlags::POLLOUT;
            }
            if output.feedback.is_some() {
                flags |= PollFlags::POLLIN;
            }
            if !flags.is_empty() {
                sources.push(PollFd::new(output.virtual_device.as_fd(), flags));
            }
            if let Some(feedback) = output
                .feedback
                .as_ref()
                .filter(|feedback| feedback.pending())
            {
                sources.push(PollFd::new(feedback.as_fd(), PollFlags::POLLOUT));
            }
        }
        for keys in self
            .seats
            .values()
            .filter_map(|seat| seat.keys.as_ref())
            .chain(self.closing_keys.iter())
        {
            if let Some(fd) = keys.poll_fd() {
                sources.push(PollFd::new(fd, PollFlags::POLLOUT));
            }
        }
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
        self.shutdown_keys();
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
    mod preparation;
    mod recognizers;

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
}
