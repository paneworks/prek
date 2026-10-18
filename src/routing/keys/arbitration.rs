use super::*;
use crate::{
    input::{EV_KEY, Frame, InputPosition},
    router::{KeyDecision, KeyDisposition, PendingKey},
    rules::{KeyEvidence, KeyPress, KeyReservation},
};

const MAX_WAITING: usize = 4096;
const MAX_HELD: usize = 128;

pub(in crate::routing) struct Claim {
    id: u64,
    evidence: KeyEvidence,
    routes: BTreeMap<Source, u64>,
}
impl Claim {
    pub(in crate::routing) fn presses(&self) -> impl Iterator<Item = &KeyPress> {
        self.evidence.iter()
    }
    pub(in crate::routing) fn valid(&self, routing: &Routing, intent: &RouteIntent) -> bool {
        let owned: BTreeSet<_> = self.evidence.iter().collect();
        self.id == intent.id()
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
    waiting: BTreeMap<KeyPress, Waiting>,
    pub(in crate::routing) held: Vec<Held>,
    offsets: BTreeMap<String, Duration>,
}
impl State {
    pub(in crate::routing) fn waiting_remove(&mut self, source: &Source, at: Duration) {
        let offset = self
            .waiting
            .iter()
            .filter(|(press, _)| press.source == *source)
            .map(|(_, waiting)| at.min(waiting.deadline).saturating_sub(waiting.at))
            .max();
        if let Some(offset) = offset {
            self.offsets
                .entry(source.seat.clone())
                .and_modify(|old| *old = (*old).max(offset))
                .or_insert(offset);
        }
        self.waiting.retain(|press, _| press.source != *source);
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
            && projected(&intent.rule.trigger)
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
        self.key_journal.offsets.retain(|seat, _| {
            self.routes.iter().any(|(source, route)| {
                source.seat == *seat && route.project_keys && !route.router.key_journal_neutral()
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
        let mut pending = BTreeMap::<usize, (Duration, KeyEvidence)>::new();
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
                .engine
                .pending_key_evidence(intent)
                .map_err(io::Error::other)?;
            if intent.rule.delivery != Delivery::Observe && self.projected_key_intent(intent) {
                let deadline = intent
                    .started()
                    .saturating_add(self.engine.config().timing.decision_window.duration());
                if deadline < frame.at {
                    continue;
                }
                for press in evidence
                    .iter()
                    .filter(|press| press.source == *source && press.position.frame == watermark)
                {
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
    fn key_claimable(&self, id: u64, evidence: &KeyEvidence, now: Duration) -> bool {
        let owned = self.owned_key_presses(id);
        let mut any = false;
        evidence.iter().all(|press| {
            any = true;
            self.key_press_claimable(id, press, now, &owned)
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
        decisions: BTreeMap<Source, Vec<KeyDecision>>,
        at: Duration,
        now: Duration,
    ) -> io::Result<()> {
        for (source, choices) in &decisions {
            self.routes
                .get(source)
                .ok_or_else(|| fault(Fault::StaleSource))?
                .router
                .validate_key_decisions(choices, at, now)
                .map_err(fault)?;
        }
        for (source, choices) in &decisions {
            if let Err(error) = self
                .routes
                .get_mut(source)
                .unwrap()
                .router
                .decide_keys_clocked(choices, at, now)
            {
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
                        .entry(source.seat.clone())
                        .and_modify(|old| *old = (*old).max(offset))
                        .or_insert(offset);
                }
            }
        }
        Ok(())
    }
    pub(in crate::routing) fn arbitrate_keys(
        &mut self,
        active: &[RouteIntent],
        outcomes: &[RouteOutcome],
        at: Duration,
        now: Duration,
    ) -> io::Result<()> {
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
                && (!custom_key(&intent.rule.trigger) || intent.rule.delivery == Delivery::Reserve)
        };
        candidates.sort_by(|a, b| {
            established(b)
                .cmp(&established(a))
                .then_with(|| b.rule.priority.cmp(&a.rule.priority))
                .then_with(|| a.rule.id.cmp(&b.rule.id))
                .then_with(|| a.id().cmp(&b.id()))
        });
        let mut higher = BTreeSet::new();
        let mut supported = BTreeSet::new();
        for intent in candidates {
            let completed = outcomes
                .iter()
                .find(|outcome| outcome.intent.id() == intent.id());
            let reserve = intent.rule.delivery == Delivery::Reserve
                || !custom_key(&intent.rule.trigger) && self.won.contains_key(&intent.id());
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
                .engine
                .pending_key_evidence(&intent)
                .map_err(io::Error::other)?;
            if custom_key(&intent.rule.trigger) && !reserve {
                if let Some(completed) = completed {
                    observed = completed.intent.key_evidence();
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
                            .include(&outcome.intent.key_evidence())
                            .map_err(io::Error::other)?;
                    }
                } else {
                    let owned = self.owned_key_presses(intent.id());
                    observed = observed
                        .select(|press| self.key_press_claimable(intent.id(), press, now, &owned))
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
            if observed
                .iter()
                .any(|press| higher.contains(&press.position))
            {
                continue;
            }
            if completed.is_none() && !reserve {
                higher.extend(observed.iter().map(|press| press.position));
                continue;
            }
            let mut evidence = if reserve || custom_key(&intent.rule.trigger) {
                observed
            } else {
                completed.unwrap().intent.key_evidence()
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
            self.settle_keys(decisions, at, now)?;
            higher.extend(evidence.iter().map(|press| press.position));
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
                        routes,
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
        self.settle_keys(ordinary, at, now)?;
        self.key_journal.waiting.retain(|press, _| {
            self.routes.get(&press.source).is_some_and(|route| {
                matches!(
                    route.router.key_disposition(press.code, press.position),
                    Some(KeyDisposition::Pending { .. })
                )
            })
        });
        self.refresh_key_barriers()
    }
    pub(in crate::routing) fn refresh_key_barriers(&mut self) -> io::Result<()> {
        let mut barriers: BTreeMap<&str, InputPosition> = BTreeMap::new();
        for press in self.key_journal.waiting.keys() {
            barriers
                .entry(&press.source.seat)
                .and_modify(|old| *old = (*old).min(press.position))
                .or_insert(press.position);
        }
        for (source, route) in self
            .routes
            .iter_mut()
            .filter(|(_, route)| route.project_keys)
        {
            route
                .router
                .key_journal_barrier(
                    barriers.get(source.seat.as_str()).copied(),
                    self.key_journal
                        .offsets
                        .get(&source.seat)
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
        if !self.key_claimable(outcome.intent.id(), &outcome.intent.key_evidence(), now) {
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
