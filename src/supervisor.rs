//! Linux process supervision. The parent owns only a private health channel and
//! its child process; all input descriptors must be opened by the worker.
//!
//! Call `Heartbeat::progress` from the forwarding loop (including idle polls),
//! never from a background health thread. It is deliberately neither Send nor
//! Sync. Connect before opening input, and drop input before returning on stop.

use nix::{
    libc,
    poll::{PollFd, PollFlags, PollTimeout, poll},
    time::{ClockId, clock_gettime},
};
use std::{
    fs, io,
    marker::PhantomData,
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
        unix::process::CommandExt,
    },
    process::{Child, Command, ExitStatus},
    rc::Rc,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

pub const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(250);
pub const STALL_TIMEOUT: Duration = Duration::from_secs(1);
pub const TERMINATION_GRACE: Duration = Duration::from_millis(250);
const LINK_ENV: &str = "PREK_WORKER_LINK_FD";
const PARENT_ENV: &str = "PREK_WORKER_PARENT";
const EPOCH_ENV: &str = "PREK_WORKER_EPOCH_NS";
const PACKET_SIZE: usize = 20;
static CONNECTED: AtomicBool = AtomicBool::new(false);
static SIGNALS_INSTALLED: AtomicBool = AtomicBool::new(false);
static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn request_termination(_: libc::c_int) {
    STOP_REQUESTED.store(true, Ordering::Relaxed);
}

/// Explicit foreground/service termination handling. The handler only sets an
/// atomic flag; the supervising loop requests orderly worker teardown. Install
/// on the supervising thread before spawning the worker. Previous dispositions
/// are restored when the guard is dropped.
pub struct TerminationSignals {
    previous: Vec<(libc::c_int, libc::sigaction)>,
    _parent_thread: PhantomData<Rc<()>>,
}

impl TerminationSignals {
    pub fn install() -> io::Result<Self> {
        SIGNALS_INSTALLED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "termination handlers already installed",
                )
            })?;
        STOP_REQUESTED.store(false, Ordering::Relaxed);
        let mut guard = Self {
            previous: Vec::with_capacity(2),
            _parent_thread: PhantomData,
        };
        // SAFETY: sigaction structs are plain C data; a zeroed signal mask is
        // initialized below before registration. The handler allocates nothing.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = request_termination as *const () as usize;
        unsafe { libc::sigemptyset(&mut action.sa_mask) };
        for signal in [libc::SIGINT, libc::SIGTERM] {
            let mut previous: libc::sigaction = unsafe { std::mem::zeroed() };
            // SAFETY: both pointers refer to correctly sized initialized C
            // structs; the function pointer has the signal-handler ABI.
            if unsafe { libc::sigaction(signal, &action, &mut previous) } < 0 {
                return Err(io::Error::last_os_error());
            }
            guard.previous.push((signal, previous));
        }
        Ok(guard)
    }

    pub fn requested(&self) -> bool {
        STOP_REQUESTED.load(Ordering::Relaxed)
    }
}

impl Drop for TerminationSignals {
    fn drop(&mut self) {
        for (signal, previous) in self.previous.iter().rev() {
            // SAFETY: restore the exact disposition saved by this guard.
            unsafe { libc::sigaction(*signal, previous, std::ptr::null_mut()) };
        }
        SIGNALS_INSTALLED.store(false, Ordering::Release);
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn now() -> io::Result<Duration> {
    let time = clock_gettime(ClockId::CLOCK_MONOTONIC).map_err(io::Error::from)?;
    Ok(Duration::new(time.tv_sec() as u64, time.tv_nsec() as u32))
}

fn start_ticks(pid: u32) -> io::Result<u64> {
    parse_start_ticks(&fs::read_to_string(format!("/proc/{pid}/stat"))?)
}

fn parse_start_ticks(stat: &str) -> io::Result<u64> {
    // comm may contain whitespace and parentheses. Fields after its final ')'
    // start at field 3; starttime is field 22 (proc_pid_stat(5)).
    stat.rsplit_once(')')
        .and_then(|(_, fields)| fields.split_whitespace().nth(19))
        .and_then(|field| field.parse().ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| invalid("invalid process birth identity"))
}

/// Kernel process birth plus a supervisor-specific epoch. Registry records may
/// serialize this identity; identities alone are never a teardown acknowledgement.
#[derive(
    Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, serde::Serialize, serde::Deserialize,
)]
#[serde(deny_unknown_fields)]
pub struct WorkerIdentity {
    pid: u32,
    birth: u64,
    epoch: u64,
}

impl WorkerIdentity {
    pub fn pid(&self) -> u32 {
        self.pid
    }
    pub(crate) fn valid(&self) -> bool {
        self.pid > 1 && self.pid <= i32::MAX as u32 && self.birth > 0 && self.epoch > 0
    }
    pub(crate) fn tag(&self) -> String {
        format!("{:08x}-{:016x}-{:016x}", self.pid, self.birth, self.epoch)
    }
    pub(crate) fn from_tag(tag: &str) -> Option<Self> {
        let parts: Vec<_> = tag.split('-').collect();
        let [pid, birth, epoch] = parts.as_slice() else {
            return None;
        };
        let identity = Self {
            pid: u32::from_str_radix(pid, 16).ok()?,
            birth: u64::from_str_radix(birth, 16).ok()?,
            epoch: u64::from_str_radix(epoch, 16).ok()?,
        };
        (identity.valid() && identity.tag() == tag).then_some(identity)
    }

    /// Used only for trusted persisted registry entries. Pin any current PID
    /// before checking its birth and exit; permission/inspection errors refuse
    /// recovery instead of treating an unknown process as dead.
    pub(crate) fn confirmed_dead(&self) -> io::Result<bool> {
        if !self.valid() {
            return Err(invalid("invalid persisted worker identity"));
        }
        // SAFETY: read-only process handle for a bounded positive PID.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, self.pid, 0) };
        if raw < 0 {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(libc::ESRCH) {
                Ok(true)
            } else {
                Err(error)
            };
        }
        // SAFETY: successful pidfd_open returns a new owned descriptor.
        let fd = unsafe { OwnedFd::from_raw_fd(raw as i32) };
        match start_ticks(self.pid) {
            Ok(birth) if birth != self.birth => return Ok(true),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let mut event = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one initialized pollfd; zero timeout never waits for death.
        if unsafe { libc::poll(&mut event, 1, 0) } < 0 {
            return Err(io::Error::last_os_error());
        }
        if event.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
            return Err(invalid("worker lifetime probe failed"));
        }
        Ok(event.revents & (libc::POLLIN | libc::POLLHUP) != 0)
    }
}

#[derive(Debug)]
struct Link(OwnedFd);
impl Link {
    fn pair() -> io::Result<(Self, Self)> {
        let mut fds = [-1; 2];
        // SAFETY: exactly two writable integers; successful socketpair gives
        // separate owned descriptors, nonblocking and close-on-exec.
        let result = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                0,
                fds.as_mut_ptr(),
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: both successful socketpair descriptors are new and owned.
        Ok(unsafe {
            (
                Self(OwnedFd::from_raw_fd(fds[0])),
                Self(OwnedFd::from_raw_fd(fds[1])),
            )
        })
    }
    fn send(&self, bytes: &[u8]) -> io::Result<()> {
        // SAFETY: live socket and initialized bounded packet. NOSIGNAL makes
        // peer death an error rather than terminating the forwarding process.
        let result = unsafe {
            libc::send(
                self.0.as_raw_fd(),
                bytes.as_ptr().cast(),
                bytes.len(),
                libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT,
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        if result as usize != bytes.len() {
            return Err(invalid("partial health packet"));
        }
        Ok(())
    }
    fn receive(&self) -> io::Result<Option<Vec<u8>>> {
        let mut bytes = [0u8; PACKET_SIZE + 1];
        // SAFETY: live socket, writable bounded buffer. MSG_TRUNC reports the
        // whole packet size so oversized messages cannot masquerade as valid.
        let result = unsafe {
            libc::recv(
                self.0.as_raw_fd(),
                bytes.as_mut_ptr().cast(),
                bytes.len(),
                libc::MSG_TRUNC | libc::MSG_DONTWAIT,
            )
        };
        if result < 0 {
            let error = io::Error::last_os_error();
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) {
                return Ok(None);
            }
            return Err(error);
        }
        if result == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        if result as usize > PACKET_SIZE {
            return Err(invalid("oversized health packet"));
        }
        Ok(Some(bytes[..result as usize].to_vec()))
    }
}
impl AsFd for Link {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

/// A private inherited channel, tied to this supervisor's kernel identity.
/// Setting environment variables alone cannot create a valid worker binding.
/// Connect exactly once at worker startup; a failed binding is terminal.
///
/// ```compile_fail
/// fn send<T: Send>() {}
/// send::<prek::supervisor::Heartbeat>();
/// ```
#[derive(Debug)]
pub struct Heartbeat {
    link: Link,
    identity: WorkerIdentity,
    sequence: u64,
    last: Option<Duration>,
    _forwarding_thread: PhantomData<Rc<()>>,
}
impl Heartbeat {
    pub fn connect() -> io::Result<Self> {
        // An inherited fd has exactly one owner. Make even concurrent/failed
        // connect calls terminal for this worker instead of risking double-close.
        CONNECTED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "worker health already claimed",
                )
            })?;
        let fd: i32 = std::env::var(LINK_ENV)
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|fd| *fd >= 3)
            .ok_or_else(|| invalid("worker health descriptor is missing"))?;
        let parent: libc::pid_t = std::env::var(PARENT_ENV)
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|pid| *pid > 0)
            .ok_or_else(|| invalid("worker supervisor identity is missing"))?;
        let mut peer: libc::ucred = unsafe { std::mem::zeroed() };
        let mut size = std::mem::size_of_val(&peer) as libc::socklen_t;
        let mut kind: libc::c_int = 0;
        let mut kind_size = std::mem::size_of_val(&kind) as libc::socklen_t;
        let mut death_signal: libc::c_int = 0;
        // SAFETY: initialized writable outputs of exactly the stated sizes;
        // validation occurs before taking ownership of this inherited fd.
        let valid = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_TYPE,
                (&mut kind as *mut libc::c_int).cast(),
                &mut kind_size,
            ) == 0
                && kind == libc::SOCK_SEQPACKET
                && libc::getsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_PEERCRED,
                    (&mut peer as *mut libc::ucred).cast(),
                    &mut size,
                ) == 0
                && size as usize == std::mem::size_of::<libc::ucred>()
                && peer.pid == parent
                && peer.uid == libc::geteuid()
                && libc::getppid() == parent
                && libc::prctl(libc::PR_GET_PDEATHSIG, &mut death_signal) == 0
                && death_signal == libc::SIGKILL
        };
        if !valid {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "worker health channel is not bound to its supervisor",
            ));
        }
        let epoch: u64 = std::env::var(EPOCH_ENV)
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|epoch| {
                *epoch > 0 && u128::from(*epoch) <= now().map(|time| time.as_nanos()).unwrap_or(0)
            })
            .ok_or_else(|| invalid("worker supervisor epoch is missing or invalid"))?;
        let pid = std::process::id();
        let identity = WorkerIdentity {
            pid,
            birth: start_ticks(pid)?,
            epoch,
        };
        // SAFETY: this trusted inherited descriptor is now owned exactly once.
        let link = Link(unsafe { OwnedFd::from_raw_fd(fd) });
        // SAFETY: live owned fd; prevent future action execs retaining it.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            link,
            identity,
            sequence: 0,
            last: None,
            _forwarding_thread: PhantomData,
        })
    }

    pub fn worker_identity(&self) -> WorkerIdentity {
        self.identity
    }

    /// Checks stop/parent loss on every call; emits at most four health packets
    /// per second. A failed send stops the worker instead of queuing health.
    pub fn progress(&mut self) -> io::Result<()> {
        if let Some(packet) = self.link.receive()? {
            return Err(if packet == b"STOP" {
                io::Error::new(
                    io::ErrorKind::Interrupted,
                    "supervisor requested orderly stop",
                )
            } else {
                invalid("invalid supervisor control packet")
            });
        }
        let time = now()?;
        if self
            .last
            .is_some_and(|last| time.saturating_sub(last) < HEARTBEAT_INTERVAL)
        {
            return Ok(());
        }
        let sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| invalid("health sequence exhausted"))?;
        let timestamp: u64 = time
            .as_nanos()
            .try_into()
            .map_err(|_| invalid("monotonic time exceeds protocol bounds"))?;
        let mut packet = [0u8; PACKET_SIZE];
        packet[..4].copy_from_slice(b"PKH1");
        packet[4..12].copy_from_slice(&sequence.to_be_bytes());
        packet[12..].copy_from_slice(&timestamp.to_be_bytes());
        self.link.send(&packet)?;
        self.sequence = sequence;
        self.last = Some(time);
        Ok(())
    }
    /// Include the channel in the forwarding poll so a stop wakes an idle loop.
    pub fn deadline(&self) -> io::Result<Duration> {
        Ok(self.last.map_or(now()?, |last| last + HEARTBEAT_INTERVAL))
    }
}
impl AsFd for Heartbeat {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.link.as_fd()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Reason {
    Exited,
    Requested,
    Stalled,
    Protocol,
    Disconnected,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Report {
    reason: Reason,
    status: ExitStatus,
    heartbeats: u64,
    forced: bool,
    identity: WorkerIdentity,
}
impl Report {
    pub fn reason(&self) -> Reason {
        self.reason
    }
    pub fn status(&self) -> ExitStatus {
        self.status
    }
    pub fn heartbeats(&self) -> u64 {
        self.heartbeats
    }
    pub fn forced(&self) -> bool {
        self.forced
    }
    pub fn worker_pid(&self) -> u32 {
        self.identity.pid
    }
    pub fn worker_identity(&self) -> WorkerIdentity {
        self.identity
    }
    /// Fences old teardown reports from a different worker lifetime.
    pub fn belongs_to(&self, supervisor: &Supervisor) -> bool {
        self.identity == supervisor.identity
    }
}
#[derive(Clone, Copy, Debug)]
enum Phase {
    Active,
    Requested(Duration),
    Terminating { reason: Reason, deadline: Duration },
    Killing { reason: Reason },
    Exited(Report),
}

/// Must be created on the actual supervising thread and kept there. The worker
/// alone opens evdev/uinput after exec; the channel conveys no device fds.
///
/// ```compile_fail
/// fn send<T: Send>() {}
/// send::<prek::supervisor::Supervisor>();
/// ```
#[derive(Debug)]
pub struct Supervisor {
    child: Child,
    link: Link,
    identity: WorkerIdentity,
    last: Duration,
    sequence: u64,
    closed: Option<Duration>,
    phase: Phase,
    _parent_thread: PhantomData<Rc<()>>,
}
impl Supervisor {
    pub fn spawn(mut command: Command) -> io::Result<Self> {
        let (link, worker) = Link::pair()?;
        let last = now()?;
        // SAFETY: read-only kernel identity of the current parent process.
        let parent = unsafe { libc::getpid() };
        let fd = worker.0.as_raw_fd();
        command
            .env(LINK_ENV, fd.to_string())
            .env(PARENT_ENV, parent.to_string())
            .env(
                EPOCH_ENV,
                u64::try_from(last.as_nanos())
                    .map_err(|_| invalid("supervisor epoch exhausted"))?
                    .to_string(),
            );
        // SAFETY: after fork, only direct prctl/getppid/fcntl syscalls and
        // allocation-free OS errors run. No locks, formatting or Rust I/O.
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) < 0 {
                    return Err(io::Error::last_os_error());
                }
                // Close the race where the parent died before prctl.
                if libc::getppid() != parent {
                    return Err(io::Error::from_raw_os_error(libc::ESRCH));
                }
                if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        drop(worker);
        let birth = match start_ticks(child.id()) {
            Ok(birth) => birth,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        let identity = WorkerIdentity {
            pid: child.id(),
            birth,
            epoch: last.as_nanos() as u64,
        };
        Ok(Self {
            child,
            link,
            identity,
            last,
            sequence: 0,
            closed: None,
            phase: Phase::Active,
            _parent_thread: PhantomData,
        })
    }
    pub fn worker_pid(&self) -> u32 {
        self.child.id()
    }
    pub fn worker_identity(&self) -> WorkerIdentity {
        self.identity
    }
    pub fn heartbeats(&self) -> u64 {
        self.sequence
    }

    fn signal(&mut self, signal: i32) -> io::Result<()> {
        // Child remains owned/unreaped until its report is cached, preventing
        // PID reuse while this signal is sent. No externally supplied PID.
        let result = unsafe { libc::kill(self.child.id() as libc::pid_t, signal) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error);
            }
        }
        Ok(())
    }
    fn terminate(&mut self, reason: Reason, time: Duration) -> io::Result<()> {
        self.signal(libc::SIGTERM)?;
        self.phase = Phase::Terminating {
            reason,
            deadline: time + TERMINATION_GRACE,
        };
        Ok(())
    }
    /// Nonblocking health/reap check. Reporting exit proves actual worker
    /// teardown; a timeout or closed channel alone never acknowledges release.
    pub fn check(&mut self) -> io::Result<Option<Report>> {
        if let Phase::Exited(report) = self.phase {
            return Ok(Some(report));
        }
        if let Some(status) = self.child.try_wait()? {
            let (reason, forced) = match self.phase {
                Phase::Requested(_) => (Reason::Requested, false),
                Phase::Terminating { reason, .. } => (reason, false),
                Phase::Killing { reason } => (reason, true),
                _ => (Reason::Exited, false),
            };
            let report = Report {
                reason,
                status,
                heartbeats: self.sequence,
                forced,
                identity: self.identity,
            };
            self.phase = Phase::Exited(report);
            return Ok(Some(report));
        }
        if matches!(self.phase, Phase::Active) && self.closed.is_none() {
            for _ in 0..32 {
                match self.link.receive() {
                    Ok(None) => break,
                    Ok(Some(packet)) => {
                        let time = now()?;
                        if !self.accept(&packet, time) {
                            self.terminate(Reason::Protocol, time)?;
                            break;
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                        self.closed = Some(now()? + TERMINATION_GRACE);
                        break;
                    }
                    Err(_) => {
                        self.terminate(Reason::Protocol, now()?)?;
                        break;
                    }
                }
            }
        }
        let time = now()?;
        match self.phase {
            Phase::Active if self.closed.is_some_and(|deadline| time >= deadline) => {
                self.terminate(Reason::Disconnected, time)?
            }
            Phase::Active if time.saturating_sub(self.last) >= STALL_TIMEOUT => {
                self.terminate(Reason::Stalled, time)?
            }
            Phase::Requested(deadline) if time >= deadline => {
                self.terminate(Reason::Requested, time)?
            }
            Phase::Terminating { reason, deadline } if time >= deadline => {
                self.signal(libc::SIGKILL)?;
                self.phase = Phase::Killing { reason };
            }
            _ => {}
        }
        Ok(None)
    }
    fn accept(&mut self, packet: &[u8], time: Duration) -> bool {
        if packet.len() != PACKET_SIZE || &packet[..4] != b"PKH1" {
            return false;
        }
        let sequence = u64::from_be_bytes(packet[4..12].try_into().unwrap());
        let timestamp = Duration::from_nanos(u64::from_be_bytes(packet[12..].try_into().unwrap()));
        if self.sequence.checked_add(1) != Some(sequence)
            || timestamp < self.last
            || timestamp > time
            || time - timestamp >= STALL_TIMEOUT
        {
            return false;
        }
        self.sequence = sequence;
        self.last = timestamp;
        true
    }
    pub fn request_stop(&mut self) -> io::Result<()> {
        if self.check()?.is_some() || !matches!(self.phase, Phase::Active) {
            return Ok(());
        }
        let time = now()?;
        if self.link.send(b"STOP").is_err() {
            return self.terminate(Reason::Requested, time);
        }
        self.phase = Phase::Requested(time + TERMINATION_GRACE);
        Ok(())
    }
    /// Bounded poll intervals; the same supervisor works in foreground/service
    /// modes. Input work and external actions never execute in this process.
    pub fn wait(&mut self) -> io::Result<Report> {
        self.wait_with_stop(|| false)
    }

    /// A nonblocking cancellation callback lets a foreground/service signal
    /// handler request stop without doing process or input work in the handler.
    pub fn wait_with_stop(&mut self, mut requested: impl FnMut() -> bool) -> io::Result<Report> {
        loop {
            if requested() {
                self.request_stop()?;
            }
            if let Some(report) = self.check()? {
                return Ok(report);
            }
            let time = now()?;
            let deadline = match self.phase {
                Phase::Active => self.closed.unwrap_or(self.last + STALL_TIMEOUT),
                Phase::Requested(deadline) | Phase::Terminating { deadline, .. } => deadline,
                _ => time + Duration::from_millis(25),
            };
            let timeout = deadline.saturating_sub(time).min(HEARTBEAT_INTERVAL);
            let timeout = PollTimeout::try_from(timeout).map_err(io::Error::other)?;
            let result = if self.closed.is_some() || !matches!(self.phase, Phase::Active) {
                poll(
                    &mut [],
                    PollTimeout::try_from(
                        Duration::from_millis(25).min(deadline.saturating_sub(time)),
                    )
                    .map_err(io::Error::other)?,
                )
            } else {
                poll(
                    &mut [PollFd::new(self.link.as_fd(), PollFlags::POLLIN)],
                    timeout,
                )
            };
            if let Err(error) = result
                && error != nix::errno::Errno::EINTR
            {
                return Err(io::Error::from(error));
            }
        }
    }
}
impl Drop for Supervisor {
    fn drop(&mut self) {
        if !matches!(self.phase, Phase::Exited(_)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn process_stat_comm_parentheses_do_not_shift_birth_identity() {
        let fields = std::iter::repeat_n("0", 19)
            .chain(["12345", "0", "0"])
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(
            parse_start_ticks(&format!("42 (odd ) name) {fields}")).unwrap(),
            12345
        );
        assert!(parse_start_ticks("42 (truncated) S 0").is_err());
    }
    #[test]
    fn private_channel_preserves_packets_and_rejects_oversized_messages() {
        let (a, b) = Link::pair().unwrap();
        assert!(a.receive().unwrap().is_none());
        b.send(b"STOP").unwrap();
        assert_eq!(a.receive().unwrap().unwrap(), b"STOP");
        b.send(&[0; PACKET_SIZE + 1]).unwrap();
        assert!(a.receive().is_err());
        drop(b);
        assert_eq!(
            a.receive().unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }
}
