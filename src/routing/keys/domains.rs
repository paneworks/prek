use super::*;
use crate::{
    input::{InputPosition, TailDomain},
    router::{DomainDecision, DomainRoot, KeyDisposition, PendingDomain},
    rules::{InputEvidence, KeyReservation, TailAtom},
};

const MAX_BINDINGS: usize = 4096;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(in crate::routing) struct Scope {
    pub(in crate::routing) source: Source,
    pub(in crate::routing) authorization: u64,
    pub(in crate::routing) root: DomainRoot,
}
#[derive(Clone)]
pub(in crate::routing) struct Binding {
    pub(in crate::routing) scope: Scope,
    _charge: Arc<KeyReservation>,
}
struct Waiting {
    at: Duration,
    deadline: Duration,
}
#[derive(Default)]
pub(in crate::routing) struct State {
    bindings: BTreeMap<TailAtom, Binding>,
    waiting: BTreeMap<Scope, Waiting>,
}
impl State {
    pub(in crate::routing) fn remove(&mut self, source: &Source, at: Duration) -> Option<Duration> {
        self.bindings.retain(|atom, _| atom.source != *source);
        let deadline = self
            .waiting
            .iter()
            .filter(|(scope, _)| scope.source == *source)
            .map(|(_, waiting)| at.min(waiting.deadline).saturating_sub(waiting.at))
            .max();
        self.waiting.retain(|scope, _| scope.source != *source);
        deadline
    }
    pub(in crate::routing) fn binding<'a>(
        &'a self,
        atom: &TailAtom,
        owned: Option<&'a BTreeMap<TailAtom, Binding>>,
    ) -> Option<&'a Binding> {
        self.bindings.get(atom).or_else(|| owned?.get(atom))
    }
    pub(in crate::routing) fn disposition(
        &self,
        routes: &BTreeMap<Source, Route>,
        atom: &TailAtom,
        owned: Option<&BTreeMap<TailAtom, Binding>>,
        id: u64,
    ) -> Option<KeyDisposition> {
        let binding = self.binding(atom, owned)?;
        let route = routes.get(&binding.scope.source)?;
        if !route.project_keys || route.authorization != binding.scope.authorization {
            return None;
        }
        route
            .router
            .domain_disposition(binding.scope.root)
            .or_else(|| {
                owned?
                    .get(atom)
                    .filter(|receipt| receipt.scope == binding.scope)
                    .map(|_| KeyDisposition::Consumed { owner: id })
            })
    }
    pub(in crate::routing) fn prune(
        &mut self,
        routes: &BTreeMap<Source, Route>,
        retained: &BTreeSet<TailAtom>,
    ) {
        self.bindings.retain(|atom, binding| {
            retained.contains(atom)
                || routes.get(&atom.source).is_some_and(|route| {
                    route.authorization == binding.scope.authorization
                        && route
                            .router
                            .domain_disposition(binding.scope.root)
                            .is_some()
                })
        });
        self.waiting.retain(|scope, _| {
            routes.get(&scope.source).is_some_and(|route| {
                matches!(
                    route.router.domain_disposition(scope.root),
                    Some(KeyDisposition::Pending { .. })
                )
            })
        });
    }
    pub(in crate::routing) fn barriers(&self) -> impl Iterator<Item = (&str, InputPosition)> {
        self.waiting
            .keys()
            .map(|scope| (scope.source.seat.as_str(), scope.root.position))
    }
    pub(in crate::routing) fn settle(
        &mut self,
        source: &Source,
        choices: &[DomainDecision],
        now: Duration,
    ) -> Option<Duration> {
        choices
            .iter()
            .filter_map(|choice| {
                self.waiting.remove(&Scope {
                    source: source.clone(),
                    authorization: choice.authorization,
                    root: choice.scope,
                })
            })
            .map(|waiting| now.min(waiting.deadline).saturating_sub(waiting.at))
            .max()
    }
    pub(in crate::routing) fn ordinary(
        &self,
        routes: &BTreeMap<Source, Route>,
        supported: &BTreeSet<Scope>,
        at: Duration,
    ) -> BTreeMap<Source, Vec<DomainDecision>> {
        let mut result: BTreeMap<Source, Vec<DomainDecision>> = BTreeMap::new();
        for (scope, waiting) in &self.waiting {
            if (!supported.contains(scope) || at >= waiting.deadline)
                && routes.get(&scope.source).is_some_and(|route| {
                    matches!(
                        route.router.domain_disposition(scope.root),
                        Some(KeyDisposition::Pending { .. })
                    )
                })
            {
                result
                    .entry(scope.source.clone())
                    .or_default()
                    .push(DomainDecision {
                        authorization: scope.authorization,
                        scope: scope.root,
                        owner: None,
                    });
            }
        }
        result
    }
}

pub(in crate::routing) fn supported(trigger: &Trigger) -> bool {
    match trigger {
        Trigger::Custom { recognizer, .. } => !recognizer
            .inputs
            .contains(&crate::recognition::InputKind::Switch),
        Trigger::Sequence { steps, .. } => steps.iter().all(supported),
        Trigger::Key { key }
        | Trigger::ButtonTaps { key, .. }
        | Trigger::ButtonHold { key, .. } => crate::input::independent_key(key.0),
        Trigger::Chord { keys, .. } => keys.iter().all(|key| crate::input::independent_key(key.0)),
        Trigger::Shape { button, .. } => {
            button.is_none_or(|key| crate::input::independent_key(key.0))
        }
        _ => true,
    }
}

impl Routing {
    pub(in crate::routing) fn input_evidence(
        &self,
        intent: &RouteIntent,
        pending: bool,
    ) -> Result<InputEvidence, &'static str> {
        if self.split_inputs && pending {
            self.engine.pending_input_evidence(intent)
        } else {
            let mut evidence = intent.input_evidence();
            if pending {
                evidence.keys = self.engine.pending_key_evidence(intent)?;
            }
            Ok(evidence)
        }
    }
    pub(in crate::routing) fn domain_claimable(
        &self,
        id: u64,
        atom: &TailAtom,
        now: Duration,
    ) -> bool {
        let owned = self
            .won
            .get(&id)
            .and_then(|won| won.key_claim.as_ref())
            .map(|claim| &claim.domains);
        match self
            .key_journal
            .domains
            .disposition(&self.routes, atom, owned, id)
        {
            Some(KeyDisposition::Pending { deadline }) => now <= deadline,
            Some(KeyDisposition::Consumed { owner }) => owner == id,
            _ => false,
        }
    }
    pub(in crate::routing) fn domain_scope(&self, id: u64, atom: &TailAtom) -> Option<&Scope> {
        self.key_journal
            .domains
            .binding(
                atom,
                self.won
                    .get(&id)
                    .and_then(|won| won.key_claim.as_ref())
                    .map(|claim| &claim.domains),
            )
            .map(|binding| &binding.scope)
    }
    pub(in crate::routing) fn stage_input_domains(
        &mut self,
        source: &Source,
        frame: crate::input::Frame,
        watermark: u64,
        keys: &[crate::router::PendingKey],
        pending: BTreeMap<TailDomain, (Duration, InputEvidence)>,
        cuts: &BTreeSet<usize>,
    ) -> io::Result<()> {
        let at = frame.at;
        let position = InputPosition {
            frame: watermark,
            event: frame.events.len() - 1,
        };
        let route = &self.routes[source];
        let mut declarations = Vec::new();
        for (&domain, (deadline, evidence)) in &pending {
            let current = route.router.domain_scope(domain);
            let live = current.filter(|root| route.router.domain_scope_live(*root));
            let continuation = live
                .filter(|root| {
                    matches!(
                        route.router.domain_disposition(*root),
                        Some(KeyDisposition::Pending { .. })
                    )
                })
                .filter(|root| {
                    evidence.tails.atoms().any(|atom| {
                        atom.source == *source
                            && atom.domain == domain
                            && self
                                .domain_scope(0, &atom)
                                .is_some_and(|scope| scope.root == *root)
                    })
                });
            if live.is_some() && continuation.is_none() {
                continue;
            }
            let bound = continuation
                .and_then(|root| match route.router.domain_disposition(root) {
                    Some(KeyDisposition::Pending { deadline }) => Some(deadline),
                    _ => None,
                })
                .unwrap_or(*deadline);
            declarations.push(PendingDomain {
                authorization: route.authorization,
                domain,
                deadline: bound,
                continuation,
            });
        }
        let authorization = route.authorization;
        let mut charges = BTreeMap::new();
        for (&domain, (_, evidence)) in &pending {
            let atom = TailAtom {
                source: source.clone(),
                position,
                domain,
            };
            if evidence.tails.atoms().any(|actual| actual == atom) {
                charges.insert(
                    domain,
                    evidence
                        .keys
                        .reserve(1024 + 2 * source.seat.capacity())
                        .map_err(io::Error::other)?,
                );
            }
        }
        if self
            .key_journal
            .domains
            .bindings
            .len()
            .saturating_add(charges.len())
            > MAX_BINDINGS
        {
            return Err(fault(Fault::Overflow));
        }
        let roots = self
            .routes
            .get_mut(source)
            .unwrap()
            .router
            .stage_ordered_domain_frame(frame, watermark, keys, &declarations, cuts, at)
            .map_err(fault)?;
        for root in roots {
            let Some((_, evidence)) = pending.get(&root.domain) else {
                continue;
            };
            let atom = TailAtom {
                source: source.clone(),
                position,
                domain: root.domain,
            };
            if !evidence.tails.atoms().any(|actual| actual == atom) {
                continue;
            }
            let charge = charges
                .remove(&root.domain)
                .ok_or_else(|| fault(Fault::InvalidCandidate))?;
            let scope = Scope {
                source: source.clone(),
                authorization,
                root,
            };
            if let Some(KeyDisposition::Pending { deadline }) =
                self.routes[source].router.domain_disposition(root)
            {
                self.key_journal
                    .domains
                    .waiting
                    .entry(scope.clone())
                    .or_insert(Waiting { at, deadline });
            }
            self.key_journal.domains.bindings.insert(
                atom,
                Binding {
                    scope,
                    _charge: Arc::new(charge),
                },
            );
        }
        Ok(())
    }
}
