//! Unprivileged same-login session re-execution without retaining input authority.
use nix::libc;
use std::{
    collections::VecDeque,
    io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::process::CommandExt,
    },
    process::{Child, Command},
    thread,
    time::Duration,
};

const READY_FD: &str = "PREK_SESSION_READY_FD";
const PARENT: &str = "PREK_SESSION_LAUNCH_PARENT";
const RETRY_EXIT: u8 = 75;
const POLL: Duration = Duration::from_millis(25);
const RETRY: Duration = Duration::from_millis(500);
const WINDOW: Duration = Duration::from_secs(60);
const STARTUP: Duration = Duration::from_secs(5);
const GRACE: Duration = Duration::from_millis(250);

#[derive(Debug)]
struct Retryable(io::Error);
impl std::fmt::Display for Retryable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for Retryable {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}
pub fn retry(error: io::Error) -> io::Error {
    io::Error::other(Retryable(error))
}
pub fn exit_code(error: &io::Error) -> Option<u8> {
    error
        .get_ref()?
        .downcast_ref::<Retryable>()
        .map(|_| RETRY_EXIT)
}
pub fn classify(error: io::Error) -> io::Error {
    if exit_code(&error).is_some() {
        return error;
    }
    if crate::ipc::monotonic().is_err() || crate::ipc::is_transport_loss(&error) {
        retry(error)
    } else {
        error
    }
}
fn unavailable(text: &'static str) -> io::Error {
    io::Error::other(text)
}
fn boottime() -> io::Result<Duration> {
    let time = nix::time::clock_gettime(nix::time::ClockId::CLOCK_BOOTTIME)?;
    let seconds =
        u64::try_from(time.tv_sec()).map_err(|_| unavailable("invalid launcher clock"))?;
    let nanos = u32::try_from(time.tv_nsec())
        .ok()
        .filter(|n| *n < 1_000_000_000)
        .ok_or_else(|| unavailable("invalid launcher clock"))?;
    Ok(Duration::new(seconds, nanos))
}

/// One-way notification that this child accepted a broker-verified registration.
/// It conveys no fence, descriptor, lease, input readiness or effect authority.
/// Connect on the exec thread before starting helpers.
pub struct Ready(Option<OwnedFd>);
impl Ready {
    pub fn connect() -> io::Result<Self> {
        let number = |name| std::env::var(name).ok().and_then(|s| s.parse::<i32>().ok());
        let fd = number(READY_FD)
            .filter(|fd| *fd >= 3)
            .ok_or_else(|| unavailable("session child has no launcher pipe"))?;
        let parent = number(PARENT)
            .filter(|pid| *pid > 1)
            .ok_or_else(|| unavailable("session child has no launcher identity"))?;
        let mut death_signal = 0;
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: read-only process/descriptor probes with initialized C outputs.
        let valid = unsafe {
            libc::getppid() == parent
                && libc::prctl(libc::PR_GET_PDEATHSIG, &mut death_signal) == 0
                && death_signal == libc::SIGKILL
                && libc::fstat(fd, &mut stat) == 0
                && stat.st_mode & libc::S_IFMT == libc::S_IFIFO
                && libc::fcntl(fd, libc::F_GETFL) & libc::O_ACCMODE == libc::O_WRONLY
        };
        if !valid {
            return Err(unavailable("invalid session launcher binding"));
        }
        // SAFETY: this validated inherited write descriptor becomes owned here.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(Some(fd)))
    }
    pub fn accepted(&mut self) -> io::Result<()> {
        let Some(fd) = self.0.take() else {
            return Err(unavailable("duplicate session readiness"));
        };
        // SAFETY: one initialized byte written to the owned nonblocking pipe.
        if unsafe { libc::write(fd.as_raw_fd(), b"R".as_ptr().cast(), 1) } != 1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

struct Owned {
    child: Child,
    ready: OwnedFd,
    exit: OwnedFd,
    closed: bool,
    accepted: bool,
}
impl Owned {
    fn spawn(command: &mut Command) -> io::Result<Self> {
        let mut fds = [-1; 2];
        // SAFETY: pipe2 initializes exactly two new owned descriptors.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        let fd = write.as_raw_fd();
        let parent = unsafe { libc::getpid() };
        command
            .env(READY_FD, fd.to_string())
            .env(PARENT, parent.to_string());
        // SAFETY: the post-fork closure uses only allocation-free kernel calls.
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) < 0
                    || libc::fcntl(fd, libc::F_SETFD, 0) < 0
                {
                    return Err(io::Error::last_os_error());
                }
                if libc::getppid() != parent {
                    return Err(io::Error::from_raw_os_error(libc::ESRCH));
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        // SAFETY: pin the exact owned, unreaped child for exit notification.
        let exit = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id(), 0) };
        if exit < 0 {
            let error = io::Error::last_os_error();
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        Ok(Self {
            child,
            ready: read,
            exit: unsafe { OwnedFd::from_raw_fd(exit as i32) },
            closed: false,
            accepted: false,
        })
    }
    fn read_ready(&mut self) -> io::Result<()> {
        if self.accepted || self.closed {
            return Ok(());
        }
        let mut byte = [0];
        // SAFETY: one writable byte and the owned nonblocking read descriptor.
        let read = unsafe { libc::read(self.ready.as_raw_fd(), byte.as_mut_ptr().cast(), 1) };
        if read == 1 {
            if byte != *b"R" {
                return Err(unavailable("invalid session readiness byte"));
            }
            self.accepted = true;
        } else if read == 0 {
            self.closed = true;
        } else if read < 0 {
            let error = io::Error::last_os_error();
            if !matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) {
                return Err(error);
            }
        }
        Ok(())
    }
    fn wait(&self, timeout: Duration) -> io::Result<()> {
        let mut events = vec![libc::pollfd {
            fd: self.exit.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        if !self.accepted && !self.closed {
            events.push(libc::pollfd {
                fd: self.ready.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        let timeout = timeout.as_millis().saturating_add(1).min(1000) as i32;
        // SAFETY: exact initialized pollfd storage and a bounded timeout.
        if unsafe { libc::poll(events.as_mut_ptr(), events.len() as libc::nfds_t, timeout) } < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
        if events
            .iter()
            .any(|event| event.revents & (libc::POLLERR | libc::POLLNVAL) != 0)
        {
            return Err(unavailable("session lifecycle descriptor failed"));
        }
        Ok(())
    }
    fn stop(&mut self) -> io::Result<()> {
        if self.child.try_wait()?.is_some() {
            return Ok(());
        }
        // SAFETY: an unreaped owned child retains this positive PID.
        if unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) } < 0
            && io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
        {
            return Err(io::Error::last_os_error());
        }
        let until = boottime()? + GRACE;
        while boottime()? < until {
            if self.child.try_wait()?.is_some() {
                return Ok(());
            }
            thread::sleep(POLL);
        }
        self.child.kill()?;
        self.child.wait()?;
        Ok(())
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Default)]
struct Budget {
    attempts: VecDeque<Duration>,
    outage: Option<Duration>,
}
impl Budget {
    fn attempt(&mut self, now: Duration) -> io::Result<()> {
        while self
            .attempts
            .front()
            .is_some_and(|at| now.saturating_sub(*at) >= WINDOW)
        {
            self.attempts.pop_front();
        }
        if self.attempts.len() >= 16
            || self
                .outage
                .is_some_and(|at| now.saturating_sub(at) >= WINDOW)
        {
            return Err(unavailable(
                "session reconnect budget exhausted; restart from the owning login session",
            ));
        }
        self.attempts.push_back(now);
        self.outage.get_or_insert(now);
        Ok(())
    }
    fn accepted(&mut self) {
        self.outage = None;
    }
}

/// Re-execute only after an explicitly retryable exit, never concurrently.
/// CLOCK_BOOTTIME bounds this authority-free guardian's lifecycle, not input.
pub fn run(mut command: impl FnMut() -> Command) -> io::Result<()> {
    let signals = crate::supervisor::TerminationSignals::install()?;
    let mut budget = Budget::default();
    loop {
        if signals.requested() {
            return Ok(());
        }
        let started = boottime()?;
        budget.attempt(started)?;
        let mut child = Owned::spawn(&mut command())?;
        loop {
            if signals.requested() {
                child.stop()?;
                return Ok(());
            }
            child.read_ready()?;
            if child.accepted {
                budget.accepted();
            }
            if let Some(status) = child.child.try_wait()? {
                if status.success() {
                    return Ok(());
                }
                if status.code() != Some(i32::from(RETRY_EXIT)) {
                    return Err(io::Error::other(format!("session agent stopped: {status}")));
                }
                break;
            }
            if !child.accepted && boottime()?.saturating_sub(started) >= STARTUP {
                child.stop()?;
                break;
            }
            child.wait(if child.accepted {
                Duration::from_secs(1)
            } else {
                (started + STARTUP).saturating_sub(boottime()?)
            })?;
        }
        drop(child);
        eprintln!("Session agent retired; reconnecting with fresh process authority.");
        let until = boottime()? + RETRY;
        while boottime()? < until {
            if signals.requested() {
                return Ok(());
            }
            thread::sleep(POLL);
        }
    }
}

#[cfg(test)]
mod tests;
