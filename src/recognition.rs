//! Bounded tickets for asynchronous recognizer proposals.
//!
//! A trusted input coordinator supplies eligibility and its current ownership.
//! Validating a reply does not claim input or grant an effect: the coordinator
//! must still apply its ordinary Rust arbitration and action permission checks.
use crate::{
    config::Millis,
    input::{Change, Source},
    input_events::Event,
    protocol::Fence,
    router::Ownership,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    time::Duration,
};

pub const MAX_RECOGNIZERS: usize = 64;
pub const MAX_PENDING: usize = 64;
pub const MAX_BYTES: usize = 1024 * 1024;
pub const MAX_FRAME_BYTES: usize = 64 * 1024;
pub const MAX_DATA_BYTES: usize = 4096;
pub const MAX_REPLY: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    Key,
    Relative,
    Wheel,
    Absolute,
    Switch,
    Touch,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Definition {
    pub name: String,
    pub selectors: BTreeSet<String>,
    pub inputs: BTreeSet<InputKind>,
    pub reply_within: Millis,
    #[serde(default)]
    pub continuous: bool,
}
fn name(text: &str) -> bool {
    !text.is_empty() && text.len() <= 128 && !text.chars().any(char::is_control)
}
fn selector(text: &str) -> bool {
    !text.is_empty() && text.len() <= 256 && !text.chars().any(char::is_control)
}
impl Definition {
    pub fn validate(&self) -> io::Result<()> {
        if !name(&self.name)
            || self.selectors.is_empty()
            || self.selectors.len() > 64
            || self.selectors.iter().any(|value| !selector(value))
            || self.inputs.is_empty()
            || self.reply_within.0 == 0
            || self.reply_within.duration() > MAX_REPLY
        {
            return Err(invalid("invalid recognizer declaration"));
        }
        Ok(())
    }
    fn project(&self, input: &Event, require_change: bool) -> io::Result<Event> {
        let Event::Frame {
            source,
            at,
            sequence,
            selectors,
            changes,
            contacts,
            contacts_started,
            contacts_ended,
            ended_contacts,
        } = input
        else {
            return Err(invalid("recognizer tickets require a normalized frame"));
        };
        let selectors: Vec<_> = selectors
            .iter()
            .filter(|selector| self.selectors.contains(*selector))
            .cloned()
            .collect();
        if selectors.is_empty() {
            return Err(invalid("source is outside declared recognizer selectors"));
        }
        let touch = self.inputs.contains(&InputKind::Touch);
        let changes: Vec<_> = changes
            .iter()
            .filter(|change| {
                self.inputs.contains(&match change {
                    Change::Key { .. } => InputKind::Key,
                    Change::Relative { .. } => InputKind::Relative,
                    Change::Wheel { .. } => InputKind::Wheel,
                    Change::Absolute { .. } => InputKind::Absolute,
                    Change::Switch { .. } => InputKind::Switch,
                })
            })
            .cloned()
            .collect();
        if changes
            .iter()
            .any(|change| matches!(change, Change::Wheel { steps, .. } if !steps.is_finite()))
        {
            return Err(invalid("nonfinite recognizer wheel input"));
        }
        if require_change
            && changes.is_empty()
            && !(touch
                && (!contacts.is_empty()
                    || !contacts_started.is_empty()
                    || !contacts_ended.is_empty()))
        {
            return Err(invalid("frame has no declared recognizer input"));
        }
        Ok(Event::Frame {
            source: source.clone(),
            at: *at,
            sequence: *sequence,
            selectors,
            changes,
            contacts: if touch { contacts.clone() } else { vec![] },
            contacts_started: if touch {
                contacts_started.clone()
            } else {
                vec![]
            },
            contacts_ended: if touch {
                contacts_ended.clone()
            } else {
                vec![]
            },
            ended_contacts: if touch {
                ended_contacts.clone()
            } else {
                vec![]
            },
        })
    }
}

/// Trusted coordinator state, never deserialized from a session request.
/// Revision changes when eligibility/target/contact membership changes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Candidate {
    pub id: u64,
    pub revision: u64,
    pub rule: String,
    pub source: Source,
    pub ownership: Option<Ownership>,
    /// A claimed source must identify this exact candidate as its winner.
    pub winner: Option<u64>,
    pub expires: Duration,
}
impl Candidate {
    fn valid(&self, now: Duration) -> bool {
        self.id > 0
            && self.revision > 0
            && selector(&self.rule)
            && self.source.generation > 0
            && selector(&self.source.seat)
            && self.expires > now
            && match self.ownership {
                None | Some(Ownership::Pending) => self.winner.is_none(),
                Some(Ownership::Claimed) => self.winner == Some(self.id),
                _ => false,
            }
    }
    fn same(&self, other: &Self) -> bool {
        self.id == other.id
            && self.revision == other.revision
            && self.rule == other.rule
            && self.source == other.source
    }
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Ticket {
    pub fence: Fence,
    pub id: u64,
    pub candidate: u64,
    pub revision: u64,
    pub rule: String,
    pub recognizer: String,
    pub cause: Cause,
    pub requested_at: Duration,
    pub deadline: Duration,
    pub input: Event,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Cause {
    Frame,
    Timer,
}
impl Ticket {
    /// Validate the delivery envelope, independently of current rule authority.
    /// Only Gate can establish eligibility and ownership for the response.
    pub fn validate_for(&self, definition: &Definition, now: Duration) -> io::Result<()> {
        self.fence.validate()?;
        definition.validate()?;
        self.input.validate()?;
        if self.id == 0
            || self.candidate == 0
            || self.revision == 0
            || !selector(&self.rule)
            || self.recognizer != definition.name
            || self.requested_at > now
            || now >= self.deadline
            || self.requested_at < self.input.at()
            || self.deadline
                > self
                    .requested_at
                    .saturating_add(definition.reply_within.duration())
            || definition.project(&self.input, self.cause == Cause::Frame)? != self.input
        {
            return Err(invalid("invalid or expired recognizer ticket"));
        }
        match &self.input {
            Event::Frame {
                changes,
                contacts,
                contacts_started,
                contacts_ended,
                ended_contacts,
                ..
            } if self.cause == Cause::Timer => {
                if !changes.is_empty()
                    || contacts.iter().any(|contact| contact.fresh)
                    || !contacts_started.is_empty()
                    || !contacts_ended.is_empty()
                    || !ended_contacts.is_empty()
                {
                    return Err(invalid("timer ticket cannot repeat input transitions"));
                }
            }
            _ if self.requested_at != self.input.at() || !self.input.fresh(now) => {
                return Err(invalid("frame ticket has a changed observation time"));
            }
            _ => {}
        }
        encoded_size(self, MAX_FRAME_BYTES)?;
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Pending,
    Discrete,
    Begin,
    Update,
    End,
    Cancel,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Proposal {
    pub fence: Fence,
    pub ticket: u64,
    pub phase: Phase,
    #[serde(default)]
    pub data: Value,
}
/// Validated recognition only. This is not a routing claim or action grant.
#[derive(Clone, Debug, PartialEq)]
pub struct Validated {
    pub candidate: Candidate,
    pub recognizer: String,
    pub ordinal: u64,
    pub phase: Phase,
    pub data: Value,
    pub observed_at: Duration,
}
struct Pending {
    ticket: Ticket,
    candidate: Candidate,
    bytes: usize,
}
struct History {
    candidate: Candidate,
    recognizer: String,
    last_frame: u64,
    last_input: Event,
    last_request: Duration,
    last_reply: u64,
    ordinal: u64,
    progress: bool,
    terminal: bool,
}
pub struct Gate {
    fence: Fence,
    definitions: BTreeMap<String, Definition>,
    next: u64,
    pending: BTreeMap<u64, Pending>,
    active: BTreeMap<u64, History>,
    bytes: usize,
}
impl Gate {
    pub fn new(fence: Fence, definitions: Vec<Definition>) -> io::Result<Self> {
        fence.validate()?;
        if definitions.len() > MAX_RECOGNIZERS {
            return Err(invalid("too many recognizer declarations"));
        }
        let mut map = BTreeMap::new();
        for definition in definitions {
            definition.validate()?;
            if map.insert(definition.name.clone(), definition).is_some() {
                return Err(invalid("duplicate recognizer name"));
            }
        }
        Ok(Self {
            fence,
            definitions: map,
            next: 0,
            pending: BTreeMap::new(),
            active: BTreeMap::new(),
            bytes: 0,
        })
    }
    pub fn offer(
        &mut self,
        recognizer: &str,
        candidate: &Candidate,
        input: &Event,
        now: Duration,
    ) -> io::Result<Ticket> {
        self.expire(now);
        input.validate()?;
        if !candidate.valid(now) || input.source() != &candidate.source || !input.fresh(now) {
            return Err(invalid("recognizer candidate/source is unavailable"));
        }
        let definition = self
            .definitions
            .get(recognizer)
            .ok_or_else(|| invalid("unregistered recognizer"))?;
        let input = definition.project(input, true)?;
        let Event::Frame { sequence, .. } = &input else {
            unreachable!()
        };
        let sequence = *sequence;
        if self.active.get(&candidate.id).is_some_and(|history| {
            !history.candidate.same(candidate)
                || history.recognizer != recognizer
                || history.candidate.ownership.is_none() != candidate.ownership.is_none()
                || history.terminal
                || history.last_frame >= sequence
                || history.last_request > input.at()
        }) {
            return Err(invalid(
                "recognizer candidate changed or frame was replayed",
            ));
        }
        if !self.active.contains_key(&candidate.id) && self.active.len() >= MAX_PENDING {
            return Err(invalid("too many active recognizer candidates"));
        }
        self.issue(recognizer, candidate, input, Cause::Frame, now)
    }
    /// Wake an existing eligible candidate without inventing a physical frame.
    /// The coordinator must recheck held state and eligibility before calling.
    pub fn offer_timer(
        &mut self,
        recognizer: &str,
        candidate: &Candidate,
        now: Duration,
    ) -> io::Result<Ticket> {
        self.expire(now);
        let history = self
            .active
            .get(&candidate.id)
            .ok_or_else(|| invalid("timer requires an existing recognizer candidate"))?;
        if !candidate.valid(now)
            || !history.candidate.same(candidate)
            || history.recognizer != recognizer
            || history.terminal
            || history.candidate.ownership.is_none() != candidate.ownership.is_none()
            || now <= history.last_request
        {
            return Err(invalid("timer candidate changed or wake was replayed"));
        }
        let mut input = history.last_input.clone();
        let Event::Frame {
            changes,
            contacts,
            contacts_started,
            contacts_ended,
            ended_contacts,
            ..
        } = &mut input
        else {
            unreachable!()
        };
        changes.clear();
        contacts_started.clear();
        contacts_ended.clear();
        ended_contacts.clear();
        for contact in contacts {
            contact.fresh = false;
        }
        self.issue(recognizer, candidate, input, Cause::Timer, now)
    }
    fn issue(
        &mut self,
        recognizer: &str,
        candidate: &Candidate,
        input: Event,
        cause: Cause,
        now: Duration,
    ) -> io::Result<Ticket> {
        let requested_at = if cause == Cause::Frame {
            input.at()
        } else {
            now
        };
        let definition = self.definitions.get(recognizer).unwrap();
        let deadline = requested_at
            .checked_add(definition.reply_within.duration())
            .ok_or_else(|| invalid("recognizer deadline overflow"))?
            .min(candidate.expires);
        if deadline <= now {
            return Err(invalid("recognizer response window expired"));
        }
        let id = self
            .next
            .checked_add(1)
            .ok_or_else(|| invalid("recognizer ticket exhausted"))?;
        let ticket = Ticket {
            fence: self.fence,
            id,
            candidate: candidate.id,
            revision: candidate.revision,
            rule: candidate.rule.clone(),
            recognizer: recognizer.into(),
            cause,
            requested_at,
            deadline,
            input,
        };
        let bytes = encoded_size(&ticket, MAX_FRAME_BYTES)?;
        if self.pending.len() >= MAX_PENDING || self.bytes + bytes > MAX_BYTES {
            return Err(invalid("recognizer ticket queue full"));
        }
        let history = self.active.entry(candidate.id).or_insert(History {
            candidate: candidate.clone(),
            recognizer: recognizer.into(),
            last_frame: 0,
            last_input: ticket.input.clone(),
            last_request: requested_at,
            last_reply: 0,
            ordinal: 0,
            progress: false,
            terminal: false,
        });
        if let Event::Frame { sequence, .. } = &ticket.input {
            history.last_frame = *sequence;
        }
        if cause == Cause::Frame {
            history.last_input = ticket.input.clone();
        }
        history.last_request = requested_at;
        self.next = id;
        self.bytes += bytes;
        self.pending.insert(
            id,
            Pending {
                ticket: ticket.clone(),
                candidate: candidate.clone(),
                bytes,
            },
        );
        Ok(ticket)
    }
    /// Recheck trusted current state immediately before ordinary arbitration.
    /// Every known ticket is consumed even when its response is rejected.
    pub fn validate(
        &mut self,
        proposal: Proposal,
        current: &Candidate,
        now: Duration,
    ) -> io::Result<Option<Validated>> {
        if proposal.fence != self.fence {
            return Err(invalid("recognizer proposal has a foreign fence"));
        }
        let pending = self
            .pending
            .remove(&proposal.ticket)
            .ok_or_else(|| invalid("unknown recognizer ticket"))?;
        self.bytes -= pending.bytes;
        let ticket = pending.ticket;
        if now < ticket.requested_at
            || now >= ticket.deadline
            || !current.valid(now)
            || !pending.candidate.same(current)
            || pending.candidate.ownership.is_none() != current.ownership.is_none()
        {
            return Err(invalid("stale recognizer source, target or ownership"));
        }
        validate_data(&proposal.data)?;
        let definition = self.definitions.get(&ticket.recognizer).unwrap();
        let history = self
            .active
            .get_mut(&current.id)
            .ok_or_else(|| invalid("recognizer candidate retired"))?;
        if history.terminal || proposal.ticket <= history.last_reply {
            return Err(invalid(
                "recognizer reply was reordered or already completed",
            ));
        }
        let allowed = match proposal.phase {
            Phase::Pending | Phase::Cancel => true,
            Phase::Discrete => !definition.continuous && !history.progress,
            Phase::Begin => definition.continuous && !history.progress,
            Phase::Update | Phase::End => definition.continuous && history.progress,
        };
        if !allowed {
            return Err(invalid("invalid recognizer phase transition"));
        }
        history.last_reply = proposal.ticket;
        if proposal.phase == Phase::Pending {
            return Ok(None);
        }
        history.ordinal = history
            .ordinal
            .checked_add(1)
            .ok_or_else(|| invalid("recognizer ordinal exhausted"))?;
        history.progress = !matches!(proposal.phase, Phase::Discrete | Phase::End | Phase::Cancel);
        history.terminal = !history.progress;
        let ordinal = history.ordinal;
        if history.terminal {
            self.discard_tickets(current.id);
        }
        Ok(Some(Validated {
            candidate: current.clone(),
            recognizer: ticket.recognizer,
            ordinal,
            phase: proposal.phase,
            data: proposal.data,
            observed_at: ticket.requested_at,
        }))
    }
    fn discard_tickets(&mut self, candidate: u64) {
        self.pending.retain(|_, pending| {
            if pending.candidate.id == candidate {
                self.bytes -= pending.bytes;
                false
            } else {
                true
            }
        });
    }
    pub fn retire(&mut self, candidate: u64) {
        self.discard_tickets(candidate);
        self.active.remove(&candidate);
    }
    pub fn retire_source(&mut self, source: &Source) {
        let ids: Vec<_> = self
            .active
            .iter()
            .filter_map(|(id, history)| (&history.candidate.source == source).then_some(*id))
            .collect();
        for id in ids {
            self.retire(id);
        }
    }
    pub fn expire(&mut self, now: Duration) {
        self.pending.retain(|_, pending| {
            if pending.ticket.deadline <= now {
                self.bytes -= pending.bytes;
                false
            } else {
                true
            }
        });
        let ids: Vec<_> = self
            .active
            .iter()
            .filter_map(|(id, history)| (history.candidate.expires <= now).then_some(*id))
            .collect();
        for id in ids {
            self.retire(id);
        }
    }
    pub fn pending(&self) -> usize {
        self.pending.len()
    }
}
fn encoded_size(value: &impl Serialize, maximum: usize) -> io::Result<usize> {
    struct Count {
        bytes: usize,
        maximum: usize,
    }
    impl io::Write for Count {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.maximum.saturating_sub(self.bytes) {
                return Err(invalid("recognizer value exceeds byte limit"));
            }
            self.bytes += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count { bytes: 0, maximum };
    serde_json::to_writer(&mut count, value)?;
    Ok(count.bytes)
}
pub fn validate_data(value: &Value) -> io::Result<()> {
    fn depth(value: &Value, level: usize, visited: &mut usize) -> bool {
        *visited += 1;
        level <= 8
            && *visited <= MAX_DATA_BYTES
            && match value {
                Value::Array(values) => values.iter().all(|value| depth(value, level + 1, visited)),
                Value::Object(fields) => fields
                    .values()
                    .all(|value| depth(value, level + 1, visited)),
                _ => true,
            }
    }
    if !depth(value, 0, &mut 0) {
        return Err(invalid("recognizer data exceeds nesting/value limit"));
    }
    encoded_size(value, MAX_DATA_BYTES)?;
    Ok(())
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests;
