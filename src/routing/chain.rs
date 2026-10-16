//! Native sequence group admission and immutable dispatch ownership proofs.
use super::*;

pub(super) const MAX_CHAIN_BINDINGS: usize = 4096;
impl Routing {
    pub(super) fn chain_existing_valid(&self, id: u64, at: Duration) -> bool {
        self.chains.get(&id).is_none_or(|scope| {
            scope.iter().all(|(source, token)| {
                self.routes.get(source).is_some_and(|route| {
                    match route.router.chain_ownership(token, at) {
                        Some(Ownership::Pending) => route.winner.is_none(),
                        Some(Ownership::Claimed) => route.winner == Some(id),
                        _ => false,
                    }
                })
            })
        })
    }
    pub(super) fn chain_scope_valid(&self, intent: &RouteIntent, at: Duration) -> bool {
        self.chains.get(&intent.id()).is_some_and(|scope| {
            scope.len() == intent.sources().len()
                && intent
                    .sources()
                    .iter()
                    .all(|source| scope.contains_key(source))
        }) && self.chain_existing_valid(intent.id(), at)
    }
    pub(super) fn prune_chains(&mut self, at: Duration, now: Duration) -> BTreeSet<u64> {
        let invalid: BTreeSet<_> = self
            .chains
            .keys()
            .copied()
            .filter(|id| {
                !self.chain_existing_valid(*id, at)
                    || self.chains[id].iter().any(|(source, token)| {
                        self.routes
                            .get(source)
                            .and_then(|route| route.router.chain_deadline(token))
                            .is_some_and(|deadline| now >= deadline)
                    })
            })
            .collect();
        for id in &invalid {
            self.engine
                .cancel_sequence(*id, at, "sequence raw-group ownership changed");
        }
        invalid
    }
    pub(super) fn bound_chains(&mut self) -> io::Result<()> {
        if self.chains.values().map(BTreeMap::len).sum::<usize>() > MAX_CHAIN_BINDINGS {
            let sources: Vec<_> = self.routes.keys().cloned().collect();
            for source in sources {
                self.recover(&source);
            }
            return Err(fault(Fault::Overflow));
        }
        Ok(())
    }
    pub(super) fn sealed_chain(
        &mut self,
        intent: &RouteIntent,
        at: Duration,
    ) -> Option<Arc<Vec<ClaimedChain>>> {
        if let Some(receipt) = self.won.get(&intent.id())?.chain.as_ref() {
            return (receipt.len() == intent.sources().len()
                && receipt
                    .iter()
                    .all(|proof| intent.sources().contains(proof.source()))
                && self.chain_receipt_valid(receipt))
            .then(|| receipt.clone());
        }
        if !self.chain_scope_valid(intent, at) {
            return None;
        }
        let receipts = self
            .chains
            .get(&intent.id())?
            .iter()
            .map(|(source, token)| self.routes.get(source)?.router.seal_chain(token, at).ok())
            .collect::<Option<Vec<_>>>()?;
        let receipt = Arc::new(receipts);
        self.won.get_mut(&intent.id())?.chain = Some(receipt.clone());
        Some(receipt)
    }
    pub(super) fn chain_receipt_valid(&self, receipts: &[ClaimedChain]) -> bool {
        !receipts.is_empty()
            && receipts.iter().all(|receipt| {
                self.routes
                    .get(receipt.source())
                    .is_some_and(|route| route.router.chain_receipt_valid(receipt))
            })
    }
    pub(super) fn cleanup_chains(&mut self, active: &[RouteIntent]) {
        self.chains.retain(|id, _| {
            active.iter().any(|intent| intent.id() == *id)
                || self.won.contains_key(id)
                || self
                    .dispatches
                    .iter()
                    .any(|dispatch| dispatch.outcome.intent.id() == *id)
                || self
                    .deferred_custom
                    .iter()
                    .any(|outcome| outcome.intent.id() == *id)
        });
    }
    pub(super) fn outcome_scope_valid(&self, intent: &RouteIntent, at: Duration) -> bool {
        if matches!(intent.rule.trigger, Trigger::Sequence { .. }) {
            self.chain_scope_valid(intent, at)
        } else {
            self.custom_group_valid(intent)
        }
    }
    pub(super) fn cancel_outcome(&mut self, intent: &RouteIntent, at: Duration, reason: &str) {
        if matches!(intent.rule.trigger, Trigger::Sequence { .. }) {
            self.engine.cancel_sequence(intent.id(), at, reason)
        } else {
            self.engine.cancel_custom(intent.id(), at, reason)
        }
    }
    pub(super) fn chain_expiry(&self, intent: &RouteIntent) -> Option<Duration> {
        self.chains
            .get(&intent.id())?
            .iter()
            .filter_map(|(source, token)| self.routes.get(source)?.router.chain_deadline(token))
            .min()
    }
    pub(super) fn cancel_chain_dispatch(&mut self, id: u64) {
        let at = self.engine.input_at();
        self.engine
            .cancel_sequence(id, at, "queued sequence dispatch is no longer eligible");
        self.won.remove(&id);
        let sources: Vec<_> = self
            .routes
            .iter()
            .filter(|(_, route)| {
                route.winner == Some(id) && route.router.ownership() == Some(Ownership::Claimed)
            })
            .map(|(source, _)| source.clone())
            .collect();
        for source in sources {
            let route = self.routes.get_mut(&source).unwrap();
            if route
                .router
                .cancel(route.ticket, route.authorization, at)
                .is_err()
            {
                self.recover(&source);
            }
        }
    }
}
