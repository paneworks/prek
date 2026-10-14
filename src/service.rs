//! Authenticated, bounded broker control plane. Device descriptors belong to a
//! separate supervised worker; this service performs no desktop actions.
//! Context/dispatch capabilities stay unavailable until their provider gates
//! are connected. Membership alone never establishes an unlocked desktop.

use crate::{
    context::State,
    hyprland::Client,
    ipc::{Channel, Endpoint},
    protocol::{self, Capability, Epoch, Fence, Mode, Reply, Request, Role},
    session::{Logind, Peer, Registration},
    worker::{Report, Subscription},
};
use nix::{
    libc,
    poll::{PollFd, PollFlags},
};
use std::{
    collections::{BTreeSet, VecDeque},
    fs, io,
    os::{
        fd::{AsFd, BorrowedFd},
        unix::fs::MetadataExt,
    },
    path::PathBuf,
    time::Duration,
};

pub const MAX_CLIENTS: usize = 8;
pub const REFRESH: Duration = Duration::from_millis(250);

pub use crate::ipc::monotonic;
fn epoch() -> io::Result<Epoch> {
    let mut bytes = [0u8; 16];
    let mut filled = 0;
    while filled < bytes.len() {
        // SAFETY: initialized bounded output and the kernel random source.
        let count = unsafe {
            libc::getrandom(
                bytes[filled..].as_mut_ptr().cast(),
                bytes.len() - filled,
                libc::GRND_NONBLOCK,
            )
        };
        if count < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if count == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        filled += count as usize;
    }
    let value = Epoch([
        u64::from_ne_bytes(bytes[..8].try_into().unwrap()),
        u64::from_ne_bytes(bytes[8..].try_into().unwrap()),
    ]);
    if value.0 == [0, 0] {
        return Err(protocol::invalid("empty random broker epoch"));
    }
    Ok(value)
}

pub struct Policy {
    pub runtime_root: PathBuf,
    pub compositor_executable: PathBuf,
    pub agent_executable: PathBuf,
    pub allowed_seats: BTreeSet<String>,
}
struct Bound {
    client: Client,
    registration: Registration,
    seat: String,
    role: Role,
    fence: Fence,
    renewed: Duration,
    checked: Duration,
    capabilities: BTreeSet<Capability>,
    observing: bool,
    previous: Option<(Fence, Duration)>,
}
struct Connection {
    channel: Channel,
    opened: Duration,
    bound: Option<Bound>,
    outgoing: VecDeque<Reply>,
    bytes: usize,
    administrator: bool,
    waiting_reload: bool,
}
impl Connection {
    fn push(&mut self, reply: Reply) -> io::Result<()> {
        reply.validate()?;
        if let Reply::Observation { fence, event } = &reply {
            self.outgoing.retain(|old| {
                !matches!(old,
                Reply::Observation { fence: old_fence, event: old_event }
                if old_fence == fence && event.supersedes_progress(old_event))
            });
            self.bytes = self
                .outgoing
                .iter()
                .map(reply_size)
                .sum::<io::Result<usize>>()?;
        }
        if self.outgoing.len() >= 16 {
            return Err(protocol::invalid("control subscriber queue is full"));
        }
        let bytes = reply_size(&reply)?;
        if self.bytes + bytes > protocol::MAX_QUEUED_BYTES {
            return Err(protocol::invalid("control subscriber byte limit exceeded"));
        }
        self.bytes += bytes;
        self.outgoing.push_back(reply);
        Ok(())
    }
    fn flush(&mut self, now: Duration) -> io::Result<()> {
        if let Some(reply) = self.outgoing.front()
            && self.channel.send(reply, now)?
        {
            let reply = self.outgoing.pop_front().unwrap();
            self.bytes -= reply_size(&reply)?;
        }
        Ok(())
    }
}

fn reply_size(reply: &Reply) -> io::Result<usize> {
    Ok(protocol::encode(&protocol::Packet {
        version: protocol::VERSION,
        sequence: u64::MAX,
        at: Duration::MAX,
        message: reply,
    })?
    .len())
}

pub struct Broker {
    endpoint: Endpoint,
    logind: Logind,
    policy: Policy,
    epoch: Epoch,
    configuration: u64,
    session: u64,
    connections: Vec<Connection>,
    cursor: usize,
    observations: bool,
    reload_requested: bool,
    reload_pending: bool,
    last_reload_error: Option<String>,
}
impl Broker {
    /// This currently exposes authenticated status in observation mode. It does
    /// not advertise context, synthetic input or action dispatch prematurely.
    pub fn open(endpoint: Endpoint, library: &std::path::Path, policy: Policy) -> io::Result<Self> {
        if policy.allowed_seats.is_empty()
            || !policy.runtime_root.is_absolute()
            || !policy.agent_executable.is_absolute()
            || !policy.compositor_executable.is_absolute()
        {
            return Err(protocol::invalid(
                "broker requires explicit trusted registration policy",
            ));
        }
        Ok(Self {
            endpoint,
            logind: Logind::open(library)?,
            policy,
            epoch: epoch()?,
            configuration: 1,
            session: 0,
            connections: Vec::new(),
            cursor: 0,
            observations: false,
            reload_requested: false,
            reload_pending: false,
            last_reload_error: None,
        })
    }
    pub fn clients(&self) -> usize {
        self.connections
            .iter()
            .filter(|connection| {
                connection
                    .bound
                    .as_ref()
                    .is_some_and(|bound| bound.role == Role::Agent)
            })
            .count()
    }
    pub fn epoch(&self) -> Epoch {
        self.epoch
    }
    pub fn enable_observations(&mut self) {
        self.observations = true;
    }
    pub fn configuration(&self) -> u64 {
        self.configuration
    }
    pub fn take_reload_request(&mut self) -> bool {
        std::mem::take(&mut self.reload_requested)
    }
    pub fn finish_reload(
        &mut self,
        result: Result<BTreeSet<String>, String>,
        now: Duration,
    ) -> io::Result<()> {
        match result {
            Ok(seats) => {
                self.configuration = self
                    .configuration
                    .checked_add(1)
                    .ok_or_else(|| protocol::invalid("configuration generation exhausted"))?;
                self.policy.allowed_seats = seats;
                self.last_reload_error = None;
                self.connections.retain_mut(|connection| {
                    let Some(bound) = connection.bound.as_mut() else {
                        return true;
                    };
                    if !self.policy.allowed_seats.contains(&bound.seat) {
                        return false;
                    }
                    bound.previous = Some(
                        bound
                            .previous
                            .unwrap_or((bound.fence, now + protocol::MAX_PACKET_AGE)),
                    );
                    bound.fence.configuration = self.configuration;
                    bound.observing = false;
                    let fence = bound.fence;
                    connection.outgoing.clear();
                    connection.bytes = 0;
                    connection.push(Reply::Configuration { fence }).is_ok()
                });
            }
            Err(error) => {
                self.last_reload_error = Some(
                    error
                        .lines()
                        .next()
                        .unwrap_or("configuration reload rejected")
                        .chars()
                        .filter(|ch| !ch.is_control())
                        .take(512)
                        .collect(),
                );
            }
        }
        for connection in &mut self.connections {
            if connection.waiting_reload {
                connection.waiting_reload = false;
                connection.push(Reply::ReloadResult {
                    configuration: self.configuration,
                    error: self.last_reload_error.clone(),
                })?;
            }
        }
        self.reload_pending = false;
        Ok(())
    }
    pub fn subscriptions(&mut self, now: Duration) -> Vec<Subscription> {
        self.connections.retain_mut(|connection| {
            let Some(bound) = connection.bound.as_mut() else {
                return true;
            };
            now >= bound.checked
                && now.saturating_sub(bound.renewed) < protocol::MAX_PACKET_AGE
                && bound
                    .registration
                    .check(&self.logind, &bound.client)
                    .is_ok()
        });
        self.connections
            .iter()
            .filter_map(|connection| connection.bound.as_ref())
            .filter(|bound| {
                bound.role == Role::Agent && bound.capabilities.contains(&Capability::Observations)
            })
            .map(|bound| Subscription {
                seat: bound.seat.clone(),
                fence: bound.fence,
                context: None,
            })
            .collect()
    }
    pub fn report(&mut self, report: Report, now: Duration) -> io::Result<()> {
        let subscription = report.subscription();
        let Some(index) = self.connections.iter().position(|connection| {
            connection.bound.as_ref().is_some_and(|bound| {
                bound.fence == subscription.fence
                    && bound.seat == subscription.seat
                    && bound.role == Role::Agent
                    && bound.capabilities.contains(&Capability::Observations)
            })
        }) else {
            return Ok(());
        };
        let mut connection = self.connections.remove(index);
        let bound = connection.bound.as_mut().unwrap();
        if now.saturating_sub(bound.renewed) >= protocol::MAX_PACKET_AGE
            || bound
                .registration
                .check(&self.logind, &bound.client)
                .is_err()
            || self.verify_agent(connection.channel.peer().pid).is_err()
        {
            return Ok(());
        }
        let fence = bound.fence;
        let reply = match report {
            Report::Ready { .. } if !bound.observing => {
                bound.observing = true;
                Reply::Subscribed { fence }
            }
            Report::Ready { .. } => {
                self.connections.insert(index, connection);
                return Ok(());
            }
            Report::Observation { event, .. } if bound.observing => {
                let source = match event.as_ref() {
                    crate::rules::Event::Matched { source, .. }
                    | crate::rules::Event::Skipped { source, .. }
                    | crate::rules::Event::Cancelled { source, .. } => source,
                };
                if source.seat != bound.seat {
                    return Err(protocol::invalid(
                        "worker observation belongs to a different seat",
                    ));
                }
                Reply::Observation { fence, event }
            }
            Report::Ended { reason, .. } => {
                let _ = connection
                    .channel
                    .send(&Reply::Revoked { fence, reason }, now);
                return Ok(());
            }
            _ => return Err(protocol::invalid("worker observation preceded activation")),
        };
        if connection.push(reply).is_ok() {
            self.connections.insert(index, connection);
        }
        // Overflow/disconnection drops only this subscriber, not the worker.
        Ok(())
    }
    pub fn deadline(&self) -> Option<Duration> {
        self.connections
            .iter()
            .map(|connection| {
                connection.bound.as_ref().map_or(
                    connection.opened
                        + if connection.administrator {
                            crate::configuration::INSTALL_TIMEOUT
                        } else {
                            protocol::MAX_PACKET_AGE
                        },
                    |bound| (bound.checked + REFRESH).min(bound.renewed + protocol::MAX_PACKET_AGE),
                )
            })
            .min()
    }
    pub fn poll_sources(&self) -> Vec<PollFd<'_>> {
        let mut fds = vec![PollFd::new(self.endpoint.as_fd(), PollFlags::POLLIN)];
        fds.extend(self.connections.iter().map(|connection| {
            PollFd::new(
                connection.channel.as_fd(),
                PollFlags::POLLIN
                    | if connection.outgoing.is_empty() {
                        PollFlags::empty()
                    } else {
                        PollFlags::POLLOUT
                    },
            )
        }));
        fds
    }
    /// One incoming message/registration per step prevents a subscriber from
    /// monopolizing supervision. All sends are nonblocking and bounded.
    pub fn step(&mut self, now: Duration) -> io::Result<()> {
        if let Some(channel) = self.endpoint.accept()?
            && self.connections.len() < MAX_CLIENTS
        {
            self.connections.push(Connection {
                channel,
                opened: now,
                bound: None,
                outgoing: VecDeque::new(),
                bytes: 0,
                administrator: false,
                waiting_reload: false,
            });
        }
        if self.connections.is_empty() {
            return Ok(());
        }
        self.cursor %= self.connections.len();
        let index = self.cursor;
        self.cursor += 1;
        let mut connection = self.connections.remove(index);
        let result = self.process(&mut connection, now);
        if result.is_ok() {
            self.connections.insert(index, connection);
        } else if let Err(error) = result {
            let reason: String = error
                .to_string()
                .chars()
                .filter(|character| !character.is_control())
                .take(512)
                .collect();
            let _ = connection
                .channel
                .send(&Reply::Rejected { reason }, monotonic()?);
        }
        // A bad, stale or disconnected client loses all queued work and its
        // seat owner immediately. No raw payload is logged or returned to it.
        Ok(())
    }
    fn process(&mut self, connection: &mut Connection, now: Duration) -> io::Result<()> {
        if now < connection.opened {
            return Err(protocol::invalid("broker timeline moved backward"));
        }
        if let Some(bound) = connection.bound.as_mut() {
            if now.saturating_sub(bound.renewed) >= protocol::MAX_PACKET_AGE {
                return Err(protocol::invalid("session agent lease expired"));
            }
            if now < bound.checked {
                return Err(protocol::invalid("session timeline moved backward"));
            }
            if now - bound.checked >= REFRESH {
                bound.registration.check(&self.logind, &bound.client)?;
                bound.checked = now;
            }
        } else if now - connection.opened
            >= if connection.administrator {
                crate::configuration::INSTALL_TIMEOUT
            } else {
                protocol::MAX_PACKET_AGE
            }
        {
            return Err(protocol::invalid("registration deadline expired"));
        }
        if let Some(packet) = connection.channel.receive::<Request>()? {
            packet.message.validate()?;
            match packet.message {
                Request::Reload {} if connection.bound.is_none() && !connection.administrator => {
                    if connection.channel.peer().uid != 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "configuration reload requires an administrator",
                        ));
                    }
                    self.verify_agent(connection.channel.peer().pid)?;
                    connection.administrator = true;
                    if self.reload_pending {
                        connection.push(Reply::ReloadResult {
                            configuration: self.configuration,
                            error: Some("configuration reload is already in progress".into()),
                        })?;
                    } else {
                        connection.waiting_reload = true;
                        self.reload_requested = true;
                        self.reload_pending = true;
                    }
                }
                Request::Register {
                    instance,
                    role,
                    capabilities,
                } if connection.bound.is_none() => {
                    if capabilities
                        .iter()
                        .any(|capability| *capability != Capability::Observations)
                        || !capabilities.is_empty() && (!self.observations || role != Role::Agent)
                    {
                        return Err(protocol::invalid(
                            "requested capabilities are not implemented by this broker",
                        ));
                    }
                    let peer = Peer::from_socket(&connection.channel)?;
                    self.verify_agent(peer.pid())?;
                    let client = Client::open(
                        &self.policy.runtime_root.join(peer.uid().to_string()),
                        &instance,
                        peer.uid(),
                        &self.policy.compositor_executable,
                    )?;
                    let registration = self.logind.register(peer, &client)?;
                    self.verify_agent(connection.channel.peer().pid)?;
                    let binding = registration.binding()?;
                    if !self.policy.allowed_seats.contains(&binding.seat)
                        || role == Role::Agent
                            && self.connections.iter().any(|other| {
                                other.bound.as_ref().is_some_and(|bound| {
                                    bound.role == Role::Agent && bound.seat == binding.seat
                                })
                            })
                    {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "seat is forbidden or already has an agent",
                        ));
                    }
                    self.session = self
                        .session
                        .checked_add(1)
                        .ok_or_else(|| protocol::invalid("session generation exhausted"))?;
                    let fence = Fence {
                        broker: self.epoch,
                        configuration: self.configuration,
                        session: self.session,
                    };
                    let seat = binding.seat.clone();
                    let session = binding.session.clone();
                    connection.bound = Some(Bound {
                        client,
                        registration,
                        seat: seat.clone(),
                        role,
                        fence,
                        renewed: now,
                        checked: now,
                        capabilities: capabilities.clone(),
                        observing: false,
                        previous: None,
                    });
                    connection.push(Reply::Welcome {
                        fence,
                        seat,
                        session,
                        mode: Mode::Observe,
                        capabilities,
                    })?;
                }
                request => {
                    let bound = connection.bound.as_mut().ok_or_else(|| {
                        protocol::invalid("register before requesting control state")
                    })?;
                    let current = request.fence() == Some(bound.fence);
                    let old_heartbeat = matches!(request, Request::Heartbeat { .. })
                        && bound.previous.is_some_and(|(previous, expires)| {
                            request.fence().is_some_and(|fence| {
                                fence.broker == previous.broker
                                    && fence.session == previous.session
                                    && fence.configuration >= previous.configuration
                                    && fence.configuration < bound.fence.configuration
                            }) && now < expires
                        });
                    if !current && !old_heartbeat {
                        return Err(protocol::invalid(
                            "stale broker/configuration/session generation",
                        ));
                    }
                    bound.registration.check(&self.logind, &bound.client)?;
                    self.verify_agent(connection.channel.peer().pid)?;
                    bound.checked = now;
                    if current {
                        bound.previous = None;
                    }
                    let fence = bound.fence;
                    match request {
                        Request::Heartbeat { .. } => {
                            bound.renewed = now;
                            connection.push(Reply::Accepted { fence, revision: 0 })?;
                        }
                        Request::Status { .. } => {
                            let fence = bound.fence;
                            connection.push(Reply::Status {
                                fence,
                                state: State::Unknown,
                                revision: 0,
                                pending: connection.outgoing.len(),
                                last_reload_error: self.last_reload_error.clone(),
                            })?;
                        }
                        Request::Disconnect { .. } => {
                            return Err(io::ErrorKind::UnexpectedEof.into());
                        }
                        _ => {
                            return Err(protocol::invalid(
                                "operation requires an unavailable provider capability",
                            ));
                        }
                    }
                }
            }
        }
        connection.flush(monotonic()?)
    }
    fn verify_agent(&self, pid: u32) -> io::Result<()> {
        let expected = fs::metadata(&self.policy.agent_executable)?;
        let actual = fs::metadata(format!("/proc/{pid}/exe"))?;
        if (actual.dev(), actual.ino()) != (expected.dev(), expected.ino()) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "session peer executable differs from broker policy",
            ));
        }
        Ok(())
    }
}
impl AsFd for Broker {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.endpoint.as_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{Action, Direction},
        gestures::{Phase, Point, Space, TouchGesture},
        input::Source,
        rules::{Event, Evidence, Stage},
    };
    fn progress(fence: Fence, ordinal: u64, stage: Stage) -> Reply {
        let phase = match stage {
            Stage::Begin => Phase::Begin,
            Stage::End => Phase::End,
            _ => Phase::Update,
        };
        Reply::Observation {
            fence,
            event: Box::new(Event::Matched {
                version: 1,
                rule: "continuous".into(),
                id: 1,
                ordinal,
                source: Source {
                    device: 1,
                    generation: 1,
                    seat: "seat0".into(),
                },
                at: Duration::from_millis(ordinal),
                config_generation: fence.configuration,
                session_generation: fence.session,
                stage,
                action: Action::Event {
                    name: "swipe".into(),
                },
                evidence: Evidence::Touch {
                    space: Space::Normalized,
                    gesture: TouchGesture::Pan {
                        fingers: 1,
                        phase,
                        direction: Direction::Right,
                        total: Point {
                            x: ordinal as f64,
                            y: 0.0,
                        },
                        delta: Point { x: 1.0, y: 0.0 },
                        velocity: Point { x: 1.0, y: 0.0 },
                        centroid: Point { x: 0.5, y: 0.5 },
                        swipe: true,
                    },
                },
                window: None,
                output: None,
                workspace: None,
            }),
        }
    }
    #[test]
    fn queued_progress_coalesces_and_preserves_begin_latest_update_and_end() {
        let (channel, mut receiver) = Channel::pair().unwrap();
        let mut connection = Connection {
            channel,
            opened: Duration::ZERO,
            bound: None,
            outgoing: VecDeque::new(),
            bytes: 0,
            administrator: false,
            waiting_reload: false,
        };
        let fence = Fence {
            broker: Epoch([1, 2]),
            configuration: 1,
            session: 3,
        };
        connection.push(progress(fence, 1, Stage::Begin)).unwrap();
        for ordinal in 2..=129 {
            connection
                .push(progress(fence, ordinal, Stage::Update))
                .unwrap();
        }
        connection.push(progress(fence, 130, Stage::End)).unwrap();
        assert_eq!(connection.outgoing.len(), 3);
        for (sequence, ordinal) in [1, 129, 130].into_iter().enumerate() {
            connection.flush(monotonic().unwrap()).unwrap();
            let packet = receiver.receive::<Reply>().unwrap().unwrap();
            assert_eq!(packet.sequence, sequence as u64 + 1);
            assert!(
                matches!(packet.message, Reply::Observation { event, .. } if matches!(event.as_ref(), Event::Matched { ordinal: actual, .. } if *actual == ordinal))
            );
        }
        assert_eq!(connection.bytes, 0);
        assert!(connection.outgoing.is_empty());
    }
}
