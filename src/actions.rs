//! Bounded user-session command execution. Broker offers require a separate,
//! fresh execution grant; observation messages never enter this executor.
use crate::{
    config::Action,
    ipc::{Channel, monotonic},
    protocol::{self, Capability, Completion, Fence, Mode, Reply, Request},
    rules::{Event, Stage},
};
use nix::{
    libc,
    poll::{PollFd, PollFlags},
};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    io,
    os::fd::{AsFd, FromRawFd, OwnedFd},
    process::{Child, Command, Stdio},
    time::Duration,
};

pub const MAX_RUNNING: usize = 8;
const MAX_PENDING: usize = 64;
const MAX_GESTURES: usize = 4096;
struct Pending {
    event: Box<Event>,
    expires: Option<Duration>,
}

struct Running {
    fence: Fence,
    id: u64,
    ordinal: u64,
    child: Child,
    pidfd: Option<OwnedFd>,
}
pub struct Executor {
    fence: Fence,
    seat: String,
    mode: Mode,
    capabilities: BTreeSet<Capability>,
    seen: BTreeMap<u64, u64>,
    pending: BTreeMap<(u64, u64), Pending>,
    running: Vec<Running>,
    outgoing: VecDeque<Request>,
    reported_available: Option<bool>,
}
impl Executor {
    pub fn new(
        fence: Fence,
        seat: String,
        mode: Mode,
        capabilities: BTreeSet<Capability>,
    ) -> io::Result<Self> {
        fence.validate()?;
        // SAFETY: read-only current process credentials.
        if unsafe { libc::geteuid() } == 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "action executor must run as the unprivileged session user",
            ));
        }
        Ok(Self {
            fence,
            seat,
            mode,
            capabilities,
            seen: BTreeMap::new(),
            pending: BTreeMap::new(),
            running: Vec::new(),
            outgoing: VecDeque::new(),
            reported_available: None,
        })
    }
    pub fn transition(&mut self, fence: Fence) -> io::Result<()> {
        fence.validate()?;
        if fence.broker != self.fence.broker
            || fence.configuration < self.fence.configuration
            || fence.session < self.fence.session
            || fence == self.fence
        {
            return Err(protocol::invalid("invalid executor generation transition"));
        }
        self.fence = fence;
        self.pending.clear();
        self.seen.clear();
        self.outgoing.clear();
        self.reported_available = None;
        // Already dispatched commands cannot be undone. Keep reaping them,
        // counting them against the bound, and discard their old completions.
        Ok(())
    }
    fn queue(&mut self, request: Request) -> io::Result<()> {
        if self.outgoing.len() >= MAX_PENDING {
            return Err(protocol::invalid("action acknowledgement queue overflow"));
        }
        self.outgoing.push_back(request);
        Ok(())
    }
    fn complete(&mut self, id: u64, ordinal: u64, result: Completion) -> io::Result<()> {
        self.queue(Request::Complete {
            fence: self.fence,
            id,
            ordinal,
            result,
        })
    }
    pub fn offer(&mut self, fence: Fence, event: Box<Event>, now: Duration) -> io::Result<()> {
        Reply::Dispatch {
            fence,
            event: event.clone(),
        }
        .validate()?;
        let Event::Matched {
            id,
            ordinal,
            source,
            at,
            action,
            ..
        } = event.as_ref()
        else {
            return Err(protocol::invalid("only a match can offer an action"));
        };
        if fence != self.fence
            || self.mode != Mode::Route
            || !self.capabilities.contains(&Capability::Commands)
            || !matches!(action, Action::Command { .. })
            || source.seat != self.seat
            || *id == 0
            || *ordinal == 0
            || source.generation == 0
            || *at > now
            || now - *at > protocol::MAX_PACKET_AGE
        {
            return Err(protocol::invalid(
                "command offer lacks current session authority",
            ));
        }
        crate::config::validate_action(action).map_err(|error| protocol::invalid(&error))?;
        if self.seen.get(id).is_some_and(|old| ordinal <= old)
            || (!self.seen.contains_key(id) && self.seen.len() >= MAX_GESTURES)
            || self.pending.len() >= MAX_PENDING
        {
            return Err(protocol::invalid(
                "duplicate action or bounded gesture queue exhausted",
            ));
        }
        let (id, ordinal) = (*id, *ordinal);
        self.queue(Request::Authorize { fence, id, ordinal })?;
        self.seen.insert(id, ordinal);
        self.pending.insert(
            (id, ordinal),
            Pending {
                event,
                expires: None,
            },
        );
        Ok(())
    }
    pub fn grant(
        &mut self,
        fence: Fence,
        id: u64,
        ordinal: u64,
        expires: Duration,
        now: Duration,
    ) -> io::Result<()> {
        if fence != self.fence || expires <= now || expires > now + crate::context::SNAPSHOT_MAX_AGE
        {
            return Err(protocol::invalid("stale action execution grant"));
        }
        let pending = self
            .pending
            .get_mut(&(id, ordinal))
            .ok_or_else(|| protocol::invalid("execution grant has no offered action"))?;
        if pending.expires.is_some() {
            return Err(protocol::invalid("duplicate execution grant"));
        }
        pending.expires = Some(expires);
        Ok(())
    }
    /// Call after draining current broker messages, so a queued generation
    /// change invalidates earlier grants before any subprocess can start.
    pub fn execute_ready(&mut self, now: Duration) -> io::Result<()> {
        let ready: Vec<_> = self
            .pending
            .iter()
            .filter_map(|(identity, pending)| pending.expires.map(|_| *identity))
            .collect();
        for (id, ordinal) in ready {
            let pending = self.pending.remove(&(id, ordinal)).unwrap();
            if now.max(monotonic()?) >= pending.expires.unwrap() {
                self.complete(id, ordinal, Completion::Cancelled)?;
                continue;
            }
            let Event::Matched {
                stage,
                action: Action::Command { argv },
                ..
            } = *pending.event
            else {
                unreachable!("offers were validated");
            };
            if !matches!(stage, Stage::Discrete | Stage::Begin) {
                self.complete(id, ordinal, Completion::Cancelled)?;
                continue;
            }
            if self.running.len() >= MAX_RUNNING {
                self.complete(id, ordinal, Completion::Unavailable)?;
                continue;
            }
            // Exact configured argv, without implicit shell interpretation.
            // Command output cannot stall the agent or become its JSON stream.
            match Command::new(&argv[0])
                .args(&argv[1..])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(child) => {
                    // SAFETY: read-only handle for the owned live child. On old
                    // kernels a bounded polling fallback still reaps children.
                    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id(), 0) };
                    let pidfd = (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd as i32) });
                    self.running.push(Running {
                        fence: self.fence,
                        id,
                        ordinal,
                        child,
                        pidfd,
                    });
                }
                Err(_) => self.complete(id, ordinal, Completion::Unavailable)?,
            }
        }
        Ok(())
    }
    pub fn reap(&mut self) -> io::Result<()> {
        for index in (0..self.running.len()).rev() {
            if let Some(status) = self.running[index].child.try_wait()? {
                let running = self.running.remove(index);
                if running.fence == self.fence {
                    self.complete(
                        running.id,
                        running.ordinal,
                        if status.success() {
                            Completion::Applied
                        } else {
                            Completion::Unavailable
                        },
                    )?;
                }
            }
        }
        Ok(())
    }
    pub fn flush(&mut self, channel: &mut Channel) -> io::Result<()> {
        let available = self.running.len() + self.pending.len() < MAX_RUNNING;
        if self.reported_available != Some(available) {
            self.queue(Request::Availability {
                fence: self.fence,
                capabilities: if available {
                    BTreeSet::from([Capability::Commands])
                } else {
                    BTreeSet::new()
                },
            })?;
            self.reported_available = Some(available);
        }
        for _ in 0..8 {
            let Some(request) = self.outgoing.front() else {
                break;
            };
            if !channel.send(request, monotonic()?)? {
                break;
            }
            self.outgoing.pop_front();
        }
        Ok(())
    }
    pub fn wants_write(&self) -> bool {
        !self.outgoing.is_empty()
    }
    pub fn poll_sources(&self) -> Vec<PollFd<'_>> {
        self.running
            .iter()
            .filter_map(|running| running.pidfd.as_ref())
            .map(|fd| PollFd::new(fd.as_fd(), PollFlags::POLLIN))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{input::Source, rules::Evidence};
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        thread,
        time::Instant,
    };

    fn fence() -> Fence {
        Fence {
            broker: protocol::Epoch([1, 2]),
            configuration: 1,
            session: 1,
        }
    }
    fn executor(mode: Mode) -> Executor {
        Executor::new(
            fence(),
            "seat0".into(),
            mode,
            BTreeSet::from([Capability::Commands]),
        )
        .unwrap()
    }
    fn path() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "prek-command-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }
    fn event(id: u64, output: &std::path::Path) -> Box<Event> {
        let mut argv: Vec<String> = std::env::var("PREK_TEST_RUNNER").ok().into_iter().collect();
        argv.extend([
            std::env::current_exe().unwrap().to_str().unwrap().into(),
            "--ignored".into(),
            "--exact".into(),
            "actions::tests::command_writer_fixture".into(),
            "--skip".into(),
            format!("output={}", output.display()),
            "--skip".into(),
            "payload=$(echo shell-expanded);*'literal'".into(),
        ]);
        Box::new(Event::Matched {
            version: 1,
            rule: "fixture".into(),
            id,
            ordinal: 1,
            source: Source {
                device: 1,
                generation: 1,
                seat: "seat0".into(),
            },
            at: monotonic().unwrap(),
            config_generation: 1,
            session_generation: 1,
            stage: Stage::Discrete,
            action: Action::Command { argv },
            evidence: Evidence::Key { down: true },
            window: None,
            output: None,
            workspace: None,
        })
    }
    fn finish(executor: &mut Executor) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !executor.running.is_empty() {
            executor.reap().unwrap();
            assert!(Instant::now() < deadline, "command did not finish");
            thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    #[ignore = "owned subprocess fixture, invoked only by command-execution tests"]
    fn command_writer_fixture() {
        let args: Vec<_> = std::env::args().collect();
        let field = |name: &str| args.iter().find_map(|arg| arg.strip_prefix(name));
        let output = field("output=").expect("missing output fixture path");
        fs::write(
            output,
            serde_json::to_vec(&serde_json::json!({
                "uid": unsafe { libc::geteuid() }, "payload": field("payload=").unwrap()
            }))
            .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn exact_argv_requires_a_fresh_grant_and_runs_once_as_the_session_user() {
        let output = path();
        let mut executor = executor(Mode::Route);
        executor
            .offer(fence(), event(1, &output), monotonic().unwrap())
            .unwrap();
        executor.execute_ready(monotonic().unwrap()).unwrap();
        assert!(!output.exists(), "an offer executed without a grant");
        let now = monotonic().unwrap();
        executor
            .grant(fence(), 1, 1, now + Duration::from_millis(250), now)
            .unwrap();
        assert!(
            executor
                .grant(fence(), 1, 1, now + Duration::from_millis(250), now)
                .is_err()
        );
        executor.execute_ready(now).unwrap();
        finish(&mut executor);
        let written: serde_json::Value =
            serde_json::from_slice(&fs::read(&output).unwrap()).unwrap();
        assert_eq!(written["uid"], unsafe { libc::geteuid() });
        assert_ne!(written["uid"], 0);
        assert_eq!(written["payload"], "$(echo shell-expanded);*'literal'");
        assert!(
            executor
                .offer(fence(), event(1, &output), monotonic().unwrap())
                .is_err()
        );
        assert!(executor.outgoing.iter().any(|request| matches!(
            request,
            Request::Complete {
                id: 1,
                ordinal: 1,
                result: Completion::Applied,
                ..
            }
        )));
        fs::remove_file(output).unwrap();
    }

    #[test]
    fn observation_mode_expired_grants_and_generation_changes_never_execute() {
        let output = path();
        assert!(
            executor(Mode::Observe)
                .offer(fence(), event(1, &output), monotonic().unwrap())
                .is_err()
        );
        let mut executor = executor(Mode::Route);
        executor
            .offer(fence(), event(1, &output), monotonic().unwrap())
            .unwrap();
        let now = monotonic().unwrap();
        executor
            .grant(fence(), 1, 1, now + Duration::from_millis(20), now)
            .unwrap();
        executor
            .execute_ready(now + Duration::from_millis(20))
            .unwrap();
        assert!(!output.exists());
        executor
            .offer(fence(), event(2, &output), monotonic().unwrap())
            .unwrap();
        let now = monotonic().unwrap();
        executor
            .grant(fence(), 2, 1, now + Duration::from_millis(250), now)
            .unwrap();
        let next = Fence {
            session: 2,
            ..fence()
        };
        executor.transition(next).unwrap();
        executor.execute_ready(monotonic().unwrap()).unwrap();
        assert!(!output.exists(), "retired granted action executed");
        assert!(
            executor
                .grant(fence(), 2, 1, now + Duration::from_millis(250), now)
                .is_err()
        );
    }

    #[test]
    fn already_dispatched_commands_are_reaped_without_old_generation_completions() {
        let output = path();
        let mut executor = executor(Mode::Route);
        executor
            .offer(fence(), event(1, &output), monotonic().unwrap())
            .unwrap();
        let now = monotonic().unwrap();
        executor
            .grant(fence(), 1, 1, now + Duration::from_millis(250), now)
            .unwrap();
        executor.execute_ready(now).unwrap();
        executor
            .transition(Fence {
                configuration: 2,
                ..fence()
            })
            .unwrap();
        finish(&mut executor);
        assert!(output.exists(), "dispatched command was incorrectly undone");
        assert!(
            !executor
                .outgoing
                .iter()
                .any(|request| matches!(request, Request::Complete { .. }))
        );
        fs::remove_file(output).unwrap();
    }
}
