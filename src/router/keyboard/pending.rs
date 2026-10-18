use super::*;
use crate::input::TailDomain;
mod domains;
mod tails;
pub use domains::{DomainDecision, DomainRoot, PendingDomain};
pub use tails::{PendingTail, TailDecision, TailDisposition};

#[derive(Default)]
struct TailFrame {
    pending: Option<PendingTail>,
    domains: Option<Vec<PendingDomain>>,
    active: bool,
    absolute_before: Option<Arc<crate::input::AbsoluteState>>,
    capacity: Option<usize>,
}

fn absolute_storage(state: &crate::input::AbsoluteState) -> usize {
    state.values.len()
        + state
            .slots
            .values()
            .map(|slot| slot.values.len() + 1)
            .sum::<usize>()
        + 1
}

#[derive(Clone, Copy, Debug)]
pub struct PendingKey {
    pub authorization: u64,
    pub event: usize,
    pub deadline: Duration,
}

#[derive(Clone, Copy, Debug)]
pub struct KeyDecision {
    pub authorization: u64,
    pub code: u16,
    pub position: InputPosition,
    pub owner: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyDisposition {
    Pending { deadline: Duration },
    Ordinary,
    Consumed { owner: u64 },
}

#[derive(Clone, Copy)]
struct Choice {
    disposition: KeyDisposition,
    at: Duration,
}
impl Choice {
    fn validate(self, owner: Option<u64>, at: Duration) -> Result<(), Fault> {
        match (self.disposition, owner) {
            (KeyDisposition::Pending { deadline }, Some(_)) if at > deadline => {
                return Err(Fault::TooLate);
            }
            (KeyDisposition::Pending { .. }, _) | (KeyDisposition::Ordinary, None) => {}
            (KeyDisposition::Consumed { owner }, Some(wanted)) if owner == wanted => {}
            _ => return Err(Fault::TooLate),
        }
        if at < self.at {
            return Err(Fault::BackwardTime);
        }
        Ok(())
    }
    fn decided(self, owner: Option<u64>, at: Duration) -> Self {
        match self.disposition {
            KeyDisposition::Pending { deadline } => Self {
                disposition: owner.map_or(KeyDisposition::Ordinary, |owner| {
                    KeyDisposition::Consumed { owner }
                }),
                at: at.min(deadline),
            },
            _ => self,
        }
    }
    fn expired(self, at: Duration) -> Self {
        match self.disposition {
            KeyDisposition::Pending { deadline } if at >= deadline => Self {
                disposition: KeyDisposition::Ordinary,
                at: deadline,
            },
            _ => self,
        }
    }
}
struct PendingPress {
    code: u16,
    references: usize,
    choice: Choice,
}
#[derive(Clone, Copy)]
struct Binding {
    position: InputPosition,
    down: bool,
}
struct Input {
    frame: Frame,
    tail: Option<InputPosition>,
    domains: Vec<DomainRoot>,
    split_tail: bool,
    absolute_before: Option<Arc<crate::input::AbsoluteState>>,
    storage: usize,
    watermark: u64,
    cursor: usize,
    positions: Vec<Option<InputPosition>>,
    edges: BTreeSet<usize>,
    cuts: BTreeSet<usize>,
    order: Option<Vec<usize>>,
}
impl Input {
    fn index(&self) -> usize {
        self.order
            .as_ref()
            .map_or(self.cursor, |order| order[self.cursor])
    }
    fn len(&self) -> usize {
        self.order
            .as_ref()
            .map_or(self.frame.events.len(), Vec::len)
    }
    fn key_event(frame: &Frame, index: usize) -> bool {
        crate::input::independent_event(&frame.events, index)
    }
    fn tail(
        &mut self,
        ready: Duration,
        reports: &mut Vec<ReadyReport>,
        domains: &domains::Domains,
    ) {
        let end = self.frame.events.len() - 1;
        let events: Vec<_> = self
            .frame
            .events
            .iter()
            .enumerate()
            .filter(|(index, _)| *index == end || !Self::key_event(&self.frame, *index))
            .filter(|(index, _)| {
                !self.split_tail
                    || TailDomain::at(&self.frame.events, *index)
                        .and_then(|domain| self.domains.iter().find(|root| root.domain == domain))
                        .and_then(|root| domains.choice(*root))
                        .is_none_or(|choice| {
                            !matches!(choice.disposition, TailDisposition::Consumed { .. })
                        })
            })
            .map(|(_, event)| *event)
            .collect();
        if self.split_tail
            && events.len() == 1
            && self.domains.iter().any(|root| {
                domains.choice(*root).is_some_and(|choice| {
                    matches!(choice.disposition, TailDisposition::Consumed { .. })
                })
            })
        {
            return;
        }
        let position = InputPosition {
            frame: self.watermark,
            event: end,
        };
        reports.push(ReadyReport {
            ready,
            absolute_before: self.absolute_before.take(),
            split_tail: self.split_tail,
            report: Report {
                frame: Frame {
                    source: self.frame.source.clone(),
                    at: self.frame.at,
                    sequence: self.frame.sequence,
                    events,
                },
                span: InputSpan {
                    sequence: self.frame.sequence,
                    first: position,
                    last: position,
                },
            },
        });
    }
}
#[derive(Default)]
pub(in crate::router) struct Journal {
    tails: tails::Tails,
    domains: domains::Domains,
    split_tail: Option<bool>,
    held: BTreeMap<u16, Binding>,
    presses: BTreeMap<InputPosition, PendingPress>,
    frames: VecDeque<Input>,
    watermark: u64,
    clock: Duration,
    pub(super) output_sequence: u64,
    #[cfg(test)]
    pub(super) last_claim: u64,
}
pub(super) struct Released {
    pub(super) reports: Vec<ReadyReport>,
    #[cfg(test)]
    pub(super) consumed: Vec<ConsumedKey>,
}
pub(super) struct ReadyReport {
    report: Report,
    ready: Duration,
    absolute_before: Option<Arc<crate::input::AbsoluteState>>,
    split_tail: bool,
}

impl std::fmt::Debug for Journal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Journal")
            .field("presses", &self.presses.len())
            .field("frames", &self.frames.len())
            .field("watermark", &self.watermark)
            .finish_non_exhaustive()
    }
}

impl Journal {
    pub(in crate::router) fn neutral(&self) -> bool {
        self.frames.is_empty() && self.tails.current().is_none() && self.domains.neutral()
    }
    pub(in crate::router) fn storage(&self) -> (usize, usize) {
        (
            self.frames.len(),
            self.frames.iter().map(|input| input.storage).sum(),
        )
    }
    pub(in crate::router) fn first(&self) -> Option<InputPosition> {
        self.frames.front().map(|input| InputPosition {
            frame: input.watermark,
            event: input.index(),
        })
    }
    pub(in crate::router) fn prefix_delivered(&self, sequence: u64, inclusive: bool) -> bool {
        self.frames.iter().all(|input| {
            input.frame.sequence > sequence || !inclusive && input.frame.sequence == sequence
        })
    }
    pub(in crate::router) fn deadline(&self) -> Option<Duration> {
        self.presses
            .values()
            .filter_map(|press| match press.choice.disposition {
                KeyDisposition::Pending { deadline } => Some(deadline),
                _ => None,
            })
            .chain(self.tails.deadline())
            .chain(self.domains.deadline())
            .min()
    }
    pub(super) fn can_release_before(&self, barrier: Option<InputPosition>) -> bool {
        let Some(input) = self.frames.front() else {
            return false;
        };
        let first = InputPosition {
            frame: input.watermark,
            event: input.index(),
        };
        barrier.is_none_or(|barrier| first < barrier)
            && input
                .positions
                .get(input.index())
                .and_then(|position| *position)
                .and_then(|position| self.presses.get(&position))
                .map(|press| press.choice)
                .is_none_or(|choice| !matches!(choice.disposition, KeyDisposition::Pending { .. }))
            && (input.index() != input.frame.events.len() - 1
                || input
                    .tail
                    .and_then(|root| self.tails.choice(root))
                    .into_iter()
                    .chain(
                        input
                            .domains
                            .iter()
                            .filter_map(|root| self.domains.choice(*root)),
                    )
                    .all(|choice| !matches!(choice.disposition, TailDisposition::Pending { .. })))
    }
    pub(super) fn disposition(&self, code: u16, position: InputPosition) -> Option<KeyDisposition> {
        self.presses
            .get(&position)
            .filter(|press| press.code == code)
            .map(|press| press.choice.disposition)
    }
    #[cfg(test)]
    pub(super) fn stage(
        &mut self,
        frame: Frame,
        watermark: u64,
        pending: &[PendingKey],
        cuts: &BTreeSet<usize>,
        offset: Duration,
    ) -> Result<(), Fault> {
        self.stage_with_tail(
            frame,
            watermark,
            pending,
            cuts,
            offset,
            TailFrame::default(),
        )
        .map(|_| ())
    }

    fn stage_with_tail(
        &mut self,
        frame: Frame,
        watermark: u64,
        pending: &[PendingKey],
        cuts: &BTreeSet<usize>,
        offset: Duration,
        tail_frame: TailFrame,
    ) -> Result<Vec<DomainRoot>, Fault> {
        if frame.events.is_empty()
            || frame
                .events
                .last()
                .is_none_or(|event| event.kind != EV_SYN || event.code != SYN_REPORT)
            || frame.events[..frame.events.len() - 1]
                .iter()
                .any(|event| event.kind == EV_SYN)
        {
            return Err(Fault::InvalidFrame);
        }
        if watermark == 0 || watermark <= self.watermark {
            return Err(Fault::BackwardTime);
        }
        let split_tail = tail_frame.domains.is_some();
        if self
            .split_tail
            .is_some_and(|previous| previous != split_tail)
        {
            return Err(Fault::UnsupportedProjection);
        }
        if frame
            .events
            .iter()
            .any(|event| event.kind == EV_KEY && event.code > 0x2ff)
        {
            return Err(Fault::InvalidFrame);
        }
        let mut waiting = BTreeMap::new();
        for entry in pending {
            if entry.deadline < frame.at
                || entry.deadline.saturating_sub(frame.at) > offset
                || waiting.insert(entry.event, entry.deadline).is_some()
            {
                return Err(Fault::InvalidCandidate);
            }
        }
        for &event in waiting.keys().chain(cuts.iter()) {
            if frame
                .events
                .get(event)
                .is_none_or(|event| event.kind != EV_KEY)
            {
                return Err(Fault::InvalidCandidate);
            }
            if !crate::input::independent_key(frame.events[event].code) {
                return Err(Fault::UnsupportedProjection);
            }
        }
        let mut held = self.held.clone();
        let mut positions = vec![None; frame.events.len()];
        let mut edges = BTreeSet::new();
        let mut additions = Vec::new();
        for (index, event) in
            frame.events.iter().enumerate().filter(|(_, event)| {
                event.kind == EV_KEY && crate::input::independent_key(event.code)
            })
        {
            if event.code > 0x2ff {
                return Err(Fault::InvalidFrame);
            }
            let down = held.get(&event.code).is_some_and(|binding| binding.down);
            if waiting.contains_key(&index) && (event.value != 1 || down) {
                return Err(Fault::TooLate);
            }
            match event.value {
                1 if !down => {
                    let position = InputPosition {
                        frame: watermark,
                        event: index,
                    };
                    let disposition = waiting
                        .get(&index)
                        .map_or(KeyDisposition::Ordinary, |&deadline| {
                            KeyDisposition::Pending { deadline }
                        });
                    additions.push((
                        position,
                        PendingPress {
                            code: event.code,
                            references: 0,
                            choice: Choice {
                                disposition,
                                at: frame.at,
                            },
                        },
                    ));
                    held.insert(
                        event.code,
                        Binding {
                            position,
                            down: true,
                        },
                    );
                    edges.insert(index);
                }
                0 => {
                    if let Some(binding) = held.get_mut(&event.code) {
                        binding.down = false;
                    }
                    if down {
                        edges.insert(index);
                    }
                }
                2 if down => {
                    edges.insert(index);
                }
                1 => {}
                _ => return Err(Fault::InvalidFrame),
            }
            positions[index] = held.get(&event.code).map(|binding| binding.position);
        }
        let end = frame.events.len() - 1;
        let has_tail = frame.events[..end]
            .iter()
            .enumerate()
            .any(|(index, _)| !Input::key_event(&frame, index));
        let tail = (!split_tail)
            .then(|| {
                self.tails.prepare(
                    has_tail.then_some(InputPosition {
                        frame: watermark,
                        event: end,
                    }),
                    tail_frame.pending,
                    tail_frame.active,
                    frame.at,
                    offset,
                )
            })
            .transpose()?;
        let domain_plan = tail_frame
            .domains
            .as_ref()
            .map(|pending| {
                self.domains
                    .prepare(&frame, watermark, pending, tail_frame.active, offset)
            })
            .transpose()?;
        let storage = frame.events.len()
            + tail_frame
                .absolute_before
                .as_ref()
                .map_or(0, |state| absolute_storage(state))
            + domain_plan
                .as_ref()
                .map_or(0, |plan| plan.bindings.len() * domains::STORAGE_PER_ROOT);
        if tail_frame
            .capacity
            .is_some_and(|capacity| storage > capacity)
        {
            return Err(Fault::Overflow);
        }
        let transformed = !pending.is_empty()
            || !cuts.is_empty()
            || tail.as_ref().is_some_and(tails::Plan::transformed)
            || domain_plan.as_ref().is_some_and(domains::Plan::transformed)
            || positions.iter().flatten().any(|position| {
                self.presses
                    .get(position)
                    .is_some_and(|press| press.choice.disposition != KeyDisposition::Ordinary)
            });
        for (index, event) in frame.events[..frame.events.len() - 1].iter().enumerate() {
            if event.kind == EV_MSC
                && event.code == MSC_SCAN
                && frame
                    .events
                    .get(index + 1)
                    .is_some_and(|next| next.kind == EV_KEY)
            {
                positions[index] = positions[index + 1];
            } else if transformed
                && !split_tail
                && !matches!(event.kind, EV_KEY | EV_REL | EV_ABS | EV_SW)
            {
                return Err(Fault::UnsupportedProjection);
            }
        }
        let order = has_tail.then(|| {
            (0..end)
                .filter(|&index| Input::key_event(&frame, index))
                .chain([end])
                .collect()
        });
        for (position, press) in additions {
            self.presses.insert(position, press);
        }
        for position in positions.iter().flatten() {
            self.presses
                .get_mut(position)
                .expect("staged press reference")
                .references += 1;
        }
        self.held = held;
        self.watermark = watermark;
        self.split_tail = Some(split_tail);
        let input_tail = tail.as_ref().and_then(tails::Plan::input);
        if let Some(tail) = tail {
            self.tails.commit(tail);
        }
        let (domain_bindings, domain_inputs) = domain_plan
            .as_ref()
            .map(|plan| (plan.bindings.clone(), plan.inputs.clone()))
            .unwrap_or_default();
        if let Some(plan) = domain_plan {
            self.domains.commit(plan);
        }
        self.frames.push_back(Input {
            frame,
            tail: input_tail,
            domains: domain_bindings,
            split_tail,
            absolute_before: tail_frame.absolute_before,
            storage,
            watermark,
            cursor: 0,
            positions,
            edges,
            cuts: cuts.clone(),
            order,
        });
        Ok(domain_inputs)
    }

    pub(super) fn validate_decisions(
        &self,
        decisions: &[KeyDecision],
        at: Duration,
    ) -> Result<(), Fault> {
        let mut seen = BTreeSet::new();
        for decision in decisions {
            if !seen.insert(decision.position) || decision.owner == Some(0) {
                return Err(Fault::InvalidCandidate);
            }
            let press = self.presses.get(&decision.position).ok_or(Fault::TooLate)?;
            if press.code != decision.code {
                return Err(Fault::InvalidCandidate);
            }
            press.choice.validate(decision.owner, at)?;
        }
        Ok(())
    }
    pub(super) fn decide(&mut self, decisions: &[KeyDecision], at: Duration) -> Result<(), Fault> {
        self.validate_decisions(decisions, at)?;
        for decision in decisions {
            let press = self.presses.get_mut(&decision.position).unwrap();
            press.choice = press.choice.decided(decision.owner, at);
        }
        Ok(())
    }
    pub(super) fn expire(&mut self, at: Duration) {
        for press in self.presses.values_mut() {
            press.choice = press.choice.expired(at);
        }
        self.tails.expire(at);
        self.domains.expire(at);
    }

    #[cfg(test)]
    pub(super) fn release(&mut self) -> Released {
        self.release_before(None)
    }
    pub(super) fn release_before(&mut self, barrier: Option<InputPosition>) -> Released {
        let mut reports = Vec::new();
        #[cfg(test)]
        let mut consumed = Vec::new();
        while let Some(input) = self.frames.front_mut() {
            if barrier.is_some_and(|barrier| {
                InputPosition {
                    frame: input.watermark,
                    event: input.index(),
                } >= barrier
            }) {
                break;
            }
            let mut events = Vec::new();
            let mut first = None;
            let mut last = input.index();
            let start = input.cursor;
            let tail_choice = input.tail.and_then(|root| self.tails.choice(root));
            let (tail_pending, tail_consumed, tail_at) = tail_choice
                .into_iter()
                .chain(
                    input
                        .domains
                        .iter()
                        .filter_map(|root| self.domains.choice(*root)),
                )
                .fold(
                    (false, false, Duration::ZERO),
                    |(pending, consumed, at), choice| {
                        (
                            pending
                                || matches!(choice.disposition, TailDisposition::Pending { .. }),
                            consumed
                                || matches!(choice.disposition, TailDisposition::Consumed { .. }),
                            at.max(choice.at),
                        )
                    },
                );
            let mut changed = start != 0 || !input.cuts.is_empty() || tail_consumed;
            while input.cursor + 1 < input.len() {
                if barrier.is_some_and(|barrier| {
                    InputPosition {
                        frame: input.watermark,
                        event: input.index(),
                    } >= barrier
                }) {
                    break;
                }
                let index = input.index();
                let choice = input.positions[index]
                    .and_then(|position| self.presses.get(&position))
                    .map(|press| press.choice);
                if choice.is_some_and(|choice| {
                    matches!(choice.disposition, KeyDisposition::Pending { .. })
                }) {
                    break;
                }
                let ready = self
                    .clock
                    .max(input.frame.at)
                    .max(choice.map_or(Duration::ZERO, |choice| choice.at));
                let dropped = if let Some(Choice {
                    disposition: KeyDisposition::Consumed { owner: _owner },
                    ..
                }) = choice
                {
                    changed = true;
                    let event = input.frame.events[index];
                    if input.edges.contains(&index)
                        || input.cuts.contains(&index)
                        || event.kind == EV_MSC && input.edges.contains(&(index + 1))
                    {
                        Self::report(
                            input,
                            &mut events,
                            &mut first,
                            last,
                            self.clock,
                            &mut reports,
                        );
                    }
                    #[cfg(test)]
                    if event.kind == EV_KEY && input.edges.contains(&index) {
                        consumed.push(ConsumedKey {
                            owner: _owner,
                            code: event.code,
                            state: match event.value {
                                0 => KeyState::Release,
                                1 => KeyState::Press,
                                _ => KeyState::Repeat,
                            },
                            position: InputPosition {
                                frame: input.watermark,
                                event: index,
                            },
                        });
                    }
                    true
                } else {
                    false
                };
                if !dropped && ready > self.clock && !events.is_empty() {
                    changed = true;
                    Self::report(
                        input,
                        &mut events,
                        &mut first,
                        last,
                        self.clock,
                        &mut reports,
                    );
                }
                self.clock = ready;
                if !dropped {
                    first.get_or_insert(index);
                    last = index;
                    events.push(input.frame.events[index]);
                    if input.cuts.contains(&index) {
                        Self::report(
                            input,
                            &mut events,
                            &mut first,
                            last,
                            self.clock,
                            &mut reports,
                        );
                    }
                }
                if let Some(position) = input.positions[index] {
                    self.presses.get_mut(&position).unwrap().references -= 1;
                }
                input.cursor += 1;
            }
            let finished = !tail_pending
                && input.cursor + 1 == input.len()
                && (input.order.is_none()
                    || barrier.is_none_or(|barrier| {
                        InputPosition {
                            frame: input.watermark,
                            event: input.index(),
                        } < barrier
                    }));
            if finished && !changed && start == 0 {
                self.clock = self.clock.max(input.frame.at).max(tail_at);
                reports.push(ReadyReport {
                    ready: self.clock,
                    absolute_before: input.absolute_before.take(),
                    split_tail: input.split_tail,
                    report: Report {
                        frame: input.frame.clone(),
                        span: InputSpan {
                            sequence: input.frame.sequence,
                            first: InputPosition {
                                frame: input.watermark,
                                event: 0,
                            },
                            last: InputPosition {
                                frame: input.watermark,
                                event: input.frame.events.len() - 1,
                            },
                        },
                    },
                });
            } else {
                Self::report(
                    input,
                    &mut events,
                    &mut first,
                    last,
                    self.clock,
                    &mut reports,
                );
                if finished && input.order.is_some() {
                    self.clock = self.clock.max(input.frame.at).max(tail_at);
                    if input.split_tail || !tail_consumed {
                        input.tail(self.clock, &mut reports, &self.domains);
                    }
                }
            }
            if finished {
                self.tails.released(input.tail);
                self.domains.released(&input.domains);
                self.frames.pop_front();
            } else {
                break;
            }
        }
        self.held.retain(|_, binding| {
            binding.down
                || self.presses.get(&binding.position).is_some_and(|press| {
                    press.references != 0 || press.choice.disposition != KeyDisposition::Ordinary
                })
        });
        let retained: BTreeSet<_> = self.held.values().map(|binding| binding.position).collect();
        self.presses
            .retain(|position, press| press.references != 0 || retained.contains(position));
        self.tails.prune();
        self.domains.prune();
        if self.frames.is_empty()
            && self.held.values().all(|binding| !binding.down)
            && self.tails.current().is_none()
            && self.domains.neutral()
        {
            self.clock = Duration::ZERO;
        }
        Released {
            reports,
            #[cfg(test)]
            consumed,
        }
    }

    fn report(
        input: &Input,
        events: &mut Vec<RawEvent>,
        first: &mut Option<usize>,
        last: usize,
        ready: Duration,
        reports: &mut Vec<ReadyReport>,
    ) {
        let mut raw = Vec::new();
        Ledger::report(&input.frame, input.watermark, events, first, last, &mut raw);
        reports.extend(raw.into_iter().map(|report| ReadyReport {
            report,
            ready,
            absolute_before: None,
            split_tail: false,
        }));
    }
}

impl Router {
    /// Retain original keyboard input and fresh pending downs under this source's authorization.
    /// Decisions and ownership are supplied by the trusted caller, not inferred from key codes.
    pub fn stage_key_frame(
        &mut self,
        frame: Frame,
        watermark: u64,
        pending: &[PendingKey],
        cuts: &BTreeSet<usize>,
        at: Duration,
    ) -> Result<(), Fault> {
        self.stage_owned_frame(frame, watermark, pending, None, cuts, at)
    }

    /// Retain original key atoms and one coherent non-key cohort.
    /// Switch-capable sources do not admit tail ownership.
    pub fn stage_owned_frame(
        &mut self,
        frame: Frame,
        watermark: u64,
        pending: &[PendingKey],
        tail: Option<PendingTail>,
        cuts: &BTreeSet<usize>,
        at: Duration,
    ) -> Result<(), Fault> {
        self.stage_scoped_frame(
            frame,
            watermark,
            pending,
            cuts,
            at,
            TailFrame {
                pending: tail,
                ..TailFrame::default()
            },
        )
        .map(|_| ())
    }

    /// Retain independent non-key domains at their original terminal SYN cutpoint.
    pub fn stage_domain_frame(
        &mut self,
        frame: Frame,
        watermark: u64,
        keys: &[PendingKey],
        domains: &[PendingDomain],
        cuts: &BTreeSet<usize>,
        at: Duration,
    ) -> Result<Vec<DomainRoot>, Fault> {
        if frame.source != self.device.source {
            return Err(Fault::StaleSource);
        }
        if self.stopped {
            return Err(Fault::Stopped);
        }
        if domains.len() > TailDomain::ALL.len() {
            return self.fail(Fault::InvalidCandidate);
        }
        self.stage_scoped_frame(
            frame,
            watermark,
            keys,
            cuts,
            at,
            TailFrame {
                domains: Some(domains.to_vec()),
                ..TailFrame::default()
            },
        )
    }

    fn stage_scoped_frame(
        &mut self,
        frame: Frame,
        watermark: u64,
        pending: &[PendingKey],
        cuts: &BTreeSet<usize>,
        at: Duration,
        mut tail_frame: TailFrame,
    ) -> Result<Vec<DomainRoot>, Fault> {
        let tail = tail_frame.pending;
        let split_tail = tail_frame.domains.is_some();
        if frame.source != self.device.source {
            return Err(Fault::StaleSource);
        }
        if self.stopped {
            return Err(Fault::Stopped);
        }
        if self.keyboard.is_some()
            || self.pending_keyboard.is_none()
                && (self.last_sequence.is_some() || self.group.is_some() || !self.queue.is_empty())
        {
            return self.fail(Fault::TooLate);
        }
        if pending
            .iter()
            .any(|entry| entry.authorization != self.authorization)
            || tail.is_some_and(|entry| entry.authorization != self.authorization)
            || tail_frame.domains.as_ref().is_some_and(|domains| {
                domains
                    .iter()
                    .any(|entry| entry.authorization != self.authorization)
            })
        {
            return self.fail(Fault::Unauthorized);
        }
        if frame.at > at
            || frame.at < self.last_at
            || self
                .last_sequence
                .is_some_and(|sequence| frame.sequence <= sequence)
        {
            return self.fail(Fault::BackwardTime);
        }
        if frame.events.is_empty()
            || frame.events.len() > 4096
            || frame
                .events
                .last()
                .is_none_or(|event| event.kind != EV_SYN || event.code != SYN_REPORT)
            || frame.events[..frame.events.len() - 1]
                .iter()
                .any(|event| event.kind == EV_SYN)
        {
            return self.fail(Fault::InvalidFrame);
        }
        if tail.is_some() && self.device.capabilities.contains_key(&EV_SW) {
            return self.fail(Fault::UnsupportedProjection);
        }
        let absolute_before = (frame.events.iter().any(|event| event.kind == EV_ABS)
            && (tail.is_some()
                || tail_frame.domains.as_ref().is_some_and(|domains| {
                    domains.iter().any(|entry| {
                        matches!(entry.domain, TailDomain::Absolute | TailDomain::Touch)
                    })
                })
                || self.pending_keyboard.as_ref().is_some_and(|journal| {
                    journal.tails.enabled() || journal.domains.absolute_enabled()
                })))
        .then(|| self.physical_absolute.clone());
        if absolute_before
            .as_ref()
            .is_some_and(|state| state.validate(&self.device.axes).is_err())
        {
            return self.fail(Fault::InvalidFrame);
        }
        self.capacity(
            frame.events.len().saturating_add(
                absolute_before
                    .as_ref()
                    .map_or(0, |state| absolute_storage(state)),
            ),
        )?;
        tail_frame.capacity = Some(self.limits.events.saturating_sub(self.storage().1));
        let mut physical = self.physical.clone();
        if let Err(error) = physical.apply(&frame, &self.device) {
            return self.fail(error);
        }
        let tail_active = !physical.contacts.is_empty()
            || physical.keys.iter().any(|code| {
                if split_tail {
                    TailDomain::at(
                        &[RawEvent {
                            kind: EV_KEY,
                            code: *code,
                            value: 1,
                        }],
                        0,
                    ) == Some(TailDomain::Touch)
                } else {
                    !crate::input::independent_key(*code)
                }
            });
        let absolute_updates: Vec<_> = frame
            .events
            .iter()
            .filter(|event| event.kind == EV_ABS)
            .copied()
            .collect();
        let sequence = frame.sequence;
        tail_frame.active = tail_active;
        tail_frame.absolute_before = absolute_before;
        let journal = self.pending_keyboard.get_or_insert_with(Journal::default);
        let domains = match journal.stage_with_tail(
            frame,
            watermark,
            pending,
            cuts,
            self.limits.offset,
            tail_frame,
        ) {
            Ok(domains) => domains,
            Err(error) => return self.fail(error),
        };
        self.physical = physical;
        if !absolute_updates.is_empty() {
            let absolute = Arc::make_mut(&mut self.physical_absolute);
            for event in absolute_updates {
                absolute.update(event);
            }
        }
        self.last_sequence = Some(sequence);
        self.last_at = at;
        Ok(domains)
    }

    /// Atomically settle exact original downs before advancing their decision deadlines.
    /// Consumed input stays consumed; ordinary decisions cannot be reclaimed.
    pub fn decide_key_presses(
        &mut self,
        decisions: &[KeyDecision],
        at: Duration,
    ) -> Result<(), Fault> {
        self.decide_input_scopes(decisions, &[], at)
    }

    /// Validate and settle original key and tail scopes as one decision batch.
    pub fn decide_input_scopes(
        &mut self,
        keys: &[KeyDecision],
        tails: &[TailDecision],
        at: Duration,
    ) -> Result<(), Fault> {
        if self.stopped {
            return Err(Fault::Stopped);
        }
        if keys
            .iter()
            .any(|decision| decision.authorization != self.authorization)
            || tails
                .iter()
                .any(|decision| decision.authorization != self.authorization)
        {
            return self.fail(Fault::Unauthorized);
        }
        if at < self.last_at {
            return self.fail(Fault::BackwardTime);
        }
        self.decide_input_clocked(keys, tails, at, at)
    }

    /// Validate all key and independent-domain decisions before settling any.
    pub fn decide_domain_scopes(
        &mut self,
        keys: &[KeyDecision],
        domains: &[DomainDecision],
        at: Duration,
    ) -> Result<(), Fault> {
        if self.stopped {
            return Err(Fault::Stopped);
        }
        if keys
            .iter()
            .any(|decision| decision.authorization != self.authorization)
            || domains
                .iter()
                .any(|decision| decision.authorization != self.authorization)
        {
            return self.fail(Fault::Unauthorized);
        }
        if at < self.last_at {
            return self.fail(Fault::BackwardTime);
        }
        self.decide_domains_clocked(keys, domains, at, at)
    }

    pub(crate) fn validate_key_decisions(
        &self,
        decisions: &[KeyDecision],
        input_at: Duration,
        admitted_at: Duration,
    ) -> Result<(), Fault> {
        self.validate_input_decisions(decisions, &[], input_at, admitted_at)
    }

    pub(crate) fn validate_input_decisions(
        &self,
        keys: &[KeyDecision],
        tails: &[TailDecision],
        input_at: Duration,
        admitted_at: Duration,
    ) -> Result<(), Fault> {
        self.validate_scoped_decisions(keys, tails, &[], input_at, admitted_at)
    }

    pub(crate) fn validate_domain_decisions(
        &self,
        keys: &[KeyDecision],
        domains: &[DomainDecision],
        input_at: Duration,
        admitted_at: Duration,
    ) -> Result<(), Fault> {
        self.validate_scoped_decisions(keys, &[], domains, input_at, admitted_at)
    }

    fn validate_scoped_decisions(
        &self,
        keys: &[KeyDecision],
        tails: &[TailDecision],
        domains: &[DomainDecision],
        input_at: Duration,
        admitted_at: Duration,
    ) -> Result<(), Fault> {
        if self.stopped {
            return Err(Fault::Stopped);
        }
        if keys
            .iter()
            .any(|decision| decision.authorization != self.authorization)
            || tails
                .iter()
                .any(|decision| decision.authorization != self.authorization)
            || domains
                .iter()
                .any(|decision| decision.authorization != self.authorization)
        {
            return Err(Fault::Unauthorized);
        }
        if admitted_at < input_at || input_at < self.last_at {
            return Err(Fault::BackwardTime);
        }
        let journal = self.pending_keyboard.as_ref().ok_or(Fault::WrongTicket)?;
        journal.validate_decisions(keys, admitted_at)?;
        journal.tails.validate(tails, admitted_at)?;
        journal.domains.validate(domains, admitted_at)
    }
    pub(crate) fn decide_keys_clocked(
        &mut self,
        decisions: &[KeyDecision],
        input_at: Duration,
        admitted_at: Duration,
    ) -> Result<(), Fault> {
        self.decide_input_clocked(decisions, &[], input_at, admitted_at)
    }

    pub(crate) fn decide_input_clocked(
        &mut self,
        keys: &[KeyDecision],
        tails: &[TailDecision],
        input_at: Duration,
        admitted_at: Duration,
    ) -> Result<(), Fault> {
        self.validate_input_decisions(keys, tails, input_at, admitted_at)?;
        let journal = self.pending_keyboard.as_mut().unwrap();
        journal.decide(keys, admitted_at)?;
        journal.tails.settle(tails, admitted_at);
        self.last_at = input_at;
        Ok(())
    }

    pub(crate) fn decide_domains_clocked(
        &mut self,
        keys: &[KeyDecision],
        domains: &[DomainDecision],
        input_at: Duration,
        admitted_at: Duration,
    ) -> Result<(), Fault> {
        self.validate_domain_decisions(keys, domains, input_at, admitted_at)?;
        let journal = self.pending_keyboard.as_mut().unwrap();
        journal.decide(keys, admitted_at)?;
        journal.domains.settle(domains, admitted_at);
        self.last_at = input_at;
        Ok(())
    }
    pub(crate) fn key_journal_barrier(
        &mut self,
        barrier: Option<InputPosition>,
        offset: Duration,
    ) -> Result<(), Fault> {
        if offset > self.limits.offset {
            return Err(Fault::MissedDelivery);
        }
        self.key_barrier = barrier;
        self.key_offset_floor = offset;
        Ok(())
    }
    pub(crate) fn key_journal_neutral(&self) -> bool {
        self.physical.neutral()
            && self.delivered.neutral()
            && self.queue.is_empty()
            && self.pending_keyboard.as_ref().is_none_or(Journal::neutral)
    }

    pub(crate) fn key_journal_ready(&self) -> bool {
        self.pending_keyboard
            .as_ref()
            .is_some_and(|journal| journal.can_release_before(self.key_barrier))
    }
    pub fn key_disposition(&self, code: u16, position: InputPosition) -> Option<KeyDisposition> {
        if self.stopped {
            return None;
        }
        self.pending_keyboard.as_ref()?.disposition(code, position)
    }

    /// Current coherent cohort root, including a pending neutral report.
    pub fn tail_scope(&self) -> Option<InputPosition> {
        if self.stopped {
            return None;
        }
        self.pending_keyboard.as_ref()?.tails.current()
    }

    pub fn tail_disposition(&self, root: InputPosition) -> Option<TailDisposition> {
        if self.stopped {
            return None;
        }
        self.pending_keyboard
            .as_ref()?
            .tails
            .choice(root)
            .map(|choice| choice.disposition)
    }

    pub fn domain_scope(&self, domain: TailDomain) -> Option<DomainRoot> {
        if self.stopped {
            return None;
        }
        Some(DomainRoot {
            domain,
            position: self.pending_keyboard.as_ref()?.domains.current(domain)?,
        })
    }

    pub fn domain_disposition(&self, scope: DomainRoot) -> Option<TailDisposition> {
        if self.stopped {
            return None;
        }
        self.pending_keyboard
            .as_ref()?
            .domains
            .choice(scope)
            .map(|choice| choice.disposition)
    }

    pub fn pending_key_position(&self) -> Option<InputPosition> {
        self.pending_keyboard.as_ref()?.first()
    }

    pub(in crate::router) fn advance_key_journal(&mut self, at: Duration) -> Result<(), Fault> {
        let Some(journal) = &mut self.pending_keyboard else {
            return Ok(());
        };
        journal.expire(at);
        let released = journal.release_before(self.key_barrier);
        self.publish_released_keys(released.reports)
    }

    fn publish_released_keys(&mut self, mut reports: Vec<ReadyReport>) -> Result<(), Fault> {
        if reports.is_empty() {
            return Ok(());
        }
        let mut sequence = self.pending_keyboard.as_ref().unwrap().output_sequence;
        let mut offset = self.offset.max(self.key_offset_floor);
        let mut absolute = reports
            .iter()
            .any(|ready| ready.absolute_before.is_some())
            .then(|| self.planned_absolute.clone());
        for ready in &mut reports {
            let report = &mut ready.report;
            if let Some(before) = ready.absolute_before.take() {
                let repairs = if ready.split_tail {
                    before.restore_domains_for(absolute.as_ref().unwrap(), &report.frame)
                } else {
                    before.restore_for(absolute.as_ref().unwrap(), &report.frame)
                };
                if report.frame.events.len().saturating_add(repairs.len()) > 4096 {
                    return self.fail(Fault::Overflow);
                }
                let insert_at = report
                    .frame
                    .events
                    .iter()
                    .position(|event| event.kind == EV_ABS)
                    .unwrap_or(report.frame.events.len() - 1);
                report.frame.events.splice(insert_at..insert_at, repairs);
            }
            if let Some(absolute) = absolute.as_mut() {
                for event in &report.frame.events {
                    absolute.update(*event);
                }
            }
            sequence = match sequence.checked_add(1) {
                Some(next) => next.max(report.frame.sequence),
                None => return self.fail(Fault::Overflow),
            };
            report.frame.sequence = sequence;
            offset = offset.max(ready.ready.saturating_sub(report.frame.at));
            if offset > self.limits.offset {
                return self.fail(Fault::MissedDelivery);
            }
            if report.frame.at.checked_add(offset).is_none() {
                return self.fail(Fault::Overflow);
            }
        }
        let count: usize = reports
            .iter()
            .map(|ready| ready.report.frame.events.len())
            .sum();
        let (frames, events) = self.storage();
        if frames
            .checked_add(reports.len())
            .is_none_or(|count| count > self.limits.frames)
            || events
                .checked_add(count)
                .is_none_or(|count| count > self.limits.events)
            || self.next_ticket.checked_add(reports.len() as u64).is_none()
        {
            return self.fail(Fault::Overflow);
        }
        self.pending_keyboard.as_mut().unwrap().output_sequence = sequence;
        for ready in reports {
            let report = ready.report;
            self.offset = self
                .offset
                .max(self.key_offset_floor)
                .max(ready.ready.saturating_sub(report.frame.at));
            self.schedule(report.frame)?;
            self.positions.insert(self.next_ticket, report.span);
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn push_decided_key_frame(
        &mut self,
        frame: Frame,
        watermark: u64,
        claims: &[KeyClaim],
        cuts: &BTreeSet<usize>,
        at: Duration,
    ) -> Result<KeyProjection, Fault> {
        if frame.source != self.device.source {
            return Err(Fault::StaleSource);
        }
        if self.stopped {
            return Err(Fault::Stopped);
        }
        if claims
            .iter()
            .any(|claim| claim.authorization != self.authorization)
        {
            return self.fail(Fault::Unauthorized);
        }
        let Some(deadline) = frame.at.checked_add(self.limits.offset) else {
            return self.fail(Fault::Overflow);
        };
        let mut ordered = BTreeMap::new();
        for claim in claims {
            if ordered.insert(claim.event, claim).is_some() {
                return self.fail(Fault::InvalidCandidate);
            }
        }
        let mut last = self
            .pending_keyboard
            .as_ref()
            .map_or(0, |journal| journal.last_claim);
        let mut decisions = Vec::new();
        let mut pending = Vec::new();
        for (event, claim) in ordered {
            let Some(input) = frame
                .events
                .get(event)
                .filter(|input| input.kind == EV_KEY && input.value == 1)
            else {
                return self.fail(Fault::InvalidCandidate);
            };
            if claim.id == 0 || claim.id <= last {
                return self.fail(Fault::InvalidCandidate);
            }
            last = claim.id;
            pending.push(PendingKey {
                authorization: claim.authorization,
                event,
                deadline,
            });
            decisions.push(KeyDecision {
                authorization: claim.authorization,
                code: input.code,
                position: InputPosition {
                    frame: watermark,
                    event,
                },
                owner: Some(claim.id),
            });
        }
        self.stage_key_frame(frame, watermark, &pending, cuts, at)?;
        self.decide_key_presses(&decisions, at)?;
        let released = self.pending_keyboard.as_mut().unwrap().release();
        let consumed = released.consumed;
        let start = self.next_ticket;
        self.publish_released_keys(released.reports)?;
        self.pending_keyboard.as_mut().unwrap().last_claim = last;
        self.advance(at)?;
        Ok(KeyProjection {
            consumed,
            tickets: (start..self.next_ticket).map(|ticket| ticket + 1).collect(),
        })
    }
}
