//! A separately supervised, unprivileged event VM. The session parent keeps
//! processing broker leases and generation changes while Lua executes.
use super::{CapabilityView, Definition, EventKind, LogRecord, StatusView, source_transfer};
use crate::{
    ipc::{Channel, monotonic},
    protocol::{self, Capability, Fence},
    supervisor::{Heartbeat, Supervisor},
};
use nix::{
    libc,
    poll::{PollFd, PollFlags, PollTimeout, poll},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeSet, VecDeque},
    io,
    os::fd::AsFd,
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};

const MAX_PENDING: usize = 64;
const MAX_BYTES: usize = 512 * 1024;
const RESPONSE_TIMEOUT: Duration = Duration::from_millis(250);
#[derive(Default)]
pub struct PendingEvents {
    entries: VecDeque<(EventKind, serde_json::Value, usize)>,
    bytes: usize,
}
impl PendingEvents {
    pub fn push(&mut self, event: EventKind, payload: serde_json::Value) -> io::Result<()> {
        let bytes = super::runtime::bounded_json(&payload, 63 * 1024)?.len();
        if (event == EventKind::Update || event == EventKind::Matched)
            && payload["event"]["stage"] == "update"
        {
            self.entries.retain(|(old, previous, _)| {
                !(*old == event
                    && previous["event"]["stage"] == "update"
                    && previous["event"]["id"] == payload["event"]["id"]
                    && previous["event"]["source"] == payload["event"]["source"])
            });
            self.bytes = self.entries.iter().map(|(_, _, bytes)| bytes).sum();
        }
        if self.entries.len() >= MAX_PENDING || self.bytes + bytes > MAX_BYTES {
            return Err(protocol::invalid(
                "Lua source-loading event queue exceeds limit",
            ));
        }
        self.bytes += bytes;
        self.entries.push_back((event, payload, bytes));
        Ok(())
    }
    pub fn drain_into(mut self, session: &mut Session) -> io::Result<()> {
        for (event, payload, _) in self.entries.drain(..) {
            session.notify(event, payload)?;
        }
        Ok(())
    }
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Work {
    Capabilities {
        view: CapabilityView,
        status: Option<StatusView>,
        id: u64,
    },
    RulesEnable {
        fence: Fence,
    },
    RuleReceipt {
        fence: Fence,
        receipt: super::RuleReceipt,
    },
    Context {
        fence: Fence,
        offset: usize,
        bytes: usize,
        #[serde(with = "super::source_transfer::chunk_bytes")]
        data: Vec<u8>,
    },
    Action {
        fence: Fence,
        sequence: u64,
        event: Box<crate::rules::Event>,
        expires: Duration,
    },
    Source {
        fence: Fence,
        offset: usize,
        bytes: usize,
        #[serde(with = "super::source_transfer::chunk_bytes")]
        data: Vec<u8>,
    },
    Event {
        fence: Fence,
        sequence: u64,
        event: EventKind,
        payload: serde_json::Value,
    },
    Recognition {
        fence: Fence,
        sequence: u64,
        ticket: Box<crate::recognition::Ticket>,
    },
    CancelRecognition {
        fence: Fence,
        sequence: u64,
        cancellation: crate::recognition::Cancellation,
    },
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Message {
    Capabilities {
        fence: Fence,
        id: u64,
    },
    RulesEnabled {
        fence: Fence,
    },
    RuleProposal {
        fence: Fence,
        proposal: super::RuleProposal,
    },
    RulesApplied {
        fence: Fence,
        id: u64,
        logs: Vec<LogRecord>,
    },
    ContextLogs {
        fence: Fence,
        logs: Vec<LogRecord>,
    },
    Resolved {
        fence: Fence,
        sequence: u64,
        id: u64,
        ordinal: u64,
        result: Option<super::ActionResult>,
    },
    Ready {
        fence: Fence,
    },
    Logs {
        fence: Fence,
        sequence: u64,
        logs: Vec<LogRecord>,
    },
    Recognized {
        fence: Fence,
        sequence: u64,
        ticket: u64,
        proposal: Option<crate::recognition::Proposal>,
        logs: Vec<LogRecord>,
    },
    RecognitionCancelled {
        fence: Fence,
        sequence: u64,
        candidate: u64,
        through: u64,
        logs: Vec<LogRecord>,
    },
    Timers {
        fence: Fence,
        logs: Vec<LogRecord>,
    },
    Failed {
        fence: Fence,
        reason: String,
    },
}
struct Notice {
    event: EventKind,
    payload: serde_json::Value,
    bytes: usize,
    recognition: Option<Box<crate::recognition::Ticket>>,
}
struct RulePending {
    id: u64,
    revision: u64,
    expires: Duration,
    sent: bool,
}
pub struct Session {
    supervisor: Supervisor,
    channel: Channel,
    fence: Fence,
    source: Vec<u8>,
    seat: String,
    capabilities_next: Option<CapabilityView>,
    capabilities_pending: Option<(u64, Duration)>,
    capabilities_id: u64,
    capabilities_last: Option<CapabilityView>,
    status_last: Option<StatusView>,
    context_source: Vec<u8>,
    context_offset: usize,
    context_next: Option<Vec<u8>>,
    context_pending: Option<Duration>,
    context_started: Option<Duration>,
    offset: usize,
    ready: bool,
    sequence: u64,
    pending: Option<(u64, Duration)>,
    pending_event: Option<EventKind>,
    pending_action: Option<(u64, u64)>,
    pending_recognition: Option<u64>,
    recognition_cancelled: bool,
    recognition_candidates: std::collections::BTreeMap<u64, crate::recognition::Cancellation>,
    recognition_tickets: std::collections::BTreeMap<u64, u64>,
    cancellations: VecDeque<crate::recognition::Cancellation>,
    pending_cancellation: Option<crate::recognition::Cancellation>,
    cancelled_recognitions: VecDeque<crate::recognition::Cancellation>,
    recognition_last: u64,
    recognizers: std::collections::BTreeMap<String, crate::recognition::Definition>,
    recognitions: VecDeque<(u64, Option<crate::recognition::Proposal>)>,
    initialized: bool,
    actions: VecDeque<(Box<crate::rules::Event>, Duration, usize)>,
    resolved: VecDeque<(u64, u64, Option<super::ActionResult>)>,
    notices: VecDeque<Notice>,
    bytes: usize,
    expires: Duration,
    rule_mutations: bool,
    rules_ready: bool,
    rules_handshake: Option<Duration>,
    rule_pending: Option<RulePending>,
    rule_receipt: Option<super::RuleReceipt>,
    rule_proposals: VecDeque<super::RuleProposal>,
    stopping: bool,
}
impl Session {
    pub fn spawn(
        executable: &Path,
        fence: Fence,
        seat: &str,
        definition: Definition,
    ) -> io::Result<Self> {
        Self::spawn_transport(executable, fence, seat, definition, false)
    }
    /// The embedding must already own a live authorized rule transport. This
    /// explicit opt-in is never inferred from the Lua source or its callbacks.
    pub fn spawn_with_rule_transactions(
        executable: &Path,
        fence: Fence,
        seat: &str,
        definition: Definition,
    ) -> io::Result<Self> {
        Self::spawn_transport(executable, fence, seat, definition, true)
    }
    fn spawn_transport(
        executable: &Path,
        fence: Fence,
        seat: &str,
        definition: Definition,
        rule_mutations: bool,
    ) -> io::Result<Self> {
        if unsafe { libc::geteuid() } == 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Lua session runtime must run as the unprivileged session user",
            ));
        }
        fence.validate()?;
        definition.validate().map_err(io::Error::other)?;
        let source = serde_json::to_vec(&definition).map_err(io::Error::other)?;
        if source.len() > super::MAX_SNAPSHOT_BYTES {
            return Err(protocol::invalid("Lua runtime snapshot exceeds byte limit"));
        }
        let mut command = Command::new(executable);
        #[cfg(debug_assertions)]
        if let Some(runner) = std::env::var_os("PREK_TEST_RUNNER") {
            command = Command::new(runner);
            command.arg(executable);
        }
        command
            .arg("lua-runtime-worker")
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let (supervisor, channel) = Supervisor::spawn_controlled(command)?;
        let mut session = Self {
            supervisor,
            channel,
            fence,
            source,
            seat: seat.into(),
            capabilities_next: None,
            capabilities_pending: None,
            capabilities_id: 0,
            capabilities_last: None,
            status_last: None,
            context_source: Vec::new(),
            context_offset: 0,
            context_next: None,
            context_pending: None,
            context_started: None,
            offset: 0,
            ready: false,
            sequence: 0,
            pending: None,
            pending_event: None,
            pending_action: None,
            pending_recognition: None,
            recognition_cancelled: false,
            recognition_candidates: Default::default(),
            recognition_tickets: Default::default(),
            cancellations: VecDeque::new(),
            pending_cancellation: None,
            cancelled_recognitions: VecDeque::new(),
            recognition_last: 0,
            recognizers: definition
                .recognizers
                .iter()
                .map(|binding| (binding.definition.name.clone(), binding.definition.clone()))
                .collect(),
            recognitions: VecDeque::new(),
            initialized: false,
            actions: VecDeque::new(),
            resolved: VecDeque::new(),
            notices: VecDeque::new(),
            bytes: 0,
            expires: monotonic()? + source_transfer::TIMEOUT,
            rule_mutations,
            rules_ready: !rule_mutations,
            rules_handshake: None,
            rule_pending: None,
            rule_receipt: None,
            rule_proposals: VecDeque::new(),
            stopping: false,
        };
        session.notify(
            EventKind::Ready,
            serde_json::json!({"fence":fence,"seat":seat}),
        )?;
        Ok(session)
    }
    pub fn notify(&mut self, event: EventKind, payload: serde_json::Value) -> io::Result<()> {
        let size = protocol::encode(&protocol::Packet {
            version: protocol::VERSION,
            sequence: 1,
            at: Duration::ZERO,
            message: Work::Event {
                fence: self.fence,
                sequence: 1,
                event,
                payload: payload.clone(),
            },
        })?
        .len();
        if size > 64 * 1024 {
            return Err(protocol::invalid("Lua event exceeds byte limit"));
        }
        if event == EventKind::Update
            || event == EventKind::Matched && payload["event"]["stage"] == "update"
        {
            self.notices.retain(|old| {
                !(old.event == event
                    && old.payload["event"]["stage"] == "update"
                    && old.payload["event"]["id"] == payload["event"]["id"]
                    && old.payload["event"]["source"] == payload["event"]["source"])
            });
            self.bytes = self.notices.iter().map(|entry| entry.bytes).sum::<usize>()
                + self
                    .actions
                    .iter()
                    .map(|(_, _, bytes)| *bytes)
                    .sum::<usize>();
        }
        if self.notices.len() + self.actions.len() >= MAX_PENDING || self.bytes + size > MAX_BYTES {
            return Err(protocol::invalid("Lua event queue exceeds limit"));
        }
        self.bytes += size;
        self.notices.push_back(Notice {
            event,
            payload,
            bytes: size,
            recognition: None,
        });
        Ok(())
    }
    /// The embedding supplies broker-negotiated flags and executor readiness.
    /// Source declarations never grant capabilities. One queued snapshot and
    /// one in-flight acknowledgement bound this metadata channel.
    pub fn update_capabilities(
        &mut self,
        negotiated: BTreeSet<Capability>,
        ready: BTreeSet<Capability>,
    ) -> io::Result<()> {
        if self.stopping {
            return Err(protocol::invalid("Lua session is stopping"));
        }
        let view = CapabilityView {
            fence: self.fence,
            negotiated,
            ready,
            sampled_at: monotonic()?,
        };
        view.validate()?;
        if let Some(previous) = &self.capabilities_last {
            if previous.negotiated != view.negotiated {
                return Err(protocol::invalid(
                    "negotiated capabilities changed within a session",
                ));
            }
            if previous.ready == view.ready
                && view.sampled_at.saturating_sub(previous.sampled_at) < Duration::from_millis(100)
            {
                return Ok(());
            }
        }
        self.capabilities_last = Some(view.clone());
        self.capabilities_next = Some(view);
        Ok(())
    }
    /// Cache broker-stamped status for the bounded parent metadata channel.
    /// It is delivered with capability updates without granting new flags.
    pub fn update_status(&mut self, view: StatusView) -> io::Result<()> {
        view.validate()?;
        if self.stopping
            || view.fence != self.fence
            || view.sampled_at > monotonic()?
            || self
                .status_last
                .as_ref()
                .is_some_and(|old| old.sampled_at > view.sampled_at)
        {
            return Err(protocol::invalid("invalid or retired Lua status update"));
        }
        if self.status_last.as_ref() == Some(&view) {
            return Ok(());
        }
        self.status_last = Some(view);
        if let Some(capabilities) = &self.capabilities_last {
            self.capabilities_next = Some(capabilities.clone());
        }
        Ok(())
    }
    pub fn deadline(&self) -> io::Result<Duration> {
        Ok(self
            .supervisor
            .deadline()?
            .min(if self.ready {
                self.pending.map_or(Duration::MAX, |(_, deadline)| deadline)
            } else {
                self.expires
            })
            .min(self.context_pending.unwrap_or(Duration::MAX))
            .min(
                self.capabilities_pending
                    .map_or(Duration::MAX, |(_, deadline)| deadline),
            )
            .min(self.rules_handshake.unwrap_or(Duration::MAX))
            .min(
                self.rule_pending
                    .as_ref()
                    .map_or(Duration::MAX, |pending| pending.expires),
            )
            .min(self.context_started.map_or(Duration::MAX, |started| {
                started + crate::context_transfer::TIMEOUT
            })))
    }
    pub fn action_ready(&self) -> bool {
        self.ready && self.initialized
    }
    pub fn take_rule_proposals(&mut self) -> Vec<super::RuleProposal> {
        self.rule_proposals.drain(..).collect()
    }
    pub fn acknowledge_rules(
        &mut self,
        fence: Fence,
        receipt: super::RuleReceipt,
    ) -> io::Result<()> {
        let pending = self
            .rule_pending
            .as_ref()
            .ok_or_else(|| protocol::invalid("no pending Lua rule proposal"))?;
        if fence != self.fence
            || pending.id != receipt.id
            || pending.sent
            || !self.rule_proposals.is_empty()
            || self.rule_receipt.is_some()
            || monotonic()? >= pending.expires
            || receipt
                .error
                .as_ref()
                .is_some_and(|error| error.len() > 512 || error.chars().any(char::is_control))
            || receipt.error.is_none() && receipt.revision != pending.revision
        {
            return Err(protocol::invalid("invalid or stale Lua rule receipt"));
        }
        self.rule_receipt = Some(receipt);
        Ok(())
    }
    pub fn request_action(
        &mut self,
        event: Box<crate::rules::Event>,
        expires: Duration,
    ) -> io::Result<()> {
        if !self.action_ready() {
            return Err(protocol::invalid("Lua action VM is not ready"));
        }
        let bytes = super::runtime::bounded_json(&event, 48 * 1024)?.len();
        if self.notices.len() + self.actions.len() >= MAX_PENDING || self.bytes + bytes > MAX_BYTES
        {
            return Err(protocol::invalid("Lua action queue exceeds limit"));
        }
        self.bytes += bytes;
        self.actions.push_back((event, expires, bytes));
        Ok(())
    }
    pub fn take_actions(&mut self) -> Vec<(u64, u64, Option<super::ActionResult>)> {
        self.resolved.drain(..).collect()
    }
    /// Private supervised transport only. The embedding must own the matching
    /// Rust Gate and revalidate every returned proposal before arbitration.
    pub fn request_recognition(&mut self, ticket: crate::recognition::Ticket) -> io::Result<()> {
        if self.stopping
            || !self.action_ready()
            || ticket.fence != self.fence
            || ticket.id <= self.recognition_last
            || ticket.input.source().seat != self.seat
        {
            return Err(protocol::invalid(
                "Lua recognizer VM is not ready or ticket was replayed",
            ));
        }
        let definition = self
            .recognizers
            .get(&ticket.recognizer)
            .ok_or_else(|| protocol::invalid("unapproved Lua recognizer"))?;
        ticket.validate_for(definition, monotonic()?)?;
        let scope = crate::recognition::Cancellation::from_ticket(
            &ticket,
            crate::recognition::Retirement::Cancelled,
        );
        if let Some(current) = self.recognition_candidates.get(&ticket.candidate) {
            if !current.same_candidate(&scope) {
                return Err(protocol::invalid(
                    "Lua recognizer candidate changed without cancellation",
                ));
            }
        } else if self.recognition_candidates.len()
            + self.cancellations.len()
            + usize::from(self.pending_cancellation.is_some())
            + self.cancelled_recognitions.len()
            >= MAX_PENDING
        {
            return Err(protocol::invalid(
                "too many admitted Lua recognizer candidates",
            ));
        }
        let bytes = protocol::encode(&protocol::Packet {
            version: protocol::VERSION,
            sequence: 1,
            at: Duration::ZERO,
            message: Work::Recognition {
                fence: self.fence,
                sequence: 1,
                ticket: Box::new(ticket.clone()),
            },
        })?
        .len();
        let queued = self
            .notices
            .iter()
            .filter(|notice| notice.recognition.is_some())
            .count();
        if self.notices.len() + self.actions.len() >= MAX_PENDING
            || self.bytes + bytes > MAX_BYTES
            || queued + self.recognitions.len() + usize::from(self.pending_recognition.is_some())
                >= MAX_PENDING
        {
            return Err(protocol::invalid("Lua recognizer queue exceeds limit"));
        }
        self.recognition_last = ticket.id;
        self.recognition_candidates.insert(ticket.candidate, scope);
        self.recognition_tickets.insert(ticket.id, ticket.candidate);
        self.bytes += bytes;
        self.notices.push_back(Notice {
            event: EventKind::Input,
            payload: serde_json::Value::Null,
            bytes,
            recognition: Some(Box::new(ticket)),
        });
        Ok(())
    }
    /// A missing proposal means the request expired or lost its permitted
    /// desktop context before execution. It never means recognition succeeded.
    pub fn take_recognitions(&mut self) -> Vec<(u64, Option<crate::recognition::Proposal>)> {
        let replies: Vec<_> = self.recognitions.drain(..).collect();
        for (ticket, _) in &replies {
            self.recognition_tickets.remove(ticket);
        }
        replies
    }
    /// Exact helper acknowledgements for the embedding's lifecycle ledger.
    /// They establish no input ownership, effects or physical release proof.
    pub fn take_recognition_cancellations(&mut self) -> Vec<crate::recognition::Cancellation> {
        self.cancelled_recognitions.drain(..).collect()
    }
    /// Revoke in the Rust Gate first. Purge unsent/completed replies and ignore
    /// an in-flight result, then deliver bounded identity-only closure cleanup.
    pub fn cancel_recognition(
        &mut self,
        cancellation: crate::recognition::Cancellation,
    ) -> io::Result<()> {
        cancellation.validate()?;
        if self.stopping
            || cancellation.fence != self.fence
            || cancellation.source.seat != self.seat
        {
            return Err(protocol::invalid(
                "foreign or stopped Lua recognizer cancellation",
            ));
        }
        let Some(current) = self.recognition_candidates.get(&cancellation.candidate) else {
            return Ok(());
        };
        if !current.same_candidate(&cancellation) || cancellation.through < current.through {
            return Err(protocol::invalid(
                "stale or changed Lua recognizer cancellation",
            ));
        }
        if self.cancellations.len() >= MAX_PENDING {
            return Err(protocol::invalid(
                "Lua recognizer cleanup queue exceeds limit; retire the helper",
            ));
        }
        let tickets: BTreeSet<_> = self
            .recognition_tickets
            .iter()
            .filter_map(|(ticket, candidate)| {
                (*candidate == cancellation.candidate).then_some(*ticket)
            })
            .collect();
        self.notices.retain(|notice| {
            if notice
                .recognition
                .as_ref()
                .is_some_and(|ticket| tickets.contains(&ticket.id))
            {
                self.bytes -= notice.bytes;
                false
            } else {
                true
            }
        });
        self.recognitions
            .retain(|(ticket, _)| !tickets.contains(ticket));
        if self
            .pending_recognition
            .is_some_and(|ticket| tickets.contains(&ticket))
        {
            self.recognition_cancelled = true;
        }
        self.recognition_tickets
            .retain(|_, candidate| *candidate != cancellation.candidate);
        self.recognition_candidates.remove(&cancellation.candidate);
        self.cancellations.push_back(cancellation);
        Ok(())
    }
    pub fn update_context(&mut self, view: crate::context_transfer::View) -> io::Result<()> {
        // SAFETY: read-only query of this unprivileged session process.
        let uid = unsafe { libc::geteuid() };
        if view.fence != self.fence
            || view.context.as_ref().is_some_and(|context| {
                context.binding.seat != self.seat || context.binding.uid != uid
            })
        {
            return Err(protocol::invalid(
                "Lua context belongs to another session owner/fence",
            ));
        }
        let data = view.encode()?;
        if self.context_offset > 0 || self.context_pending.is_some() {
            self.context_next = Some(data);
        } else {
            self.context_source = data;
            self.context_offset = 0;
        }
        Ok(())
    }
    /// Avoid invoking a callback that has not yet been sent to the helper.
    /// In-flight callbacks remain bounded and their caller discards the result.
    pub fn cancel_action(&mut self, id: u64, ordinal: u64) -> io::Result<()> {
        let position = self.actions.iter().position(|(event, _, _)| {
            matches!(event.as_ref(),
            crate::rules::Event::Matched { id: action_id, ordinal: action_ordinal, .. }
            if *action_id == id && *action_ordinal == ordinal)
        });
        if let Some(position) = position {
            if self.resolved.len() >= MAX_PENDING {
                return Err(protocol::invalid("Lua action result queue overflow"));
            }
            self.bytes -= self.actions.remove(position).unwrap().2;
            self.resolved.push_back((id, ordinal, None));
        }
        Ok(())
    }
    pub fn finished(&self) -> bool {
        self.ready
            && self.pending.is_none()
            && self.notices.is_empty()
            && self.actions.is_empty()
            && self.cancellations.is_empty()
            && self.capabilities_next.is_none()
            && self.capabilities_pending.is_none()
            && self.context_source.is_empty()
            && self.context_pending.is_none()
            && self.rules_ready
            && self.rule_pending.is_none()
            && self.rule_proposals.is_empty()
    }
    pub fn stop(&mut self) -> io::Result<()> {
        self.stopping = true;
        self.capabilities_next = None;
        self.status_last = None;
        self.rule_mutations = false;
        self.rules_ready = true;
        self.rules_handshake = None;
        self.rule_proposals.clear();
        self.rule_pending = None;
        self.rule_receipt = None;
        self.context_source.clear();
        self.context_next = None;
        self.context_offset = 0;
        self.context_started = None;
        self.notices.clear();
        self.actions.clear();
        self.resolved.clear();
        self.recognitions.clear();
        self.recognition_candidates.clear();
        self.recognition_tickets.clear();
        self.cancellations.clear();
        self.cancelled_recognitions.clear();
        self.recognition_cancelled = self.pending_recognition.is_some();
        self.bytes = 0;
        self.notify(EventKind::Stop, serde_json::json!({"fence":self.fence}))
    }
    pub fn poll_sources(&self) -> Vec<PollFd<'_>> {
        vec![
            PollFd::new(self.supervisor.as_fd(), PollFlags::POLLIN),
            PollFd::new(
                self.channel.as_fd(),
                PollFlags::POLLIN
                    | if self.wants_write() {
                        PollFlags::POLLOUT
                    } else {
                        PollFlags::empty()
                    },
            ),
        ]
    }
    fn wants_write(&self) -> bool {
        if self.capabilities_pending.is_some() {
            return false;
        }
        self.offset < self.source.len()
            || self.ready && !self.rules_ready && self.rules_handshake.is_none()
            || self.ready
                && self.rules_ready
                && self.capabilities_next.is_some()
                && self.pending.is_none()
                && self.context_pending.is_none()
                && self.context_offset == 0
                && self.rule_pending.is_none()
            || self.rule_receipt.is_some()
                && self.pending.is_none()
                && self.context_pending.is_none()
            || self.ready
                && self.rules_ready
                && self.context_pending.is_none()
                && self.context_offset < self.context_source.len()
            || self.action_ready()
                && self.context_pending.is_none()
                && self.pending.is_none()
                && !self.cancellations.is_empty()
            || self.ready
                && self.rules_ready
                && self.rule_pending.is_none()
                && self.pending.is_none()
                && (!self.notices.is_empty() || !self.actions.is_empty())
    }
    pub fn step(&mut self, now: Duration) -> io::Result<Vec<LogRecord>> {
        if let Some(report) = self.supervisor.check()? {
            return Err(io::Error::other(format!(
                "Lua worker {:?}: {}",
                report.reason(),
                report.status()
            )));
        }
        if !self.ready && now >= self.expires
            || self.pending.is_some_and(|(_, expiry)| now >= expiry)
            || self.context_pending.is_some_and(|expiry| now >= expiry)
            || self
                .capabilities_pending
                .is_some_and(|(_, expiry)| now >= expiry)
            || self.rules_handshake.is_some_and(|expiry| now >= expiry)
            || self
                .rule_pending
                .as_ref()
                .is_some_and(|pending| now >= pending.expires)
            || self.context_started.is_some_and(|started| {
                now.saturating_sub(started) >= crate::context_transfer::TIMEOUT
            })
        {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Lua runtime reply timed out",
            ));
        }
        let mut logs = Vec::new();
        for _ in 0..4 {
            let Some(packet) = self.channel.receive::<Message>()? else {
                break;
            };
            match packet.message {
                Message::Capabilities { fence, id }
                    if fence == self.fence
                        && self
                            .capabilities_pending
                            .is_some_and(|(expected, _)| id == expected) =>
                {
                    self.capabilities_pending = None;
                }
                Message::RulesEnabled { .. }
                | Message::RuleProposal { .. }
                | Message::RulesApplied { .. }
                    if self.stopping => {}
                Message::RulesEnabled { fence }
                    if fence == self.fence
                        && self.rule_mutations
                        && self.rules_handshake.is_some() =>
                {
                    self.rules_handshake = None;
                    self.rules_ready = true;
                }
                Message::RuleProposal { fence, proposal }
                    if fence == self.fence
                        && self.rule_mutations
                        && self.rules_ready
                        && self.initialized =>
                {
                    proposal.transaction.validate().map_err(io::Error::other)?;
                    if proposal.id == 0
                        || self.rule_pending.is_some()
                        || !self.rule_proposals.is_empty()
                    {
                        return Err(protocol::invalid("invalid or duplicate Lua rule proposal"));
                    }
                    let revision = proposal
                        .transaction
                        .revision
                        .checked_add(1)
                        .ok_or_else(|| protocol::invalid("Lua rule revision exhausted"))?;
                    self.rule_pending = Some(RulePending {
                        id: proposal.id,
                        revision,
                        expires: now + RESPONSE_TIMEOUT,
                        sent: false,
                    });
                    self.rule_proposals.push_back(proposal);
                }
                Message::RulesApplied {
                    fence,
                    id,
                    logs: batch,
                } if fence == self.fence
                    && self
                        .rule_pending
                        .as_ref()
                        .is_some_and(|pending| pending.id == id && pending.sent) =>
                {
                    self.rule_pending = None;
                    logs.extend(batch);
                }
                Message::ContextLogs { fence, logs: batch }
                    if fence == self.fence && self.ready && self.context_pending.is_some() =>
                {
                    self.context_pending = None;
                    self.context_source = self.context_next.take().unwrap_or_default();
                    self.context_offset = 0;
                    logs.extend(batch);
                }
                Message::Ready { fence }
                    if fence == self.fence && !self.ready && self.offset == self.source.len() =>
                {
                    self.ready = true;
                    self.source.clear();
                    self.offset = 0;
                }
                Message::Logs {
                    fence,
                    sequence,
                    logs: batch,
                } if fence == self.fence
                    && self.ready
                    && self
                        .pending
                        .is_some_and(|(expected, _)| sequence == expected) =>
                {
                    if self.pending_action.is_some()
                        || self.pending_recognition.is_some()
                        || self.pending_cancellation.is_some()
                    {
                        return Err(protocol::invalid("Lua action returned an event response"));
                    }
                    if self.pending_event.take() == Some(EventKind::Ready) {
                        self.initialized = true;
                    }
                    self.pending = None;
                    logs.extend(batch);
                }
                Message::Recognized {
                    fence,
                    sequence,
                    ticket,
                    proposal,
                    logs: batch,
                } if fence == self.fence
                    && self.ready
                    && self
                        .pending
                        .is_some_and(|(expected, _)| sequence == expected)
                    && self.pending_recognition == Some(ticket) =>
                {
                    if self.recognitions.len() >= MAX_PENDING
                        || proposal.as_ref().is_some_and(|proposal| {
                            proposal.fence != fence || proposal.ticket != ticket
                        })
                    {
                        return Err(protocol::invalid(
                            "Lua recognizer returned an invalid ticket or full result queue",
                        ));
                    }
                    if let Some(proposal) = &proposal {
                        crate::recognition::validate_data(&proposal.data)?;
                    }
                    self.pending = None;
                    self.pending_recognition = None;
                    if !self.stopping && !self.recognition_cancelled {
                        self.recognitions.push_back((ticket, proposal));
                        logs.extend(batch);
                    } else {
                        self.recognition_tickets.remove(&ticket);
                    }
                    self.recognition_cancelled = false;
                }
                Message::RecognitionCancelled {
                    fence,
                    sequence,
                    candidate,
                    through,
                    logs: batch,
                } if fence == self.fence
                    && self.ready
                    && self
                        .pending
                        .is_some_and(|(expected, _)| sequence == expected)
                    && self.pending_cancellation.as_ref().is_some_and(|pending| {
                        pending.candidate == candidate && pending.through == through
                    }) =>
                {
                    self.pending = None;
                    let cancellation = self.pending_cancellation.take().unwrap();
                    if !self.stopping {
                        if self.cancelled_recognitions.len() >= MAX_PENDING {
                            return Err(protocol::invalid(
                                "Lua recognition cleanup receipt queue exceeds limit",
                            ));
                        }
                        self.cancelled_recognitions.push_back(cancellation);
                        logs.extend(batch);
                    }
                }
                Message::Resolved {
                    fence,
                    sequence,
                    id,
                    ordinal,
                    result,
                } if fence == self.fence
                    && self.ready
                    && self
                        .pending
                        .is_some_and(|(expected, _)| sequence == expected)
                    && self.pending_action == Some((id, ordinal)) =>
                {
                    self.pending = None;
                    self.pending_action = None;
                    if self.resolved.len() >= MAX_PENDING {
                        return Err(protocol::invalid("Lua action result queue overflow"));
                    }
                    self.resolved.push_back((id, ordinal, result));
                }
                Message::Failed { fence, reason } if fence == self.fence => {
                    return Err(io::Error::other(reason));
                }
                Message::Timers { fence, logs: batch } if fence == self.fence && self.ready => {
                    logs.extend(batch);
                }
                _ => return Err(protocol::invalid("unexpected or stale Lua worker response")),
            }
        }
        if self.capabilities_pending.is_some() {
            return Ok(logs);
        }
        if self.offset < self.source.len() {
            let end = (self.offset + source_transfer::CHUNK_BYTES).min(self.source.len());
            if self.channel.send(
                &Work::Source {
                    fence: self.fence,
                    offset: self.offset,
                    bytes: self.source.len(),
                    data: self.source[self.offset..end].into(),
                },
                now,
            )? {
                self.offset = end;
            }
        } else if self.ready && !self.rules_ready {
            if self.rules_handshake.is_none()
                && self
                    .channel
                    .send(&Work::RulesEnable { fence: self.fence }, now)?
            {
                self.rules_handshake = Some(now + RESPONSE_TIMEOUT);
            }
        } else if self.pending.is_none()
            && self.context_pending.is_none()
            && self.rule_receipt.is_some()
        {
            let receipt = self.rule_receipt.as_ref().unwrap();
            if self.channel.send(
                &Work::RuleReceipt {
                    fence: self.fence,
                    receipt: receipt.clone(),
                },
                now,
            )? {
                self.rule_receipt = None;
                self.rule_pending.as_mut().unwrap().sent = true;
            }
        } else if self.ready
            && self.rules_ready
            && self.pending.is_none()
            && self.context_pending.is_none()
            && self.context_offset == 0
            && self.rule_pending.is_none()
            && let Some(view) = self.capabilities_next.as_ref()
        {
            let id = self
                .capabilities_id
                .checked_add(1)
                .ok_or_else(|| protocol::invalid("Lua capability sequence exhausted"))?;
            if self.channel.send(
                &Work::Capabilities {
                    view: view.clone(),
                    status: self.status_last.clone(),
                    id,
                },
                now,
            )? {
                self.capabilities_next = None;
                self.capabilities_id = id;
                self.capabilities_pending = Some((id, now + RESPONSE_TIMEOUT));
            }
        } else if self.ready
            && self.rules_ready
            && self.context_pending.is_none()
            && self.context_offset < self.context_source.len()
        {
            let end = (self.context_offset + crate::context_transfer::CHUNK_BYTES)
                .min(self.context_source.len());
            if self.channel.send(
                &Work::Context {
                    fence: self.fence,
                    offset: self.context_offset,
                    bytes: self.context_source.len(),
                    data: self.context_source[self.context_offset..end].to_vec(),
                },
                now,
            )? {
                self.context_started.get_or_insert(now);
                self.context_offset = end;
                if end == self.context_source.len() {
                    self.context_source.clear();
                    self.context_offset = 0;
                    self.context_started = None;
                    self.context_pending = Some(now + RESPONSE_TIMEOUT);
                }
            }
        } else if self.context_pending.is_none()
            && self.action_ready()
            && self.pending.is_none()
            && !self.cancellations.is_empty()
        {
            let cancellation = self.cancellations.front().unwrap().clone();
            let sequence = self
                .sequence
                .checked_add(1)
                .ok_or_else(|| protocol::invalid("Lua cleanup sequence exhausted"))?;
            if self.channel.send(
                &Work::CancelRecognition {
                    fence: self.fence,
                    sequence,
                    cancellation: cancellation.clone(),
                },
                now,
            )? {
                self.cancellations.pop_front();
                self.sequence = sequence;
                self.pending = Some((sequence, now + RESPONSE_TIMEOUT));
                self.pending_cancellation = Some(cancellation);
            }
        } else if self.context_pending.is_none()
            && self.rule_pending.is_none()
            && self.action_ready()
            && self.pending.is_none()
            && !self.actions.is_empty()
        {
            let (event, expires, _) = self.actions.front().unwrap();
            let crate::rules::Event::Matched { id, ordinal, .. } = event.as_ref() else {
                return Err(protocol::invalid(
                    "Lua action work requires a matched event",
                ));
            };
            let identity = (*id, *ordinal);
            let sequence = self
                .sequence
                .checked_add(1)
                .ok_or_else(|| protocol::invalid("Lua sequence exhausted"))?;
            if self.channel.send(
                &Work::Action {
                    fence: self.fence,
                    sequence,
                    event: event.clone(),
                    expires: *expires,
                },
                now,
            )? {
                self.bytes -= self.actions.pop_front().unwrap().2;
                self.sequence = sequence;
                self.pending = Some((sequence, now + RESPONSE_TIMEOUT));
                self.pending_action = Some(identity);
            }
        } else if self.ready
            && self.context_pending.is_none()
            && self.pending.is_none()
            && self.rules_ready
            && self.rule_pending.is_none()
            && let Some(notice) = self.notices.front()
        {
            let event = notice.event;
            let ticket = notice.recognition.as_ref().map(|ticket| ticket.id);
            let sequence = self
                .sequence
                .checked_add(1)
                .ok_or_else(|| protocol::invalid("Lua event sequence exhausted"))?;
            let work = if let Some(ticket) = &notice.recognition {
                Work::Recognition {
                    fence: self.fence,
                    sequence,
                    ticket: ticket.clone(),
                }
            } else {
                Work::Event {
                    fence: self.fence,
                    sequence,
                    event: notice.event,
                    payload: notice.payload.clone(),
                }
            };
            if self.channel.send(&work, now)? {
                self.bytes -= self.notices.pop_front().unwrap().bytes;
                self.sequence = sequence;
                self.pending = Some((sequence, now + RESPONSE_TIMEOUT));
                self.pending_event = if ticket.is_none() { Some(event) } else { None };
                self.pending_recognition = ticket;
            }
        }
        Ok(logs)
    }
}

pub fn worker() -> io::Result<()> {
    crate::lua_compiler::runtime_limits()?;
    let mut health = Heartbeat::connect()?;
    let mut channel = health
        .take_control()
        .ok_or_else(|| protocol::invalid("Lua worker requires private supervision"))?;
    let mut assembly = None::<source_transfer::Assembly>;
    let mut runtime = None::<super::Runtime>;
    let mut fence = None;
    let mut sequence = 0;
    let mut capabilities_id = 0;
    let mut outgoing = None;
    let mut timers_enabled = false;
    let mut rules_enabled = false;
    let mut context_assembly: Option<crate::context_transfer::Assembly> = None;
    loop {
        health.progress()?;
        let now = monotonic()?;
        if outgoing.is_none()
            && let Some(proposal) = runtime
                .as_mut()
                .and_then(super::Runtime::take_rule_proposal)
        {
            outgoing = Some(Message::RuleProposal {
                fence: fence.unwrap(),
                proposal,
            });
        }
        if context_assembly
            .as_ref()
            .is_some_and(|assembly| now >= assembly.deadline())
        {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Lua context transfer timed out",
            ));
        }
        if outgoing.is_none()
            && let Some(packet) = channel.receive::<Work>()?
        {
            match packet.message {
                Work::Capabilities { view, status, id }
                    if fence == Some(view.fence)
                        && runtime.is_some()
                        && id == capabilities_id + 1 =>
                {
                    capabilities_id = id;
                    let incoming = view.fence;
                    outgoing = Some(
                        match runtime
                            .as_mut()
                            .unwrap()
                            .set_capabilities(view, monotonic()?)
                            .and_then(|()| {
                                if let Some(status) = status {
                                    runtime
                                        .as_mut()
                                        .unwrap()
                                        .set_status(status, monotonic().map_err(super::error)?)
                                } else {
                                    runtime.as_mut().unwrap().clear_status();
                                    Ok(())
                                }
                            }) {
                            Ok(()) => Message::Capabilities {
                                fence: incoming,
                                id,
                            },
                            Err(error) => Message::Failed {
                                fence: incoming,
                                reason: error.0.chars().take(512).collect(),
                            },
                        },
                    );
                }
                Work::RulesEnable { fence: incoming }
                    if fence == Some(incoming)
                        && runtime.is_some()
                        && !rules_enabled
                        && !timers_enabled =>
                {
                    runtime.as_mut().unwrap().enable_rule_transactions();
                    rules_enabled = true;
                    outgoing = Some(Message::RulesEnabled { fence: incoming });
                }
                Work::RuleReceipt {
                    fence: incoming,
                    receipt,
                } if fence == Some(incoming) && rules_enabled && runtime.is_some() => {
                    crate::lua_compiler::runtime_cpu_budget()?;
                    let id = receipt.id;
                    let payload = serde_json::json!({"revision":receipt.revision.to_string(),"accepted":receipt.error.is_none(),"error":receipt.error});
                    let runtime = runtime.as_mut().unwrap();
                    runtime
                        .acknowledge_rules(receipt)
                        .map_err(io::Error::other)?;
                    runtime.set_clock(monotonic()?).map_err(io::Error::other)?;
                    outgoing = Some(match runtime.dispatch(EventKind::RulesChanged, &payload) {
                        Ok(logs) => Message::RulesApplied {
                            fence: incoming,
                            id,
                            logs,
                        },
                        Err(error) => Message::Failed {
                            fence: incoming,
                            reason: error.0.chars().take(512).collect(),
                        },
                    });
                }
                Work::Context {
                    fence: incoming,
                    offset,
                    bytes,
                    data,
                } if fence == Some(incoming) && runtime.is_some() => {
                    let now = monotonic()?;
                    if offset == 0 {
                        context_assembly =
                            Some(crate::context_transfer::Assembly::new(incoming, now)?);
                    }
                    let assembly = context_assembly
                        .as_mut()
                        .ok_or_else(|| protocol::invalid("Lua context has no initial chunk"))?;
                    if let Some(view) = assembly.push(incoming, offset, bytes, &data, now)? {
                        crate::lua_compiler::runtime_cpu_budget()?;
                        let state = view.state;
                        let available = view.context.as_ref().is_some_and(|context| {
                            now.saturating_sub(view.sampled_at) < crate::context_transfer::FRESHNESS
                                && now.saturating_sub(context.snapshot.captured_at)
                                    < crate::context_transfer::FRESHNESS
                        });
                        let revision = view
                            .context
                            .as_ref()
                            .map_or(0, |context| context.snapshot.revision);
                        let runtime = runtime.as_mut().unwrap();
                        runtime.set_context(view, now).map_err(io::Error::other)?;
                        outgoing = Some(match runtime.dispatch(EventKind::ContextChanged, &serde_json::json!({"fence":incoming,"state":state,"available":available,"revision":revision})) {
                            Ok(logs) => Message::ContextLogs { fence: incoming, logs },
                            Err(error) => Message::Failed { fence:incoming, reason:error.0.chars().take(512).collect() },
                        });
                        context_assembly = None;
                    }
                }
                Work::Source {
                    fence: incoming,
                    offset,
                    bytes,
                    data,
                } if runtime.is_none() => {
                    if assembly.is_none() {
                        incoming.validate()?;
                        fence = Some(incoming);
                        assembly = Some(source_transfer::Assembly::new(incoming, now)?);
                    }
                    if let Some(definition) = assembly
                        .as_mut()
                        .unwrap()
                        .push(incoming, offset, bytes, &data, now)?
                    {
                        let definition = definition.ok_or_else(|| {
                            protocol::invalid("Lua worker received an empty snapshot")
                        })?;
                        crate::lua_compiler::runtime_cpu_budget()?;
                        match definition.instantiate_progress(&mut || health.progress()) {
                            Ok(loaded) => {
                                runtime = Some(loaded);
                                assembly = None;
                                outgoing = Some(Message::Ready { fence: incoming });
                            }
                            Err(error) => {
                                outgoing = Some(Message::Failed {
                                    fence: incoming,
                                    reason: error.0.chars().take(512).collect(),
                                })
                            }
                        }
                    }
                }
                Work::Action {
                    fence: incoming,
                    sequence: next,
                    event,
                    expires,
                } if fence == Some(incoming) && runtime.is_some() && next == sequence + 1 => {
                    crate::lua_compiler::runtime_cpu_budget()?;
                    sequence = next;
                    let crate::rules::Event::Matched {
                        id,
                        ordinal,
                        action,
                        config_generation,
                        session_generation,
                        ..
                    } = event.as_ref()
                    else {
                        return Err(protocol::invalid("invalid Lua action event"));
                    };
                    if *config_generation != incoming.configuration
                        || *session_generation != incoming.session
                    {
                        return Err(protocol::invalid("Lua action event generation mismatch"));
                    }
                    let now = monotonic()?;
                    outgoing = Some(if now >= expires {
                        Message::Resolved {
                            fence: incoming,
                            sequence,
                            id: *id,
                            ordinal: *ordinal,
                            result: None,
                        }
                    } else {
                        let runtime = runtime.as_mut().unwrap();
                        runtime.set_clock(now).map_err(io::Error::other)?;
                        let rule_checkpoint = runtime.rule_proposal_checkpoint();
                        match runtime.resolve_action(
                            action,
                            &serde_json::json!({"fence":incoming,"event":event}),
                        ) {
                            Ok(result) => {
                                let fresh = monotonic()? < expires;
                                if !fresh {
                                    runtime.discard_rule_proposal_since(
                                        rule_checkpoint,
                                        "Lua action authorization expired",
                                    );
                                }
                                Message::Resolved {
                                    fence: incoming,
                                    sequence,
                                    id: *id,
                                    ordinal: *ordinal,
                                    result: fresh.then_some(result),
                                }
                            }
                            Err(error) => Message::Failed {
                                fence: incoming,
                                reason: error.0.chars().take(512).collect(),
                            },
                        }
                    });
                }
                Work::CancelRecognition {
                    fence: incoming,
                    sequence: next,
                    cancellation,
                } if fence == Some(incoming)
                    && runtime.is_some()
                    && timers_enabled
                    && cancellation.fence == incoming
                    && next == sequence + 1 =>
                {
                    crate::lua_compiler::runtime_cpu_budget()?;
                    sequence = next;
                    runtime
                        .as_mut()
                        .unwrap()
                        .set_clock(monotonic()?)
                        .map_err(io::Error::other)?;
                    outgoing = Some(
                        match runtime.as_mut().unwrap().cancel_recognition(&cancellation) {
                            Ok(logs) => Message::RecognitionCancelled {
                                fence: incoming,
                                sequence,
                                candidate: cancellation.candidate,
                                through: cancellation.through,
                                logs,
                            },
                            Err(error) => Message::Failed {
                                fence: incoming,
                                reason: error.0.chars().take(512).collect(),
                            },
                        },
                    );
                }
                Work::Recognition {
                    fence: incoming,
                    sequence: next,
                    ticket,
                } if fence == Some(incoming)
                    && runtime.is_some()
                    && timers_enabled
                    && ticket.fence == incoming
                    && next == sequence + 1 =>
                {
                    crate::lua_compiler::runtime_cpu_budget()?;
                    sequence = next;
                    runtime
                        .as_mut()
                        .unwrap()
                        .set_clock(monotonic()?)
                        .map_err(io::Error::other)?;
                    outgoing = Some(match runtime.as_mut().unwrap().recognize(&ticket) {
                        Ok(result) => Message::Recognized {
                            fence: incoming,
                            sequence,
                            ticket: ticket.id,
                            proposal: result.proposal,
                            logs: result.logs,
                        },
                        Err(error) => Message::Failed {
                            fence: incoming,
                            reason: error.0.chars().take(512).collect(),
                        },
                    });
                }
                Work::Event {
                    fence: incoming,
                    sequence: next,
                    event,
                    payload,
                } if fence == Some(incoming) && runtime.is_some() && next == sequence + 1 => {
                    crate::lua_compiler::runtime_cpu_budget()?;
                    sequence = next;
                    if event == EventKind::Ready {
                        timers_enabled = true;
                    }
                    if event == EventKind::Stop {
                        timers_enabled = false;
                        rules_enabled = false;
                        runtime.as_mut().unwrap().disable_rule_transactions();
                        runtime.as_mut().unwrap().clear_capabilities();
                        runtime.as_mut().unwrap().clear_status();
                        runtime.as_mut().unwrap().recognition_candidates.clear();
                        context_assembly = None;
                    }
                    runtime
                        .as_mut()
                        .unwrap()
                        .set_clock(monotonic()?)
                        .map_err(io::Error::other)?;
                    outgoing = Some(match runtime.as_mut().unwrap().dispatch(event, &payload) {
                        Ok(logs) => Message::Logs {
                            fence: incoming,
                            sequence,
                            logs,
                        },
                        Err(error) => Message::Failed {
                            fence: incoming,
                            reason: error.0.chars().take(512).collect(),
                        },
                    });
                }
                _ => {
                    return Err(protocol::invalid(
                        "invalid Lua worker source or event generation",
                    ));
                }
            }
        }
        if let Some(message) = &outgoing
            && channel.send(message, monotonic()?)?
        {
            let failed = matches!(outgoing, Some(Message::Failed { .. }));
            outgoing = None;
            if failed {
                return Ok(());
            }
        }
        let scheduler_now = monotonic()?;
        if timers_enabled
            && outgoing.is_none()
            && let Some(runtime) = runtime.as_mut()
            && runtime
                .scheduler_deadline()
                .map_err(io::Error::other)?
                .is_some_and(|due| due <= scheduler_now)
        {
            crate::lua_compiler::runtime_cpu_budget()?;
            let current = fence.unwrap();
            outgoing = match runtime.tick(monotonic()?, &serde_json::json!({"fence":current})) {
                Ok(logs) if logs.is_empty() => None,
                Ok(logs) => Some(Message::Timers {
                    fence: current,
                    logs,
                }),
                Err(error) => Some(Message::Failed {
                    fence: current,
                    reason: error.0.chars().take(512).collect(),
                }),
            };
        }
        // A successful callback can submit without logs. Make its proposal
        // writable now instead of waiting for an unrelated heartbeat or timer.
        if outgoing.is_none()
            && let Some(proposal) = runtime
                .as_mut()
                .and_then(super::Runtime::take_rule_proposal)
        {
            outgoing = Some(Message::RuleProposal {
                fence: fence.unwrap(),
                proposal,
            });
        }
        let until = health.deadline()?.min(
            assembly
                .as_ref()
                .map_or(Duration::MAX, source_transfer::Assembly::deadline),
        );
        let until = until.min(
            context_assembly
                .as_ref()
                .map_or(Duration::MAX, crate::context_transfer::Assembly::deadline),
        );
        let until = if timers_enabled
            && outgoing.is_none()
            && let Some(runtime) = runtime.as_mut()
        {
            until.min(
                runtime
                    .scheduler_deadline()
                    .map_err(io::Error::other)?
                    .unwrap_or(Duration::MAX),
            )
        } else {
            until
        };
        let mut fds = vec![
            PollFd::new(health.as_fd(), PollFlags::POLLIN),
            PollFd::new(
                channel.as_fd(),
                PollFlags::POLLIN
                    | if outgoing.is_some() {
                        PollFlags::POLLOUT
                    } else {
                        PollFlags::empty()
                    },
            ),
        ];
        let timeout =
            PollTimeout::try_from(until.saturating_sub(monotonic()?)).map_err(io::Error::other)?;
        if let Err(error) = poll(&mut fds, timeout)
            && error != nix::errno::Errno::EINTR
        {
            return Err(error.into());
        }
    }
}
