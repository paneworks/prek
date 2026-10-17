//! Native custom-leaf bindings to complete retained parent scopes.
use super::*;
use crate::rules::RecognitionOwnership;

pub(super) struct BoundCustom {
    pub(super) owner: u64,
    pub(super) tokens: BTreeMap<Source, ChainToken>,
}

pub(super) struct CustomRoutes<'a> {
    pub(super) routes: &'a BTreeMap<Source, Route>,
    pub(super) chains: &'a BTreeMap<u64, BTreeMap<Source, ChainToken>>,
    pub(super) groups: &'a BTreeMap<u64, (Source, u64, u64)>,
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
        if !matches!(owner.rule.trigger, Trigger::Sequence { .. }) {
            if owner.id() != leaf.id() {
                return refused();
            }
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
        }
    }
}
