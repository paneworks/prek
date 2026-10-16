//! Configured recognition, arbitration and raw output ownership. Kernel I/O and
//! action execution remain separate: callers acknowledge writes and revalidate
//! dispatch fences in the session executor. Only verified, neutral transports
//! may be attached here.

use crate::{
    config::{Config, Delivery, Trigger},
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
    custom_groups: BTreeMap<u64, (Source, u64, u64)>,
    deferred_custom: VecDeque<RouteOutcome>,
    deferred_bytes: usize,
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
fn custom_ownership(
    routes: &BTreeMap<Source, Route>,
    groups: &BTreeMap<u64, (Source, u64, u64)>,
    source: &Source,
    rule: &crate::config::Rule,
    id: u64,
) -> (Option<Ownership>, Option<u64>) {
    if rule.delivery == Delivery::Observe {
        return (None, None);
    }
    let Some(route) = routes.get(source) else {
        return (Some(Ownership::Forwarded), None);
    };
    if !groups
        .get(&id)
        .is_some_and(|(bound, ticket, authorization)| {
            bound == source && *ticket == route.ticket && *authorization == route.authorization
        })
    {
        return (Some(Ownership::Forwarded), None);
    }
    (
        route.router.ownership().or(Some(Ownership::Forwarded)),
        route.winner,
    )
}
impl Routing {
    fn custom_group_valid(&self, intent: &RouteIntent) -> bool {
        matches!(
            custom_ownership(
                &self.routes,
                &self.custom_groups,
                intent.source(),
                &intent.rule,
                intent.id()
            ),
            (Some(Ownership::Pending), None)
        ) || matches!(custom_ownership(&self.routes, &self.custom_groups, intent.source(), &intent.rule, intent.id()), (Some(Ownership::Claimed), Some(winner)) if winner == intent.id())
    }
    pub(crate) fn configuration_generation(&mut self, generation: u64) -> io::Result<()> {
        if generation == 0
            || !self.routes.is_empty()
            || self
                .engine
                .recognition_fence()
                .is_some_and(|fence| fence.configuration != generation)
        {
            return Err(io::Error::other(
                "routing generation requires a fresh coordinator",
            ));
        }
        self.engine.configuration_generation(generation);
        Ok(())
    }
    pub fn new(config: Config) -> io::Result<Self> {
        Self::build(config, None)
    }
    /// This does not admit kernel routing or authenticate a session provider.
    pub fn with_recognition(config: Config, fence: crate::protocol::Fence) -> io::Result<Self> {
        Self::build(config, Some(fence))
    }
    fn build(config: Config, fence: Option<crate::protocol::Fence>) -> io::Result<Self> {
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
        let mut engine = if let Some(fence) = fence {
            Engine::with_recognition(config, fence)?
        } else {
            Engine::new(config)?
        };
        engine.push(Record::Header { version: 1 })?;
        engine.enable_routing();
        Ok(Self {
            custom_groups: BTreeMap::new(),
            deferred_custom: VecDeque::new(),
            deferred_bytes: 0,
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
        self.attach_with_state(device, ready, mapping, None, at)
    }

    pub fn attach_with_state(
        &mut self,
        device: Device,
        ready: &Ready,
        mapping: Option<crate::gestures::Mapping>,
        absolute: Option<crate::input::AbsoluteState>,
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
        let mut router =
            Router::new(device.clone(), ready, authorization, limits, at).map_err(fault)?;
        if let Some(state) = &absolute {
            state.validate(&device.axes).map_err(io::Error::other)?;
            router.seed_absolute(state).map_err(fault)?;
        }
        self.engine.push(Record::Device {
            device: device.clone(),
            keys: BTreeSet::new(),
            contacts: Vec::new(),
        })?;
        if let Some(state) = absolute {
            self.engine.push(Record::Absolute {
                source: device.source.clone(),
                state,
            })?;
        }
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
            && (!intent.rule.action.requires_hyprland()
                || self.engine.config().policy.allow_hyprland_actions
                    && intent.hyprland_target_available(self.engine.authority(), at))
            && (!intent.rule.action.requires_power()
                || self.engine.config().policy.allow_power_actions
                    && intent.power_target_available()
                    && !matches!(
                        intent.rule.action,
                        crate::config::Action::Power {
                            action: crate::config::PowerAction::LockAndBlank
                        }
                    ))
            && at < self.capability_expires
            && crate::protocol::action_capabilities(&intent.rule.action)
                .is_subset(&self.capabilities)
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
        self.push_clocked(record, None)
    }
    pub(crate) fn push_at(&mut self, record: Record, now: Duration) -> io::Result<()> {
        self.push_clocked(record, Some(now))
    }
    fn push_clocked(&mut self, record: Record, now: Option<Duration>) -> io::Result<()> {
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
        let now = now.unwrap_or(at);
        if at > now {
            return Err(io::Error::other("routing input watermark is in the future"));
        }
        let source = match &record {
            Record::Frame { frame } => Some(frame.source.clone()),
            _ => None,
        };
        let frame = match &record {
            Record::Frame { frame } => Some(frame.clone()),
            _ => None,
        };
        let fresh_group = source
            .as_ref()
            .zip(frame.as_ref())
            .is_some_and(|(source, frame)| {
                self.routes.get(source).is_some_and(|route| {
                    route.router.physical_neutral()
                        && frame.events.iter().any(|event| {
                            event.kind == crate::input::EV_KEY && event.value == 1
                                || event.kind == crate::input::EV_ABS
                                    && event.code == crate::input::ABS_MT_TRACKING_ID
                                    && event.value >= 0
                                || event.kind == crate::input::EV_REL && event.value != 0
                                || event.kind == crate::input::EV_SW
                        })
                })
            });
        if fresh_group && let Some(source) = &source {
            let previous: Vec<_> = self
                .custom_groups
                .iter()
                .filter_map(|(id, (bound, _, _))| (bound == source).then_some(*id))
                .collect();
            for id in previous {
                self.engine.cancel_custom(
                    id,
                    at,
                    "new physical group superseded custom recognition",
                );
            }
        }
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
        let mut outcomes = self.engine.take_route_outcomes();
        let deferred = std::mem::take(&mut self.deferred_custom);
        self.deferred_bytes = 0;
        for outcome in deferred {
            if outcome.expires.is_some_and(|expiry| now >= expiry)
                || fresh_group && source.as_ref() == Some(outcome.intent.source())
                || !self.custom_group_valid(&outcome.intent)
            {
                self.engine.cancel_custom(
                    outcome.intent.id(),
                    at,
                    "deferred custom recognition expired or group changed",
                );
            } else {
                outcomes.push(outcome);
            }
        }
        let mut updates = BTreeMap::new();
        for outcome in &outcomes {
            if matches!(outcome.intent.rule.trigger, Trigger::Custom { .. })
                && let Event::Matched {
                    id,
                    ordinal,
                    stage: crate::rules::Stage::Update,
                    ..
                } = &outcome.event
            {
                updates
                    .entry(*id)
                    .and_modify(|old: &mut u64| *old = (*old).max(*ordinal))
                    .or_insert(*ordinal);
            }
        }
        outcomes.retain(|outcome| !matches!(&outcome.event, Event::Matched { id, ordinal, stage: crate::rules::Stage::Update, .. } if updates.get(id).is_some_and(|latest| ordinal != latest)));
        outcomes.sort_by_key(|outcome| match &outcome.event {
            Event::Matched { id, ordinal, .. } => (*id, *ordinal),
            _ => (0, 0),
        });
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
                        && self.available(intent, now)
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
                let completion = route
                    .choices
                    .first()
                    .is_some_and(|intent| matches!(intent.rule.trigger, Trigger::Custom { .. }));
                if let Err(error) = route
                    .router
                    .push_with_completion(frame, candidate, at, completion)
                {
                    self.recover(&source);
                    return Err(fault(error));
                }
                if starts {
                    for intent in &route.choices {
                        if matches!(intent.rule.trigger, Trigger::Custom { .. }) {
                            self.custom_groups.entry(intent.id()).or_insert((
                                source.clone(),
                                route.ticket,
                                route.authorization,
                            ));
                        }
                    }
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
        self.resolve(at, now, &active, &claims, &outcomes)?;
        for intent in &active {
            if matches!(intent.rule.trigger, Trigger::Custom { .. })
                && self.routes.get(intent.source()).is_some_and(|route| {
                    route.router.ownership() == Some(Ownership::Claimed)
                        && route.winner != Some(intent.id())
                })
            {
                self.engine.cancel_custom(
                    intent.id(),
                    at,
                    "another candidate owns the input group",
                );
            }
        }
        for outcome in outcomes {
            let id = outcome.intent.id();
            let observe = outcome.intent.rule.delivery == Delivery::Observe;
            if self.available(&outcome.intent, now) && (observe || self.won.contains_key(&id)) {
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
            } else if matches!(outcome.intent.rule.trigger, Trigger::Custom { .. })
                && self.available(&outcome.intent, now)
                && self.custom_group_valid(&outcome.intent)
                && self.routes[outcome.intent.source()].router.ownership()
                    == Some(Ownership::Pending)
                && outcome.expires.is_some_and(|expiry| now < expiry)
            {
                let bytes = crate::recognition::encoded_size(
                    &outcome.event,
                    crate::recognition::MAX_FRAME_BYTES,
                )?;
                if self.deferred_custom.len() >= crate::recognition::MAX_PENDING * 3
                    || self.deferred_bytes + bytes > crate::recognition::MAX_BYTES
                {
                    let source = outcome.intent.source().clone();
                    self.recover(&source);
                    return Err(fault(Fault::Overflow));
                }
                self.deferred_bytes += bytes;
                self.deferred_custom.push_back(outcome);
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
        for route in self.routes.values_mut() {
            if route.router.completion_pending()
                && route
                    .winner
                    .is_some_and(|id| !active.iter().any(|intent| intent.id() == id))
            {
                route.router.release_completion();
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
        self.custom_groups
            .retain(|id, (source, ticket, authorization)| {
                self.routes.get(source).is_some_and(|route| {
                    route.ticket == *ticket && route.authorization == *authorization
                }) && (active.iter().any(|intent| intent.id() == *id)
                    || self
                        .deferred_custom
                        .iter()
                        .any(|outcome| outcome.intent.id() == *id)
                    || self.won.contains_key(id))
            });
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
        now: Duration,
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
                    intent.sources().contains(&source)
                        && intent.rule.delivery != Delivery::Observe
                        && (!matches!(intent.rule.trigger, Trigger::Custom { .. })
                            || self.custom_group_valid(intent))
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
                    .any(|intent| self.engine.validate_route(intent, now).is_err())
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
                    && (!self.available(&won.intent, now)
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
                    self.available(intent, now)
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
    pub(crate) fn due_after_input(
        &mut self,
        source: &Source,
        now: Duration,
    ) -> io::Result<Option<&Scheduled>> {
        let result = self
            .routes
            .get_mut(source)
            .ok_or_else(|| fault(Fault::StaleSource))?
            .router
            .due_after_input(now)
            .map(|_| ())
            .map_err(fault);
        if let Err(error) = result {
            self.recover(source);
            return Err(error);
        }
        self.routes
            .get_mut(source)
            .unwrap()
            .router
            .due_after_input(now)
            .map_err(fault)
    }
    pub(crate) fn delivered_after_input(
        &mut self,
        source: &Source,
        ticket: u64,
        now: Duration,
    ) -> io::Result<()> {
        let result = self
            .routes
            .get_mut(source)
            .ok_or_else(|| fault(Fault::StaleSource))?
            .router
            .delivered_after_input(ticket, now)
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
                && dispatch.outcome.expires.is_none_or(|expiry| at < expiry)
            {
                result.push(dispatch.outcome.event);
            }
        }
        result
    }
    pub fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }
    pub fn recognition_requests(
        &mut self,
        at: Duration,
    ) -> io::Result<Vec<crate::recognition::Ticket>> {
        self.recognition_requests_after_input(at, at)
    }
    pub(crate) fn recognition_requests_after_input(
        &mut self,
        at: Duration,
        now: Duration,
    ) -> io::Result<Vec<crate::recognition::Ticket>> {
        self.push_at(Record::Tick { at }, now)?;
        let routes = &self.routes;
        let groups = &self.custom_groups;
        let tickets = self
            .engine
            .recognition_requests_with(at, |source, rule, id| {
                custom_ownership(routes, groups, source, rule, id)
            })?;
        self.push_at(Record::Tick { at }, now)?;
        Ok(tickets)
    }
    pub fn recognition_response(
        &mut self,
        proposal: crate::recognition::Proposal,
        at: Duration,
    ) -> io::Result<()> {
        self.recognition_response_after_input(proposal, at, at)
    }
    pub(crate) fn recognition_response_after_input(
        &mut self,
        proposal: crate::recognition::Proposal,
        input_at: Duration,
        now: Duration,
    ) -> io::Result<()> {
        self.push_at(Record::Tick { at: input_at }, now)?;
        let routes = &self.routes;
        let groups = &self.custom_groups;
        let result = self.engine.recognition_response_clocked(
            proposal,
            input_at,
            now,
            |source, rule, id| custom_ownership(routes, groups, source, rule, id),
        );
        let synchronized = self.push_at(Record::Tick { at: input_at }, now);
        result.and(synchronized)
    }
    pub(crate) fn recognition_ready(&mut self, ready: bool, now: Duration) -> io::Result<()> {
        let at = self.engine.input_at();
        if at > now {
            return Err(io::Error::other("recognizer readiness clock regressed"));
        }
        self.engine.recognition_ready(ready, now)?;
        self.push_at(Record::Tick { at }, now)
    }
    pub(crate) fn input_states(&self, at: Duration) -> Vec<crate::input_events::Event> {
        self.engine.input_states(at)
    }
    pub(crate) fn capture_input(&mut self, enabled: bool) {
        self.engine.capture_input(enabled);
    }
    pub(crate) fn take_input(&mut self) -> Option<crate::input_events::Event> {
        self.engine.take_input()
    }
    pub fn take_recognition_cancellations(&mut self) -> Vec<crate::recognition::Cancellation> {
        self.engine.take_recognition_cancellations()
    }
    pub fn rule_revision(&self) -> u64 {
        self.engine.rule_revision()
    }
    /// Trusted embeddings authorize the transaction before calling this.
    /// Valid edits retire every transport and queued dispatch; callers must
    /// consume recovery and establish fresh neutral readiness with a newer
    /// source incarnation before reattach.
    pub fn transact_rules(
        &mut self,
        transaction: &crate::rule_transactions::Transaction,
        at: Duration,
    ) -> io::Result<u64> {
        let config = transaction
            .prepare(self.engine.config(), self.engine.rule_revision())
            .map_err(io::Error::other)?;
        // Includes the transport-specific timing checks before any retirement.
        Self::build(config, self.engine.recognition_fence())?;
        let revision = self.engine.transact_rules(transaction, at)?;
        let sources: Vec<_> = self.routes.keys().cloned().collect();
        for source in sources {
            self.recover(&source);
        }
        self.events
            .retain(|event| !matches!(event, Event::Matched { .. }));
        self.events.extend(self.engine.take_events());
        Ok(revision)
    }
    pub fn reload(&mut self, config: Config, at: Duration) -> io::Result<()> {
        // Compile/validate first. An invalid replacement preserves both the
        // old model and every transport's pending/claimed ownership.
        Self::build(config.clone(), self.engine.recognition_fence())?;
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
    pub(crate) fn has_source(&self, source: &Source) -> bool {
        self.routes.contains_key(source)
    }
    pub fn recover(&mut self, source: &Source) {
        self.engine.retire_routing_recognition(source);
        self.custom_groups
            .retain(|_, (bound, _, _)| bound != source);
        self.deferred_custom
            .retain(|outcome| outcome.intent.source() != source);
        self.deferred_bytes = self
            .deferred_custom
            .iter()
            .map(|outcome| {
                crate::recognition::encoded_size(
                    &outcome.event,
                    crate::recognition::MAX_FRAME_BYTES,
                )
                .unwrap_or(crate::recognition::MAX_FRAME_BYTES)
            })
            .sum();
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
            .chain(
                self.deferred_custom
                    .iter()
                    .filter_map(|outcome| outcome.expires),
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
    mod custom;
    mod sequence;
    use super::*;
    use crate::config::Action;
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
                        keyboard_known: true,
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
    fn keyboard_power_claim_requires_policy_target_and_current_consumer() {
        use crate::config::{Key, PowerAction};
        for case in [
            "allowed",
            "policy",
            "consumer",
            "target",
            "missing",
            "unsupported",
            "locked",
            "overlay",
        ] {
            let mut device = device();
            device.axes.clear();
            device.properties.clear();
            device.capabilities = BTreeMap::from([(EV_SYN, vec![SYN_REPORT]), (EV_KEY, vec![30])]);
            let mut config = config();
            config.policy.allow_power_actions = case != "policy";
            let rule = &mut config.rules[0];
            rule.trigger = Trigger::Key { key: Key(30) };
            rule.delivery = Delivery::Reserve;
            if case == "locked" {
                rule.contexts.push(Context::Locked);
            }
            rule.action = Action::Power {
                action: if case == "unsupported" {
                    PowerAction::LockAndBlank
                } else {
                    PowerAction::Blank
                },
            };
            rule.output = if case == "target" {
                None
            } else {
                Some(if case == "missing" { "missing" } else { "test" }.into())
            };
            let mut routing = Routing::new(config).unwrap();
            context(&mut routing, 0);
            if matches!(case, "locked" | "overlay") {
                if case == "locked" {
                    routing.authority_mut().lock_requested();
                    let generation = routing.authority_mut().generation();
                    routing.authority_mut().lock_confirmed(generation).unwrap();
                }
                let mut snapshot = crate::context_transfer::tests::view(ms(0))
                    .context
                    .unwrap()
                    .snapshot;
                snapshot.generation = routing.authority_mut().generation();
                snapshot.state = if case == "locked" {
                    State::Locked
                } else {
                    State::Desktop
                };
                snapshot.secure_overlay = case == "overlay";
                snapshot.outputs = vec![OutputState {
                    geometry: output(),
                    geometry_generation: 2,
                    active_workspace: Some(1),
                    screen: Screen::On,
                    keyboard_known: true,
                    keyboard: None,
                }];
                snapshot.windows.clear();
                snapshot.regions.clear();
                routing.authority_mut().update(snapshot, ms(0)).unwrap();
            }
            routing
                .attach(device.clone(), &ready(&device), None, ms(0))
                .unwrap();
            let generation = routing.authority_mut().generation();
            routing.actions_available(
                generation,
                if case == "consumer" {
                    BTreeSet::new()
                } else {
                    BTreeSet::from([Capability::PowerActions])
                },
                ms(0),
            );
            routing
                .push(Record::Frame {
                    frame: Frame {
                        source: device.source.clone(),
                        at: ms(0),
                        sequence: 1,
                        events: vec![
                            RawEvent {
                                kind: EV_KEY,
                                code: 30,
                                value: 1,
                            },
                            RawEvent {
                                kind: EV_SYN,
                                code: SYN_REPORT,
                                value: 0,
                            },
                        ],
                    },
                })
                .unwrap();
            let forwarded = drain(&mut routing, 0);
            let dispatches = routing.take_dispatches(ms(0));
            if case == "allowed" {
                assert!(forwarded.is_empty());
                assert_eq!(dispatches.len(), 1);
                assert!(
                    matches!(&dispatches[0],Event::Matched{output:Some(output),stage:crate::rules::Stage::Begin,..} if output=="test")
                );
            } else {
                assert_eq!(forwarded.len(), 1, "{case} must preserve ordinary input");
                assert!(dispatches.is_empty());
            }
        }
    }
    #[test]
    fn keyboard_window_actions_require_policy_frozen_target_workspace_and_consumer_before_claim() {
        use crate::config::{EffectKind, Key, WindowAction};
        for case in [
            "allowed",
            "stock",
            "stock-unfocused",
            "move",
            "lua",
            "close-multi",
            "policy",
            "consumer",
            "window",
            "output",
            "identity",
            "workspace",
            "left-bound",
            "right-bound",
            "multi-output",
            "unsupported",
            "locked",
            "overlay",
            "screen",
        ] {
            let mut device = device();
            device.axes.clear();
            device.properties.clear();
            device.capabilities = BTreeMap::from([(EV_SYN, vec![SYN_REPORT]), (EV_KEY, vec![30])]);
            let mut config = config();
            config.policy.allow_hyprland_actions = case != "policy";
            let rule = &mut config.rules[0];
            rule.trigger = Trigger::Key { key: Key(30) };
            rule.delivery = Delivery::Reserve;
            rule.output = (case != "output").then(|| "test".into());
            rule.action = if case == "lua" {
                Action::Lua {
                    handler: "window".into(),
                    requires: BTreeSet::from([EffectKind::HyprlandActions]),
                }
            } else {
                Action::Hyprland {
                    action: match case {
                        "move" | "multi-output" | "right-bound" => WindowAction::MoveRightAndFollow,
                        "left-bound" => WindowAction::MoveLeftAndFollow,
                        "unsupported" => WindowAction::ReorderUp,
                        _ => WindowAction::Close,
                    },
                }
            };
            if case == "locked" {
                rule.contexts.push(Context::Locked);
            }
            if case == "screen" {
                rule.screen = Some(Screen::Off);
            }
            let mut routing = Routing::new(config).unwrap();
            context(&mut routing, 0);
            if case == "locked" {
                routing.authority_mut().lock_requested();
                let generation = routing.authority_mut().generation();
                routing.authority_mut().lock_confirmed(generation).unwrap();
            }
            let workspace = match case {
                "left-bound" => 1,
                "right-bound" => i64::from(i32::MAX),
                _ => 2,
            };
            let geometry = output();
            let rect = crate::context::Rect {
                origin: geometry.origin,
                size: geometry.logical_size(),
            };
            let mut snapshot = Snapshot {
                generation: routing.authority_mut().generation(),
                revision: 2,
                captured_at: ms(0),
                state: if case == "locked" {
                    State::Locked
                } else {
                    State::Desktop
                },
                secure_overlay: case == "overlay",
                outputs: vec![OutputState {
                    geometry,
                    geometry_generation: 2,
                    active_workspace: Some(if case == "workspace" { 3 } else { workspace }),
                    screen: if case == "screen" {
                        Screen::Off
                    } else {
                        Screen::On
                    },
                    keyboard_known: true,
                    keyboard: None,
                }],
                windows: vec![crate::context::Window {
                    id: if case == "identity" {
                        "invalid"
                    } else {
                        "0x1234:abc"
                    }
                    .into(),
                    output: "test".into(),
                    workspace,
                    rect,
                    content: rect,
                    mapped: true,
                    visible: !matches!(case, "stock" | "stock-unfocused"),
                    focused: case != "stock-unfocused",
                    z_order: Some(1),
                }],
                regions: vec![],
            };
            if case == "window" {
                snapshot.windows.clear();
            }
            if matches!(case, "close-multi" | "multi-output") {
                let mut other = snapshot.outputs[0].clone();
                other.geometry.name = "other".into();
                other.geometry.origin.x = 600.0;
                other.active_workspace = Some(77);
                snapshot.outputs.push(other);
            }
            routing.authority_mut().update(snapshot, ms(0)).unwrap();
            routing
                .attach(device.clone(), &ready(&device), None, ms(0))
                .unwrap();
            let generation = routing.authority_mut().generation();
            routing.actions_available(
                generation,
                if case == "consumer" {
                    BTreeSet::new()
                } else {
                    BTreeSet::from([Capability::HyprlandActions, Capability::LuaActions])
                },
                ms(0),
            );
            routing
                .push(Record::Frame {
                    frame: Frame {
                        source: device.source.clone(),
                        at: ms(0),
                        sequence: 1,
                        events: vec![
                            RawEvent {
                                kind: EV_KEY,
                                code: 30,
                                value: 1,
                            },
                            RawEvent {
                                kind: EV_SYN,
                                code: SYN_REPORT,
                                value: 0,
                            },
                        ],
                    },
                })
                .unwrap();
            let forwarded = drain(&mut routing, 0);
            let dispatches = routing.take_dispatches(ms(0));
            if matches!(case, "allowed" | "stock" | "move" | "lua" | "close-multi") {
                assert!(forwarded.is_empty(), "{case}: input was not claimed");
                assert_eq!(dispatches.len(), 1, "{case}");
                assert!(
                    matches!(&dispatches[0],Event::Matched {window:Some(window),output:Some(output),workspace:Some(2),..} if window=="0x1234:abc" && output=="test")
                );
            } else {
                assert_eq!(forwarded.len(), 1, "{case} must preserve ordinary input");
                assert!(dispatches.is_empty(), "{case}");
            }
        }
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
    fn rule_transaction_retains_invalid_claims_then_retires_transports_and_old_dispatches() {
        use crate::rule_transactions::{Change, Transaction};
        let config = config();
        let id = config.rules[0].id.clone();
        let mut routing = routing(config.clone(), true);
        down(&mut routing, 2);
        movement(&mut routing, 2, 140, 1500, 2);
        let invalid = Transaction {
            revision: 1,
            changes: vec![
                Change::Enabled {
                    id: id.clone(),
                    enabled: false,
                },
                Change::Remove {
                    id: "missing".into(),
                },
            ],
        };
        assert!(routing.transact_rules(&invalid, ms(150)).is_err());
        assert!(routing.take_recoveries().is_empty());
        assert_eq!(routing.rule_revision(), 1);
        movement(&mut routing, 3, 180, 2000, 2);
        up(&mut routing, 4, 200, 2);
        assert!(!routing.dispatches.is_empty());
        let valid = Transaction {
            revision: 1,
            changes: vec![Change::Enabled { id, enabled: false }],
        };
        assert_eq!(routing.transact_rules(&valid, ms(210)).unwrap(), 2);
        assert_eq!(routing.engine.generation(), 1);
        assert!(
            routing.routes.is_empty() && routing.won.is_empty() && routing.dispatches.is_empty()
        );
        assert!(
            routing
                .take_events()
                .iter()
                .all(|event| !matches!(event, Event::Matched { .. }))
        );
        assert_eq!(routing.take_recoveries().len(), 1);
        assert!(routing.transact_rules(&valid, ms(220)).is_err());
        assert_eq!(routing.rule_revision(), 2);
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
