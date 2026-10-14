//! A separately supervised, unprivileged event VM. The session parent keeps
//! processing broker leases and generation changes while Lua executes.
use super::{Definition, EventKind, LogRecord, source_transfer};
use crate::{
    ipc::{Channel, monotonic},
    protocol::{self, Fence},
    supervisor::{Heartbeat, Supervisor},
};
use nix::{
    libc,
    poll::{PollFd, PollFlags, PollTimeout, poll},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
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
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Message {
    Ready {
        fence: Fence,
    },
    Logs {
        fence: Fence,
        sequence: u64,
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
}
pub struct Session {
    supervisor: Supervisor,
    channel: Channel,
    fence: Fence,
    source: Vec<u8>,
    offset: usize,
    ready: bool,
    sequence: u64,
    pending: Option<(u64, Duration)>,
    notices: VecDeque<Notice>,
    bytes: usize,
    expires: Duration,
}
impl Session {
    pub fn spawn(
        executable: &Path,
        fence: Fence,
        seat: &str,
        definition: Definition,
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
            offset: 0,
            ready: false,
            sequence: 0,
            pending: None,
            notices: VecDeque::new(),
            bytes: 0,
            expires: monotonic()? + source_transfer::TIMEOUT,
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
            self.bytes = self.notices.iter().map(|entry| entry.bytes).sum();
        }
        if self.notices.len() >= MAX_PENDING || self.bytes + size > MAX_BYTES {
            return Err(protocol::invalid("Lua event queue exceeds limit"));
        }
        self.bytes += size;
        self.notices.push_back(Notice {
            event,
            payload,
            bytes: size,
        });
        Ok(())
    }
    pub fn deadline(&self) -> io::Result<Duration> {
        Ok(self.supervisor.deadline()?.min(if self.ready {
            self.pending.map_or(Duration::MAX, |(_, deadline)| deadline)
        } else {
            self.expires
        }))
    }
    pub fn finished(&self) -> bool {
        self.ready && self.pending.is_none() && self.notices.is_empty()
    }
    pub fn stop(&mut self) -> io::Result<()> {
        self.notices.clear();
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
        self.offset < self.source.len()
            || self.ready && self.pending.is_none() && !self.notices.is_empty()
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
                    self.pending = None;
                    logs.extend(batch);
                }
                Message::Failed { fence, reason } if fence == self.fence => {
                    return Err(io::Error::other(reason));
                }
                _ => return Err(protocol::invalid("unexpected or stale Lua worker response")),
            }
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
        } else if self.ready
            && self.pending.is_none()
            && let Some(notice) = self.notices.front()
        {
            let sequence = self
                .sequence
                .checked_add(1)
                .ok_or_else(|| protocol::invalid("Lua event sequence exhausted"))?;
            if self.channel.send(
                &Work::Event {
                    fence: self.fence,
                    sequence,
                    event: notice.event,
                    payload: notice.payload.clone(),
                },
                now,
            )? {
                self.bytes -= self.notices.pop_front().unwrap().bytes;
                self.sequence = sequence;
                self.pending = Some((sequence, now + RESPONSE_TIMEOUT));
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
    let mut runtime = None;
    let mut fence = None;
    let mut sequence = 0;
    let mut outgoing = None;
    loop {
        health.progress()?;
        let now = monotonic()?;
        if outgoing.is_none()
            && let Some(packet) = channel.receive::<Work>()?
        {
            match packet.message {
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
                Work::Event {
                    fence: incoming,
                    sequence: next,
                    event,
                    payload,
                } if fence == Some(incoming) && runtime.is_some() && next == sequence + 1 => {
                    crate::lua_compiler::runtime_cpu_budget()?;
                    sequence = next;
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
        let until = health.deadline()?.min(
            assembly
                .as_ref()
                .map_or(Duration::MAX, source_transfer::Assembly::deadline),
        );
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
