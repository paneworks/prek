//! Per-press keyboard ownership and complete-report projection.
use super::*;
use crate::input::{InputPosition, KeyState};

pub(super) mod pending;

const EV_MSC: u16 = 4;
const MSC_SCAN: u16 = 4;

/// Fully recognized press selected by the trusted rule arbitrator.
#[derive(Clone, Copy, Debug)]
pub struct KeyClaim {
    pub id: u64,
    pub authorization: u64,
    pub event: usize,
}

/// Original input coordinates covered by one queued output report.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InputSpan {
    pub sequence: u64,
    pub first: InputPosition,
    pub last: InputPosition,
}

/// Per-press consumption receipt; not authority to execute an action.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConsumedKey {
    pub owner: u64,
    pub code: u16,
    pub state: KeyState,
    pub position: InputPosition,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeyProjection {
    pub consumed: Vec<ConsumedKey>,
    pub tickets: Vec<u64>,
}

#[derive(Clone, Debug, Default)]
pub(super) struct Ledger {
    held: BTreeMap<u16, Press>,
    last_claim: u64,
    watermark: u64,
    output_sequence: u64,
}

#[derive(Clone, Debug)]
struct Press {
    down: bool,
    owner: Option<u64>,
}

struct Report {
    frame: Frame,
    span: InputSpan,
}
struct Plan {
    ledger: Ledger,
    consumed: Vec<ConsumedKey>,
    reports: Vec<Report>,
}

impl Ledger {
    fn plan(
        &self,
        frame: &Frame,
        watermark: u64,
        claims: &[KeyClaim],
        cuts: &BTreeSet<usize>,
    ) -> Result<Plan, Fault> {
        if watermark == 0 || watermark <= self.watermark {
            return Err(Fault::BackwardTime);
        }
        if claims.len() > frame.events.len() || cuts.len() > frame.events.len() {
            return Err(Fault::InvalidCandidate);
        }
        let mut claims_by_event = BTreeMap::new();
        for claim in claims {
            if claim.id == 0
                || claim.id <= self.last_claim
                || claims_by_event.insert(claim.event, claim).is_some()
            {
                return Err(Fault::InvalidCandidate);
            }
        }
        for &index in cuts.iter().chain(claims_by_event.keys()) {
            if frame
                .events
                .get(index)
                .is_none_or(|event| event.kind != EV_KEY)
            {
                return Err(Fault::InvalidCandidate);
            }
        }
        let mut ledger = self.clone();
        let mut consumed = Vec::new();
        let mut dropped = BTreeSet::new();
        for (index, event) in frame
            .events
            .iter()
            .enumerate()
            .filter(|(_, event)| event.kind == EV_KEY)
        {
            if event.code > 0x2ff {
                return Err(Fault::InvalidFrame);
            }
            let claim = claims_by_event.get(&index);
            let was_down = ledger.held.get(&event.code).is_some_and(|press| press.down);
            if claim.is_some() && (event.value != 1 || was_down) {
                return Err(Fault::TooLate);
            }
            let state = match event.value {
                0 => KeyState::Release,
                1 => KeyState::Press,
                2 => KeyState::Repeat,
                _ => return Err(Fault::InvalidFrame),
            };
            let fresh = event.value == 1 && !was_down;
            if fresh {
                if let Some(claim) = claim {
                    if claim.id <= ledger.last_claim {
                        return Err(Fault::InvalidCandidate);
                    }
                    ledger.last_claim = claim.id;
                }
                ledger.held.insert(
                    event.code,
                    Press {
                        down: true,
                        owner: claim.map(|claim| claim.id),
                    },
                );
            }
            if let Some(owner) = ledger.held.get(&event.code).and_then(|press| press.owner) {
                dropped.insert(index);
                if fresh || was_down && event.value != 1 {
                    consumed.push(ConsumedKey {
                        owner,
                        code: event.code,
                        state,
                        position: InputPosition {
                            frame: watermark,
                            event: index,
                        },
                    });
                }
            }
            if event.value == 0 {
                if let Some(press) = ledger.held.get_mut(&event.code) {
                    press.down = false;
                }
                if ledger
                    .held
                    .get(&event.code)
                    .is_some_and(|press| press.owner.is_none())
                {
                    ledger.held.remove(&event.code);
                }
            }
        }
        let transformed = !dropped.is_empty() || !cuts.is_empty();
        if transformed {
            for (index, event) in frame.events[..frame.events.len() - 1].iter().enumerate() {
                if event.kind == EV_MSC && event.code == MSC_SCAN {
                    if frame
                        .events
                        .get(index + 1)
                        .is_none_or(|next| next.kind != EV_KEY)
                    {
                        return Err(Fault::UnsupportedProjection);
                    }
                    if dropped.contains(&(index + 1)) {
                        dropped.insert(index);
                    }
                } else if event.kind != EV_KEY {
                    return Err(Fault::UnsupportedProjection);
                }
            }
        }
        let mut reports = Vec::new();
        if transformed {
            let consuming_cuts: BTreeSet<_> =
                consumed.iter().map(|edge| edge.position.event).collect();
            let mut events = Vec::new();
            let mut first = None;
            let mut last = 0;
            for (index, event) in frame.events[..frame.events.len() - 1].iter().enumerate() {
                if dropped.contains(&index) {
                    if consuming_cuts.contains(&index) || cuts.contains(&index) {
                        Self::report(
                            frame,
                            watermark,
                            &mut events,
                            &mut first,
                            last,
                            &mut reports,
                        );
                    }
                    continue;
                }
                first.get_or_insert(index);
                last = index;
                events.push(*event);
                if cuts.contains(&index) {
                    Self::report(
                        frame,
                        watermark,
                        &mut events,
                        &mut first,
                        last,
                        &mut reports,
                    );
                }
            }
            Self::report(
                frame,
                watermark,
                &mut events,
                &mut first,
                last,
                &mut reports,
            );
        } else {
            reports.push(Report {
                frame: frame.clone(),
                span: InputSpan {
                    sequence: frame.sequence,
                    first: InputPosition {
                        frame: watermark,
                        event: 0,
                    },
                    last: InputPosition {
                        frame: watermark,
                        event: frame.events.len() - 1,
                    },
                },
            });
        }
        for report in &mut reports {
            ledger.output_sequence = ledger
                .output_sequence
                .checked_add(1)
                .ok_or(Fault::Overflow)?
                .max(frame.sequence);
            report.frame.sequence = ledger.output_sequence;
        }
        ledger.watermark = watermark;
        Ok(Plan {
            ledger,
            consumed,
            reports,
        })
    }

    fn report(
        original: &Frame,
        watermark: u64,
        events: &mut Vec<RawEvent>,
        first: &mut Option<usize>,
        last: usize,
        reports: &mut Vec<Report>,
    ) {
        let Some(start) = first.take() else {
            return;
        };
        events.push(*original.events.last().unwrap());
        reports.push(Report {
            frame: Frame {
                source: original.source.clone(),
                at: original.at,
                sequence: original.sequence,
                events: std::mem::take(events),
            },
            span: InputSpan {
                sequence: original.sequence,
                first: InputPosition {
                    frame: watermark,
                    event: start,
                },
                last: InputPosition {
                    frame: watermark,
                    event: last,
                },
            },
        });
    }
}

impl Router {
    /// Project fresh selected presses and drain consumed repeats and releases.
    /// Cuts follow observed key events; consumed key edges create implicit cuts.
    pub fn push_key_frame(
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
        if self.pending_keyboard.is_some() {
            return self.fail(Fault::TooLate);
        }
        if self.keyboard.is_none()
            && (self.last_sequence.is_some() || self.group.is_some() || !self.queue.is_empty())
        {
            return self.fail(Fault::TooLate);
        }
        if claims
            .iter()
            .any(|claim| claim.authorization != self.authorization)
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
        let mut physical = self.physical.clone();
        if let Err(error) = physical.apply(&frame, &self.device) {
            return self.fail(error);
        }
        let mut plan = match self
            .keyboard
            .as_ref()
            .unwrap_or(&Ledger::default())
            .plan(&frame, watermark, claims, cuts)
        {
            Ok(plan) => plan,
            Err(error) => return self.fail(error),
        };
        // Each report carries the persistent values its output still lacks.
        for report in &mut plan.reports {
            let repairs = self
                .physical_absolute
                .restore_for(&self.planned_absolute, &report.frame);
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
        if self
            .queue
            .len()
            .checked_add(plan.reports.len())
            .is_none_or(|count| count > self.limits.frames)
            || self
                .queue
                .iter()
                .map(|entry| entry.frame.events.len())
                .sum::<usize>()
                .checked_add(
                    plan.reports
                        .iter()
                        .map(|report| report.frame.events.len())
                        .sum(),
                )
                .is_none_or(|count| count > self.limits.events)
            || self
                .next_ticket
                .checked_add(plan.reports.len() as u64)
                .is_none()
        {
            return self.fail(Fault::Overflow);
        }
        let mut tickets = Vec::with_capacity(plan.reports.len());
        for report in plan.reports {
            self.schedule(report.frame)?;
            self.positions.insert(self.next_ticket, report.span);
            tickets.push(self.next_ticket);
        }
        let physical_absolute = Arc::make_mut(&mut self.physical_absolute);
        for event in &frame.events {
            physical_absolute.update(*event);
        }
        self.physical = physical;
        self.last_sequence = Some(frame.sequence);
        self.keyboard = Some(plan.ledger);
        self.advance(at)?;
        Ok(KeyProjection {
            consumed: plan.consumed,
            tickets,
        })
    }

    pub fn input_span(&self, ticket: u64) -> Option<InputSpan> {
        self.positions.get(&ticket).copied()
    }

    pub fn position_prefix_delivered(&self, position: InputPosition, inclusive: bool) -> bool {
        !self.stopped
            && self
                .pending_keyboard
                .as_ref()
                .and_then(|journal| journal.first())
                .is_none_or(|first| first > position || !inclusive && first == position)
            && self.queue.iter().all(|entry| {
                self.positions.get(&entry.ticket).is_some_and(|span| {
                    span.first > position || !inclusive && span.first == position
                })
            })
            && self.group.as_ref().is_none_or(|group| {
                group.ownership != Ownership::Pending
                    || group.pending.iter().all(|frame| {
                        self.input_marks
                            .get(&frame.sequence)
                            .is_some_and(|&watermark| {
                                let first = InputPosition {
                                    frame: watermark,
                                    event: 0,
                                };
                                first > position || !inclusive && first == position
                            })
                    })
            })
    }

    pub(crate) fn mark_input(&mut self, sequence: u64, watermark: u64) -> Result<(), Fault> {
        if self.stopped {
            return Err(Fault::Stopped);
        }
        if watermark == 0
            || self.keyboard.is_some()
            || self.pending_keyboard.is_some()
            || self.last_sequence.is_some_and(|last| sequence <= last)
            || self.input_marks.contains_key(&sequence)
        {
            return self.fail(Fault::InvalidCandidate);
        }
        if self.input_marks.len() > self.limits.frames {
            return self.fail(Fault::Overflow);
        }
        self.input_marks.insert(sequence, watermark);
        Ok(())
    }

    pub(super) fn prune_input_marks(&mut self) {
        self.input_marks.retain(|sequence, _| {
            self.group.as_ref().is_some_and(|group| {
                group
                    .pending
                    .iter()
                    .any(|frame| frame.sequence == *sequence)
            })
        });
    }
}

#[cfg(test)]
mod tests;
