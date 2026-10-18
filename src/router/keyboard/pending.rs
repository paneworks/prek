use super::*;

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
    watermark: u64,
    cursor: usize,
    positions: Vec<Option<InputPosition>>,
    edges: BTreeSet<usize>,
    cuts: BTreeSet<usize>,
}
#[derive(Default)]
pub(in crate::router) struct Journal {
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
    pub(in crate::router) fn storage(&self) -> (usize, usize) {
        (
            self.frames.len(),
            self.frames
                .iter()
                .map(|input| input.frame.events.len())
                .sum(),
        )
    }
    pub(in crate::router) fn first(&self) -> Option<InputPosition> {
        self.frames.front().map(|input| InputPosition {
            frame: input.watermark,
            event: input.cursor,
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
            .min()
    }
    pub(super) fn can_release_before(&self, barrier: Option<InputPosition>) -> bool {
        let Some(input) = self.frames.front() else {
            return false;
        };
        let first = InputPosition {
            frame: input.watermark,
            event: input.cursor,
        };
        barrier.is_none_or(|barrier| first < barrier)
            && input
                .positions
                .get(input.cursor)
                .and_then(|position| *position)
                .and_then(|position| self.presses.get(&position))
                .is_none_or(|press| {
                    !matches!(press.choice.disposition, KeyDisposition::Pending { .. })
                })
    }
    pub(super) fn disposition(&self, code: u16, position: InputPosition) -> Option<KeyDisposition> {
        self.presses
            .get(&position)
            .filter(|press| press.code == code)
            .map(|press| press.choice.disposition)
    }
    pub(super) fn stage(
        &mut self,
        frame: Frame,
        watermark: u64,
        pending: &[PendingKey],
        cuts: &BTreeSet<usize>,
        offset: Duration,
    ) -> Result<(), Fault> {
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
        }
        let mut held = self.held.clone();
        let mut positions = vec![None; frame.events.len()];
        let mut edges = BTreeSet::new();
        let mut additions = Vec::new();
        for (index, event) in frame
            .events
            .iter()
            .enumerate()
            .filter(|(_, event)| event.kind == EV_KEY)
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
        let transformed = !pending.is_empty()
            || !cuts.is_empty()
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
            } else if event.kind != EV_KEY && transformed {
                return Err(Fault::UnsupportedProjection);
            }
        }
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
        self.frames.push_back(Input {
            frame,
            watermark,
            cursor: 0,
            positions,
            edges,
            cuts: cuts.clone(),
        });
        Ok(())
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
            match (press.choice.disposition, decision.owner) {
                (KeyDisposition::Pending { deadline }, Some(_)) if at > deadline => {
                    return Err(Fault::TooLate);
                }
                (KeyDisposition::Pending { .. }, _) | (KeyDisposition::Ordinary, None) => {}
                (KeyDisposition::Consumed { owner }, Some(wanted)) if owner == wanted => {}
                _ => return Err(Fault::TooLate),
            }
            if at < press.choice.at {
                return Err(Fault::BackwardTime);
            }
        }
        Ok(())
    }
    pub(super) fn decide(&mut self, decisions: &[KeyDecision], at: Duration) -> Result<(), Fault> {
        self.validate_decisions(decisions, at)?;
        for decision in decisions {
            let press = self.presses.get_mut(&decision.position).unwrap();
            if let KeyDisposition::Pending { deadline } = press.choice.disposition {
                press.choice = Choice {
                    disposition: decision.owner.map_or(KeyDisposition::Ordinary, |owner| {
                        KeyDisposition::Consumed { owner }
                    }),
                    at: at.min(deadline),
                };
            }
        }
        Ok(())
    }
    pub(super) fn expire(&mut self, at: Duration) {
        for press in self.presses.values_mut() {
            if let KeyDisposition::Pending { deadline } = press.choice.disposition
                && at >= deadline
            {
                press.choice = Choice {
                    disposition: KeyDisposition::Ordinary,
                    at: deadline,
                };
            }
        }
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
                    event: input.cursor,
                } >= barrier
            }) {
                break;
            }
            let mut events = Vec::new();
            let mut first = None;
            let mut last = input.cursor;
            let start = input.cursor;
            let mut changed = start != 0 || !input.cuts.is_empty();
            while input.cursor + 1 < input.frame.events.len() {
                if barrier.is_some_and(|barrier| {
                    InputPosition {
                        frame: input.watermark,
                        event: input.cursor,
                    } >= barrier
                }) {
                    break;
                }
                let index = input.cursor;
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
            let finished = input.cursor + 1 == input.frame.events.len();
            if finished && !changed && start == 0 {
                reports.push(ReadyReport {
                    ready: self.clock.max(input.frame.at),
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
            }
            if finished {
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
        if self.frames.is_empty() && self.held.values().all(|binding| !binding.down) {
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
        reports.extend(raw.into_iter().map(|report| ReadyReport { report, ready }));
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
        if self
            .device
            .capabilities
            .keys()
            .any(|kind| matches!(*kind, EV_ABS | EV_REL | EV_SW))
        {
            return self.fail(Fault::UnsupportedProjection);
        }
        self.capacity(frame.events.len())?;
        let mut physical = self.physical.clone();
        if let Err(error) = physical.apply(&frame, &self.device) {
            return self.fail(error);
        }
        let sequence = frame.sequence;
        let journal = self.pending_keyboard.get_or_insert_with(Journal::default);
        if let Err(error) = journal.stage(frame, watermark, pending, cuts, self.limits.offset) {
            return self.fail(error);
        }
        self.physical = physical;
        self.last_sequence = Some(sequence);
        self.last_at = at;
        Ok(())
    }

    /// Atomically settle exact original downs before advancing their decision deadlines.
    /// Consumed input stays consumed; ordinary decisions cannot be reclaimed.
    pub fn decide_key_presses(
        &mut self,
        decisions: &[KeyDecision],
        at: Duration,
    ) -> Result<(), Fault> {
        if self.stopped {
            return Err(Fault::Stopped);
        }
        if decisions
            .iter()
            .any(|decision| decision.authorization != self.authorization)
        {
            return self.fail(Fault::Unauthorized);
        }
        if at < self.last_at {
            return self.fail(Fault::BackwardTime);
        }
        self.pending_keyboard
            .as_mut()
            .ok_or(Fault::WrongTicket)?
            .decide(decisions, at)?;
        self.last_at = at;
        Ok(())
    }

    pub(crate) fn validate_key_decisions(
        &self,
        decisions: &[KeyDecision],
        input_at: Duration,
        admitted_at: Duration,
    ) -> Result<(), Fault> {
        if self.stopped {
            return Err(Fault::Stopped);
        }
        if decisions
            .iter()
            .any(|decision| decision.authorization != self.authorization)
        {
            return Err(Fault::Unauthorized);
        }
        if admitted_at < input_at || input_at < self.last_at {
            return Err(Fault::BackwardTime);
        }
        self.pending_keyboard
            .as_ref()
            .ok_or(Fault::WrongTicket)?
            .validate_decisions(decisions, admitted_at)
    }
    pub(crate) fn decide_keys_clocked(
        &mut self,
        decisions: &[KeyDecision],
        input_at: Duration,
        admitted_at: Duration,
    ) -> Result<(), Fault> {
        self.validate_key_decisions(decisions, input_at, admitted_at)?;
        self.pending_keyboard
            .as_mut()
            .unwrap()
            .decide(decisions, admitted_at)?;
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
            && self
                .pending_keyboard
                .as_ref()
                .is_none_or(|journal| journal.first().is_none())
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
        for ready in &mut reports {
            let report = &mut ready.report;
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
