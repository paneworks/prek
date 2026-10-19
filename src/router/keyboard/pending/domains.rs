use super::*;
use crate::input::TailDomain;

/// Original cohort root within one independent non-key family.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct DomainRoot {
    pub domain: TailDomain,
    pub position: InputPosition,
}

#[derive(Clone, Copy, Debug)]
pub struct PendingDomain {
    pub authorization: u64,
    pub domain: TailDomain,
    pub deadline: Duration,
    pub continuation: Option<DomainRoot>,
}

#[derive(Clone, Copy, Debug)]
pub struct DomainDecision {
    pub authorization: u64,
    pub scope: DomainRoot,
    pub owner: Option<u64>,
}

pub(super) const STORAGE_PER_ROOT: usize = 32;

#[derive(Default)]
pub(super) struct Domains {
    scopes: BTreeMap<TailDomain, tails::Tails>,
    switches: BTreeSet<u16>,
    /// Coupled scalar absolute roots mapped to the contact root whose fate they share.
    followers: BTreeMap<InputPosition, InputPosition>,
}
pub(super) struct Plan {
    plans: Vec<(TailDomain, tails::Plan)>,
    pub(super) bindings: Vec<DomainRoot>,
    pub(super) inputs: Vec<DomainRoot>,
    switches: BTreeSet<u16>,
    followers: Vec<(InputPosition, InputPosition)>,
}
impl Plan {
    pub(super) fn transformed(&self) -> bool {
        self.plans.iter().any(|(_, plan)| plan.transformed())
    }
}
impl Domains {
    pub(super) fn absolute_enabled(&self) -> bool {
        [TailDomain::Absolute, TailDomain::Touch]
            .iter()
            .any(|domain| self.scopes.get(domain).is_some_and(tails::Tails::enabled))
    }
    pub(super) fn neutral(&self) -> bool {
        self.scopes
            .values()
            .all(|scopes| scopes.current().is_none())
    }
    pub(super) fn current(&self, domain: TailDomain) -> Option<InputPosition> {
        self.scopes.get(&domain)?.current()
    }
    fn leader(&self, root: DomainRoot) -> Option<InputPosition> {
        (root.domain == TailDomain::Absolute)
            .then(|| self.followers.get(&root.position).copied())
            .flatten()
    }
    pub(super) fn choice(&self, root: DomainRoot) -> Option<Choice> {
        let own = self.scopes.get(&root.domain)?.choice(root.position)?;
        match self.leader(root) {
            Some(leader) => self.scopes.get(&TailDomain::Touch)?.choice(leader),
            None => Some(own),
        }
    }
    pub(super) fn live(&self, root: DomainRoot) -> bool {
        self.scopes
            .get(&root.domain)
            .is_some_and(|scopes| scopes.live(root.position))
    }
    pub(super) fn deadline(&self) -> Option<Duration> {
        self.scopes
            .values()
            .filter_map(tails::Tails::deadline)
            .min()
    }
    pub(super) fn prepare(
        &self,
        frame: &Frame,
        watermark: u64,
        pending: &[PendingDomain],
        flags: &super::TailFrame,
        offset: Duration,
    ) -> Result<Plan, Fault> {
        if pending.len() > TailDomain::ALL.len() {
            return Err(Fault::InvalidCandidate);
        }
        let mut admissions = BTreeMap::new();
        for admission in pending {
            if matches!(admission.domain, TailDomain::Switch | TailDomain::Opaque) {
                return Err(Fault::UnsupportedProjection);
            }
            if admission
                .continuation
                .is_some_and(|root| root.domain != admission.domain)
            {
                return Err(Fault::InvalidCandidate);
            }
            let pending = PendingTail {
                authorization: admission.authorization,
                deadline: admission.deadline,
                continuation: admission.continuation.map(|root| root.position),
            };
            if admissions.insert(admission.domain, pending).is_some() {
                return Err(Fault::InvalidCandidate);
            }
        }
        let present: BTreeSet<_> = (0..frame.events.len())
            .filter_map(|index| TailDomain::at(&frame.events, index))
            .collect();
        let mut switches = self.switches.clone();
        for event in frame.events.iter().filter(|event| event.kind == EV_SW) {
            match event.value {
                1 => {
                    switches.insert(event.code);
                }
                0 => {
                    switches.remove(&event.code);
                }
                _ => return Err(Fault::InvalidFrame),
            }
        }
        let switch_active = !switches.is_empty();
        let mut plan = Plan {
            plans: Vec::new(),
            bindings: Vec::new(),
            inputs: Vec::new(),
            switches,
            followers: Vec::new(),
        };
        let deferred = flags.coupled.then_some(TailDomain::Absolute);
        let mut leader = None;
        for domain in TailDomain::ALL
            .into_iter()
            .filter(|domain| Some(*domain) != deferred)
            .chain(deferred)
        {
            let follows = deferred == Some(domain) && leader.is_some();
            let scopes = self.scopes.get(&domain);
            let empty = tails::Tails::default();
            let position = present.contains(&domain).then_some(InputPosition {
                frame: watermark,
                event: frame.events.len() - 1,
            });
            let next = scopes.unwrap_or(&empty).prepare_retained(
                position,
                admissions.get(&domain).copied().filter(|_| !follows),
                domain == TailDomain::Touch && flags.active
                    || domain == TailDomain::Switch && switch_active,
                frame.at,
                offset,
                flags.preserve_pending,
            )?;
            if let Some(position) = next.input() {
                let root = DomainRoot { domain, position };
                plan.bindings.push(root);
                if present.contains(&domain) {
                    plan.inputs.push(root);
                }
                if domain == TailDomain::Touch {
                    leader = Some(position);
                }
                if let Some(leader) = leader.filter(|_| follows) {
                    plan.followers.push((position, leader));
                }
            }
            plan.plans.push((domain, next));
        }
        Ok(plan)
    }
    pub(super) fn commit(&mut self, plan: Plan) {
        self.switches = plan.switches;
        for (domain, next) in plan.plans {
            self.scopes.entry(domain).or_default().commit(next);
        }
        self.followers.extend(plan.followers);
    }
    pub(super) fn validate(&self, decisions: &[DomainDecision], at: Duration) -> Result<(), Fault> {
        let mut seen = BTreeSet::new();
        for decision in decisions {
            if !seen.insert(decision.scope) || decision.owner == Some(0) {
                return Err(Fault::InvalidCandidate);
            }
            self.choice(decision.scope)
                .ok_or(Fault::TooLate)?
                .validate(decision.owner, at)?;
        }
        Ok(())
    }
    pub(super) fn settle(&mut self, decisions: &[DomainDecision], at: Duration) {
        for decision in decisions {
            if self.leader(decision.scope).is_some() {
                continue;
            }
            self.scopes.get_mut(&decision.scope.domain).unwrap().settle(
                &[TailDecision {
                    authorization: decision.authorization,
                    position: decision.scope.position,
                    owner: decision.owner,
                }],
                at,
            );
        }
    }
    pub(super) fn expire(&mut self, at: Duration) {
        for scopes in self.scopes.values_mut() {
            scopes.expire(at);
        }
    }
    pub(super) fn released(&mut self, bindings: &[DomainRoot]) {
        for root in bindings {
            self.scopes
                .get_mut(&root.domain)
                .unwrap()
                .released(Some(root.position));
        }
    }
    pub(super) fn prune(&mut self) {
        for scopes in self.scopes.values_mut() {
            scopes.prune();
        }
        let absolute = self.scopes.get(&TailDomain::Absolute);
        self.followers
            .retain(|root, _| absolute.is_some_and(|scopes| scopes.choice(*root).is_some()));
    }
}
