//! Authenticated, bounded broker control plane. Device descriptors belong to a
//! separate supervised worker; this service performs no desktop actions.
//! Context/dispatch capabilities stay unavailable until their provider gates
//! are connected. Membership alone never establishes an unlocked desktop.

use crate::{
    context::State,
    hyprland::Client,
    ipc::{Channel, Endpoint},
    protocol::{self, Epoch, Fence, Mode, Reply, Request, Role},
    session::{Logind, Peer, Registration},
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
}
struct Connection {
    channel: Channel,
    opened: Duration,
    bound: Option<Bound>,
    outgoing: VecDeque<Reply>,
}
impl Connection {
    fn push(&mut self, reply: Reply) -> io::Result<()> {
        reply.validate()?;
        if self.outgoing.len() >= 16 {
            return Err(protocol::invalid("control subscriber queue is full"));
        }
        self.outgoing.push_back(reply);
        Ok(())
    }
    fn flush(&mut self, now: Duration) -> io::Result<()> {
        if let Some(reply) = self.outgoing.front()
            && self.channel.send(reply, now)?
        {
            self.outgoing.pop_front();
        }
        Ok(())
    }
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
    pub fn deadline(&self) -> Option<Duration> {
        self.connections
            .iter()
            .map(|connection| {
                connection
                    .bound
                    .as_ref()
                    .map_or(connection.opened + protocol::MAX_PACKET_AGE, |bound| {
                        (bound.checked + REFRESH).min(bound.renewed + protocol::MAX_PACKET_AGE)
                    })
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
        } else if now - connection.opened >= protocol::MAX_PACKET_AGE {
            return Err(protocol::invalid("registration deadline expired"));
        }
        if let Some(packet) = connection.channel.receive::<Request>()? {
            packet.message.validate()?;
            match packet.message {
                Request::Register {
                    instance,
                    role,
                    capabilities,
                } if connection.bound.is_none() => {
                    if !capabilities.is_empty() {
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
                    });
                    connection.push(Reply::Welcome {
                        fence,
                        seat,
                        session,
                        mode: Mode::Observe,
                        capabilities: BTreeSet::new(),
                    })?;
                }
                request => {
                    let bound = connection.bound.as_mut().ok_or_else(|| {
                        protocol::invalid("register before requesting control state")
                    })?;
                    if request.fence() != Some(bound.fence) {
                        return Err(protocol::invalid(
                            "stale broker/configuration/session generation",
                        ));
                    }
                    bound.registration.check(&self.logind, &bound.client)?;
                    self.verify_agent(connection.channel.peer().pid)?;
                    bound.checked = now;
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
