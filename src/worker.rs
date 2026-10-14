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
    Ended {
        subscription: Subscription,
        reason: String,
    },
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Message {
    Configuration {
        generation: u64,
        error: Option<String>,
    },
    Outcome {
        report: Report,
    },
}
impl Report {
    pub fn subscription(&self) -> &Subscription {
        match self {
            Self::Ready { subscription }
            | Self::Observation { subscription, .. }
            | Self::Ended { subscription, .. } => subscription,
        }
    }
}

struct Observer {
    subscription: Subscription,
    started: Duration,
    expires: Duration,
    engine: Engine,
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
        }
        // Discard unsent outcomes as soon as their registration is replaced.
        let removed: Vec<_> = self
            .observers
            .values()
            .filter(|observer| !update.seats.contains(&observer.subscription))
            .map(|observer| observer.subscription.clone())
            .collect();
        for subscription in removed {
            self.remove(&subscription);
        }
        self.reports
            .retain(|(report, _)| update.seats.contains(report.subscription()));
        self.bytes = self.reports.iter().map(|(_, bytes)| bytes).sum();
        self.blocked.retain(|seat, fence| {
            update
                .seats
                .iter()
                .any(|subscription| &subscription.seat == seat && &subscription.fence == fence)
        });
        for subscription in update.seats {
            if self.blocked.get(&subscription.seat) == Some(&subscription.fence) {
                continue;
            }
            if let Some(observer) = self.observers.get_mut(&subscription.seat) {
                observer.expires = now + protocol::MAX_PACKET_AGE;
                continue;
            }
            let engine = self.baseline.fork_observer(
                self.config.clone(),
                &subscription.seat,
                subscription.fence.session,
                now,
            )?;
            self.observers.insert(
                subscription.seat.clone(),
                Observer {
                    subscription: subscription.clone(),
                    started: now,
                    expires: now + protocol::MAX_PACKET_AGE,
                    engine,
                },
            );
            self.queue(Report::Ready { subscription }, now)?;
        }
        self.revision = update.revision;
        Ok(())
    }
    fn remove(&mut self, subscription: &Subscription) {
        if self
            .observers
            .get(&subscription.seat)
            .is_some_and(|observer| &observer.subscription == subscription)
        {
            self.observers.remove(&subscription.seat);
        }
        self.reports
            .retain(|(report, _)| report.subscription() != subscription);
        self.bytes = self.reports.iter().map(|(_, bytes)| bytes).sum();
    }
    fn end(&mut self, subscription: Subscription, reason: &str, now: Duration) -> io::Result<()> {
        self.remove(&subscription);
        self.blocked
            .insert(subscription.seat.clone(), subscription.fence);
        self.queue(
            Report::Ended {
                subscription,
                reason: reason.into(),
            },
            now,
        )
    }
    fn expire(&mut self, now: Duration) -> io::Result<()> {
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
            Err(_) if matches!(report, Report::Observation { .. }) => {
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
                if old_subscription == subscription && event.supersedes_progress(old_event))
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
            Record::Mapping { source, .. }
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
            observer.engine.push(record)?;
            for event in observer.engine.take_events() {
                reports.push(Report::Observation {
                    subscription: observer.subscription.clone(),
                    event: Box::new(event),
                });
            }
        }
        for report in reports {
            if self
                .observers
                .get(&report.subscription().seat)
                .is_some_and(|observer| &observer.subscription == report.subscription())
            {
                self.queue(report, now)?;
            }
        }
        Ok(())
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
                Command::Subscriptions { update } => self.registry.update(update, now)?,
                command => {
                    if let Some(installation) = self.assembly.accept(command, now)? {
                        let result = installation.config.and_then(|config| {
                            self.registry.install(config, installation.generation, now)
                        });
                        if !self.control.is_empty() {
                            return Err(protocol::invalid(
                                "configuration acknowledgements are not drained",
                            ));
                        }
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
        }
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
