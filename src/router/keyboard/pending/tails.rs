use super::*;

pub type TailDisposition = KeyDisposition;

/// Bounded admission for one complete non-key report cohort.
#[derive(Clone, Copy, Debug)]
pub struct PendingTail {
    pub authorization: u64,
    pub deadline: Duration,
    /// Exact still-pending root for deliberate cross-group retention.
    pub continuation: Option<InputPosition>,
}

/// Decision for an exact original cohort root under its source authorization.
#[derive(Clone, Copy, Debug)]
pub struct TailDecision {
    pub authorization: u64,
    pub position: InputPosition,
    pub owner: Option<u64>,
}

#[derive(Clone, Copy)]
struct Scope {
    choice: Choice,
    references: usize,
    active: bool,
}

#[derive(Default)]
pub(super) struct Tails {
    scopes: BTreeMap<InputPosition, Scope>,
    current: Option<InputPosition>,
    enabled: bool,
}

pub(super) struct Plan {
    input: Option<InputPosition>,
    current: Option<InputPosition>,
    previous: Option<(InputPosition, Scope)>,
    update: Option<(InputPosition, Scope)>,
    enabled: bool,
}

impl Plan {
    pub(super) fn input(&self) -> Option<InputPosition> {
        self.input
    }
    pub(super) fn transformed(&self) -> bool {
        self.input.is_some()
            && self
                .update
                .is_some_and(|(_, scope)| scope.choice.disposition != TailDisposition::Ordinary)
    }
}

impl Tails {
    pub(super) fn enabled(&self) -> bool {
        self.enabled
    }
    pub(super) fn current(&self) -> Option<InputPosition> {
        self.current
    }
    pub(super) fn choice(&self, root: InputPosition) -> Option<Choice> {
        self.scopes.get(&root).map(|scope| scope.choice)
    }
    pub(super) fn live(&self, root: InputPosition) -> bool {
        self.scopes.get(&root).is_some_and(|scope| scope.active)
    }
    pub(super) fn deadline(&self) -> Option<Duration> {
        self.scopes
            .values()
            .filter_map(|scope| match scope.choice.disposition {
                TailDisposition::Pending { deadline } => Some(deadline),
                _ => None,
            })
            .min()
    }
    pub(super) fn prepare(
        &self,
        position: Option<InputPosition>,
        pending: Option<PendingTail>,
        active: bool,
        at: Duration,
        offset: Duration,
    ) -> Result<Plan, Fault> {
        self.prepare_retained(position, pending, active, at, offset, false)
    }
    pub(super) fn prepare_retained(
        &self,
        position: Option<InputPosition>,
        pending: Option<PendingTail>,
        active: bool,
        at: Duration,
        offset: Duration,
        preserve_pending: bool,
    ) -> Result<Plan, Fault> {
        if let Some(pending) = pending
            && (position.is_none() && pending.continuation.is_none()
                || pending.deadline < at
                || pending.deadline.saturating_sub(at) > offset)
        {
            return Err(Fault::InvalidCandidate);
        }
        let previous = self
            .current
            .map(|root| {
                self.scopes
                    .get(&root)
                    .copied()
                    .map(|scope| (root, scope))
                    .ok_or(Fault::WrongTicket)
            })
            .transpose()?;
        let continuation = pending.and_then(|pending| pending.continuation);
        if let Some(continuation) = continuation
            && previous.is_none_or(|(root, scope)| {
                root != continuation
                    || !matches!(scope.choice.disposition, TailDisposition::Pending { .. })
            })
        {
            return Err(Fault::TooLate);
        }
        if pending.is_some()
            && continuation.is_none()
            && previous.is_some_and(|(_, scope)| scope.active)
        {
            return Err(Fault::TooLate);
        }
        let fresh = position.is_some()
            && continuation.is_none()
            && previous.is_some_and(|(_, scope)| !scope.active);
        let Some(root) = (if fresh {
            position
        } else {
            self.current.or(position)
        }) else {
            return Ok(Plan {
                input: None,
                current: None,
                previous: None,
                update: None,
                enabled: self.enabled,
            });
        };
        let mut scope = if !fresh && let Some((_, scope)) = previous {
            scope
        } else {
            Scope {
                choice: Choice {
                    disposition: pending.map_or(TailDisposition::Ordinary, |pending| {
                        TailDisposition::Pending {
                            deadline: pending.deadline,
                        }
                    }),
                    at,
                },
                references: 0,
                active,
            }
        };
        if let Some(pending) = pending
            && scope.choice.disposition
                != (TailDisposition::Pending {
                    deadline: pending.deadline,
                })
        {
            return Err(Fault::TooLate);
        }
        scope.active = active;
        let input = (position.is_some() || self.current.is_some()).then_some(root);
        scope.references = scope
            .references
            .checked_add(usize::from(input.is_some()))
            .ok_or(Fault::Overflow)?;
        Ok(Plan {
            input,
            previous: fresh
                .then(|| {
                    previous.map(|(root, mut scope)| {
                        if !preserve_pending
                            || !matches!(scope.choice.disposition, TailDisposition::Pending { .. })
                        {
                            scope.choice = scope.choice.decided(None, at);
                        }
                        (root, scope)
                    })
                })
                .flatten(),
            current: (active
                || matches!(scope.choice.disposition, TailDisposition::Pending { .. }))
            .then_some(root),
            update: Some((root, scope)),
            enabled: self.enabled || pending.is_some(),
        })
    }
    pub(super) fn commit(&mut self, plan: Plan) {
        if let Some((root, scope)) = plan.previous {
            self.scopes.insert(root, scope);
        }
        if let Some((root, scope)) = plan.update {
            self.scopes.insert(root, scope);
        }
        self.current = plan.current;
        self.enabled = plan.enabled;
    }
    pub(super) fn validate(&self, decisions: &[TailDecision], at: Duration) -> Result<(), Fault> {
        let mut seen = BTreeSet::new();
        for decision in decisions {
            if !seen.insert(decision.position) || decision.owner == Some(0) {
                return Err(Fault::InvalidCandidate);
            }
            self.choice(decision.position)
                .ok_or(Fault::TooLate)?
                .validate(decision.owner, at)?;
        }
        Ok(())
    }
    pub(super) fn settle(&mut self, decisions: &[TailDecision], at: Duration) {
        for decision in decisions {
            let scope = self.scopes.get_mut(&decision.position).unwrap();
            scope.choice = scope.choice.decided(decision.owner, at);
        }
        self.retire();
    }
    pub(super) fn expire(&mut self, at: Duration) {
        for scope in self.scopes.values_mut() {
            scope.choice = scope.choice.expired(at);
        }
        self.retire();
    }
    fn retire(&mut self) {
        if self.current.is_some_and(|root| {
            self.scopes.get(&root).is_none_or(|scope| {
                !scope.active
                    && !matches!(scope.choice.disposition, TailDisposition::Pending { .. })
            })
        }) {
            self.current = None;
        }
    }
    pub(super) fn released(&mut self, root: Option<InputPosition>) {
        if let Some(root) = root {
            let scope = self.scopes.get_mut(&root).unwrap();
            scope.references = scope
                .references
                .checked_sub(1)
                .expect("retained tail reference");
        }
    }
    pub(super) fn prune(&mut self) {
        self.scopes
            .retain(|root, scope| scope.references != 0 || self.current == Some(*root));
    }
}
