//! Disposable, unprivileged Lua configuration compilation. The parent owns
//! only nonblocking pipes and the child handle; no input descriptor is shared.
use crate::{config::Config, ipc::monotonic};
use nix::{
    libc,
    poll::{PollFd, PollFlags, PollTimeout, poll},
};
use std::{
    io::{self, Read, Write},
    os::{
        fd::{AsFd, AsRawFd, FromRawFd, OwnedFd},
        unix::process::CommandExt,
    },
    path::Path,
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio},
    time::Duration,
};

pub const TIMEOUT: Duration = Duration::from_secs(2);
pub const PROCESS_MEMORY: u64 = 256 * 1024 * 1024;
const MAX_ERROR: usize = 4096;
const IO_SLICE: usize = 64 * 1024;
pub struct Compiled {
    pub text: String,
    pub config: Config,
    pub runtime: Option<crate::lua::Definition>,
}
pub struct Job {
    child: Child,
    pidfd: Option<OwnedFd>,
    input: Option<ChildStdin>,
    output: Option<ChildStdout>,
    error: Option<ChildStderr>,
    source: Vec<u8>,
    approved: crate::lua::Sources,
    written: usize,
    result: Vec<u8>,
    diagnostics: Vec<u8>,
    name: String,
    expires: Duration,
    finished: bool,
}
fn nonblocking(fd: &impl AsFd) -> io::Result<()> {
    // SAFETY: descriptor is owned, both fcntl commands use scalar flags.
    let flags = unsafe { libc::fcntl(fd.as_fd().as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe {
            libc::fcntl(
                fd.as_fd().as_raw_fd(),
                libc::F_SETFL,
                flags | libc::O_NONBLOCK,
            )
        } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
#[cfg(target_env = "musl")]
type LimitResource = libc::c_int;
#[cfg(not(target_env = "musl"))]
type LimitResource = libc::__rlimit_resource_t;
fn limit(resource: LimitResource, value: u64) -> io::Result<()> {
    let limit = libc::rlimit {
        rlim_cur: value,
        rlim_max: value,
    };
    // SAFETY: initialized limit structure for a scalar resource.
    if unsafe { libc::setrlimit(resource, &limit) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
/// Called after exec, so loader setup is independent from the Lua heap limit.
pub fn worker_limits() -> io::Result<()> {
    runtime_limits()?;
    limit(libc::RLIMIT_CPU, 2)?;
    Ok(())
}
pub(crate) fn runtime_limits() -> io::Result<()> {
    if unsafe { libc::geteuid() } == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Lua compiler cannot run as root",
        ));
    }
    limit(libc::RLIMIT_CORE, 0)?;
    limit(libc::RLIMIT_DATA, PROCESS_MEMORY)?;
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
pub(crate) fn runtime_cpu_budget() -> io::Result<()> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    let mut limits = std::mem::MaybeUninit::<libc::rlimit>::zeroed();
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0
        || unsafe { libc::getrlimit(libc::RLIMIT_CPU, limits.as_mut_ptr()) } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let usage = unsafe { usage.assume_init() };
    let mut limits = unsafe { limits.assume_init() };
    let used = (usage.ru_utime.tv_sec + usage.ru_stime.tv_sec) as u64;
    limits.rlim_cur = used.saturating_add(3).min(limits.rlim_max);
    if unsafe { libc::setrlimit(libc::RLIMIT_CPU, &limits) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
impl Job {
    pub fn spawn(executable: &Path, source: String, name: &str) -> io::Result<Self> {
        Self::spawn_command(Command::new(executable), source, name)
    }
    pub fn spawn_sources(executable: &Path, sources: crate::lua::Sources) -> io::Result<Self> {
        Self::spawn_sources_command(Command::new(executable), sources)
    }
    /// Explicit runner for debug integration tests on emulated architectures.
    #[cfg(debug_assertions)]
    pub fn spawn_with_runner(
        runner: &Path,
        executable: &Path,
        source: String,
        name: &str,
    ) -> io::Result<Self> {
        let mut command = Command::new(runner);
        command.arg(executable);
        Self::spawn_command(command, source, name)
    }
    #[cfg(debug_assertions)]
    pub fn spawn_sources_with_runner(
        runner: &Path,
        executable: &Path,
        sources: crate::lua::Sources,
    ) -> io::Result<Self> {
        let mut command = Command::new(runner);
        command.arg(executable);
        Self::spawn_sources_command(command, sources)
    }
    fn spawn_command(command: Command, source: String, name: &str) -> io::Result<Self> {
        Self::spawn_sources_command(command, crate::lua::Sources::single(source, name.into()))
    }
    fn spawn_sources_command(
        mut command: Command,
        approved: crate::lua::Sources,
    ) -> io::Result<Self> {
        approved.validate().map_err(io::Error::other)?;
        let source = serde_json::to_vec(&approved).map_err(io::Error::other)?;
        let name = approved.name.clone();
        let expires = monotonic()? + TIMEOUT;
        command
            .args(["lua-compile-worker", "--name", &name])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_clear();
        let parent = unsafe { libc::getpid() };
        let privileged = unsafe { libc::geteuid() } == 0;
        // SAFETY: after fork this closure performs only async-signal-safe
        // scalar syscalls; it captures no references or allocation machinery.
        unsafe {
            command.pre_exec(move || {
                if privileged
                    && (libc::setgroups(0, std::ptr::null()) != 0
                        || libc::setgid(65534) != 0
                        || libc::setuid(65534) != 0)
                {
                    return Err(io::Error::last_os_error());
                }
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
                    || libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0
                {
                    return Err(io::Error::last_os_error());
                }
                if libc::getppid() != parent {
                    return Err(io::Error::from_raw_os_error(libc::ESRCH));
                }
                Ok(())
            });
        }
        let child = command.spawn()?;
        let mut job = Self {
            child,
            pidfd: None,
            input: None,
            output: None,
            error: None,
            source,
            approved,
            written: 0,
            result: Vec::new(),
            diagnostics: Vec::new(),
            name,
            expires,
            finished: false,
        };
        job.input = job.child.stdin.take();
        job.output = job.child.stdout.take();
        job.error = job.child.stderr.take();
        nonblocking(job.input.as_ref().unwrap())?;
        nonblocking(job.output.as_ref().unwrap())?;
        nonblocking(job.error.as_ref().unwrap())?;
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, job.child.id(), 0) };
        if fd >= 0 {
            job.pidfd = Some(unsafe { OwnedFd::from_raw_fd(fd as i32) });
        }
        Ok(job)
    }
    pub fn deadline(&self) -> Duration {
        self.expires
    }
    pub fn poll_sources(&self) -> Vec<PollFd<'_>> {
        let mut fds = Vec::new();
        if let Some(input) = &self.input {
            fds.push(PollFd::new(input.as_fd(), PollFlags::POLLOUT));
        }
        if let Some(output) = &self.output {
            fds.push(PollFd::new(output.as_fd(), PollFlags::POLLIN));
        }
        if let Some(error) = &self.error {
            fds.push(PollFd::new(error.as_fd(), PollFlags::POLLIN));
        }
        if let Some(pidfd) = &self.pidfd {
            fds.push(PollFd::new(pidfd.as_fd(), PollFlags::POLLIN));
        }
        fds
    }
    pub fn step(&mut self) -> io::Result<Option<Compiled>> {
        if self.finished {
            return Err(io::Error::other("compiler result already taken"));
        }
        if monotonic()? >= self.expires {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "isolated Lua compilation timed out",
            ));
        }
        if let Some(input) = &mut self.input {
            let end = (self.written + IO_SLICE).min(self.source.len());
            let mut closed = false;
            match input.write(&self.source[self.written..end]) {
                Ok(0) if self.written < self.source.len() => {
                    return Err(io::ErrorKind::WriteZero.into());
                }
                Ok(count) => self.written += count,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                // A compiler that stopped reading is judged by its exit; an
                // unread source can never match the returned snapshot.
                Err(error) if error.kind() == io::ErrorKind::BrokenPipe => closed = true,
                Err(error) => return Err(error),
            }
            if closed || self.written == self.source.len() {
                self.input.take();
            }
        }
        drain(
            &mut self.output,
            &mut self.result,
            crate::lua::MAX_SNAPSHOT_BYTES,
        )?;
        drain(&mut self.error, &mut self.diagnostics, MAX_ERROR)?;
        if let Some(status) = self.child.try_wait()? {
            if self.output.is_some() || self.error.is_some() {
                return Ok(None);
            }
            self.finished = true;
            if !status.success() {
                let reason = String::from_utf8_lossy(&self.diagnostics).trim().to_owned();
                if std::os::unix::process::ExitStatusExt::signal(&status).is_some()
                    && reason.contains("memory allocation of")
                {
                    return Err(io::Error::other(
                        "Lua configuration exceeded the isolated compiler memory limit",
                    ));
                }
                return Err(io::Error::other(if reason.is_empty() {
                    format!("isolated Lua compiler failed: {status}")
                } else {
                    reason
                }));
            }
            let definition: crate::lua::Definition =
                serde_json::from_slice(&self.result).map_err(io::Error::other)?;
            if definition.source != self.approved.source
                || definition.name != self.name
                || definition.modules != self.approved.modules
            {
                return Err(io::Error::other(
                    "Lua compiler returned a different source snapshot",
                ));
            }
            let config = definition.validate().map_err(io::Error::other)?;
            let text = definition.model.clone();
            let runtime = (!definition.registrations.is_empty()
                || !definition.timers.is_empty()
                || !definition.handlers.is_empty()
                || !definition.tasks.is_empty())
            .then_some(definition);
            return Ok(Some(Compiled {
                text,
                config,
                runtime,
            }));
        }
        Ok(None)
    }
    pub fn wait(mut self) -> io::Result<Compiled> {
        loop {
            if let Some(compiled) = self.step()? {
                return Ok(compiled);
            }
            let remaining = self
                .expires
                .saturating_sub(monotonic()?)
                .min(Duration::from_millis(50));
            let timeout = PollTimeout::try_from(remaining).map_err(io::Error::other)?;
            if let Err(error) = poll(&mut self.poll_sources(), timeout)
                && error != nix::errno::Errno::EINTR
            {
                return Err(error.into());
            }
        }
    }
}
fn drain<R: Read>(reader: &mut Option<R>, data: &mut Vec<u8>, limit: usize) -> io::Result<()> {
    let Some(input) = reader else {
        return Ok(());
    };
    let mut buffer = [0; 8192];
    let mut total = 0;
    while total < IO_SLICE {
        match input.read(&mut buffer) {
            Ok(0) => {
                *reader = None;
                break;
            }
            Ok(count) => {
                if data.len() + count > limit {
                    return Err(io::Error::other(
                        "isolated compiler output exceeds byte limit",
                    ));
                }
                data.extend_from_slice(&buffer[..count]);
                total += count;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}
impl Drop for Job {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell(script: &str) -> Job {
        shell_source(script, String::new())
    }
    fn shell_source(script: &str, source: String) -> Job {
        let executable = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|path| path.join("sh"))
            .find(|path| path.is_file())
            .unwrap();
        let mut command = Command::new(executable);
        command.args(["-c", script]);
        Job::spawn_command(command, source, "fixture.lua").unwrap()
    }

    #[test]
    fn abandoned_compiler_is_killed_and_reaped() {
        let job = shell("while :; do :; done");
        let pid = job.child.id() as libc::pid_t;
        drop(job);
        assert_eq!(
            unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) },
            -1
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    #[test]
    fn timeout_terminates_and_reaps_the_compiler() {
        let mut job = shell("while :; do :; done");
        let pid = job.child.id() as libc::pid_t;
        job.expires = monotonic().unwrap();
        assert_eq!(job.step().err().unwrap().kind(), io::ErrorKind::TimedOut);
        drop(job);
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    }

    #[test]
    fn compiler_diagnostics_are_bounded_and_failure_is_reaped() {
        let job = shell("while :; do printf '0123456789012345678901234567890123456789' >&2; done");
        let pid = job.child.id() as libc::pid_t;
        let error = job.wait().err().unwrap();
        assert!(error.to_string().contains("byte limit"), "{error}");
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    }

    #[test]
    fn compiler_allocation_abort_reports_the_memory_limit_not_runtime_text() {
        let job = shell(
            "printf 'memory allocation of 134217728 bytes failed\\nnote: run with `RUST_BACKTRACE=1`\\n' >&2; kill -ABRT $$",
        );
        let error = job.wait().err().unwrap().to_string();
        assert_eq!(
            error,
            "Lua configuration exceeded the isolated compiler memory limit"
        );
        let job = shell("printf 'memory allocation of 1 bytes failed\\n' >&2; exit 3");
        let error = job.wait().err().unwrap().to_string();
        assert!(error.contains("memory allocation"), "{error}");
        // More source than a pipe holds guarantees writes after the reader closed.
        let job = shell_source(
            "exec 0<&-; printf 'memory allocation of 8 bytes failed\\n' >&2; kill -ABRT $$",
            format!("-- {}\nreturn {{version=1}}\n", "x".repeat(300_000)),
        );
        let error = job.wait().err().unwrap().to_string();
        assert_eq!(
            error,
            "Lua configuration exceeded the isolated compiler memory limit"
        );
        let job = shell_source(
            "exec 0<&-; exit 0",
            format!("-- {}\nreturn {{version=1}}\n", "x".repeat(300_000)),
        );
        assert!(job.wait().is_err());
    }

    #[test]
    fn compiler_cannot_gain_privileges_and_root_drops_groups() {
        let job = shell("while :; do :; done");
        let path = format!("/proc/{}/status", job.child.id());
        let mut status = String::new();
        for _ in 0..100 {
            status = std::fs::read_to_string(&path).unwrap();
            if status.contains("NoNewPrivs:\t1") {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(status.contains("NoNewPrivs:\t1"), "{status}");
        if unsafe { libc::geteuid() } == 0 {
            assert!(
                status.contains("Uid:\t65534\t65534\t65534\t65534"),
                "{status}"
            );
            assert!(
                status.contains("Gid:\t65534\t65534\t65534\t65534"),
                "{status}"
            );
            assert!(
                status.lines().any(|line| line.trim_end() == "Groups:"),
                "{status}"
            );
        }
    }
}
