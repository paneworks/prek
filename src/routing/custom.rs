//! Native custom-leaf bindings to complete retained parent scopes.
use super::*;
use crate::rules::RecognitionOwnership;

pub(super) struct BoundCustom {
    pub(super) owner: u64,
    pub(super) tokens: BTreeMap<Source, ChainToken>,
}

pub(super) struct CustomRoutes<'a> {
    pub(super) domains: &'a keys::domains::State,
    pub(super) split_inputs: bool,
    pub(super) routes: &'a BTreeMap<Source, Route>,
    pub(super) chains: &'a BTreeMap<u64, BTreeMap<Source, ChainToken>>,
    pub(super) groups: &'a BTreeMap<u64, (Source, u64, u64)>,
    pub(super) won: &'a BTreeMap<u64, Won>,
    pub(super) bindings: &'a mut BTreeMap<u64, BoundCustom>,
    pub(super) at: Duration,
    pub(super) now: Duration,
}
impl CustomRoutes<'_> {
    pub(super) fn ownership(
        &mut self,
        leaf: &RouteIntent,
        owner: &RouteIntent,
        admit: bool,
    ) -> RecognitionOwnership {
        let refused = || RecognitionOwnership {
            ownership: Some(Ownership::Forwarded),
            winner: None,
            deadline: None,
            skip_offer: false,
        };
        if owner.rule.id != leaf.rule.id
            || owner.rule.delivery != leaf.rule.delivery
            || !matches!(leaf.rule.trigger, Trigger::Custom { .. })
        {
            return refused();
        }
        if owner.rule.delivery == Delivery::Observe {
            return RecognitionOwnership::default();
        }
        let sequence = matches!(owner.rule.trigger, Trigger::Sequence { .. });
        if !sequence && owner.id() != leaf.id() {
            return refused();
        }
        if self
            .routes
            .get(leaf.source())
            .is_some_and(|route| route.project_keys)
            && if self.split_inputs {
                keys::domains::supported(&owner.rule.trigger)
            } else {
                keys::arbitration::projected(&owner.rule.trigger)
            }
        {
            if sequence && !owner.sources().contains(leaf.source()) {
                return refused();
            }
            return self.key_ownership(leaf, owner);
        }
        if !matches!(owner.rule.trigger, Trigger::Sequence { .. }) {
            let (ownership, winner) = custom_ownership(
                self.routes,
                self.groups,
                leaf.source(),
                &leaf.rule,
                leaf.id(),
            );
            return RecognitionOwnership {
                ownership,
                winner,
                deadline: None,
                skip_offer: false,
            };
        }
        let Some(scope) = self.chains.get(&owner.id()) else {
            return refused();
        };
        if !owner.sources().contains(leaf.source())
            || scope.len() != owner.sources().len()
            || !owner
                .sources()
                .iter()
                .all(|source| scope.contains_key(source))
        {
            return refused();
        }
        let mut deadline = None;
        for (source, token) in scope {
            let Some(route) = self.routes.get(source) else {
                return refused();
            };
            let valid = match route.router.chain_ownership(token, self.at) {
                Some(Ownership::Pending) => route.winner.is_none(),
                Some(Ownership::Claimed) => route.winner == Some(owner.id()),
                _ => false,
            };
            if !valid {
                return refused();
            }
            if let Some(bound) = route.router.chain_deadline(token) {
                if self.now >= bound {
                    return refused();
                }
                deadline = Some(deadline.map_or(bound, |old: Duration| old.min(bound)));
            }
        }
        if let Some(binding) = self.bindings.get(&leaf.id()) {
            if binding.owner != owner.id() || binding.tokens != *scope {
                return refused();
            }
        } else {
            if !admit
                || self
                    .bindings
                    .values()
                    .map(|binding| binding.tokens.len())
                    .sum::<usize>()
                    + scope.len()
                    > chain::MAX_CHAIN_BINDINGS
            {
                return refused();
            }
            self.bindings.insert(
                leaf.id(),
                BoundCustom {
                    owner: owner.id(),
                    tokens: scope.clone(),
                },
            );
        }
        let ownership = self.routes[leaf.source()].router.ownership();
        RecognitionOwnership {
            ownership,
            winner: (ownership == Some(Ownership::Claimed)).then_some(leaf.id()),
            deadline,
            skip_offer: false,
        }
    }
    fn key_ownership(&self, leaf: &RouteIntent, parent: &RouteIntent) -> RecognitionOwnership {
        use crate::router::KeyDisposition;
        let refused = || RecognitionOwnership {
            ownership: Some(Ownership::Forwarded),
            winner: None,
            deadline: None,
            skip_offer: false,
        };
        let mut evidence = leaf.input_evidence();
        if matches!(parent.rule.trigger, Trigger::Sequence { .. })
            && evidence.include(&parent.input_evidence()).is_err()
        {
            return refused();
        }
        let owned: BTreeSet<_> = self
            .won
            .get(&parent.id())
            .and_then(|won| won.key_claim.as_ref())
            .into_iter()
            .flat_map(|claim| claim.presses())
            .collect();
        let mut pending = false;
        let mut deadline: Option<Duration> = None;
        let mut any = false;
        let mut lost = false;
        for press in evidence.iter() {
            let Some(route) = self
                .routes
                .get(&press.source)
                .filter(|route| route.project_keys)
            else {
                return refused();
            };
            match route.router.key_disposition(press.code, press.position) {
                Some(KeyDisposition::Pending { deadline: bound }) if self.now < bound => {
                    any = true;
                    pending = true;
                    deadline = Some(deadline.map_or(bound, |old| old.min(bound)));
                }
                Some(KeyDisposition::Consumed { owner }) if owner == parent.id() => any = true,
                None if owned.contains(press) => any = true,
                _ => lost = true,
            }
        }
        let domain_receipt = self
            .won
            .get(&parent.id())
            .and_then(|won| won.key_claim.as_ref())
            .map(|claim| &claim.domains);
        for atom in evidence.tails.atoms() {
            if !self.split_inputs {
                return refused();
            }
            match self
                .domains
                .disposition(self.routes, &atom, domain_receipt, parent.id())
            {
                Some(KeyDisposition::Pending { deadline: bound }) if self.now < bound => {
                    any = true;
                    pending = true;
                    deadline = Some(deadline.map_or(bound, |old| old.min(bound)));
                }
                Some(KeyDisposition::Consumed { owner }) if owner == parent.id() => any = true,
                _ => lost = true,
            }
        }
        if !any {
            return refused();
        }
        if lost {
            return RecognitionOwnership {
                skip_offer: true,
                ..refused()
            };
        }
        RecognitionOwnership {
            ownership: Some(if pending {
                Ownership::Pending
            } else {
                Ownership::Claimed
            }),
            winner: (!pending).then_some(leaf.id()),
            deadline,
            skip_offer: false,
        }
    }
}
