//! Private supervisor/worker observation transport. Public clients never receive
//! raw frames, descriptors, or the ability to establish desktop authority.

use crate::{
    config::Config,
    configuration::{Assembly, Command, INSTALL_TIMEOUT},
    ipc::Channel,
    protocol::{self, Fence, MAX_QUEUED_BYTES, MAX_QUEUED_PACKETS},
    replay::Record,
    rules::{Engine, Event},
};
use nix::poll::{PollFd, PollFlags};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    io,
    os::fd::AsFd,
    time::Duration,
};

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Subscription {
    pub seat: String,
    pub fence: Fence,
    #[serde(default)]
    pub context: Option<Box<ProvidedContext>>,
    #[serde(default)]
    pub capabilities: BTreeSet<crate::protocol::Capability>,
    #[serde(default)]
    pub consumer: Option<crate::transport::Consumer>,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProvidedContext {
    pub binding: crate::context::Binding,
    pub snapshot: crate::context::Snapshot,
}
impl Subscription {
    fn same_registration(&self, other: &Self) -> bool {
        self.seat == other.seat && self.fence == other.fence
    }
    fn identity(&self) -> Self {
        Self {
            seat: self.seat.clone(),
            fence: self.fence,
            context: None,
            capabilities: BTreeSet::new(),
            consumer: None,
        }
    }
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Subscriptions {
    pub revision: u64,
    pub seats: Vec<Subscription>,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Report {
    Ready {
        subscription: Subscription,
    },
    Observation {
        subscription: Subscription,
        event: Box<Event>,
    },
    Input {
        subscription: Subscription,
        input: Box<crate::input_events::Event>,
    },
    Ended {
        subscription: Subscription,
        reason: String,
    },
    ContextLost {
        subscription: Subscription,
    },
    Dispatch {
        subscription: Subscription,
        event: Box<Event>,
    },
    SourceInvalidated {
        subscription: Subscription,
        source: crate::input::Source,
    },
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Message {
    RulesChanged {
        seat: String,
        fence: Fence,
        id: u64,
        revision: u64,
        error: Option<String>,
    },
    Configuration {
        generation: u64,
        error: Option<String>,
    },
    Outcome {
        report: Report,
    },
    Transport {
        subscription: Subscription,
        descriptor: Box<crate::transport::Descriptor>,
    },
}
impl Report {
    pub fn subscription(&self) -> &Subscription {
        match self {
            Self::Ready { subscription }
            | Self::Observation { subscription, .. }
            | Self::Input { subscription, .. }
            | Self::Ended { subscription, .. }
            | Self::ContextLost { subscription }
            | Self::Dispatch { subscription, .. } => subscription,
            Self::SourceInvalidated { subscription, .. } => subscription,
        }
    }
}

struct Observer {
    subscription: Subscription,
    started: Duration,
    expires: Duration,
    engine: Engine,
    input_enabled: bool,
    input_started: Duration,
}

fn permits_input(config: &Config, subscription: &Subscription, now: Duration) -> bool {
    crate::input_events::permitted_subscription(
        config,
        &subscription.capabilities,
        subscription.context.as_deref(),
        &subscription.seat,
        subscription.fence.session,
        now,
    )
}
struct Registry {
    config: Config,
    baseline: Engine,
    observers: BTreeMap<String, Observer>,
    blocked: BTreeMap<String, Fence>,
    revision: u64,
    reports: VecDeque<(Report, usize)>,
    bytes: usize,
}
impl Registry {
    fn new(config: Config) -> io::Result<Self> {
        let mut baseline = config.clone();
        for rule in &mut baseline.rules {
            rule.enabled = false;
        }
        Ok(Self {
            config,
            baseline: Engine::new(baseline)?,
            observers: BTreeMap::new(),
            blocked: BTreeMap::new(),
            revision: 0,
            reports: VecDeque::new(),
            bytes: 0,
        })
    }
    fn update(&mut self, update: Subscriptions, now: Duration) -> io::Result<()> {
        if !update.seats.is_empty()
            && update
                .seats
                .iter()
                .all(|subscription| subscription.fence.configuration < self.baseline.generation())
        {
            return Ok(());
        }
        if update.revision <= self.revision || update.seats.len() > crate::service::MAX_CLIENTS {
            return Err(protocol::invalid(
                "invalid worker subscription revision or count",
            ));
        }
        let mut seats = BTreeSet::new();
        let mut epoch = None;
        let mut authorities = BTreeMap::new();
        for subscription in &update.seats {
            subscription.fence.validate()?;
            if !self
                .config
                .policy
                .allowed_seats
                .contains(&subscription.seat)
                || !seats.insert(subscription.seat.clone())
                || subscription.fence.configuration != self.baseline.generation()
                || epoch.is_some_and(|value| value != subscription.fence.broker)
            {
                return Err(protocol::invalid(
                    "worker subscription differs from device policy",
                ));
            }
            epoch = Some(subscription.fence.broker);
            let existing = self
                .observers
                .get(&subscription.seat)
                .filter(|observer| observer.subscription.same_registration(subscription));
            if let Some(observer) = existing {
                match (&observer.subscription.context, &subscription.context) {
                    (None, None) => {}
                    (Some(old), Some(new)) if old.binding == new.binding => {
                        if new.snapshot.revision < old.snapshot.revision
                            || (new.snapshot.revision == old.snapshot.revision && new != old)
                        {
                            return Err(protocol::invalid(
                                "provided context revision regressed or changed",
                            ));
                        }
                    }
                    _ => {
                        return Err(protocol::invalid(
                            "context transition needs a fresh registration fence",
                        ));
                    }
                }
            }
            if let Some(context) = &subscription.context {
                if context.binding.seat != subscription.seat
                    || context.snapshot.generation != subscription.fence.session
                {
                    return Err(protocol::invalid(
                        "provided context differs from registration",
                    ));
                }
                let mut authority = existing
                    .map_or_else(crate::context::Authority::default, |observer| {
                        observer.engine.authority().clone()
                    });
                if existing.is_none() {
                    authority
                        .adopt(context.binding.clone(), subscription.fence.session)
                        .map_err(io::Error::other)?;
                }
                if authority.generation() != subscription.fence.session {
                    return Err(io::Error::other(crate::context::Rejection::StaleGeneration));
                }
                if existing.is_none_or(|observer| {
                    observer
                        .subscription
                        .context
                        .as_ref()
                        .is_none_or(|old| old.snapshot.revision < context.snapshot.revision)
                }) {
                    authority
                        .update(context.snapshot.clone(), now)
                        .map_err(io::Error::other)?;
                } else {
                    authority.tick(now);
                    if authority.generation() != subscription.fence.session {
                        return Err(io::Error::other(crate::context::Rejection::StaleSnapshot));
                    }
                }
                authorities.insert(subscription.seat.clone(), authority);
            }
        }
        // Discard unsent outcomes as soon as their registration is replaced.
        let removed: Vec<_> = self
            .observers
            .values()
            .filter(|observer| {
                !update
                    .seats
                    .iter()
                    .any(|subscription| subscription.same_registration(&observer.subscription))
            })
            .map(|observer| observer.subscription.clone())
            .collect();
        for subscription in removed {
            self.remove(&subscription);
        }
        self.reports.retain(|(report, _)| {
            update.seats.iter().any(|subscription| {
                subscription.same_registration(report.subscription())
                    && (!matches!(report, Report::Input { .. })
                        || permits_input(&self.config, subscription, now))
            })
        });
        self.bytes = self.reports.iter().map(|(_, bytes)| bytes).sum();
        self.blocked.retain(|seat, fence| {
            update
                .seats
                .iter()
                .any(|subscription| &subscription.seat == seat && &subscription.fence == fence)
        });
        let mut initial_inputs = Vec::new();
        for subscription in update.seats {
            if self.blocked.get(&subscription.seat) == Some(&subscription.fence) {
                continue;
            }
            if let Some(observer) = self.observers.get_mut(&subscription.seat) {
                let input_enabled = permits_input(&self.config, &subscription, now);
                if input_enabled && !observer.input_enabled {
                    observer.input_started = now;
                    initial_inputs.extend(observer.engine.input_states(now).into_iter().map(
                        |input| Report::Input {
                            subscription: subscription.identity(),
                            input: Box::new(input),
                        },
                    ));
                }
                observer.input_enabled = input_enabled;
                observer.expires = now + protocol::MAX_PACKET_AGE;
                if let Some(authority) = authorities.remove(&subscription.seat) {
                    *observer.engine.authority_mut() = authority;
                }
                observer.subscription = subscription;
                continue;
            }
            let mut engine = self.baseline.fork_observer(
                self.config.clone(),
                &subscription.seat,
                subscription.fence.session,
                now,
            )?;
            if let Some(authority) = authorities.remove(&subscription.seat) {
                *engine.authority_mut() = authority;
            }
            let input_enabled = permits_input(&self.config, &subscription, now);
            if input_enabled {
                initial_inputs.extend(engine.input_states(now).into_iter().map(|input| {
                    Report::Input {
                        subscription: subscription.identity(),
                        input: Box::new(input),
                    }
                }));
            }
            self.observers.insert(
                subscription.seat.clone(),
                Observer {
                    subscription: subscription.clone(),
                    started: now,
                    expires: now + protocol::MAX_PACKET_AGE,
                    engine,
                    input_enabled,
                    input_started: now,
                },
            );
            self.queue(
                Report::Ready {
                    subscription: subscription.identity(),
                },
                now,
            )?;
        }
        for report in initial_inputs {
            self.queue(report, now)?;
        }
        self.revision = update.revision;
        Ok(())
    }
    fn remove(&mut self, subscription: &Subscription) {
        if self
            .observers
            .get(&subscription.seat)
            .is_some_and(|observer| observer.subscription.same_registration(subscription))
        {
            self.observers.remove(&subscription.seat);
        }
        self.reports
            .retain(|(report, _)| !report.subscription().same_registration(subscription));
        self.bytes = self.reports.iter().map(|(_, bytes)| bytes).sum();
    }
    fn end(&mut self, subscription: Subscription, reason: &str, now: Duration) -> io::Result<()> {
        self.remove(&subscription);
        self.blocked
            .insert(subscription.seat.clone(), subscription.fence);
        self.queue(
            Report::Ended {
                subscription: subscription.identity(),
                reason: reason.into(),
            },
            now,
        )
    }
    fn expire(&mut self, now: Duration) -> io::Result<()> {
        let lost: Vec<_> = self
            .observers
            .values_mut()
            .filter_map(|observer| {
                observer.subscription.context.as_ref()?;
                observer.engine.authority_mut().tick(now);
                (observer.engine.authority().generation() != observer.subscription.fence.session)
                    .then(|| observer.subscription.identity())
            })
            .collect();
        for subscription in lost {
            self.context_lost(subscription, now)?;
        }
        let expired: Vec<_> = self
            .observers
            .values()
            .filter(|observer| now >= observer.expires)
            .map(|observer| observer.subscription.clone())
            .collect();
        for subscription in expired {
            self.end(subscription, "worker observation lease expired", now)?;
        }
        Ok(())
    }
    fn context_lost(&mut self, subscription: Subscription, now: Duration) -> io::Result<()> {
        self.remove(&subscription);
        self.blocked
            .insert(subscription.seat.clone(), subscription.fence);
        self.queue(
            Report::ContextLost {
                subscription: subscription.identity(),
            },
            now,
        )
    }
    fn queue(&mut self, report: Report, now: Duration) -> io::Result<()> {
        // Assign transport sequence numbers only on successful sends. Account
        // for the largest number when bounding serialized retained bytes.
        let encoded = protocol::encode(&protocol::Packet {
            version: protocol::VERSION,
            sequence: u64::MAX,
            at: Duration::MAX,
            message: Message::Outcome {
                report: report.clone(),
            },
        });
        let bytes = match encoded {
            Ok(bytes) => bytes.len(),
            Err(_) if matches!(report, Report::Observation { .. } | Report::Input { .. }) => {
                return self.end(
                    report.subscription().clone(),
                    "worker observation exceeds packet limit",
                    now,
                );
            }
            Err(error) => return Err(error),
        };
        if let Report::Observation {
            subscription,
            event,
        } = &report
        {
            self.reports.retain(|(old, _)| {
                !matches!(old,
                Report::Observation { subscription: old_subscription, event: old_event }
                if old_subscription.same_registration(subscription) && event.supersedes_progress(old_event))
            });
            self.bytes = self.reports.iter().map(|(_, bytes)| bytes).sum();
        }
        if self.reports.len() >= MAX_QUEUED_PACKETS || self.bytes + bytes > MAX_QUEUED_BYTES {
            // No observation subscriber can stop physical input/health. Revoke
            // all affected streams, discard their queued work, and keep reading.
            let subscriptions: Vec<_> = self
                .observers
                .values()
                .map(|observer| observer.subscription.clone())
                .collect();
            self.reports.clear();
            self.bytes = 0;
            for subscription in subscriptions {
                self.end(subscription, "worker observation queue overflow", now)?;
            }
            return Ok(());
        }
        self.reports.push_back((report, bytes));
        self.bytes += bytes;
        Ok(())
    }
    fn push(&mut self, record: Record, now: Duration) -> io::Result<()> {
        self.expire(now)?;
        self.baseline.push(record.clone())?;
        self.baseline.take_events();
        let source = match &record {
            Record::Device { device, .. } => Some(&device.source),
            Record::Frame { frame } => Some(&frame.source),
            Record::Absolute { source, .. }
            | Record::Mapping { source, .. }
            | Record::Reconcile { source, .. }
            | Record::Remove { source, .. } => Some(source),
            Record::Tick { .. } => None,
            Record::Header { .. } => return Ok(()),
            _ => return Err(protocol::invalid("unsupported live worker record")),
        };
        let mut reports = Vec::new();
        for observer in self.observers.values_mut() {
            if source.is_some_and(|source| source.seat != observer.subscription.seat) {
                continue;
            }
            let input_enabled = observer.input_enabled
                && permits_input(&self.config, &observer.subscription, now)
                && !matches!(&record,Record::Frame {frame} if frame.at<observer.input_started)
                && !matches!(&record,Record::Mapping {at,..}|Record::Reconcile {at,..}|Record::Remove {at,..} if *at<observer.input_started);
            let record = match &record {
                Record::Frame { frame } if frame.at < observer.started => {
                    let Some(reconcile) = self
                        .baseline
                        .reconciliation(&frame.source, observer.started)
                    else {
                        continue;
                    };
                    reconcile
                }
                Record::Tick { at } if *at < observer.started => continue,
                Record::Mapping {
                    source,
                    at,
                    mapping,
                } if *at < observer.started => Record::Mapping {
                    source: source.clone(),
                    at: observer.started,
                    mapping: mapping.clone(),
                },
                Record::Remove { source, at } if *at < observer.started => Record::Remove {
                    source: source.clone(),
                    at: observer.started,
                },
                Record::Reconcile {
                    source,
                    at,
                    keys,
                    contacts,
                } if *at < observer.started => Record::Reconcile {
                    source: source.clone(),
                    at: observer.started,
                    keys: keys.clone(),
                    contacts: contacts.clone(),
                },
                _ => record.clone(),
            };
            let invalidated = match &record {
                Record::Mapping { source, .. }
                | Record::Reconcile { source, .. }
                | Record::Remove { source, .. } => Some(source.clone()),
                _ => None,
            };
            use crate::input_events::ResetReason;
            let before = if input_enabled {
                match &record {
                    Record::Remove { source, at } => {
                        observer
                            .engine
                            .input_reset(source, *at, ResetReason::Removed)
                    }
                    Record::Device { device, .. } => observer
                        .engine
                        .input_states(now)
                        .into_iter()
                        .find(|input| {
                            input.source().device == device.source.device
                                && input.source() != &device.source
                        })
                        .and_then(|input| {
                            observer
                                .engine
                                .input_reset(input.source(), now, ResetReason::Removed)
                        }),
                    _ => None,
                }
            } else {
                None
            };
            observer.engine.capture_input(input_enabled);
            observer.engine.push(record.clone())?;
            let mut inputs: Vec<_> = before.into_iter().collect();
            if input_enabled {
                match &record {
                    Record::Device { device, .. } => inputs.extend(observer.engine.input_reset(
                        &device.source,
                        now,
                        ResetReason::Attached,
                    )),
                    Record::Reconcile { source, at, .. } => inputs.extend(
                        observer
                            .engine
                            .input_reset(source, *at, ResetReason::Reconciled),
                    ),
                    Record::Mapping { source, at, .. } => inputs.extend(
                        observer
                            .engine
                            .input_reset(source, *at, ResetReason::MappingChanged),
                    ),
                    Record::Frame { frame } if frame.at >= observer.input_started => {
                        if let Some(input) = observer.engine.take_input() {
                            inputs.push(input);
                        } else {
                            inputs.extend(observer.engine.input_reset(
                                &frame.source,
                                frame.at,
                                ResetReason::Lost,
                            ));
                        }
                    }
                    _ => {}
                }
            }
            for input in inputs {
                reports.push(Report::Input {
                    subscription: observer.subscription.identity(),
                    input: Box::new(input),
                });
            }
            if let Some(source) = invalidated {
                reports.push(Report::SourceInvalidated {
                    subscription: observer.subscription.identity(),
                    source,
                });
            }
            for event in observer.engine.take_events() {
                reports.push(Report::Observation {
                    subscription: observer.subscription.identity(),
                    event: Box::new(event),
                });
            }
        }
        for report in reports {
            if self
                .observers
                .get(&report.subscription().seat)
                .is_some_and(|observer| {
                    observer
                        .subscription
                        .same_registration(report.subscription())
                })
            {
                self.queue(report, now)?;
            }
        }
        Ok(())
    }
    fn transact_rules(
        &mut self,
        seat: &str,
        fence: Fence,
        transaction: &crate::rule_transactions::Transaction,
        now: Duration,
    ) -> io::Result<u64> {
        fence.validate()?;
        let observer = self
            .observers
            .get_mut(seat)
            .filter(|observer| observer.subscription.fence == fence && now < observer.expires)
            .ok_or_else(|| {
                protocol::invalid("runtime rules require the current owning seat registration")
            })?;
        let revision = observer.engine.transact_rules(transaction, now)?;
        let subscription = observer.subscription.identity();
        let events = observer.engine.take_events();
        // Discard outcomes prepared by retired rules before publishing their
        // cancellation and the new revision. Other seats remain untouched.
        self.reports.retain(|(report, _)| {
            !report.subscription().same_registration(&subscription)
                || !matches!(report, Report::Observation { .. } | Report::Dispatch { .. })
        });
        self.bytes = self.reports.iter().map(|(_, bytes)| *bytes).sum();
        for event in events {
            if matches!(event, Event::Cancelled { .. }) {
                self.queue(
                    Report::Observation {
                        subscription: subscription.clone(),
                        event: Box::new(event),
                    },
                    now,
                )?;
            }
        }
        Ok(revision)
    }
    fn deadline(&self) -> Option<Duration> {
        self.observers
            .values()
            .flat_map(|observer| {
                observer
                    .engine
                    .deadline()
                    .into_iter()
                    .chain([observer.expires])
                    .chain(observer.subscription.context.as_ref().map(|context| {
                        context
                            .snapshot
                            .captured_at
                            .saturating_add(crate::context::SNAPSHOT_MAX_AGE)
                            .saturating_add(Duration::from_millis(1))
                    }))
            })
            .min()
    }
    fn install(&mut self, config: Config, generation: u64, now: Duration) -> io::Result<()> {
        if self.baseline.generation().checked_add(1) != Some(generation) {
            return Err(protocol::invalid(
                "configuration generation must advance exactly once",
            ));
        }
        // Validate all new recognizers against retained device state before
        // cancelling a single old candidate or changing the baseline.
        self.baseline.prepare_replacement(config.clone(), now)?;
        let mut baseline = config.clone();
        for rule in &mut baseline.rules {
            rule.enabled = false;
        }
        let baseline = self.baseline.prepare_replacement(baseline, now)?;
        self.config = config;
        self.baseline = baseline;
        self.observers.clear();
        self.blocked.clear();
        self.reports.clear();
        self.bytes = 0;
        Ok(())
    }
}

/// Receive exactly the broker's validated startup bytes. Input descriptors are
/// opened only after this completes; the original path is never reread.
pub fn bootstrap(
    channel: &mut Channel,
    health: &mut crate::supervisor::Heartbeat,
) -> io::Result<Config> {
    use nix::poll::{PollTimeout, poll};
    let mut assembly = Assembly::default();
    let deadline = crate::ipc::monotonic()? + INSTALL_TIMEOUT;
    loop {
        health.progress()?;
        let now = crate::ipc::monotonic()?;
        if now >= deadline {
            return Err(protocol::invalid("startup configuration transfer expired"));
        }
        for _ in 0..8 {
            let Some(packet) = channel.receive::<Command>()? else {
                break;
            };
            if let Some(installation) = assembly.accept(packet.message, now)? {
                if installation.generation != 1 {
                    return Err(protocol::invalid("invalid startup generation"));
                }
                let config = installation.config?;
                Engine::new(config.clone())?;
                return Ok(config);
            }
        }
        let timeout = PollTimeout::try_from(
            (deadline.min(health.deadline()?)).saturating_sub(crate::ipc::monotonic()?),
        )
        .map_err(io::Error::other)?;
        if let Err(error) = poll(
            &mut [
                PollFd::new(channel.as_fd(), PollFlags::POLLIN),
                PollFd::new(health.as_fd(), PollFlags::POLLIN),
            ],
            timeout,
        ) && error != nix::errno::Errno::EINTR
        {
            return Err(error.into());
        }
    }
}

pub struct Stream {
    channel: Channel,
    registry: Registry,
    assembly: Assembly,
    control: VecDeque<Message>,
}
impl Stream {
    pub fn new(config: Config, channel: Channel) -> io::Result<Self> {
        Ok(Self {
            channel,
            registry: Registry::new(config)?,
            assembly: Assembly::default(),
            control: VecDeque::from([Message::Configuration {
                generation: 1,
                error: None,
            }]),
        })
    }
    pub fn push(&mut self, record: Record) -> io::Result<()> {
        self.registry.push(record, crate::ipc::monotonic()?)
    }
    pub fn service(&mut self) -> io::Result<()> {
        let now = crate::ipc::monotonic()?;
        self.registry.expire(now)?;
        for _ in 0..8 {
            let Some(packet) = self.channel.receive::<Command>()? else {
                break;
            };
            match packet.message {
                Command::Rules {
                    seat,
                    fence,
                    id,
                    transaction,
                } => {
                    if self.control.len() >= crate::protocol::MAX_QUEUED_PACKETS || id == 0 {
                        return Err(protocol::invalid(
                            "invalid runtime rule request/response count",
                        ));
                    }
                    let result = self
                        .registry
                        .transact_rules(&seat, fence, &transaction, now);
                    let revision = result.as_ref().copied().unwrap_or_else(|_| {
                        self.registry
                            .observers
                            .get(&seat)
                            .map_or(0, |observer| observer.engine.rule_revision())
                    });
                    self.control.push_back(Message::RulesChanged {
                        seat,
                        fence,
                        id,
                        revision,
                        error: result.err().map(|error| {
                            error
                                .to_string()
                                .chars()
                                .filter(|character| !character.is_control())
                                .take(128)
                                .collect()
                        }),
                    });
                }
                Command::Subscriptions { update } => {
                    let contexts: Vec<_> = update
                        .seats
                        .iter()
                        .filter(|subscription| subscription.context.is_some())
                        .map(Subscription::identity)
                        .collect();
                    if let Err(error) = self.registry.update(update, now) {
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
                            for subscription in contexts {
                                self.registry.context_lost(subscription, now)?;
                            }
                        } else {
                            return Err(error);
                        }
                    }
                }
                command => {
                    if let Some(installation) = self.assembly.accept(command, now)? {
                        if self.control.len() >= crate::protocol::MAX_QUEUED_PACKETS
                            || self
                                .control
                                .iter()
                                .any(|message| matches!(message, Message::Configuration { .. }))
                        {
                            return Err(protocol::invalid(
                                "configuration acknowledgements are not drained",
                            ));
                        }
                        let result = installation.config.and_then(|config| {
                            self.registry.install(config, installation.generation, now)
                        });
                        self.control.push_back(Message::Configuration {
                            generation: installation.generation,
                            error: result.err().map(|error| {
                                error
                                    .to_string()
                                    .chars()
                                    .filter(|ch| !ch.is_control())
                                    .take(512)
                                    .collect()
                            }),
                        });
                    }
                }
            }
        }
        if self
            .assembly
            .deadline()
            .is_some_and(|deadline| now >= deadline)
        {
            return Err(protocol::invalid("configuration transfer expired"));
        }
        Ok(())
    }
    pub fn flush(&mut self) -> io::Result<()> {
        let now = crate::ipc::monotonic()?;
        if let Some(message) = self.control.front() {
            if !self.channel.send(message, now)? {
                return Ok(());
            }
            self.control.pop_front();
        }
        for _ in 0..16 {
            let Some((report, bytes)) = self.registry.reports.front() else {
                break;
            };
            if !self.channel.send(
                &Message::Outcome {
                    report: report.clone(),
                },
                now,
            )? {
                break;
            }
            self.registry.bytes -= bytes;
            self.registry.reports.pop_front();
        }
        Ok(())
    }
    pub fn deadline(&self) -> Option<Duration> {
        self.registry
            .deadline()
            .into_iter()
            .chain(self.assembly.deadline())
            .min()
    }
    pub fn config(&self) -> &Config {
        &self.registry.config
    }
    pub fn generation(&self) -> u64 {
        self.registry.baseline.generation()
    }
    pub fn poll_source(&self) -> PollFd<'_> {
        PollFd::new(
            self.channel.as_fd(),
            PollFlags::POLLIN
                | if self.registry.reports.is_empty() && self.control.is_empty() {
                    PollFlags::empty()
                } else {
                    PollFlags::POLLOUT
                },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{Context, Key, Millis, Trigger},
        input::{Device, EV_KEY, EV_SYN, Frame, RawEvent, SYN_REPORT, Source},
        protocol::Epoch,
        rules::Evidence,
    };
    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }
    fn source(id: u64, seat: &str) -> Source {
        Source {
            device: id,
            generation: 1,
            seat: seat.into(),
        }
    }
    fn config() -> Config {
        let mut config = Config::preset("desktop").unwrap();
        config.policy.allowed_seats.push("seat1".into());
        config.devices.get_mut("keyboard").unwrap().keys = vec![Key(30)];
        config.rules[0].enabled = true;
        config.rules[0].trigger = Trigger::ButtonHold {
            key: Key(30),
            after: Millis(600),
            repeat: None,
        };
        config
    }
    fn subscription(seat: &str, session: u64) -> Subscription {
        Subscription {
            seat: seat.into(),
            fence: Fence {
                broker: Epoch([1, 2]),
                configuration: 1,
                session,
            },
            context: None,
            capabilities: BTreeSet::new(),
            consumer: None,
        }
    }
    fn with_context(mut subscription: Subscription, revision: u64, at: u64) -> Subscription {
        subscription.context = Some(Box::new(ProvidedContext {
            binding: crate::context::Binding {
                seat: subscription.seat.clone(),
                session: "verified-session".into(),
                uid: 1000,
                compositor_pid: 42,
                compositor_start: 123,
                kind: crate::context::SessionKind::User,
            },
            snapshot: crate::context::Snapshot {
                generation: subscription.fence.session,
                revision,
                captured_at: ms(at),
                state: crate::context::State::Desktop,
                outputs: vec![],
                windows: vec![],
                regions: vec![],
                secure_overlay: false,
            },
        }));
        subscription
    }
    fn registry(config: Config) -> Registry {
        let mut registry = Registry::new(config).unwrap();
        registry.push(Record::Header { version: 1 }, ms(0)).unwrap();
        for (id, seat) in [(1, "seat0"), (2, "seat1")] {
            let device = Device {
                source: source(id, seat),
                identity: format!("fixture-{id}"),
                name: "fixture".into(),
                path: "fixture".into(),
                physical: "fixture".into(),
                unique: String::new(),
                vendor: 1,
                product: 1,
                properties: vec![],
                axes: BTreeMap::new(),
                capabilities: BTreeMap::from([(EV_KEY, vec![30])]),
                virtual_device: false,
            };
            registry
                .push(
                    Record::Device {
                        device,
                        keys: BTreeSet::new(),
                        contacts: vec![],
                    },
                    ms(0),
                )
                .unwrap();
        }
        registry
    }
    fn update(registry: &mut Registry, revision: u64, seats: Vec<Subscription>, at: u64) {
        registry
            .update(Subscriptions { revision, seats }, ms(at))
            .unwrap();
    }
    fn key(registry: &mut Registry, source: Source, at: u64, sequence: u64, down: bool, now: u64) {
        registry
            .push(
                Record::Frame {
                    frame: Frame {
                        source,
                        at: ms(at),
                        sequence,
                        events: vec![
                            RawEvent {
                                kind: EV_KEY,
                                code: 30,
                                value: i32::from(down),
                            },
                            RawEvent {
                                kind: EV_SYN,
                                code: SYN_REPORT,
                                value: 0,
                            },
                        ],
                    },
                },
                ms(now),
            )
            .unwrap();
    }
    fn tick(registry: &mut Registry, at: u64) {
        registry.push(Record::Tick { at: ms(at) }, ms(at)).unwrap();
    }
    fn outcomes(registry: &Registry) -> Vec<&Event> {
        registry
            .reports
            .iter()
            .filter_map(|(report, _)| match report {
                Report::Observation { event, .. } => Some(event.as_ref()),
                _ => None,
            })
            .collect()
    }
    fn input_subscription(seat: &str, at: u64) -> Subscription {
        let mut subscription = with_context(subscription(seat, 1), 1, at);
        subscription.capabilities = BTreeSet::from([
            crate::protocol::Capability::InputEvents,
            crate::protocol::Capability::LuaEvents,
            crate::protocol::Capability::Observations,
            crate::protocol::Capability::Context,
        ]);
        subscription
    }
    fn inputs(registry: &Registry) -> Vec<&crate::input_events::Event> {
        registry
            .reports
            .iter()
            .filter_map(|(report, _)| {
                if let Report::Input { input, .. } = report {
                    Some(input.as_ref())
                } else {
                    None
                }
            })
            .collect()
    }
    #[test]
    fn normalized_input_is_explicit_scoped_and_excludes_pre_subscription_backlog() {
        let mut config = config();
        config.policy.allow_input_events = true;
        let mut registry = registry(config);
        update(
            &mut registry,
            1,
            vec![input_subscription("seat0", 100)],
            100,
        );
        assert_eq!(inputs(&registry).len(), 1);
        assert!(matches!(
            inputs(&registry)[0],
            crate::input_events::Event::Reset {
                reason: crate::input_events::ResetReason::Attached,
                ..
            }
        ));
        registry.reports.clear();
        registry.bytes = 0;
        key(&mut registry, source(1, "seat0"), 50, 1, true, 101);
        key(&mut registry, source(1, "seat0"), 60, 2, false, 102);
        assert!(
            inputs(&registry).is_empty(),
            "old typing must not leak through reconciliation snapshots"
        );
        key(&mut registry, source(2, "seat1"), 103, 1, true, 103);
        assert!(inputs(&registry).is_empty());
        key(&mut registry, source(1, "seat0"), 104, 3, true, 104);
        assert!(
            matches!(inputs(&registry)[0],crate::input_events::Event::Frame {sequence:3,changes,..} if changes==&vec![crate::input::Change::Key {code:30,state:crate::input::KeyState::Press}])
        );
        let mut denied = input_subscription("seat0", 105);
        denied.context.as_mut().unwrap().snapshot.revision = 2;
        denied
            .capabilities
            .remove(&crate::protocol::Capability::InputEvents);
        update(&mut registry, 2, vec![denied], 105);
        assert!(
            inputs(&registry).is_empty(),
            "revocation must purge pending input"
        );
        key(&mut registry, source(1, "seat0"), 106, 4, false, 106);
        assert!(inputs(&registry).is_empty());
    }
    #[test]
    fn normalized_input_policy_capabilities_context_and_allowlists_fail_closed() {
        let mut config = config();
        let subscription = input_subscription("seat0", 100);
        assert!(!permits_input(&config, &subscription, ms(100)));
        config.policy.allow_input_events = true;
        assert!(permits_input(&config, &subscription, ms(100)));
        config.mode = crate::config::Mode::Route;
        assert!(!permits_input(&config, &subscription, ms(100)));
        config.mode = crate::config::Mode::Observe;
        for capability in &subscription.capabilities {
            let mut copy = subscription.clone();
            copy.capabilities.remove(capability);
            assert!(!permits_input(&config, &copy, ms(100)));
        }
        for state in [
            crate::context::State::Locked,
            crate::context::State::Locking,
            crate::context::State::Greeter,
            crate::context::State::Console,
            crate::context::State::Unknown,
        ] {
            let mut copy = subscription.clone();
            copy.context.as_mut().unwrap().snapshot.state = state;
            assert!(!permits_input(&config, &copy, ms(100)));
        }
        let mut copy = subscription.clone();
        copy.context.as_mut().unwrap().snapshot.secure_overlay = true;
        assert!(!permits_input(&config, &copy, ms(100)));
        assert!(!permits_input(&config, &subscription, ms(99)));
        assert!(!permits_input(&config, &subscription, ms(351)));
        config.policy.allowed_devices.clear();
        config.rules.clear();
        let mut registry = registry(config);
        update(&mut registry, 1, vec![subscription], 100);
        key(&mut registry, source(1, "seat0"), 101, 1, true, 101);
        assert!(inputs(&registry).is_empty());
    }
    #[test]
    fn normalized_input_source_removal_and_reconciliation_reset_state_without_invented_presses() {
        let mut config = config();
        config.policy.allow_input_events = true;
        let mut registry = registry(config);
        update(
            &mut registry,
            1,
            vec![input_subscription("seat0", 100)],
            100,
        );
        registry.reports.clear();
        registry.bytes = 0;
        registry
            .push(
                Record::Reconcile {
                    source: source(1, "seat0"),
                    at: ms(101),
                    keys: BTreeSet::from([30]),
                    contacts: vec![],
                },
                ms(101),
            )
            .unwrap();
        assert!(
            matches!(inputs(&registry)[0],crate::input_events::Event::Reset {reason:crate::input_events::ResetReason::Reconciled,keys,..} if keys.contains(&30))
        );
        registry
            .push(
                Record::Remove {
                    source: source(1, "seat0"),
                    at: ms(102),
                },
                ms(102),
            )
            .unwrap();
        assert!(
            matches!(inputs(&registry)[1],crate::input_events::Event::Reset {reason:crate::input_events::ResetReason::Removed,keys,contacts,..} if keys.is_empty()&&contacts.is_empty())
        );
    }
    #[test]
    fn normalized_input_late_opt_in_discards_old_frames_and_expiry_discards_pending_input() {
        let mut config = config();
        config.policy.allow_input_events = true;
        let mut registry = registry(config);
        update(
            &mut registry,
            1,
            vec![with_context(subscription("seat0", 1), 1, 100)],
            100,
        );
        let mut active = input_subscription("seat0", 150);
        active.context.as_mut().unwrap().snapshot.revision = 2;
        update(&mut registry, 2, vec![active], 150);
        registry.reports.clear();
        registry.bytes = 0;
        key(&mut registry, source(1, "seat0"), 125, 1, true, 151);
        key(&mut registry, source(1, "seat0"), 130, 2, false, 152);
        assert!(inputs(&registry).is_empty());
        key(&mut registry, source(1, "seat0"), 153, 3, true, 153);
        assert_eq!(inputs(&registry).len(), 1);
        tick(&mut registry, 401);
        assert!(inputs(&registry).is_empty());
        assert!(
            registry
                .reports
                .iter()
                .any(|(report, _)| matches!(report, Report::ContextLost { .. }))
        );
    }
    #[test]
    fn normalized_input_overflow_ends_stream_without_blocking_the_reader() {
        let mut config = config();
        config.policy.allow_input_events = true;
        let mut registry = registry(config);
        update(
            &mut registry,
            1,
            vec![input_subscription("seat0", 100)],
            100,
        );
        for sequence in 1..=u64::try_from(MAX_QUEUED_PACKETS + 4).unwrap() {
            key(
                &mut registry,
                source(1, "seat0"),
                101,
                sequence,
                sequence % 2 == 1,
                101,
            );
        }
        assert!(inputs(&registry).is_empty());
        assert!(registry.observers.is_empty());
        assert!(registry.reports.iter().any(
            |(report, _)| matches!(report,Report::Ended {reason,..} if reason.contains("overflow"))
        ));
        assert!(registry.bytes <= MAX_QUEUED_BYTES);
    }
    #[test]
    fn normalized_input_virtual_devices_and_unmatched_selectors_are_excluded() {
        let mut config = config();
        config.policy.allow_input_events = true;
        let mut device = Device {
            source: source(1, "seat0"),
            identity: "test".into(),
            name: "test".into(),
            path: String::new(),
            physical: String::new(),
            unique: String::new(),
            vendor: 1,
            product: 1,
            properties: vec![],
            axes: BTreeMap::new(),
            capabilities: BTreeMap::from([(EV_KEY, vec![30])]),
            virtual_device: false,
        };
        assert_eq!(
            crate::input_events::Event::selected(&config, &device),
            vec!["keyboard"]
        );
        device.virtual_device = true;
        assert!(crate::input_events::Event::selected(&config, &device).is_empty());
        device.virtual_device = false;
        device.capabilities.clear();
        assert!(crate::input_events::Event::selected(&config, &device).is_empty());
    }
    #[test]
    fn source_invalidation_is_reported_without_an_active_candidate_or_other_seat_leak() {
        let mut registry = registry(config());
        update(
            &mut registry,
            1,
            vec![subscription("seat0", 7), subscription("seat1", 8)],
            0,
        );
        registry.reports.clear();
        registry.bytes = 0;
        registry
            .push(
                Record::Remove {
                    source: source(1, "seat0"),
                    at: ms(10),
                },
                ms(10),
            )
            .unwrap();
        let invalidated = registry
            .reports
            .iter()
            .filter_map(|(report, _)| match report {
                Report::SourceInvalidated {
                    subscription,
                    source,
                } => Some((subscription, source)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(invalidated.len(), 1);
        assert_eq!(invalidated[0].0.seat, "seat0");
        assert_eq!(invalidated[0].0.fence.session, 7);
        assert_eq!(*invalidated[0].1, source(1, "seat0"));
    }
    #[test]
    fn private_rule_command_returns_correlated_receipts_and_retains_last_good_revision() {
        use crate::rule_transactions::{Change, Transaction};
        let (mut parent, child) = Channel::pair().unwrap();
        let mut stream = Stream::new(config(), child).unwrap();
        stream.flush().unwrap();
        assert!(matches!(
            parent.receive::<Message>().unwrap().unwrap().message,
            Message::Configuration {
                generation: 1,
                error: None
            }
        ));
        let owner = subscription("seat0", 1);
        assert!(
            parent
                .send(
                    &Command::Subscriptions {
                        update: Subscriptions {
                            revision: 1,
                            seats: vec![owner.clone()]
                        }
                    },
                    crate::ipc::monotonic().unwrap()
                )
                .unwrap()
        );
        stream.service().unwrap();
        stream.flush().unwrap();
        while parent.receive::<Message>().unwrap().is_some() {}
        let id = stream.config().rules[0].id.clone();
        for (request, revision, enabled, expected, error) in [
            (1, 1, false, 2, false),
            (2, 1, true, 2, true),
            (3, 2, true, 3, false),
        ] {
            let transaction = Transaction {
                revision,
                changes: vec![Change::Enabled {
                    id: id.clone(),
                    enabled,
                }],
            };
            assert!(
                parent
                    .send(
                        &Command::Rules {
                            seat: "seat0".into(),
                            fence: owner.fence,
                            id: request,
                            transaction
                        },
                        crate::ipc::monotonic().unwrap()
                    )
                    .unwrap()
            );
            stream.service().unwrap();
            stream.flush().unwrap();
            let response = parent.receive::<Message>().unwrap().unwrap().message;
            assert!(
                matches!(response,Message::RulesChanged {seat,fence,id,revision,error:reason}
                if seat=="seat0" && fence==owner.fence && id==request && revision==expected
                    && reason.is_some()==error)
            );
        }
        assert_eq!(stream.registry.observers["seat0"].engine.rule_revision(), 3);
        assert!(stream.registry.observers["seat0"].engine.config().rules[0].enabled);
        let transaction = Transaction {
            revision: 3,
            changes: vec![Change::Enabled { id, enabled: false }],
        };
        assert!(
            parent
                .send(
                    &Command::Rules {
                        seat: "seat0".into(),
                        fence: owner.fence,
                        id: 4,
                        transaction
                    },
                    crate::ipc::monotonic().unwrap()
                )
                .unwrap()
        );
        stream.service().unwrap();
        assert_eq!(stream.control.len(), 1);
        let mut replacement =
            crate::configuration::Transfer::new(toml::to_string(&config()).unwrap(), 2).unwrap();
        while !replacement
            .send(&mut parent, crate::ipc::monotonic().unwrap())
            .unwrap()
        {
            stream.service().unwrap();
        }
        stream.service().unwrap();
        assert_eq!(stream.control.len(), 2);
        stream.flush().unwrap();
        assert!(matches!(
            parent.receive::<Message>().unwrap().unwrap().message,
            Message::RulesChanged {
                id: 4,
                revision: 4,
                error: None,
                ..
            }
        ));
        stream.flush().unwrap();
        assert!(matches!(
            parent.receive::<Message>().unwrap().unwrap().message,
            Message::Configuration {
                generation: 2,
                error: None
            }
        ));
    }

    #[test]
    fn only_subscribed_seat_receives_fenced_outcomes() {
        let mut registry = registry(config());
        update(&mut registry, 1, vec![subscription("seat0", 7)], 0);
        key(&mut registry, source(2, "seat1"), 10, 1, true, 10);
        key(&mut registry, source(1, "seat0"), 20, 1, true, 20);
        tick(&mut registry, 620);
        assert_eq!(outcomes(&registry).len(), 1);
        assert!(matches!(outcomes(&registry)[0], Event::Matched {
            source, config_generation: 1, session_generation: 7, at, evidence: Evidence::Hold { repeat: false }, ..
        } if source.seat == "seat0" && *at == ms(620)));
        let bytes = protocol::encode(&protocol::Packet {
            version: 1,
            sequence: 1,
            at: ms(620),
            message: &registry.reports.back().unwrap().0,
        })
        .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(json["message"].get("frame").is_none());
        assert_eq!(json["message"]["event"]["session_generation"], 7);
    }
    #[test]
    fn runtime_rule_transaction_is_atomic_fenced_and_isolated_to_its_observer() {
        use crate::rule_transactions::{Change, Transaction};
        let mut registry = registry(config());
        let left = subscription("seat0", 1);
        let right = subscription("seat1", 2);
        update(&mut registry, 1, vec![left.clone(), right.clone()], 0);
        registry.reports.clear();
        registry.bytes = 0;
        key(&mut registry, source(1, "seat0"), 0, 1, true, 0);
        key(&mut registry, source(2, "seat1"), 0, 1, true, 0);
        tick(&mut registry, 600);
        let rule = registry.config.rules[0].id.clone();
        let transaction = Transaction {
            revision: 1,
            changes: vec![Change::Enabled {
                id: rule,
                enabled: false,
            }],
        };
        let wrong = Fence {
            session: 9,
            ..left.fence
        };
        assert!(
            registry
                .transact_rules("seat0", wrong, &transaction, ms(620))
                .is_err()
        );
        assert!(
            registry
                .transact_rules("missing", left.fence, &transaction, ms(620))
                .is_err()
        );
        let before = registry.reports.len();
        let invalid = Transaction {
            revision: 1,
            changes: vec![
                transaction.changes[0].clone(),
                Change::Remove {
                    id: "missing".into(),
                },
            ],
        };
        assert!(
            registry
                .transact_rules("seat0", left.fence, &invalid, ms(620))
                .is_err()
        );
        assert_eq!(registry.reports.len(), before);
        assert_eq!(
            registry
                .transact_rules("seat0", left.fence, &transaction, ms(650))
                .unwrap(),
            2
        );
        assert!(!registry.observers["seat0"].engine.config().rules[0].enabled);
        assert!(registry.observers["seat1"].engine.config().rules[0].enabled);
        assert!(registry.config.rules[0].enabled);
        assert_eq!(registry.observers["seat0"].engine.generation(), 1);
        assert_eq!(registry.observers["seat1"].engine.rule_revision(), 1);
        assert!(!registry.reports.iter().any(|(report, _)| matches!(report,
            Report::Observation {subscription,event} if subscription.seat=="seat0"
                && matches!(event.as_ref(),Event::Matched {..}))));
        assert!(registry.reports.iter().any(|(report, _)| matches!(report,
            Report::Observation {subscription,event} if subscription.seat=="seat1"
                && matches!(event.as_ref(),Event::Matched {..}))));
        update(&mut registry, 2, vec![left.clone(), right], 700);
        assert_eq!(registry.observers["seat0"].engine.rule_revision(), 2);
        assert!(!registry.observers["seat0"].engine.config().rules[0].enabled);
        assert!(
            registry
                .transact_rules("seat0", left.fence, &transaction, ms(710))
                .is_err()
        );
    }
    #[test]
    fn renewal_preserves_candidates_and_replacement_discards_held_input_and_unsent_work() {
        let mut registry = registry(config());
        update(&mut registry, 1, vec![subscription("seat0", 1)], 0);
        key(&mut registry, source(1, "seat0"), 10, 1, true, 10);
        update(&mut registry, 2, vec![subscription("seat0", 1)], 400);
        tick(&mut registry, 610);
        assert_eq!(outcomes(&registry).len(), 1);
        update(&mut registry, 3, vec![subscription("seat0", 3)], 620);
        assert!(outcomes(&registry).is_empty());
        tick(&mut registry, 1300);
        assert!(outcomes(&registry).is_empty());
        key(&mut registry, source(1, "seat0"), 1310, 2, false, 1310);
        key(&mut registry, source(1, "seat0"), 1320, 3, true, 1320);
        update(&mut registry, 4, vec![subscription("seat0", 3)], 1500);
        tick(&mut registry, 1920);
        assert!(
            matches!(outcomes(&registry)[0], Event::Matched { session_generation: 3, at, .. } if *at == ms(1920))
        );
    }
    #[test]
    fn unread_input_older_than_activation_is_reconciled_without_a_new_press() {
        let mut registry = registry(config());
        update(&mut registry, 1, vec![subscription("seat0", 5)], 200);
        key(&mut registry, source(1, "seat0"), 150, 1, true, 220);
        tick(&mut registry, 800);
        assert!(outcomes(&registry).is_empty());
        key(&mut registry, source(1, "seat0"), 810, 2, false, 810);
        key(&mut registry, source(1, "seat0"), 820, 3, true, 820);
        update(&mut registry, 2, vec![subscription("seat0", 5)], 1000);
        tick(&mut registry, 1420);
        assert_eq!(outcomes(&registry).len(), 1);
    }
    #[test]
    fn expired_or_removed_subscribers_cannot_receive_or_resume_old_work() {
        let mut registry = registry(config());
        update(&mut registry, 1, vec![subscription("seat0", 2)], 0);
        key(&mut registry, source(1, "seat0"), 500, 1, true, 500);
        tick(&mut registry, 1100);
        assert!(outcomes(&registry).is_empty());
        assert!(matches!(
            &registry.reports.front().unwrap().0,
            Report::Ended { .. }
        ));
        update(&mut registry, 2, vec![subscription("seat0", 2)], 1200);
        assert!(registry.observers.is_empty());
        update(&mut registry, 3, vec![], 1200);
        update(&mut registry, 4, vec![subscription("seat0", 3)], 1200);
        tick(&mut registry, 1800);
        assert!(outcomes(&registry).is_empty());
        update(&mut registry, 5, vec![], 1810);
        assert!(registry.reports.is_empty());
    }
    #[test]
    fn session_membership_does_not_establish_desktop_authority() {
        let mut config = config();
        config.rules[0].contexts = vec![Context::Desktop];
        let mut registry = registry(config);
        update(&mut registry, 1, vec![subscription("seat0", 1)], 0);
        key(&mut registry, source(1, "seat0"), 10, 1, true, 10);
        tick(&mut registry, 610);
        assert!(
            !outcomes(&registry)
                .iter()
                .any(|event| matches!(event, Event::Matched { .. }))
        );
        assert!(
            outcomes(&registry)
                .iter()
                .any(|event| matches!(event, Event::Skipped { .. }))
        );
    }
    #[test]
    fn verified_context_refresh_preserves_candidates_without_leaking_provider_payloads() {
        let mut config = config();
        config.rules[0].contexts = vec![Context::Desktop];
        config.rules[0].trigger = Trigger::ButtonHold {
            key: Key(30),
            after: Millis(200),
            repeat: None,
        };
        let mut registry = registry(config);
        update(
            &mut registry,
            1,
            vec![with_context(subscription("seat0", 7), 1, 0)],
            0,
        );
        key(&mut registry, source(1, "seat0"), 10, 1, true, 10);
        update(
            &mut registry,
            2,
            vec![with_context(subscription("seat0", 7), 2, 100)],
            100,
        );
        tick(&mut registry, 210);
        assert!(matches!(
            outcomes(&registry)[0],
            Event::Matched {
                session_generation: 7,
                ..
            }
        ));
        assert!(
            registry
                .reports
                .iter()
                .all(|(report, _)| report.subscription().context.is_none())
        );
        assert_eq!(registry.reports.len(), 2);
    }
    #[test]
    fn context_activation_loss_and_binding_replacement_need_a_new_fence() {
        let mut registry = registry(config());
        update(&mut registry, 1, vec![subscription("seat0", 1)], 0);
        assert!(
            registry
                .update(
                    Subscriptions {
                        revision: 2,
                        seats: vec![with_context(subscription("seat0", 1), 1, 10)]
                    },
                    ms(10)
                )
                .is_err()
        );
        assert_eq!(registry.revision, 1);
        update(
            &mut registry,
            2,
            vec![with_context(subscription("seat0", 2), 1, 10)],
            10,
        );
        assert!(
            registry
                .update(
                    Subscriptions {
                        revision: 3,
                        seats: vec![subscription("seat0", 2)]
                    },
                    ms(20)
                )
                .is_err()
        );
        let mut replacement = with_context(subscription("seat0", 2), 2, 20);
        replacement
            .context
            .as_mut()
            .unwrap()
            .binding
            .compositor_start += 1;
        assert!(
            registry
                .update(
                    Subscriptions {
                        revision: 3,
                        seats: vec![replacement]
                    },
                    ms(20)
                )
                .is_err()
        );
        assert_eq!(registry.revision, 2);
        assert_eq!(registry.observers["seat0"].subscription.fence.session, 2);
    }
    #[test]
    fn invalid_context_batch_is_rejected_before_any_registration_changes() {
        let mut registry = registry(config());
        update(
            &mut registry,
            1,
            vec![with_context(subscription("seat0", 7), 1, 0)],
            0,
        );
        let mut invalid = with_context(subscription("seat1", 8), 1, 10);
        invalid.context.as_mut().unwrap().snapshot.generation = 7;
        assert!(
            registry
                .update(
                    Subscriptions {
                        revision: 2,
                        seats: vec![with_context(subscription("seat0", 9), 1, 10), invalid,]
                    },
                    ms(10)
                )
                .is_err()
        );
        assert_eq!(registry.revision, 1);
        assert_eq!(registry.observers.len(), 1);
        assert_eq!(registry.observers["seat0"].subscription.fence.session, 7);
    }
    #[test]
    fn old_or_expired_context_cannot_renew_authority() {
        let mut registry = registry(config());
        update(
            &mut registry,
            1,
            vec![with_context(subscription("seat0", 7), 2, 0)],
            0,
        );
        for (revision, at) in [(1, 10), (2, 300)] {
            assert!(
                registry
                    .update(
                        Subscriptions {
                            revision: 2,
                            seats: vec![with_context(subscription("seat0", 7), revision, 0)]
                        },
                        ms(at)
                    )
                    .is_err()
            );
        }
        assert_eq!(registry.revision, 1);
        assert_eq!(registry.observers["seat0"].expires, ms(1000));
    }
    #[test]
    fn context_expiry_revokes_observation_before_an_idle_hold_and_keeps_reading() {
        let mut config = config();
        config.rules[0].contexts = vec![Context::Desktop];
        let mut registry = registry(config);
        update(
            &mut registry,
            1,
            vec![with_context(subscription("seat0", 7), 1, 0)],
            0,
        );
        key(&mut registry, source(1, "seat0"), 10, 1, true, 10);
        assert_eq!(registry.deadline(), Some(ms(251)));
        registry.expire(ms(251)).unwrap();
        assert!(registry.observers.is_empty());
        assert!(
            matches!(&registry.reports.front().unwrap().0, Report::ContextLost { subscription }
            if subscription.context.is_none() && subscription.fence.session == 7)
        );
        key(&mut registry, source(1, "seat0"), 300, 2, false, 300);
        update(
            &mut registry,
            2,
            vec![with_context(subscription("seat0", 8), 1, 300)],
            300,
        );
        key(&mut registry, source(1, "seat0"), 310, 3, true, 310);
        assert_eq!(registry.observers["seat0"].subscription.fence.session, 8);
    }
    #[test]
    fn stale_private_context_does_not_stop_the_input_worker() {
        let (mut broker, worker) = Channel::pair().unwrap();
        let mut stream = Stream::new(config(), worker).unwrap();
        let now = crate::ipc::monotonic().unwrap();
        let update = Subscriptions {
            revision: 1,
            seats: vec![with_context(subscription("seat0", 7), 1, 0)],
        };
        assert!(
            broker
                .send(&Command::Subscriptions { update }, now)
                .unwrap()
        );
        stream.service().unwrap();
        stream.flush().unwrap();
        assert!(matches!(
            broker.receive::<Message>().unwrap().unwrap().message,
            Message::Configuration { .. }
        ));
        assert!(matches!(
            broker.receive::<Message>().unwrap().unwrap().message,
            Message::Outcome {
                report: Report::ContextLost { .. }
            }
        ));
        let mut fresh = with_context(subscription("seat0", 8), 1, 0);
        fresh.context.as_mut().unwrap().snapshot.captured_at = crate::ipc::monotonic().unwrap();
        assert!(
            broker
                .send(
                    &Command::Subscriptions {
                        update: Subscriptions {
                            revision: 2,
                            seats: vec![fresh]
                        }
                    },
                    crate::ipc::monotonic().unwrap()
                )
                .unwrap()
        );
        stream.service().unwrap();
        assert_eq!(stream.registry.observers.len(), 1);
    }
    #[test]
    fn queue_overflow_revokes_observation_and_keeps_input_state_current() {
        let mut config = config();
        config.rules[0].trigger = Trigger::Key { key: Key(30) };
        let mut registry = registry(config);
        update(&mut registry, 1, vec![subscription("seat0", 1)], 0);
        for index in 0..140 {
            key(
                &mut registry,
                source(1, "seat0"),
                index * 2 + 1,
                index * 2 + 1,
                true,
                index * 2 + 1,
            );
            key(
                &mut registry,
                source(1, "seat0"),
                index * 2 + 2,
                index * 2 + 2,
                false,
                index * 2 + 2,
            );
        }
        assert!(registry.observers.is_empty());
        assert_eq!(registry.reports.len(), 1);
        assert!(
            matches!(&registry.reports[0].0, Report::Ended { reason, .. } if reason.contains("overflow"))
        );
        assert!(registry.bytes < MAX_QUEUED_BYTES);
        update(&mut registry, 2, vec![subscription("seat0", 1)], 300);
        assert!(registry.observers.is_empty());
        update(&mut registry, 3, vec![subscription("seat0", 2)], 310);
        key(&mut registry, source(1, "seat0"), 320, 281, true, 320);
        assert!(matches!(
            outcomes(&registry)[0],
            Event::Matched {
                session_generation: 2,
                ..
            }
        ));
    }
    #[test]
    fn invalid_subscription_does_not_replace_working_observation() {
        let mut registry = registry(config());
        update(&mut registry, 1, vec![subscription("seat0", 1)], 0);
        let invalid = Subscriptions {
            revision: 2,
            seats: vec![subscription("seat0", 2), subscription("seat0", 3)],
        };
        assert!(registry.update(invalid, ms(10)).is_err());
        assert_eq!(registry.observers["seat0"].subscription.fence.session, 1);
        let invalid = Subscriptions {
            revision: 1,
            seats: vec![],
        };
        assert!(registry.update(invalid, ms(10)).is_err());
        assert_eq!(registry.revision, 1);
    }
    #[test]
    fn reload_reconciles_held_input_and_reports_only_new_rules_and_generations() {
        let mut registry = registry(config());
        update(&mut registry, 1, vec![subscription("seat0", 7)], 0);
        key(&mut registry, source(1, "seat0"), 10, 1, true, 10);
        let mut next = config();
        next.rules[0].trigger = Trigger::ButtonHold {
            key: Key(30),
            after: Millis(900),
            repeat: None,
        };
        next.rules[0].action = crate::config::Action::Event {
            name: "reloaded".into(),
        };
        registry.install(next, 2, ms(500)).unwrap();
        let mut next_subscription = subscription("seat0", 7);
        next_subscription.fence.configuration = 2;
        update(&mut registry, 2, vec![subscription("seat0", 7)], 501);
        assert!(
            registry.observers.is_empty(),
            "an in-flight old subscription cannot reactivate old rules"
        );
        update(&mut registry, 3, vec![next_subscription.clone()], 510);
        tick(&mut registry, 1200);
        assert!(outcomes(&registry).is_empty());
        key(&mut registry, source(1, "seat0"), 1210, 2, false, 1210);
        key(&mut registry, source(1, "seat0"), 1220, 3, true, 1220);
        update(&mut registry, 4, vec![next_subscription], 1400);
        tick(&mut registry, 2120);
        assert!(matches!(outcomes(&registry)[0], Event::Matched {
            config_generation: 2, session_generation: 7, action: crate::config::Action::Event { name }, at, ..
        } if name == "reloaded" && *at == ms(2120)));
    }
    #[test]
    fn rejected_reload_preserves_last_good_generation_and_an_active_hold() {
        let mut registry = registry(config());
        update(&mut registry, 1, vec![subscription("seat0", 1)], 0);
        key(&mut registry, source(1, "seat0"), 10, 1, true, 10);
        let mut invalid = config();
        invalid.rules[0].trigger = Trigger::Shape {
            name: "missing-template".into(),
            fingers: None,
            button: None,
        };
        assert!(registry.install(invalid, 2, ms(500)).is_err());
        assert!(registry.install(config(), 3, ms(500)).is_err());
        assert_eq!(registry.baseline.generation(), 1);
        tick(&mut registry, 610);
        assert!(
            matches!(outcomes(&registry)[0], Event::Matched { config_generation: 1, at, .. } if *at == ms(610))
        );
    }
    #[test]
    fn initial_touch_is_suppressed_until_all_contacts_are_lifted() {
        use crate::input::{
            ABS_MT_POSITION_X, ABS_MT_POSITION_Y, ABS_MT_SLOT, ABS_MT_TRACKING_ID, Axis, Contact,
            ContactId, EV_ABS,
        };
        let mut config = config();
        config.devices.get_mut("keyboard").unwrap().keys.clear();
        config.devices.get_mut("keyboard").unwrap().absolute_axes =
            vec![ABS_MT_POSITION_X, ABS_MT_POSITION_Y];
        config.rules[0].trigger = Trigger::TouchHold {
            fingers: 1,
            after: Millis(600),
        };
        let mut registry = registry(config);
        let source = source(3, "seat0");
        let coordinate = Axis {
            minimum: 0,
            maximum: 1000,
            resolution: 10,
            fuzz: 0,
            flat: 0,
        };
        let device = Device {
            source: source.clone(),
            identity: "touch-fixture".into(),
            name: "fixture".into(),
            path: "fixture".into(),
            physical: "fixture".into(),
            unique: String::new(),
            vendor: 1,
            product: 1,
            properties: vec![],
            capabilities: BTreeMap::from([(
                EV_ABS,
                vec![
                    ABS_MT_SLOT,
                    ABS_MT_TRACKING_ID,
                    ABS_MT_POSITION_X,
                    ABS_MT_POSITION_Y,
                ],
            )]),
            axes: BTreeMap::from([
                (
                    ABS_MT_SLOT,
                    Axis {
                        minimum: 0,
                        maximum: 0,
                        resolution: 0,
                        fuzz: 0,
                        flat: 0,
                    },
                ),
                (
                    ABS_MT_TRACKING_ID,
                    Axis {
                        minimum: -1,
                        maximum: 65535,
                        resolution: 0,
                        fuzz: 0,
                        flat: 0,
                    },
                ),
                (ABS_MT_POSITION_X, coordinate),
                (ABS_MT_POSITION_Y, coordinate),
            ]),
            virtual_device: false,
        };
        registry
            .push(
                Record::Device {
                    device,
                    keys: BTreeSet::new(),
                    contacts: vec![Contact {
                        id: ContactId {
                            slot: 0,
                            tracking: 7,
                            generation: 1,
                        },
                        x: 500,
                        y: 500,
                        pressure: None,
                        fresh: false,
                    }],
                },
                ms(0),
            )
            .unwrap();
        update(&mut registry, 1, vec![subscription("seat0", 5)], 200);
        tick(&mut registry, 900);
        assert!(outcomes(&registry).is_empty());
        for (sequence, at, values) in [
            (1, 910, vec![(ABS_MT_SLOT, 0), (ABS_MT_TRACKING_ID, -1)]),
            (
                2,
                920,
                vec![
                    (ABS_MT_SLOT, 0),
                    (ABS_MT_TRACKING_ID, 8),
                    (ABS_MT_POSITION_X, 500),
                    (ABS_MT_POSITION_Y, 500),
                ],
            ),
        ] {
            let mut events: Vec<_> = values
                .into_iter()
                .map(|(code, value)| RawEvent {
                    kind: EV_ABS,
                    code,
                    value,
                })
                .collect();
            events.push(RawEvent {
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0,
            });
            registry
                .push(
                    Record::Frame {
                        frame: Frame {
                            source: source.clone(),
                            at: ms(at),
                            sequence,
                            events,
                        },
                    },
                    ms(at),
                )
                .unwrap();
        }
        update(&mut registry, 2, vec![subscription("seat0", 5)], 1000);
        tick(&mut registry, 1520);
        assert_eq!(outcomes(&registry).len(), 1);
        assert!(matches!(
            outcomes(&registry)[0],
            Event::Matched {
                session_generation: 5,
                evidence: Evidence::Touch {
                    gesture: crate::gestures::TouchGesture::Hold { fingers: 1, .. },
                    ..
                },
                ..
            }
        ));
    }
}
