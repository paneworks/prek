//! Bounded native action receipts. Tickets identify attempts, never authority.
use super::{admission::Owner, controls, native};
use crate::protocol::Fence;
use std::{collections::BTreeMap, io, time::Duration};

const PENDING: usize = 8;
const RECORDS: usize = 64;
const GESTURES: usize = 4096;
const RECEIPT_WAIT: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Ticket {
    fence: Fence,
    owner: Owner,
    id: u64,
}
impl Ticket {
    pub fn id(&self) -> u64 {
        self.id
    }
    pub fn fence(&self) -> Fence {
        self.fence
    }
    pub fn output(&self) -> &str {
        &self.owner.output
    }
}

/// Dispatched means controller acceptance, not completed rendering. Uncertain
/// includes lost receipts and retirement after queueing; it never permits retry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    Dispatched,
    Rejected(native::Refusal),
    Unavailable,
    Uncertain,
}
struct Record {
    ticket: Ticket,
    gesture: String,
    wire_ordinal: u32,
    until: Duration,
    outcome: Option<Outcome>,
}
struct Gesture {
    ordinal: u32,
    wire_ordinal: u32,
    action: crate::config::MorfAction,
    ended: bool,
}
#[derive(Default)]
pub(crate) struct Queue {
    next: u64,
    records: BTreeMap<u64, Record>,
    gestures: BTreeMap<String, Gesture>,
    cursor: u64,
}
impl Queue {
    pub fn reserve(
        &mut self,
        fence: Fence,
        owner: Owner,
        request: &controls::Request,
        now: Duration,
        deadline: Duration,
        authorized_until: Duration,
    ) -> io::Result<Ticket> {
        request.validate()?;
        fence.validate()?;
        let epoch = format!("{:016x}{:016x}", fence.broker.0[0], fence.broker.0[1]);
        if !super::admission::valid_owners(std::slice::from_ref(&owner))
            || request.output != owner.output
            || request.epoch != epoch
            || deadline <= now
            || deadline > authorized_until
            || deadline - now > Duration::from_millis(250)
            || self.records.len() >= RECORDS
            || self
                .records
                .values()
                .filter(|record| record.outcome.is_none())
                .count()
                >= PENDING
        {
            return Err(io::Error::other(
                "native action scope, deadline or capacity unavailable",
            ));
        }
        let first = matches!(
            request.phase,
            controls::Phase::Begin | controls::Phase::Discrete
        );
        match self.gestures.get(&request.gesture) {
            Some(old)
                if first
                    || old.ended
                    || old.ordinal >= request.ordinal
                    || old.action != request.action =>
            {
                return Err(io::Error::other("retired or repeated native gesture"));
            }
            None if !first || self.gestures.len() >= GESTURES => {
                return Err(io::Error::other(
                    "native progress has no begin or gesture capacity",
                ));
            }
            _ => {}
        }
        // Worker updates may coalesce. The original ordinal still identifies
        // the grant; Morf receives consecutive submitted attempts.
        let wire_ordinal = self
            .gestures
            .get(&request.gesture)
            .map_or(Some(1), |old| old.wire_ordinal.checked_add(1))
            .filter(|value| *value <= controls::MAX_ORDINAL)
            .ok_or_else(|| io::Error::other("native progress identity exhausted"))?;
        let id = self
            .next
            .checked_add(1)
            .ok_or_else(|| io::Error::other("native action identity exhausted"))?;
        let until = deadline
            .checked_add(RECEIPT_WAIT)
            .ok_or_else(|| io::Error::other("native receipt deadline overflow"))?;
        let ticket = Ticket { fence, owner, id };
        self.next = id;
        self.gestures.insert(
            request.gesture.clone(),
            Gesture {
                ordinal: request.ordinal,
                wire_ordinal,
                action: request.action,
                ended: matches!(
                    request.phase,
                    controls::Phase::Discrete | controls::Phase::End | controls::Phase::Cancel
                ),
            },
        );
        self.records.insert(
            id,
            Record {
                ticket: ticket.clone(),
                gesture: request.gesture.clone(),
                wire_ordinal,
                until,
                outcome: None,
            },
        );
        Ok(ticket)
    }
    pub fn wire_ordinal(&self, ticket: &Ticket) -> u32 {
        self.records[&ticket.id].wire_ordinal
    }
    pub fn submitted(&mut self, id: u64, result: &Result<native::Reply, native::Error>) {
        let record = self.records.get_mut(&id).expect("reserved native action");
        record.outcome = match result {
            Ok(reply) if reply.id() != Some(id) => Some(Outcome::Uncertain),
            Ok(reply) => outcome(reply),
            Err(error) => Some(if error.uncertain {
                Outcome::Uncertain
            } else {
                Outcome::Unavailable
            }),
        };
        if record
            .outcome
            .is_some_and(|outcome| outcome != Outcome::Dispatched)
        {
            self.gestures.get_mut(&record.gesture).unwrap().ended = true;
        }
    }
    /// Round-robin reads prevent an unready first receipt starving later work.
    pub fn next_receipt(&mut self, now: Duration) -> Option<u64> {
        for record in self
            .records
            .values_mut()
            .filter(|record| record.outcome.is_none())
        {
            if now >= record.until {
                record.outcome = Some(Outcome::Uncertain);
                self.gestures.get_mut(&record.gesture).unwrap().ended = true;
            }
        }
        let pending = |record: &&Record| record.outcome.is_none();
        let next = self
            .records
            .range((
                std::ops::Bound::Excluded(self.cursor),
                std::ops::Bound::Unbounded,
            ))
            .map(|(_, record)| record)
            .find(pending)
            .or_else(|| self.records.values().find(pending))?;
        self.cursor = next.ticket.id;
        Some(self.cursor)
    }
    pub fn observed(&mut self, id: u64, reply: &native::Reply) {
        if let Some(record) = self
            .records
            .get_mut(&id)
            .filter(|record| record.outcome.is_none())
        {
            record.outcome = if reply.id() == Some(id) {
                outcome(reply)
            } else {
                Some(Outcome::Uncertain)
            };
            if record
                .outcome
                .is_some_and(|outcome| outcome != Outcome::Dispatched)
            {
                self.gestures.get_mut(&record.gesture).unwrap().ended = true;
            }
        }
    }
    pub fn retire(&mut self) {
        for gesture in self.gestures.values_mut() {
            gesture.ended = true;
        }
        for record in self
            .records
            .values_mut()
            .filter(|record| record.outcome.is_none())
        {
            record.outcome = Some(Outcome::Uncertain);
        }
    }
    pub fn take(&mut self, ticket: &Ticket) -> Option<Outcome> {
        let record = self.records.get(&ticket.id)?;
        if record.ticket != *ticket {
            return None;
        }
        let result = record.outcome?;
        self.records.remove(&ticket.id);
        Some(result)
    }
}
fn outcome(reply: &native::Reply) -> Option<Outcome> {
    match reply.status() {
        native::Status::Queued => None,
        native::Status::Dispatched => Some(Outcome::Dispatched),
        native::Status::Rejected => Some(Outcome::Rejected(
            reply.refusal().expect("validated refusal"),
        )),
        native::Status::Accepted => Some(Outcome::Uncertain),
    }
}

#[cfg(test)]
mod tests;
