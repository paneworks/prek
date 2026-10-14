//! Configured recognition, arbitration and raw output ownership. Kernel I/O and
//! action execution remain separate: callers acknowledge writes and revalidate
//! dispatch fences in the session executor. Only verified, neutral transports
//! may be attached here.

use crate::{
    config::{Action, Config, Delivery, Trigger},
    context::Authority,
    input::{Device, Source},
    protocol::Capability,
    replay::Record,
    router::{Candidate, Fault, Limits, Ownership, Policy, Recovery, Router, Scheduled},
    rules::{Engine, Event, RouteIntent, RouteOutcome},
    transport::Ready,
};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    io,
    time::Duration,
};

const MAX_DISPATCHES: usize = 128;
struct Route {
    router: Router,
    authorization: u64,
    ticket: u64,
    first: Duration,
    choices: Vec<RouteIntent>,
    winner: Option<u64>,
    joined: bool,
}
struct Won {
    intent: RouteIntent,
    emitted: bool,
}
struct Dispatch {
    outcome: RouteOutcome,
    configuration: u64,
}

pub struct Routing {
    engine: Engine,
    routes: BTreeMap<Source, Route>,
    next_ticket: u64,
    capabilities: BTreeSet<Capability>,
    capability_generation: u64,
    capability_expires: Duration,
    won: BTreeMap<u64, Won>,
    dispatches: VecDeque<Dispatch>,
    events: Vec<Event>,
    recoveries: Vec<Recovery>,
    authority_generation: u64,
}
fn fault(error: Fault) -> io::Error {
    io::Error::other(format!("routing: {error:?}"))
}
fn capability(action: &Action) -> Capability {
    match action {
        Action::Event { .. } => Capability::Observations,
        Action::Morf { .. } => Capability::MorfActions,
        Action::Hyprland { .. } => Capability::HyprlandActions,
        Action::Command { .. } => Capability::Commands,
        Action::Keys { .. } | Action::Remap { .. } => Capability::SyntheticInput,
        Action::Power { .. } => Capability::PowerActions,
    }
}
impl Routing {
    pub(crate) fn configuration_generation(&mut self, generation: u64) -> io::Result<()> {
        if generation == 0 || !self.routes.is_empty() {
            return Err(io::Error::other(
                "routing generation requires a fresh coordinator",
            ));
        }
        self.engine.configuration_generation(generation);
        Ok(())
    }
    pub fn new(config: Config) -> io::Result<Self> {
        if config.mode != crate::config::Mode::Route {
            return Err(io::Error::other(
                "configured routing requires explicit route mode",
            ));
        }
        if config
            .rules
            .iter()
            .any(|rule| rule.enabled && rule.delivery == Delivery::Buffer)
            && config.timing.join_window.0 >= config.timing.decision_window.0
        {
            return Err(io::Error::other(
                "buffered routing requires join_window < decision_window",
            ));
        }
        let mut engine = Engine::new(config)?;
        engine.push(Record::Header { version: 1 })?;
        engine.enable_routing();
        Ok(Self {
            engine,
            routes: BTreeMap::new(),
            next_ticket: 0,
            capabilities: BTreeSet::new(),
            capability_generation: 0,
            capability_expires: Duration::ZERO,
            won: BTreeMap::new(),
            dispatches: VecDeque::new(),
            events: Vec::new(),
            recoveries: Vec::new(),
            authority_generation: 0,
        })
    }
    /// Registration/provider verification belongs to the trusted caller.
    pub fn authority_mut(&mut self) -> &mut Authority {
        self.engine.authority_mut()
    }
    pub(crate) fn authority(&self) -> &Authority {
        self.engine.authority()
    }
    /// Availability alone creates no authority. Its generation must match the
    /// active authority and its bounded lease is checked at claim and dequeue.
    pub fn actions_available(
        &mut self,
        generation: u64,
        capabilities: BTreeSet<Capability>,
        at: Duration,
    ) {
        self.capability_generation = generation;
        self.capabilities = capabilities;
        self.capability_expires = at + crate::context::PROVIDER_LEASE;
    }
    pub fn attach(
        &mut self,
        device: Device,
        ready: &Ready,
        mapping: Option<crate::gestures::Mapping>,
        at: Duration,
    ) -> io::Result<()> {
        if self
            .routes
            .keys()
            .any(|source| source.device == device.source.device)
        {
            return Err(io::Error::other(
                "detach the old route before replacing a source",
            ));
        }
        if !self
            .engine
            .config()
            .policy
            .allowed_seats
            .contains(&device.source.seat)
            || !self.engine.config().devices.iter().any(|(id, selector)| {
                self.engine.config().policy.allowed_devices.contains(id)
                    && selector.matches(&device)
            })
        {
            return Err(io::Error::other(
                "route source is outside the configured allowlist",
            ));
        }
        self.next_ticket = self
            .next_ticket
            .checked_add(1)
            .ok_or_else(|| fault(Fault::Overflow))?;
        let authorization = self.next_ticket;
        let limits = Limits {
            offset: self.engine.config().timing.decision_window.duration(),
            ..Limits::default()
        };
        let router =
            Router::new(device.clone(), ready, authorization, limits, at).map_err(fault)?;
        self.engine.push(Record::Device {
            device: device.clone(),
            keys: BTreeSet::new(),
            contacts: Vec::new(),
        })?;
        if let Some(mapping) = mapping {
            self.engine.push(Record::Mapping {
                source: device.source.clone(),
                at,
                mapping,
            })?;
        }
        self.routes.insert(
            device.source,
            Route {
                router,
                authorization,
                ticket: 0,
                first: at,
                choices: Vec::new(),
                winner: None,
                joined: false,
            },
        );
        self.authority_generation = self.engine.authority_mut().generation();
        Ok(())
    }
    fn available(&self, intent: &RouteIntent, at: Duration) -> bool {
        self.capability_generation == self.authority_generation
            && at < self.capability_expires
            && self.capabilities.contains(&capability(&intent.rule.action))
            && self.engine.validate_route(intent, at).is_ok()
    }
    fn viable(&self, intent: &RouteIntent, at: Duration, first: Duration, finished: bool) -> bool {
        let Some(fingers) = intent.rule.trigger.fingers() else {
            return true;
        };
        let join = self.engine.config().timing.join_window.duration();
        if intent.joined_at() > intent.started() + join || intent.contacts() > usize::from(fingers)
        {
            return false;
        }
        intent.contacts() == usize::from(fingers) || (!finished && at < first + join)
    }
    pub fn push(&mut self, record: Record) -> io::Result<()> {
        let at = match &record {
            Record::Frame { frame } => frame.at,
            Record::Tick { at }
            | Record::Mapping { at, .. }
            | Record::Reconcile { at, .. }
            | Record::Remove { at, .. } => *at,
            _ => {
                return Err(io::Error::other(
                    "routing accepts frames, ticks and lifecycle records",
                ));
            }
        };
        let source = match &record {
            Record::Frame { frame } => Some(frame.source.clone()),
            _ => None,
        };
        let frame = match &record {
            Record::Frame { frame } => Some(frame.clone()),
            _ => None,
        };
        self.engine.push(record.clone())?;
        if self.engine.authority_mut().generation() != self.authority_generation {
            let sources: Vec<_> = self.routes.keys().cloned().collect();
            for source in sources {
                self.recover(&source);
            }
            self.authority_generation = self.engine.authority_mut().generation();
        }
        if let Record::Mapping { source, .. }
        | Record::Reconcile { source, .. }
        | Record::Remove { source, .. } = &record
        {
            self.recover(source);
        }
        let outcomes = self.engine.take_route_outcomes();
        let claims = self.engine.take_route_claims();
        let active = self.engine.route_intents();
        let mut intents = active.clone();
        intents.extend(outcomes.iter().map(|outcome| outcome.intent.clone()));
        if let (Some(source), Some(frame)) = (source, frame) {
            let mut choices: Vec<_> = intents
                .into_iter()
                .filter(|intent| {
                    intent.sources().contains(&source)
                        && intent.rule.delivery != Delivery::Observe
                        && self.available(intent, at)
                })
                .collect();
            choices.sort_by(|a, b| {
                b.rule
                    .priority
                    .cmp(&a.rule.priority)
                    .then_with(|| a.rule.id.cmp(&b.rule.id))
            });
            if let Some(route) = self.routes.get_mut(&source) {
                let starts = route.router.physical_neutral()
                    && frame.events.iter().any(|event| {
                        event.kind == crate::input::EV_KEY && event.value == 1
                            || event.kind == crate::input::EV_ABS
                                && event.code == crate::input::ABS_MT_TRACKING_ID
                                && event.value >= 0
                            || event.kind == crate::input::EV_REL && event.value != 0
                    });
                let mut candidate = None;
                if starts {
                    self.next_ticket = self
                        .next_ticket
                        .checked_add(1)
                        .ok_or_else(|| fault(Fault::Overflow))?;
                    route.ticket = self.next_ticket;
                    route.first = at;
                    route.winner = None;
                    route.joined = false;
                    route.choices = choices.clone();
                    if let Some(intent) = choices.first() {
                        let policy = match intent.rule.delivery {
                            Delivery::Reserve => Policy::Reserve,
                            _ => Policy::Buffer {
                                join: if intent.rule.trigger.contains_touch() {
                                    self.engine.config().timing.join_window.duration()
                                } else {
                                    Duration::ZERO
                                },
                                decision: self.engine.config().timing.decision_window.duration(),
                            },
                        };
                        candidate = Some(Candidate {
                            id: route.ticket,
                            authorization: route.authorization,
                            policy,
                        });
                    }
                } else if route.router.ownership() == Some(Ownership::Pending) {
                    route.choices = choices;
                }
                if let Err(error) = route.router.push(frame, candidate, at) {
                    self.recover(&source);
                    return Err(fault(error));
                }
                if starts
                    && candidate.is_some_and(|candidate| candidate.policy == Policy::Reserve)
                    && let Some(intent) = route.choices.first()
                {
                    route.winner = Some(intent.id());
                    self.won.insert(
                        intent.id(),
                        Won {
                            intent: intent.clone(),
                            emitted: false,
                        },
                    );
                }
            }
        }
        self.resolve(at, &active, &claims, &outcomes)?;
        for outcome in outcomes {
            let id = outcome.intent.id();
            let observe = outcome.intent.rule.delivery == Delivery::Observe;
            if self.available(&outcome.intent, at) && (observe || self.won.contains_key(&id)) {
                if self.dispatches.len() >= MAX_DISPATCHES {
                    let sources: Vec<_> = outcome.intent.sources().iter().cloned().collect();
                    for source in sources {
                        self.recover(&source);
                    }
                    return Err(fault(Fault::Overflow));
                }
                if let Some(won) = self.won.get_mut(&id) {
                    won.emitted = true;
                }
                self.dispatches.push_back(Dispatch {
                    outcome,
                    configuration: self.engine.generation(),
                });
            } else {
                self.events.push(Event::Skipped {
                    version: 1,
                    rule: outcome.intent.rule.id.clone(),
                    source: outcome.intent.source().clone(),
                    at,
                    reason: "routing ownership or action consumer unavailable".into(),
                });
            }
        }
        for event in self.engine.take_events() {
            if !matches!(event, Event::Matched { .. }) {
                self.events.push(event);
            }
        }
        let dead: Vec<_> = self
            .won
            .iter()
            .filter(|(id, _)| !active.iter().any(|intent| intent.id() == **id))
            .map(|(id, _)| *id)
            .collect();
        for id in dead {
            self.won.remove(&id);
        }
        if self.events.len() > 16384 {
            let sources: Vec<_> = self.routes.keys().cloned().collect();
            for source in sources {
                self.recover(&source);
            }
            self.events.clear();
            return Err(fault(Fault::Overflow));
        }
        Ok(())
    }
    fn resolve(
        &mut self,
        at: Duration,
        active: &[RouteIntent],
        claims: &[RouteIntent],
        outcomes: &[RouteOutcome],
    ) -> io::Result<()> {
        let sources: Vec<_> = self.routes.keys().cloned().collect();
        for source in sources {
            if let Err(error) = self.routes.get_mut(&source).unwrap().router.advance(at) {
                self.recover(&source);
                return Err(fault(error));
            }
            let mut choices: Vec<_> = active
                .iter()
                .cloned()
                .chain(outcomes.iter().map(|outcome| outcome.intent.clone()))
                .filter(|intent| {
                    intent.sources().contains(&source) && intent.rule.delivery != Delivery::Observe
                })
                .collect();
            choices.sort_by(|a, b| {
                b.rule
                    .priority
                    .cmp(&a.rule.priority)
                    .then_with(|| a.rule.id.cmp(&b.rule.id))
            });
            self.routes.get_mut(&source).unwrap().choices = choices;
            if self.routes[&source].router.ownership() == Some(Ownership::Pending)
                && self.routes[&source]
                    .choices
                    .iter()
                    .any(|intent| self.engine.validate_route(intent, at).is_err())
            {
                self.recover(&source);
                continue;
            }
            if at >= self.routes[&source].first + self.engine.config().timing.join_window.duration()
            {
                self.routes.get_mut(&source).unwrap().joined = true;
            }
            let route = &self.routes[&source];
            if let Some(id) = route.winner {
                if let Some(won) = self.won.get(&id)
                    && (!self.available(&won.intent, at)
                        || !active.iter().any(|intent| intent.id() == id)
                            && !outcomes.iter().any(|outcome| outcome.intent.id() == id))
                {
                    let route = self.routes.get_mut(&source).unwrap();
                    if matches!(route.router.ownership(), Some(Ownership::Claimed)) {
                        route
                            .router
                            .cancel(route.ticket, route.authorization, at)
                            .map_err(fault)?;
                    }
                    self.dispatches
                        .retain(|dispatch| dispatch.outcome.intent.id() != id);
                }
                continue;
            }
            if route.router.ownership() != Some(Ownership::Pending) {
                continue;
            }
            let finished = route.router.physical_neutral();
            let first = route.first;
            let choice = route
                .choices
                .iter()
                .find(|intent| {
                    self.available(intent, at)
                        && self.viable(intent, at, first, finished)
                        && (!claims.iter().any(|claim| claim.sources().contains(&source))
                            || !matches!(
                                intent.rule.trigger,
                                Trigger::Swipe { .. } | Trigger::TouchTaps { .. }
                            )
                            || claims.iter().any(|claim| claim.id() == intent.id()))
                })
                .cloned();
            let matched = choice.as_ref().is_some_and(|intent| {
                outcomes
                    .iter()
                    .any(|outcome| outcome.intent.id() == intent.id())
            });
            let can_claim = choice.as_ref().is_some_and(|intent| matched || claims.iter().any(|claim| claim.id() == intent.id())
                || matches!(intent.rule.trigger, Trigger::TouchTaps { fingers, .. } if fingers >= 2 && intent.contacts() == usize::from(fingers)));
            if can_claim && let Some(intent) = choice {
                if !finished
                    && at < first + self.engine.config().timing.join_window.duration()
                    && intent.rule.trigger.contains_touch()
                {
                    continue;
                }
                if intent.sources().iter().all(|source| {
                    self.routes.get(source).is_some_and(|route| {
                        route.router.ownership() == Some(Ownership::Pending)
                            && route
                                .router
                                .claimable(route.ticket, route.authorization, at)
                                .is_ok()
                            || route.router.ownership() == Some(Ownership::Claimed)
                                && route.winner == Some(intent.id())
                    })
                }) {
                    for source in intent.sources() {
                        let route = self.routes.get_mut(source).unwrap();
                        if route.router.ownership() == Some(Ownership::Pending) {
                            let result = if route.router.physical_neutral() {
                                route
                                    .router
                                    .claim_finished(route.ticket, route.authorization, at)
                            } else {
                                route.router.claim(route.ticket, route.authorization, at)
                            };
                            if let Err(error) = result {
                                let sources: Vec<_> = intent.sources().iter().cloned().collect();
                                for source in sources {
                                    self.recover(&source);
                                }
                                return Err(fault(error));
                            }
                        }
                        route.winner = Some(intent.id());
                    }
                    self.won.insert(
                        intent.id(),
                        Won {
                            intent,
                            emitted: false,
                        },
                    );
                }
            } else if choice.is_none()
                || finished
                    && !route.choices.iter().any(|intent| {
                        matches!(
                            intent.rule.trigger,
                            Trigger::ButtonTaps { .. }
                                | Trigger::TouchTaps { .. }
                                | Trigger::Sequence { .. }
                        )
                    })
            {
                let route = self.routes.get_mut(&source).unwrap();
                route
                    .router
                    .cancel(route.ticket, route.authorization, at)
                    .map_err(fault)?;
            }
        }
        Ok(())
    }
    pub fn due(&mut self, source: &Source, at: Duration) -> io::Result<Option<&Scheduled>> {
        let advance = self
            .routes
            .get_mut(source)
            .ok_or_else(|| fault(Fault::StaleSource))?
            .router
            .advance(at);
        if let Err(error) = advance {
            self.recover(source);
            return Err(fault(error));
        }
        self.routes
            .get_mut(source)
            .ok_or_else(|| fault(Fault::StaleSource))?
            .router
            .due(at)
            .map_err(fault)
    }
    pub fn delivered(&mut self, source: &Source, ticket: u64, at: Duration) -> io::Result<()> {
        let result = self
            .routes
            .get_mut(source)
            .ok_or_else(|| fault(Fault::StaleSource))?
            .router
            .delivered(ticket, at)
            .map_err(fault);
        if result.is_err() {
            self.recover(source);
        }
        result
    }
    pub fn delivery_offset(&self, source: &Source) -> Option<Duration> {
        self.routes
            .get(source)
            .map(|route| route.router.delivery_offset())
    }
    pub fn take_dispatches(&mut self, at: Duration) -> Vec<Event> {
        let mut result = Vec::new();
        while let Some(dispatch) = self.dispatches.pop_front() {
            if dispatch.configuration == self.engine.generation()
                && self.available(&dispatch.outcome.intent, at)
            {
                result.push(dispatch.outcome.event);
            }
        }
        result
    }
    pub fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }
    pub fn reload(&mut self, config: Config, at: Duration) -> io::Result<()> {
        // Compile/validate first. An invalid replacement preserves both the
        // old model and every transport's pending/claimed ownership.
        Self::new(config.clone())?;
        self.engine.reload(config, at)?;
        let sources: Vec<_> = self.routes.keys().cloned().collect();
        for source in sources {
            self.recover(&source);
        }
        self.events.extend(self.engine.take_events());
        Ok(())
    }
    pub fn take_recoveries(&mut self) -> Vec<Recovery> {
        std::mem::take(&mut self.recoveries)
    }
    pub fn recover(&mut self, source: &Source) {
        if let Some(mut route) = self.routes.remove(source)
            && let Some(recovery) = route.router.recover()
        {
            self.recoveries.push(recovery);
        }
        self.won
            .retain(|_, won| !won.intent.sources().contains(source));
        self.dispatches
            .retain(|dispatch| !dispatch.outcome.intent.sources().contains(source));
    }
    pub fn deadline(&self) -> Option<Duration> {
        self.engine
            .deadline()
            .into_iter()
            .chain(
                (!self.won.is_empty() || !self.dispatches.is_empty())
                    .then_some(self.capability_expires),
            )
            .chain(self.routes.values().flat_map(|route| {
                route.router.next_deadline().into_iter().chain(
                    (route.router.ownership() == Some(Ownership::Pending) && !route.joined)
                        .then_some(
                            route.first + self.engine.config().timing.join_window.duration(),
                        ),
                )
            }))
            .min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{Context, Direction, Distance, Screen, Selector},
        context::{Binding, OutputState, SessionKind, Snapshot, State},
        gestures::{Mapping, Output, Point, Transform},
        input::{
            ABS_MT_POSITION_X, ABS_MT_POSITION_Y, ABS_MT_SLOT, ABS_MT_TRACKING_ID, EV_ABS, EV_KEY,
            EV_SYN, Frame, RawEvent, SYN_REPORT,
        },
        transport::{Consumer, ConsumerEvidence, Descriptor, KernelIdentity, Readiness},
    };
    fn ms(at: u64) -> Duration {
        Duration::from_millis(at)
    }
    fn device() -> Device {
        let mut device = crate::transport::tests::touch();
        for axis in device.axes.values_mut() {
            axis.fuzz = 0;
        }
        device
    }
    fn config() -> Config {
        let mut config = Config::preset("desktop").unwrap();
        config.mode = crate::config::Mode::Route;
        config.devices = BTreeMap::from([(
            "touch".into(),
            Selector {
                identity: Some(device().identity),
                ..Selector::default()
            },
        )]);
        config.policy.allowed_devices = vec!["touch".into()];
        let rule = &mut config.rules[0];
        rule.enabled = true;
        rule.devices = vec!["touch".into()];
        rule.contexts = vec![Context::Desktop];
        rule.delivery = Delivery::Buffer;
        rule.trigger = Trigger::Swipe {
            fingers: 2,
            direction: Direction::Down,
            travel: Distance::Millimetres(40.0),
            fallback_travel: Distance::Normalized(0.2),
            continuous: false,
        };
        rule.action = Action::Event {
            name: "two-down".into(),
        };
        config
    }
    fn output() -> Output {
        Output {
            name: "test".into(),
            pixel_width: 600.0,
            pixel_height: 1200.0,
            scale: 1.0,
            origin: Point { x: 0.0, y: 0.0 },
            transform: Transform::Normal,
        }
    }
    fn ready(device: &Device) -> Ready {
        let descriptor = Descriptor::mirror(
            device,
            KernelIdentity {
                bus: 3,
                vendor: device.vendor,
                product: device.product,
                version: 1,
            },
            None,
            &BTreeMap::new(),
            BTreeMap::new(),
        )
        .unwrap();
        let consumer = Consumer::IsolatedReceiver { pid: 1, start: 1 };
        let mut gate = Readiness::new(descriptor.clone(), consumer.clone(), ms(0)).unwrap();
        gate.metadata(&descriptor, ms(0)).unwrap();
        gate.consumer(
            &ConsumerEvidence {
                consumer,
                marker: descriptor.marker,
                name: descriptor.name,
                class: descriptor.class,
                seat: device.source.seat.clone(),
                settings: BTreeMap::new(),
                at: ms(0),
            },
            ms(0),
        )
        .unwrap();
        gate.neutral(true, ms(0)).unwrap();
        gate.finish(ms(0)).unwrap()
    }
    fn context(routing: &mut Routing, at: u64) {
        if at == 0 {
            routing
                .authority_mut()
                .bind(
                    Binding {
                        seat: "seat0".into(),
                        session: "test".into(),
                        uid: 1000,
                        compositor_pid: 10,
                        compositor_start: 20,
                        kind: SessionKind::User,
                    },
                    true,
                    false,
                )
                .unwrap();
        }
        routing
            .authority_mut()
            .update(
                Snapshot {
                    generation: 1,
                    revision: at + 1,
                    captured_at: ms(at),
                    state: State::Desktop,
                    outputs: vec![OutputState {
                        geometry: output(),
                        geometry_generation: 2,
                        active_workspace: Some(1),
                        screen: Screen::On,
                        keyboard: None,
                    }],
                    windows: vec![],
                    regions: vec![],
                    secure_overlay: false,
                },
                ms(at),
            )
            .unwrap();
        routing.actions_available(1, BTreeSet::from([Capability::Observations]), ms(at));
    }
    fn routing(config: Config, with_context: bool) -> Routing {
        let mut routing = Routing::new(config).unwrap();
        if with_context {
            context(&mut routing, 0);
        }
        let device = device();
        let mut mapping = Mapping::from_device(&device).unwrap();
        mapping.output = Some(output());
        mapping.generation = 2;
        routing
            .attach(device.clone(), &ready(&device), Some(mapping), ms(0))
            .unwrap();
        routing
    }
    fn frame(routing: &mut Routing, sequence: u64, at: u64, events: &[(u16, u16, i32)]) {
        let mut events: Vec<_> = events
            .iter()
            .map(|&(kind, code, value)| RawEvent { kind, code, value })
            .collect();
        events.push(RawEvent {
            kind: EV_SYN,
            code: SYN_REPORT,
            value: 0,
        });
        routing
            .push(Record::Frame {
                frame: Frame {
                    source: device().source,
                    at: ms(at),
                    sequence,
                    events,
                },
            })
            .unwrap();
    }
    fn down(routing: &mut Routing, fingers: usize) {
        let mut events = vec![(EV_KEY, 0x14a, 1)];
        for slot in 0..fingers {
            events.extend([
                (EV_ABS, ABS_MT_SLOT, slot as i32),
                (EV_ABS, ABS_MT_TRACKING_ID, slot as i32 + 10),
                (EV_ABS, ABS_MT_POSITION_X, 1000 + slot as i32 * 500),
                (EV_ABS, ABS_MT_POSITION_Y, 1000),
            ]);
        }
        frame(routing, 1, 0, &events);
    }
    fn movement(routing: &mut Routing, sequence: u64, at: u64, y: i32, fingers: usize) {
        let mut events = Vec::new();
        for slot in 0..fingers {
            events.extend([
                (EV_ABS, ABS_MT_SLOT, slot as i32),
                (EV_ABS, ABS_MT_POSITION_Y, y),
            ]);
        }
        frame(routing, sequence, at, &events);
    }
    fn up(routing: &mut Routing, sequence: u64, at: u64, fingers: usize) {
        let mut events = vec![(EV_KEY, 0x14a, 0)];
        for slot in 0..fingers {
            events.extend([
                (EV_ABS, ABS_MT_SLOT, slot as i32),
                (EV_ABS, ABS_MT_TRACKING_ID, -1),
            ]);
        }
        frame(routing, sequence, at, &events);
    }
    fn drain(routing: &mut Routing, at: u64) -> Vec<Frame> {
        let mut frames = Vec::new();
        while let Some(scheduled) = routing.due(&device().source, ms(at)).unwrap().cloned() {
            frames.push(scheduled.frame);
            routing
                .delivered(&device().source, scheduled.ticket, ms(at))
                .unwrap();
        }
        frames
    }
    #[test]
    fn configured_coherent_swipe_claims_before_completion_and_delivers_one_action_without_raw_touch()
     {
        let mut routing = routing(config(), true);
        down(&mut routing, 2);
        movement(&mut routing, 2, 140, 1500, 2);
        assert!(drain(&mut routing, 140).is_empty());
        assert!(
            routing.take_dispatches(ms(140)).is_empty(),
            "short travel must not run the final action"
        );
        movement(&mut routing, 3, 180, 2000, 2);
        up(&mut routing, 4, 200, 2);
        assert!(drain(&mut routing, 200).is_empty());
        let dispatches = routing.take_dispatches(ms(200));
        assert_eq!(dispatches.len(), 1, "{:?}", routing.take_events());
        assert!(
            matches!(&dispatches[0], Event::Matched { action: Action::Event { name }, .. } if name == "two-down")
        );
    }
    #[test]
    fn one_finger_app_tap_falls_through_at_join_and_preserves_release_cadence() {
        let mut routing = routing(config(), true);
        down(&mut routing, 1);
        routing.push(Record::Tick { at: ms(120) }).unwrap();
        assert_eq!(drain(&mut routing, 120).len(), 1);
        up(&mut routing, 2, 150, 1);
        assert!(drain(&mut routing, 150).is_empty());
        context(&mut routing, 200);
        routing.push(Record::Tick { at: ms(270) }).unwrap();
        assert_eq!(drain(&mut routing, 270).len(), 1);
        assert!(routing.take_dispatches(ms(270)).is_empty());
    }
    #[test]
    fn unknown_context_does_not_buffer_or_consume_a_configured_rule() {
        let mut routing = routing(config(), false);
        down(&mut routing, 2);
        assert_eq!(drain(&mut routing, 0).len(), 1);
        movement(&mut routing, 2, 140, 2000, 2);
        assert_eq!(drain(&mut routing, 140).len(), 1);
        up(&mut routing, 3, 200, 2);
        assert_eq!(drain(&mut routing, 200).len(), 1);
        assert!(routing.take_dispatches(ms(200)).is_empty());
    }
    #[test]
    fn incomplete_claimed_swipe_drains_without_an_action_or_replay() {
        let mut routing = routing(config(), true);
        down(&mut routing, 2);
        movement(&mut routing, 2, 140, 1500, 2);
        up(&mut routing, 3, 180, 2);
        assert!(drain(&mut routing, 180).is_empty());
        assert!(routing.take_dispatches(ms(180)).is_empty());
    }
    #[test]
    fn queued_action_is_fenced_again_after_context_revocation() {
        let mut routing = routing(config(), true);
        down(&mut routing, 2);
        movement(&mut routing, 2, 140, 2000, 2);
        up(&mut routing, 3, 180, 2);
        routing.authority_mut().active_changed(false);
        assert!(routing.take_dispatches(ms(180)).is_empty());
        routing.push(Record::Tick { at: ms(181) }).unwrap();
        assert_eq!(routing.take_recoveries().len(), 1);
    }
    #[test]
    fn competing_finger_counts_choose_the_highest_eligible_rule_once() {
        let mut config = config();
        config.rules[0].priority = 10;
        let mut three = config.rules[0].clone();
        three.id = "three".into();
        three.priority = 100;
        three.trigger = Trigger::Swipe {
            fingers: 3,
            direction: Direction::Down,
            travel: Distance::Millimetres(40.0),
            fallback_travel: Distance::Normalized(0.2),
            continuous: false,
        };
        three.action = Action::Event {
            name: "three-down".into(),
        };
        config.rules.push(three);
        let mut routing = routing(config, true);
        down(&mut routing, 3);
        movement(&mut routing, 2, 140, 2000, 3);
        up(&mut routing, 3, 180, 3);
        let dispatches = routing.take_dispatches(ms(180));
        assert_eq!(dispatches.len(), 1, "{:?}", routing.take_events());
        assert!(
            matches!(&dispatches[0], Event::Matched { action: Action::Event { name }, .. } if name == "three-down")
        );
        assert!(drain(&mut routing, 180).is_empty());
    }
    #[test]
    fn direction_mismatch_cannot_block_the_eligible_lower_priority_swipe() {
        let mut config = config();
        let mut opposite = config.rules[0].clone();
        opposite.id = "up".into();
        opposite.priority = 100;
        opposite.trigger = Trigger::Swipe {
            fingers: 2,
            direction: Direction::Up,
            travel: Distance::Millimetres(40.0),
            fallback_travel: Distance::Normalized(0.2),
            continuous: false,
        };
        opposite.action = Action::Event {
            name: "two-up".into(),
        };
        config.rules.push(opposite);
        let mut routing = routing(config, true);
        down(&mut routing, 2);
        movement(&mut routing, 2, 140, 2000, 2);
        up(&mut routing, 3, 180, 2);
        let dispatches = routing.take_dispatches(ms(180));
        assert_eq!(dispatches.len(), 1, "{:?}", routing.take_events());
        assert!(
            matches!(&dispatches[0], Event::Matched { action: Action::Event { name }, .. } if name == "two-down")
        );
    }
    #[test]
    fn extra_finger_after_claim_cancels_and_drains_without_a_final_click() {
        let mut routing = routing(config(), true);
        down(&mut routing, 2);
        movement(&mut routing, 2, 140, 1500, 2);
        frame(
            &mut routing,
            3,
            160,
            &[
                (EV_ABS, ABS_MT_SLOT, 2),
                (EV_ABS, ABS_MT_TRACKING_ID, 30),
                (EV_ABS, ABS_MT_POSITION_X, 2500),
                (EV_ABS, ABS_MT_POSITION_Y, 1500),
            ],
        );
        up(&mut routing, 4, 180, 3);
        assert!(drain(&mut routing, 180).is_empty());
        assert!(routing.take_dispatches(ms(180)).is_empty());
    }
    #[test]
    fn forwarded_one_finger_cannot_be_promoted_by_a_late_second_finger() {
        let mut routing = routing(config(), true);
        down(&mut routing, 1);
        routing.push(Record::Tick { at: ms(120) }).unwrap();
        assert_eq!(drain(&mut routing, 120).len(), 1);
        frame(
            &mut routing,
            2,
            140,
            &[
                (EV_ABS, ABS_MT_SLOT, 1),
                (EV_ABS, ABS_MT_TRACKING_ID, 20),
                (EV_ABS, ABS_MT_POSITION_X, 1500),
                (EV_ABS, ABS_MT_POSITION_Y, 1000),
            ],
        );
        assert!(drain(&mut routing, 140).is_empty());
        context(&mut routing, 200);
        movement(&mut routing, 3, 220, 2000, 2);
        assert_eq!(drain(&mut routing, 260).len(), 1);
        up(&mut routing, 4, 270, 2);
        assert_eq!(drain(&mut routing, 340).len(), 1);
        assert_eq!(drain(&mut routing, 390).len(), 1);
        assert!(routing.take_dispatches(ms(390)).is_empty());
    }
    #[test]
    fn an_unavailable_action_consumer_leaves_ordinary_input_immediate() {
        let mut routing = routing(config(), true);
        routing.actions_available(1, BTreeSet::new(), ms(0));
        down(&mut routing, 2);
        assert_eq!(drain(&mut routing, 0).len(), 1);
        movement(&mut routing, 2, 140, 2000, 2);
        assert_eq!(drain(&mut routing, 140).len(), 1);
        up(&mut routing, 3, 180, 2);
        assert_eq!(drain(&mut routing, 180).len(), 1);
        assert!(routing.take_dispatches(ms(180)).is_empty());
    }
    #[test]
    fn observing_rule_in_route_mode_dispatches_and_also_preserves_raw_input() {
        let mut config = config();
        config.rules[0].delivery = Delivery::Observe;
        let mut routing = routing(config, true);
        down(&mut routing, 2);
        assert_eq!(drain(&mut routing, 0).len(), 1);
        movement(&mut routing, 2, 140, 2000, 2);
        assert_eq!(drain(&mut routing, 140).len(), 1);
        up(&mut routing, 3, 180, 2);
        assert_eq!(drain(&mut routing, 180).len(), 1);
        assert_eq!(routing.take_dispatches(ms(180)).len(), 1);
    }
    #[test]
    fn valid_reload_requires_teardown_and_invalid_reload_preserves_claimed_progress() {
        let mut routing = routing(config(), true);
        down(&mut routing, 2);
        movement(&mut routing, 2, 140, 1500, 2);
        let mut invalid = config();
        invalid.rules[0].trigger = Trigger::Shape {
            name: "missing".into(),
            fingers: Some(2),
            button: None,
        };
        assert!(routing.reload(invalid, ms(150)).is_err());
        assert!(routing.take_recoveries().is_empty());
        movement(&mut routing, 3, 180, 2000, 2);
        up(&mut routing, 4, 200, 2);
        assert_eq!(routing.take_dispatches(ms(200)).len(), 1);
        routing.reload(config(), ms(210)).unwrap();
        assert_eq!(routing.take_recoveries().len(), 1);
    }
    #[test]
    fn decision_timeout_commits_once_and_later_motion_cannot_claim() {
        let mut routing = routing(config(), true);
        down(&mut routing, 2);
        routing.push(Record::Tick { at: ms(120) }).unwrap();
        assert!(drain(&mut routing, 120).is_empty());
        assert_eq!(routing.deadline(), Some(ms(300)));
        context(&mut routing, 200);
        routing.push(Record::Tick { at: ms(300) }).unwrap();
        assert_eq!(drain(&mut routing, 300).len(), 1);
        context(&mut routing, 400);
        movement(&mut routing, 2, 400, 2000, 2);
        assert!(drain(&mut routing, 400).is_empty());
        up(&mut routing, 3, 500, 2);
        assert!(routing.take_dispatches(ms(500)).is_empty());
        assert_eq!(drain(&mut routing, 700).len(), 1);
        assert_eq!(drain(&mut routing, 800).len(), 1);
    }
    #[test]
    fn dispatch_backpressure_requests_recovery_instead_of_unbounded_retention() {
        use crate::config::Key;
        let mut config = config();
        config.rules[0].contexts = vec![Context::Console];
        config.rules[0].delivery = Delivery::Observe;
        config.rules[0].trigger = Trigger::Key { key: Key(30) };
        let mut routing = Routing::new(config).unwrap();
        routing
            .authority_mut()
            .enter_console("seat0".into())
            .unwrap();
        routing.actions_available(1, BTreeSet::from([Capability::Observations]), ms(0));
        let mut keyboard = device();
        keyboard.axes.clear();
        keyboard.properties.clear();
        keyboard.capabilities = BTreeMap::from([(EV_SYN, vec![SYN_REPORT]), (EV_KEY, vec![30])]);
        routing
            .attach(keyboard.clone(), &ready(&keyboard), None, ms(0))
            .unwrap();
        let mut overflow = false;
        for index in 1..=300u64 {
            let value = if index % 2 == 1 { 1 } else { 0 };
            let record = Record::Frame {
                frame: Frame {
                    source: keyboard.source.clone(),
                    at: ms(index),
                    sequence: index,
                    events: vec![
                        RawEvent {
                            kind: EV_KEY,
                            code: 30,
                            value,
                        },
                        RawEvent {
                            kind: EV_SYN,
                            code: SYN_REPORT,
                            value: 0,
                        },
                    ],
                },
            };
            if routing.push(record).is_err() {
                overflow = true;
                break;
            }
            drain(&mut routing, index);
        }
        assert!(overflow);
        assert_eq!(routing.take_recoveries().len(), 1);
        assert!(routing.take_dispatches(ms(300)).is_empty());
    }
    #[test]
    fn reserved_key_sequence_keeps_intermediate_steps_private_and_drains_the_key() {
        use crate::config::{Key, Millis};
        let mut config = config();
        config.rules[0].contexts = vec![Context::Console];
        config.rules[0].delivery = Delivery::Reserve;
        config.rules[0].trigger = Trigger::Sequence {
            timeout: Millis(2000),
            steps: vec![
                Trigger::Key { key: Key(30) },
                Trigger::ButtonHold {
                    key: Key(30),
                    after: Millis(600),
                    repeat: None,
                },
            ],
        };
        let mut routing = Routing::new(config).unwrap();
        routing
            .authority_mut()
            .enter_console("seat0".into())
            .unwrap();
        routing.actions_available(1, BTreeSet::from([Capability::Observations]), ms(0));
        let mut keyboard = device();
        keyboard.axes.clear();
        keyboard.properties.clear();
        keyboard.capabilities = BTreeMap::from([(EV_SYN, vec![SYN_REPORT]), (EV_KEY, vec![30])]);
        routing
            .attach(keyboard.clone(), &ready(&keyboard), None, ms(0))
            .unwrap();
        let event = |sequence, at, value| Record::Frame {
            frame: Frame {
                source: keyboard.source.clone(),
                at: ms(at),
                sequence,
                events: vec![
                    RawEvent {
                        kind: EV_KEY,
                        code: 30,
                        value,
                    },
                    RawEvent {
                        kind: EV_SYN,
                        code: SYN_REPORT,
                        value: 0,
                    },
                ],
            },
        };
        routing.push(event(1, 0, 1)).unwrap();
        assert!(drain(&mut routing, 0).is_empty());
        assert!(routing.take_dispatches(ms(0)).is_empty());
        routing.push(Record::Tick { at: ms(600) }).unwrap();
        let dispatched = routing.take_dispatches(ms(600));
        assert_eq!(dispatched.len(), 1, "{:?}", routing.take_events());
        assert!(
            matches!(&dispatched[0], Event::Matched { evidence: crate::rules::Evidence::Sequence { steps }, .. } if steps.len() == 2)
        );
        routing.push(event(2, 610, 0)).unwrap();
        assert!(drain(&mut routing, 610).is_empty());
    }
    #[test]
    fn missed_downstream_delivery_requests_neutral_teardown() {
        let mut routing = routing(config(), false);
        down(&mut routing, 1);
        assert!(routing.due(&device().source, ms(21)).is_err());
        let recovery = routing.take_recoveries();
        assert_eq!(recovery.len(), 1);
        assert!(recovery[0].destroy_virtual_and_release_grab);
        assert!(
            recovery[0].neutral_events.is_empty(),
            "nothing was acknowledged downstream"
        );
    }
}
