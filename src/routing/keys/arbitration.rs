use super::*;
use crate::{
    input::{EV_KEY, Frame, InputPosition},
    router::DomainDecision,
    router::{KeyDecision, KeyDisposition, PendingKey},
    rules::{InputEvidence, KeyPress, KeyReservation, TailAtom},
};

const MAX_WAITING: usize = 4096;
const MAX_HELD: usize = 128;

pub(in crate::routing) struct Claim {
    id: u64,
    evidence: InputEvidence,
    pub(in crate::routing) domains: BTreeMap<TailAtom, domains::Binding>,
    routes: BTreeMap<Source, u64>,
    _charge: KeyReservation,
}
impl Claim {
    pub(in crate::routing) fn presses(&self) -> impl Iterator<Item = &KeyPress> {
        self.evidence.iter()
    }
    pub(in crate::routing) fn valid(&self, routing: &Routing, intent: &RouteIntent) -> bool {
        let owned: BTreeSet<_> = self.evidence.iter().collect();
        self.id == intent.id()
            && intent
                .input_evidence()
                .tails
                .atoms()
                .all(|atom| self.domains.contains_key(&atom))
            && intent
                .sources()
                .iter()
                .chain(std::iter::once(intent.source()))
                .all(|source| self.routes.contains_key(source))
            && self.routes.iter().all(|(source, authorization)| {
                routing.routes.get(source).is_some_and(|route| {
                    route.project_keys && route.authorization == *authorization
                })
            })
            && intent
                .key_evidence()
                .iter()
                .all(|press| owned.contains(press))
    }
}
struct Waiting {
    at: Duration,
    deadline: Duration,
    _charge: KeyReservation,
}
pub(in crate::routing) struct Held {
    pub(in crate::routing) outcome: RouteOutcome,
    pub(in crate::routing) charge: KeyReservation,
}
#[derive(Default)]
pub(in crate::routing) struct State {
    pub(in crate::routing) domains: domains::State,
    waiting: BTreeMap<KeyPress, Waiting>,
    pub(in crate::routing) held: Vec<Held>,
    pub(in crate::routing) offsets: BTreeMap<OrderDomain, Duration>,
    pub(in crate::routing) joins: BTreeMap<Source, Duration>,
}
impl State {
    pub(in crate::routing) fn waiting_remove(
        &mut self,
        source: &Source,
        domain: Option<&OrderDomain>,
        at: Duration,
    ) {
        let offset = self
            .waiting
            .iter()
            .filter(|(press, _)| press.source == *source)
            .map(|(_, waiting)| at.min(waiting.deadline).saturating_sub(waiting.at))
            .max()
            .into_iter()
            .chain(self.domains.remove(source, at))
            .max();
        if let Some(offset) = offset
            && let Some(domain) = domain
        {
            self.offsets
                .entry(domain.clone())
                .and_modify(|old| *old = (*old).max(offset))
                .or_insert(offset);
        }
        self.waiting.retain(|press, _| press.source != *source);
        self.joins.remove(source);
        self.held
            .retain(|held| !held.outcome.intent.sources().contains(source));
    }
}
pub(super) fn native(trigger: &Trigger) -> bool {
    match trigger {
        Trigger::Key { .. }
        | Trigger::ButtonTaps { .. }
        | Trigger::ButtonHold { .. }
        | Trigger::Chord { .. } => true,
        Trigger::Sequence { steps, .. } => steps.iter().all(native),
        _ => false,
    }
}
pub(in crate::routing) fn custom_key(trigger: &Trigger) -> bool {
    match trigger {
        Trigger::Sequence { steps, .. } => {
            steps.iter().any(custom_key) && steps.iter().all(projected)
        }
        _ => crate::rules::key_only_custom(trigger),
    }
}
pub(in crate::routing) fn projected(trigger: &Trigger) -> bool {
    native(trigger) || custom_key(trigger)
}
pub(in crate::routing) fn outcome_id(outcome: &RouteOutcome) -> (u64, u64) {
    (
        outcome.intent.id(),
        match outcome.event {
            Event::Matched { ordinal, .. } => ordinal,
            _ => 0,
        },
    )
}
impl Routing {
    pub(in crate::routing) fn projected_key_intent(&self, intent: &RouteIntent) -> bool {
        self.ordered_keys
            && if self.split_inputs {
                domains::supported(&intent.rule.trigger)
            } else {
                projected(&intent.rule.trigger)
            }
            && intent.sources().iter().all(|source| {
                self.routes
                    .get(source)
                    .is_some_and(|route| route.project_keys)
            })
            && self
                .routes
                .get(intent.source())
                .is_some_and(|route| route.project_keys)
    }
    pub(in crate::routing) fn key_claim_valid(&self, claim: &Claim, intent: &RouteIntent) -> bool {
        claim.valid(self, intent)
    }
    pub(in crate::routing) fn reset_key_epochs(&mut self) {
        self.key_journal.offsets.retain(|domain, _| {
            self.routes.values().any(|route| {
                route.order_domain == *domain
                    && route.project_keys
                    && !route.router.key_journal_neutral()
            })
        });
    }
    pub(in crate::routing) fn stage_key_input(
        &mut self,
        source: &Source,
        frame: Frame,
        previous: &[RouteIntent],
        outcomes: &[RouteOutcome],
        now: Duration,
    ) -> io::Result<()> {
        let watermark = self.engine.input_watermark();
        let mut pending = BTreeMap::<usize, (Duration, InputEvidence)>::new();
        let mut pending_domains =
            BTreeMap::<crate::input::TailDomain, (Option<Duration>, InputEvidence)>::new();
        let mut cuts = BTreeSet::new();
        let active = self.engine.route_intents();
        for intent in active.iter().chain(
            outcomes
                .iter()
                .filter(|outcome| {
                    !matches!(
                        outcome.event,
                        Event::Matched {
                            stage: crate::rules::Stage::Cancel,
                            ..
                        }
                    )
                })
                .map(|outcome| &outcome.intent),
        ) {
            if !self.available(intent, now) {
                continue;
            }
            let evidence = self
                .input_evidence(intent, true)
                .map_err(io::Error::other)?;
            if intent.rule.delivery != Delivery::Observe && self.projected_key_intent(intent) {
                let deadline = intent
                    .started()
                    .saturating_add(self.engine.config().timing.decision_window.duration());
                // Past its window an intent only binds report atoms to scopes it may already own.
                let open = (deadline >= frame.at).then_some(deadline);
                for press in evidence.iter().filter(|press| {
                    open.is_some() && press.source == *source && press.position.frame == watermark
                }) {
                    if frame.events.get(press.position.event).is_none_or(|event| {
                        event.kind != EV_KEY || event.value != 1 || event.code != press.code
                    }) {
                        self.recover(source);
                        return Err(fault(Fault::InvalidCandidate));
                    }
                    pending
                        .entry(press.position.event)
                        .and_modify(|(old, _)| *old = (*old).min(deadline))
                        .or_insert((deadline, evidence.clone()));
                }
                if self.split_inputs {
                    for atom in evidence
                        .tails
                        .atoms()
                        .filter(|atom| atom.source == *source && atom.position.frame == watermark)
                    {
                        if let Some((old, previous)) = pending_domains.get_mut(&atom.domain) {
                            *old = match (*old, open) {
                                (Some(old), Some(open)) => Some(old.min(open)),
                                (old, open) => old.or(open),
                            };
                            previous.include(&evidence).map_err(io::Error::other)?;
                        } else {
                            pending_domains.insert(atom.domain, (open, evidence.clone()));
                        }
                    }
                }
            }
        }
        for outcome in outcomes.iter().filter(|outcome| {
            outcome.intent.rule.delivery == Delivery::Observe
                && self.available(&outcome.intent, now)
        }) {
            if let Some(position) = outcome.intent.position()
                && position.frame == watermark
                && let Some(event) = frame.events.get(position.event)
                && event.kind == EV_KEY
                && (outcome.intent.has_key_press(source, event.code, position)
                    || outcome.intent.source() == source
                        && matches!(outcome.intent.rule.trigger, Trigger::Key { key } if key.0 == event.code))
            {
                cuts.insert(position.event);
            }
        }
        let authorization = self.routes[source].authorization;
        for (index, event) in frame
            .events
            .iter()
            .enumerate()
            .filter(|(_, event)| event.kind == EV_KEY && event.value == 2)
        {
            if previous.iter().chain(active.iter()).chain(outcomes.iter().map(|outcome| &outcome.intent))
                .any(|intent| intent.source() == source && intent.rule.delivery == Delivery::Observe
                    && matches!(intent.rule.action, Action::Remap { .. })
                    && matches!(intent.rule.trigger, Trigger::Key { key } if key.0 == event.code)
                    && self.available(intent, now)
                    && !outcomes.iter().any(|outcome| outcome.intent.id() == intent.id()
                        && matches!(outcome.event, Event::Matched { stage: crate::rules::Stage::End, .. })
                        && outcome.intent.position().is_some_and(|position| position.event < index))) {
                cuts.insert(index);
            }
        }
        let declarations: Vec<_> = pending
            .iter()
            .map(|(&event, (deadline, _))| PendingKey {
                authorization,
                event,
                deadline: *deadline,
            })
            .collect();
        for (&event, (deadline, evidence)) in &pending {
            let press = KeyPress {
                source: source.clone(),
                position: InputPosition {
                    frame: watermark,
                    event,
                },
                code: frame.events[event].code,
            };
            if self.key_journal.waiting.len() >= MAX_WAITING {
                self.recover(source);
                return Err(fault(Fault::Overflow));
            }
            let charge = evidence
                .reserve(512 + press.source.seat.capacity())
                .map_err(io::Error::other)?;
            self.key_journal.waiting.insert(
                press,
                Waiting {
                    at: frame.at,
                    deadline: *deadline,
                    _charge: charge,
                },
            );
        }
        let at = frame.at;
        if self.split_inputs {
            if let Err(error) = self.stage_input_domains(
                source,
                frame,
                watermark,
                &declarations,
                pending_domains,
                &cuts,
            ) {
                self.recover(source);
                return Err(error);
            }
            return Ok(());
        }
        if let Err(error) = self.routes.get_mut(source).unwrap().router.stage_key_frame(
            frame,
            watermark,
            &declarations,
            &cuts,
            at,
        ) {
            self.recover(source);
            return Err(fault(error));
        }
        Ok(())
    }
    fn key_claimable(&self, id: u64, evidence: &InputEvidence, now: Duration) -> bool {
        let owned = self.owned_key_presses(id);
        let mut any = false;
        evidence.iter().all(|press| {
            any = true;
            self.key_press_claimable(id, press, now, &owned)
        }) && evidence.tails.atoms().all(|atom| {
            any = true;
            self.split_inputs && self.domain_claimable(id, &atom, now)
        }) && any
    }
    fn owned_key_presses(&self, id: u64) -> BTreeSet<&KeyPress> {
        self.won
            .get(&id)
            .and_then(|won| won.key_claim.as_ref())
            .into_iter()
            .flat_map(|claim| claim.evidence.iter())
            .collect()
    }
    fn key_press_claimable(
        &self,
        id: u64,
        press: &KeyPress,
        now: Duration,
        owned: &BTreeSet<&KeyPress>,
    ) -> bool {
        let Some(route) = self
            .routes
            .get(&press.source)
            .filter(|route| route.project_keys)
        else {
            return false;
        };
        match route.router.key_disposition(press.code, press.position) {
            Some(KeyDisposition::Pending { deadline }) => now <= deadline,
            Some(KeyDisposition::Consumed { owner }) => owner == id,
            None => owned.contains(press),
            _ => false,
        }
    }
    fn settle_keys(
        &mut self,
        mut decisions: BTreeMap<Source, Vec<KeyDecision>>,
        domains: BTreeMap<Source, Vec<DomainDecision>>,
        at: Duration,
        now: Duration,
    ) -> io::Result<()> {
        for source in domains.keys() {
            decisions.entry(source.clone()).or_default();
        }
        for (source, choices) in &decisions {
            let router = &self
                .routes
                .get(source)
                .ok_or_else(|| fault(Fault::StaleSource))?
                .router;
            if self.split_inputs {
                router.validate_domain_decisions(
                    choices,
                    domains.get(source).map_or(&[], Vec::as_slice),
                    at,
                    now,
                )
            } else {
                router.validate_key_decisions(choices, at, now)
            }
            .map_err(fault)?;
        }
        for (source, choices) in &decisions {
            let router = &mut self.routes.get_mut(source).unwrap().router;
            if let Err(error) = if self.split_inputs {
                router.decide_domains_clocked(
                    choices,
                    domains.get(source).map_or(&[], Vec::as_slice),
                    at,
                    now,
                )
            } else {
                router.decide_keys_clocked(choices, at, now)
            } {
                for source in decisions.keys() {
                    self.recover(source);
                }
                return Err(fault(error));
            }
        }
        for (source, choices) in decisions {
            for decision in choices {
                let press = KeyPress {
                    source: source.clone(),
                    code: decision.code,
                    position: decision.position,
                };
                if let Some(waiting) = self.key_journal.waiting.remove(&press) {
                    let offset = now.min(waiting.deadline).saturating_sub(waiting.at);
                    self.key_journal
                        .offsets
                        .entry(self.routes[&source].order_domain.clone())
                        .and_modify(|old| *old = (*old).max(offset))
                        .or_insert(offset);
                }
            }
            if let Some(offset) = self.key_journal.domains.settle(
                &source,
                domains.get(&source).map_or(&[], Vec::as_slice),
                now,
            ) {
                self.key_journal
                    .offsets
                    .entry(self.routes[&source].order_domain.clone())
                    .and_modify(|old| *old = (*old).max(offset))
                    .or_insert(offset);
            }
        }
        Ok(())
    }
    pub(in crate::routing) fn arbitrate_keys(
        &mut self,
        active: &[RouteIntent],
        outcomes: &[RouteOutcome],
        claims: &[RouteIntent],
        at: Duration,
        now: Duration,
    ) -> io::Result<()> {
        self.key_journal.joins.clear();
        let join = self.engine.config().timing.join_window.duration();
        let mut candidates = BTreeMap::new();
        for intent in active.iter().chain(
            outcomes
                .iter()
                .filter(|outcome| {
                    !matches!(
                        outcome.event,
                        Event::Matched {
                            stage: crate::rules::Stage::Cancel,
                            ..
                        }
                    )
                })
                .map(|outcome| &outcome.intent),
        ) {
            if intent.rule.delivery != Delivery::Observe
                && self.projected_key_intent(intent)
                && self.available(intent, now)
            {
                candidates
                    .entry(intent.id())
                    .or_insert_with(|| intent.clone());
            }
        }
        let mut candidates: Vec<_> = candidates.into_values().collect();
        let established = |intent: &RouteIntent| {
            self.won
                .get(&intent.id())
                .is_some_and(|won| won.key_claim.is_some())
                && (intent.rule.trigger.recognizers().next().is_none()
                    || intent.rule.delivery == Delivery::Reserve)
        };
        candidates.sort_by(|a, b| {
            established(b)
                .cmp(&established(a))
                .then_with(|| b.rule.priority.cmp(&a.rule.priority))
                .then_with(|| a.rule.id.cmp(&b.rule.id))
                .then_with(|| a.id().cmp(&b.id()))
        });
        let mut higher = BTreeSet::new();
        let mut higher_domains = BTreeSet::new();
        let mut supported = BTreeSet::new();
        let mut supported_domains = BTreeSet::new();
        for intent in candidates {
            let finished = self
                .routes
                .get(intent.source())
                .is_none_or(|route| route.router.physical_neutral());
            if self.split_inputs && !self.viable(&intent, at, intent.started(), finished) {
                self.engine
                    .cancel_key_candidate(intent.id(), at, "input group no longer viable");
                continue;
            }
            let completed = outcomes
                .iter()
                .find(|outcome| outcome.intent.id() == intent.id());
            let reserve = intent.rule.delivery == Delivery::Reserve
                || intent.rule.trigger.recognizers().next().is_none()
                    && self.won.contains_key(&intent.id());
            if completed.is_none()
                && !reserve
                && now
                    >= intent
                        .started()
                        .saturating_add(self.engine.config().timing.decision_window.duration())
            {
                self.engine
                    .cancel_key_candidate(intent.id(), at, "key decision deadline reached");
                continue;
            }
            let mut observed = self
                .input_evidence(&intent, true)
                .map_err(io::Error::other)?;
            if intent.rule.trigger.recognizers().next().is_some() && !reserve {
                if let Some(completed) = completed {
                    observed = completed.intent.input_evidence();
                    for outcome in outcomes.iter().filter(|outcome| {
                        outcome.intent.id() == intent.id()
                            && !matches!(
                                outcome.event,
                                Event::Matched {
                                    stage: crate::rules::Stage::Cancel,
                                    ..
                                }
                            )
                    }) {
                        observed
                            .include(&outcome.intent.input_evidence())
                            .map_err(io::Error::other)?;
                    }
                } else {
                    let owned = self.owned_key_presses(intent.id());
                    observed.keys = observed
                        .keys
                        .select(|press| self.key_press_claimable(intent.id(), press, now, &owned))
                        .map_err(io::Error::other)?;
                    observed.tails = observed
                        .tails
                        .select(|atom| self.domain_claimable(intent.id(), atom, now))
                        .map_err(io::Error::other)?;
                }
            }
            if !self.key_claimable(intent.id(), &observed, now) {
                self.engine.cancel_key_candidate(
                    intent.id(),
                    at,
                    "original key input is no longer claimable",
                );
                continue;
            }
            supported.extend(observed.iter().cloned());
            let scopes: BTreeSet<_> = observed
                .tails
                .atoms()
                .filter_map(|atom| self.domain_scope(intent.id(), &atom).cloned())
                .collect();
            supported_domains.extend(scopes.iter().cloned());
            let native_claim = claims.iter().any(|claim| claim.id() == intent.id());
            if self.split_inputs
                && !native_claim
                && matches!(
                    intent.rule.trigger,
                    Trigger::Swipe { .. } | Trigger::TouchTaps { .. }
                )
                && claims.iter().any(|claim| {
                    claim
                        .sources()
                        .iter()
                        .any(|source| intent.sources().contains(source))
                })
            {
                continue;
            }
            if observed.iter().any(|press| higher.contains(press))
                || scopes.iter().any(|scope| higher_domains.contains(scope))
            {
                continue;
            }
            let claimed = completed.is_some()
                || self.split_inputs
                    && (native_claim
                        || matches!(intent.rule.trigger, Trigger::TouchTaps { fingers, .. }
                            if fingers >= 2 && intent.contacts() == usize::from(fingers)));
            let joining = self.split_inputs
                && intent.rule.trigger.contains_touch()
                && !finished
                && at < intent.started().saturating_add(join);
            if !reserve && (!claimed || joining) {
                if claimed {
                    let wake = intent.started().saturating_add(join);
                    self.key_journal
                        .joins
                        .entry(intent.source().clone())
                        .and_modify(|old| *old = (*old).min(wake))
                        .or_insert(wake);
                }
                higher.extend(observed.iter().cloned());
                higher_domains.extend(scopes);
                continue;
            }
            let mut evidence = match completed {
                Some(completed)
                    if !reserve && intent.rule.trigger.recognizers().next().is_none() =>
                {
                    completed.intent.input_evidence()
                }
                _ => observed,
            };
            if let Some(owned) = self
                .won
                .get(&intent.id())
                .and_then(|won| won.key_claim.as_ref())
            {
                evidence
                    .include(&owned.evidence)
                    .map_err(io::Error::other)?;
            }
            if !self.key_claimable(intent.id(), &evidence, now) {
                continue;
            }
            let mut decisions: BTreeMap<Source, Vec<KeyDecision>> = BTreeMap::new();
            let mut domain_decisions: BTreeMap<Source, Vec<DomainDecision>> = BTreeMap::new();
            let mut domain_receipt = BTreeMap::new();
            let mut routes = BTreeMap::new();
            for source in intent
                .sources()
                .iter()
                .chain(std::iter::once(intent.source()))
            {
                routes.insert(source.clone(), self.routes[source].authorization);
            }
            for press in evidence.iter() {
                let route = &self.routes[&press.source];
                routes.insert(press.source.clone(), route.authorization);
                if route
                    .router
                    .key_disposition(press.code, press.position)
                    .is_some()
                {
                    decisions
                        .entry(press.source.clone())
                        .or_default()
                        .push(KeyDecision {
                            authorization: route.authorization,
                            code: press.code,
                            position: press.position,
                            owner: Some(intent.id()),
                        });
                }
            }
            for atom in evidence.tails.atoms() {
                let owned = self
                    .won
                    .get(&intent.id())
                    .and_then(|won| won.key_claim.as_ref())
                    .map(|claim| &claim.domains);
                let binding = self
                    .key_journal
                    .domains
                    .binding(&atom, owned)
                    .ok_or_else(|| fault(Fault::InvalidCandidate))?
                    .clone();
                let scope = &binding.scope;
                routes.insert(scope.source.clone(), scope.authorization);
                if self.routes[&scope.source]
                    .router
                    .domain_disposition(scope.root)
                    .is_some()
                {
                    let choices = domain_decisions.entry(scope.source.clone()).or_default();
                    if !choices.iter().any(|choice| choice.scope == scope.root) {
                        choices.push(DomainDecision {
                            authorization: scope.authorization,
                            scope: scope.root,
                            owner: Some(intent.id()),
                        });
                    }
                }
                higher_domains.insert(scope.clone());
                domain_receipt.insert(atom, binding);
            }
            let charge = evidence
                .reserve(
                    1024 + routes
                        .keys()
                        .map(|source| source.seat.capacity() + 256)
                        .sum::<usize>()
                        + domain_receipt
                            .keys()
                            .map(|atom| 512 + atom.source.seat.capacity())
                            .sum::<usize>(),
                )
                .map_err(io::Error::other)?;
            self.settle_keys(decisions, domain_decisions, at, now)?;
            higher.extend(evidence.iter().cloned());
            let id = intent.id();
            self.won.insert(
                id,
                Won {
                    intent,
                    emitted: false,
                    chain: None,
                    key_claim: Some(Arc::new(Claim {
                        id,
                        evidence,
                        domains: domain_receipt,
                        routes,
                        _charge: charge,
                    })),
                },
            );
        }
        let mut ordinary: BTreeMap<Source, Vec<KeyDecision>> = BTreeMap::new();
        for (press, waiting) in &self.key_journal.waiting {
            if (!supported.contains(press) || at >= waiting.deadline)
                && let Some(route) = self.routes.get(&press.source)
                && matches!(
                    route.router.key_disposition(press.code, press.position),
                    Some(KeyDisposition::Pending { .. })
                )
            {
                ordinary
                    .entry(press.source.clone())
                    .or_default()
                    .push(KeyDecision {
                        authorization: route.authorization,
                        code: press.code,
                        position: press.position,
                        owner: None,
                    });
            }
        }
        let ordinary_domains =
            self.key_journal
                .domains
                .ordinary(&self.routes, &supported_domains, at);
        self.settle_keys(ordinary, ordinary_domains, at, now)?;
        self.key_journal.waiting.retain(|press, _| {
            self.routes.get(&press.source).is_some_and(|route| {
                matches!(
                    route.router.key_disposition(press.code, press.position),
                    Some(KeyDisposition::Pending { .. })
                )
            })
        });
        let retained = active
            .iter()
            .chain(outcomes.iter().map(|outcome| &outcome.intent))
            .flat_map(|intent| intent.input_evidence().tails.atoms().collect::<Vec<_>>())
            .collect();
        self.key_journal.domains.prune(&self.routes, &retained);
        self.refresh_key_barriers()
    }
    pub(in crate::routing) fn refresh_key_barriers(&mut self) -> io::Result<()> {
        let mut barriers: BTreeMap<OrderDomain, InputPosition> = BTreeMap::new();
        for press in self.key_journal.waiting.keys() {
            let Some(route) = self.routes.get(&press.source) else {
                continue;
            };
            barriers
                .entry(route.order_domain.clone())
                .and_modify(|old| *old = (*old).min(press.position))
                .or_insert(press.position);
        }
        for (source, position) in self.key_journal.domains.barriers() {
            let Some(route) = self.routes.get(source) else {
                continue;
            };
            barriers
                .entry(route.order_domain.clone())
                .and_modify(|old| *old = (*old).min(position))
                .or_insert(position);
        }
        for route in self.routes.values_mut().filter(|route| route.project_keys) {
            route
                .router
                .key_journal_barrier(
                    barriers.get(&route.order_domain).copied(),
                    self.key_journal
                        .offsets
                        .get(&route.order_domain)
                        .copied()
                        .unwrap_or_default(),
                )
                .map_err(fault)?;
        }
        Ok(())
    }
    pub(in crate::routing) fn hold_key_outcome(
        &mut self,
        outcome: RouteOutcome,
        now: Duration,
        charge: Option<KeyReservation>,
    ) -> io::Result<()> {
        if !self.key_claimable(outcome.intent.id(), &outcome.intent.input_evidence(), now) {
            return Ok(());
        }
        if now
            >= outcome
                .intent
                .started()
                .saturating_add(self.engine.config().timing.decision_window.duration())
        {
            return Ok(());
        }
        if self.key_journal.held.len() >= MAX_HELD {
            return Err(fault(Fault::Overflow));
        }
        let bytes =
            crate::recognition::encoded_size(&outcome.event, crate::recognition::MAX_BYTES)?
                + crate::recognition::encoded_size(
                    &outcome.intent.rule,
                    crate::recognition::MAX_BYTES,
                )?;
        let charge = match charge {
            Some(charge) => charge,
            None => outcome
                .intent
                .key_evidence()
                .reserve(bytes.saturating_mul(16) + 4096)
                .map_err(io::Error::other)?,
        };
        self.key_journal.held.push(Held { outcome, charge });
        Ok(())
    }
}
