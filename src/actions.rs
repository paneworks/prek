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
    resolving: bool,
    resolved: Option<Vec<Action>>,
}

struct Running {
    fence: Fence,
    id: u64,
    ordinal: u64,
    child: Child,
    pidfd: Option<OwnedFd>,
}
type ExecutionKey = ([u64; 2], u64, u64, u64, u64);
struct ExecutionGroup {
    remaining: usize,
    result: Completion,
    multiple_effects: bool,
}
fn execution_key(fence: Fence, id: u64, ordinal: u64) -> ExecutionKey {
    (
        fence.broker.0,
        fence.configuration,
        fence.session,
        id,
        ordinal,
    )
}
pub struct Executor {
    fence: Fence,
    seat: String,
    mode: Mode,
    capabilities: BTreeSet<Capability>,
    seen: BTreeMap<u64, u64>,
    cancelled: BTreeMap<u64, u64>,
    pending: BTreeMap<(u64, u64), Pending>,
    running: Vec<Running>,
    outgoing: VecDeque<Request>,
    reported_available: Option<bool>,
    lua_ready: bool,
    groups: BTreeMap<ExecutionKey, ExecutionGroup>,
    emitted: VecDeque<(Fence, u64, u64, String)>,
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
            cancelled: BTreeMap::new(),
            pending: BTreeMap::new(),
            running: Vec::new(),
            outgoing: VecDeque::new(),
            reported_available: None,
            lua_ready: false,
            groups: BTreeMap::new(),
            emitted: VecDeque::new(),
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
        self.cancelled.clear();
        self.outgoing.clear();
        self.reported_available = None;
        self.lua_ready = false;
        self.emitted.clear();
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
            || !matches!(action, Action::Command { .. } | Action::Lua { .. })
            || !protocol::action_capabilities(action).is_subset(&self.capabilities)
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
                resolving: false,
                resolved: None,
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
        if id == 0 || ordinal == 0 {
            return Err(protocol::invalid("invalid action grant identity"));
        }
        if fence == self.fence && self.cancelled.get(&id).is_some_and(|last| ordinal <= *last) {
            // Cancellation may cross an execution reply already sent by the
            // broker. It never revives work, even if that reply has expired.
            return Ok(());
        }
        if fence != self.fence || expires > now + crate::context::SNAPSHOT_MAX_AGE {
            return Err(protocol::invalid("stale action execution grant"));
        }
        let pending = self
            .pending
            .get_mut(&(id, ordinal))
            .ok_or_else(|| protocol::invalid("execution grant has no offered action"))?;
        if pending.expires.is_some() {
            return Err(protocol::invalid("duplicate execution grant"));
        }
        if expires <= now {
            self.pending.remove(&(id, ordinal));
            self.remember_cancellation(id, ordinal);
            self.complete(id, ordinal, Completion::Cancelled)?;
            return Ok(());
        }
        pending.expires = Some(expires);
        Ok(())
    }
    /// Call after draining current broker messages, so a queued generation
    /// change invalidates earlier grants before any subprocess can start.
    pub fn execute_ready(&mut self, now: Duration) -> io::Result<()> {
        self.execute_ready_with_lua(now, None)
    }
    pub fn lua_ready(&mut self, ready: bool) {
        if self.lua_ready != ready {
            self.lua_ready = ready;
            self.reported_available = None;
        }
    }
    pub fn resolve_action(
        &mut self,
        id: u64,
        ordinal: u64,
        result: Option<crate::lua::ActionResult>,
        now: Duration,
    ) -> io::Result<Vec<crate::lua::LogRecord>> {
        let pending = self
            .pending
            .get_mut(&(id, ordinal))
            .ok_or_else(|| protocol::invalid("Lua result has no offered action"))?;
        if !pending.resolving {
            return Err(protocol::invalid(
                "unexpected or duplicate Lua action result",
            ));
        }
        let Some(result) = result.filter(|_| pending.expires.is_some_and(|expiry| now < expiry))
        else {
            self.pending.remove(&(id, ordinal));
            self.complete(id, ordinal, Completion::Cancelled)?;
            return Ok(Vec::new());
        };
        if result.actions.len() > 32 {
            return Err(protocol::invalid("Lua plan exceeds effect limit"));
        }
        let Event::Matched {
            action: Action::Lua { requires, .. },
            ..
        } = pending.event.as_ref()
        else {
            return Err(protocol::invalid("Lua result has no named action"));
        };
        for action in &result.actions {
            crate::config::validate_action(action).map_err(|error| protocol::invalid(&error))?;
            if !action
                .effect_kind()
                .is_some_and(|kind| requires.contains(&kind))
                || !protocol::action_capabilities(action).is_subset(&self.capabilities)
            {
                return Err(protocol::invalid(
                    "Lua action result exceeds its execution grant",
                ));
            }
        }
        pending.resolving = false;
        pending.resolved = Some(result.actions);
        Ok(result.logs)
    }
    pub fn take_events(&mut self) -> Vec<(Fence, u64, u64, String)> {
        self.emitted.drain(..).collect()
    }
    pub fn cancel_lua(&mut self) -> io::Result<()> {
        self.lua_ready(false);
        let pending = self
            .pending
            .iter()
            .filter(|(_, pending)| {
                matches!(
                    pending.event.as_ref(),
                    Event::Matched {
                        action: Action::Lua { .. },
                        ..
                    }
                )
            })
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for (id, ordinal) in pending {
            self.pending.remove(&(id, ordinal));
            self.remember_cancellation(id, ordinal);
            self.complete(id, ordinal, Completion::Unavailable)?;
        }
        Ok(())
    }
    pub fn cancel_action(&mut self, fence: Fence, id: u64, ordinal: u64) -> io::Result<()> {
        if fence != self.fence || id == 0 || ordinal == 0 {
            return Err(protocol::invalid("stale action cancellation"));
        }
        let Some(pending) = self.pending.get_mut(&(id, ordinal)) else {
            // Ordered control can repeat a cancellation while an earlier
            // acknowledgement is in flight. Already dispatched children remain
            // owned and reaped; their effects cannot be undone.
            return if self.seen.get(&id).is_some_and(|seen| ordinal <= *seen) {
                Ok(())
            } else {
                Err(protocol::invalid("cancellation has no offered action"))
            };
        };
        if pending.resolving {
            // The helper still owns its response. Keep the identity until that
            // response is discarded, using the existing expiry path.
            pending.expires = Some(Duration::ZERO);
        } else {
            self.pending.remove(&(id, ordinal));
            self.complete(id, ordinal, Completion::Cancelled)?;
        }
        self.remember_cancellation(id, ordinal);
        Ok(())
    }
    fn remember_cancellation(&mut self, id: u64, ordinal: u64) {
        // Only seen identities can be canceled, so this shares the existing
        // per-generation gesture bound rather than growing with repeat count.
        self.cancelled
            .entry(id)
            .and_modify(|last| *last = (*last).max(ordinal))
            .or_insert(ordinal);
    }
    pub fn execute_ready_with_lua(
        &mut self,
        now: Duration,
        mut lua: Option<&mut crate::lua::session::Session>,
    ) -> io::Result<()> {
        let ready: Vec<_> = self
            .pending
            .iter()
            .filter_map(|(identity, pending)| pending.expires.map(|_| *identity))
            .collect();
        for (id, ordinal) in ready {
            let mut pending = self.pending.remove(&(id, ordinal)).unwrap();
            if now.max(monotonic()?) >= pending.expires.unwrap() {
                // The helper still owns one response for this identity. Keep
                // its bounded entry until that response (or VM failure) arrives;
                // resolve_action will discard its effects and acknowledge once.
                if pending.resolving {
                    self.pending.insert((id, ordinal), pending);
                    continue;
                }
                self.complete(id, ordinal, Completion::Cancelled)?;
                continue;
            }
            let Event::Matched { stage, action, .. } = pending.event.as_ref() else {
                unreachable!("offers were validated");
            };
            if matches!(action, Action::Lua { .. }) && pending.resolved.is_none() {
                if pending.resolving {
                    self.pending.insert((id, ordinal), pending);
                    continue;
                }
                if let Some(lua) = lua.as_deref_mut().filter(|lua| lua.action_ready()) {
                    lua.request_action(pending.event.clone(), pending.expires.unwrap())?;
                    pending.resolving = true;
                    self.pending.insert((id, ordinal), pending);
                } else {
                    self.complete(id, ordinal, Completion::Unavailable)?;
                }
                continue;
            }
            if matches!(action, Action::Command { .. })
                && !matches!(stage, Stage::Discrete | Stage::Begin)
            {
                self.complete(id, ordinal, Completion::Cancelled)?;
                continue;
            }
            let actions = pending
                .resolved
                .take()
                .unwrap_or_else(|| vec![action.clone()]);
            let commands = actions
                .iter()
                .filter(|action| matches!(action, Action::Command { .. }))
                .count();
            if self.running.len() + commands > MAX_RUNNING
                || self.emitted.len() + actions.len() > MAX_PENDING
                || actions
                    .iter()
                    .any(|action| !matches!(action, Action::Command { .. } | Action::Event { .. }))
            {
                self.complete(id, ordinal, Completion::Unavailable)?;
                continue;
            }
            let mut count = 0;
            let mut applied = false;
            let mut emitted_event = false;
            let mut result = Completion::Applied;
            for action in actions {
                if monotonic()? >= pending.expires.unwrap() {
                    result = if !applied {
                        Completion::Cancelled
                    } else {
                        Completion::Uncertain
                    };
                    break;
                }
                if let Action::Event { name } = action {
                    self.emitted.push_back((self.fence, id, ordinal, name));
                    applied = true;
                    emitted_event = true;
                    continue;
                }
                let Action::Command { argv } = action else {
                    unreachable!()
                };
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
                        count += 1;
                        applied = true;
                    }
                    Err(_) => {
                        result = if !applied {
                            Completion::Unavailable
                        } else {
                            Completion::Uncertain
                        };
                        break;
                    }
                }
            }
            if count == 0 {
                self.complete(id, ordinal, result)?;
            } else {
                self.groups.insert(
                    execution_key(self.fence, id, ordinal),
                    ExecutionGroup {
                        remaining: count,
                        result,
                        multiple_effects: count > 1 || emitted_event,
                    },
                );
            }
        }
        Ok(())
    }
    pub fn reap(&mut self) -> io::Result<()> {
        for index in (0..self.running.len()).rev() {
            if let Some(status) = self.running[index].child.try_wait()? {
                let running = self.running.remove(index);
                if running.fence == self.fence {
                    let group = self
                        .groups
                        .get_mut(&execution_key(running.fence, running.id, running.ordinal))
                        .ok_or_else(|| protocol::invalid("missing action execution group"))?;
                    group.remaining -= 1;
                    if !status.success() && group.result == Completion::Applied {
                        group.result = if group.multiple_effects {
                            Completion::Uncertain
                        } else {
                            Completion::Unavailable
                        };
                    }
                    if group.remaining != 0 {
                        continue;
                    }
                    let result = group.result;
                    self.groups
                        .remove(&execution_key(running.fence, running.id, running.ordinal));
                    self.complete(running.id, running.ordinal, result)?;
                } else {
                    self.groups
                        .remove(&execution_key(running.fence, running.id, running.ordinal));
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
                    [Capability::Commands, Capability::LuaActions]
                        .into_iter()
                        .filter(|capability| {
                            self.capabilities.contains(capability)
                                && (*capability != Capability::LuaActions || self.lua_ready)
                        })
                        .collect()
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
    fn a_named_multi_command_plan_has_one_completion_and_no_duplicate_execution() {
        let first = path();
        let second = path();
        let command1 = event(1, &first);
        let command2 = event(2, &second);
        let Event::Matched {
            action: effect1, ..
        } = command1.as_ref()
        else {
            unreachable!()
        };
        let Event::Matched {
            action: effect2, ..
        } = command2.as_ref()
        else {
            unreachable!()
        };
        let mut named = command1.clone();
        let Event::Matched { action, .. } = named.as_mut() else {
            unreachable!()
        };
        *action = Action::Lua {
            handler: "two".into(),
            requires: BTreeSet::from([crate::config::EffectKind::Commands]),
        };
        let mut executor = executor(Mode::Route);
        executor.capabilities.insert(Capability::LuaActions);
        executor
            .offer(fence(), named.clone(), monotonic().unwrap())
            .unwrap();
        executor
            .grant(
                fence(),
                1,
                1,
                monotonic().unwrap() + Duration::from_millis(250),
                monotonic().unwrap(),
            )
            .unwrap();
        executor.pending.get_mut(&(1, 1)).unwrap().resolving = true;
        executor
            .resolve_action(
                1,
                1,
                Some(crate::lua::ActionResult {
                    actions: vec![effect1.clone(), effect2.clone()],
                    logs: vec![],
                }),
                monotonic().unwrap(),
            )
            .unwrap();
        executor.execute_ready(monotonic().unwrap()).unwrap();
        finish(&mut executor);
        assert!(first.exists() && second.exists());
        assert_eq!(
            executor
                .outgoing
                .iter()
                .filter(|request| matches!(
                    request,
                    Request::Complete {
                        id: 1,
                        ordinal: 1,
                        result: Completion::Applied,
                        ..
                    }
                ))
                .count(),
            1
        );
        assert!(
            executor
                .offer(fence(), named, monotonic().unwrap())
                .is_err()
        );
        std::fs::remove_file(first).unwrap();
        std::fs::remove_file(second).unwrap();
    }

    #[test]
    fn an_event_followed_by_a_failed_command_reports_partial_execution() {
        use crate::config::EffectKind;
        let mut named = event(1, &path());
        let Event::Matched { action, .. } = named.as_mut() else {
            unreachable!()
        };
        *action = Action::Lua {
            handler: "partial".into(),
            requires: BTreeSet::from([EffectKind::Observations, EffectKind::Commands]),
        };
        let mut executor = executor(Mode::Route);
        executor
            .capabilities
            .extend([Capability::LuaActions, Capability::Observations]);
        executor
            .offer(fence(), named, monotonic().unwrap())
            .unwrap();
        let now = monotonic().unwrap();
        executor
            .grant(fence(), 1, 1, now + Duration::from_millis(250), now)
            .unwrap();
        executor.pending.get_mut(&(1, 1)).unwrap().resolving = true;
        executor
            .resolve_action(
                1,
                1,
                Some(crate::lua::ActionResult {
                    actions: vec![
                        Action::Event {
                            name: "already-emitted".into(),
                        },
                        Action::Command {
                            argv: vec![path().to_str().unwrap().into()],
                        },
                    ],
                    logs: vec![],
                }),
                monotonic().unwrap(),
            )
            .unwrap();
        executor.execute_ready(monotonic().unwrap()).unwrap();
        // QEMU may surface a failed exec as a child exit instead of spawn's
        // error return. Both outcomes must preserve the earlier event effect.
        finish(&mut executor);
        assert_eq!(
            executor.take_events(),
            vec![(fence(), 1, 1, "already-emitted".into())]
        );
        assert!(executor.running.is_empty());
        assert!(executor.outgoing.iter().any(|request| matches!(
            request,
            Request::Complete {
                id: 1,
                ordinal: 1,
                result: Completion::Uncertain,
                ..
            }
        )));
    }

    #[test]
    fn expiry_during_resolution_discards_the_late_plan_and_completes_once() {
        let output = path();
        let mut named = event(1, &output);
        let Event::Matched { action, .. } = named.as_mut() else {
            unreachable!()
        };
        let effect = action.clone();
        *action = Action::Lua {
            handler: "late".into(),
            requires: BTreeSet::from([crate::config::EffectKind::Commands]),
        };
        let mut executor = executor(Mode::Route);
        executor.capabilities.insert(Capability::LuaActions);
        executor
            .offer(fence(), named, monotonic().unwrap())
            .unwrap();
        let now = monotonic().unwrap();
        executor
            .grant(fence(), 1, 1, now + Duration::from_millis(250), now)
            .unwrap();
        executor.pending.get_mut(&(1, 1)).unwrap().resolving = true;
        let expired = now + Duration::from_millis(251);
        executor.execute_ready(expired).unwrap();
        assert!(executor.pending.contains_key(&(1, 1)));
        assert!(
            executor
                .resolve_action(
                    1,
                    1,
                    Some(crate::lua::ActionResult {
                        actions: vec![effect],
                        logs: vec![],
                    }),
                    expired
                )
                .unwrap()
                .is_empty()
        );
        executor.execute_ready(expired).unwrap();
        assert!(!output.exists());
        assert!(executor.pending.is_empty() && executor.running.is_empty());
        assert_eq!(
            executor
                .outgoing
                .iter()
                .filter(|request| matches!(
                    request,
                    Request::Complete {
                        id: 1,
                        ordinal: 1,
                        result: Completion::Cancelled,
                        ..
                    }
                ))
                .count(),
            1
        );
    }

    #[test]
    fn scoped_cancellation_stops_pending_effects_and_preserves_other_actions() {
        let cancelled = path();
        let other = path();
        let mut executor = executor(Mode::Route);
        executor
            .offer(fence(), event(1, &cancelled), monotonic().unwrap())
            .unwrap();
        executor
            .offer(fence(), event(2, &other), monotonic().unwrap())
            .unwrap();
        let now = monotonic().unwrap();
        executor
            .grant(fence(), 2, 1, now + Duration::from_millis(250), now)
            .unwrap();
        executor.cancel_action(fence(), 1, 1).unwrap();
        executor.cancel_action(fence(), 1, 1).unwrap();
        executor.grant(fence(), 1, 1, Duration::ZERO, now).unwrap();
        executor.execute_ready(monotonic().unwrap()).unwrap();
        finish(&mut executor);
        assert!(!cancelled.exists() && other.exists());
        assert_eq!(
            executor
                .outgoing
                .iter()
                .filter(|request| matches!(
                    request,
                    Request::Complete {
                        id: 1,
                        ordinal: 1,
                        result: Completion::Cancelled,
                        ..
                    }
                ))
                .count(),
            1
        );
        fs::remove_file(other).unwrap();
        assert!(executor.cancel_action(fence(), 99, 1).is_err());
        assert!(
            executor
                .cancel_action(
                    Fence {
                        session: 2,
                        ..fence()
                    },
                    1,
                    1
                )
                .is_err()
        );
    }

    #[test]
    fn cancellation_discards_an_inflight_lua_result_without_retiring_other_work() {
        let output = path();
        let mut named = event(1, &output);
        let Event::Matched { action, .. } = named.as_mut() else {
            unreachable!()
        };
        let effect = action.clone();
        *action = Action::Lua {
            handler: "pending".into(),
            requires: BTreeSet::from([crate::config::EffectKind::Commands]),
        };
        let mut executor = executor(Mode::Route);
        executor.capabilities.insert(Capability::LuaActions);
        let now = monotonic().unwrap();
        executor.offer(fence(), named, now).unwrap();
        executor
            .grant(fence(), 1, 1, now + Duration::from_millis(250), now)
            .unwrap();
        executor.pending.get_mut(&(1, 1)).unwrap().resolving = true;
        executor.cancel_action(fence(), 1, 1).unwrap();
        executor.execute_ready(now).unwrap();
        assert!(executor.pending.contains_key(&(1, 1)));
        assert!(
            executor
                .resolve_action(
                    1,
                    1,
                    Some(crate::lua::ActionResult {
                        actions: vec![effect],
                        logs: vec![]
                    }),
                    now
                )
                .unwrap()
                .is_empty()
        );
        executor.execute_ready(now).unwrap();
        assert!(!output.exists() && executor.pending.is_empty());
    }

    #[test]
    fn a_resolved_named_plan_is_discarded_on_generation_change() {
        let output = path();
        let mut named = event(1, &output);
        let Event::Matched { action, .. } = named.as_mut() else {
            unreachable!()
        };
        let effect = action.clone();
        *action = Action::Lua {
            handler: "retired".into(),
            requires: BTreeSet::from([crate::config::EffectKind::Commands]),
        };
        let mut executor = executor(Mode::Route);
        executor.capabilities.insert(Capability::LuaActions);
        executor
            .offer(fence(), named, monotonic().unwrap())
            .unwrap();
        executor
            .grant(
                fence(),
                1,
                1,
                monotonic().unwrap() + Duration::from_millis(250),
                monotonic().unwrap(),
            )
            .unwrap();
        executor.pending.get_mut(&(1, 1)).unwrap().resolving = true;
        executor
            .resolve_action(
                1,
                1,
                Some(crate::lua::ActionResult {
                    actions: vec![effect],
                    logs: vec![],
                }),
                monotonic().unwrap(),
            )
            .unwrap();
        executor
            .transition(Fence {
                configuration: 2,
                ..fence()
            })
            .unwrap();
        executor.execute_ready(monotonic().unwrap()).unwrap();
        assert!(!output.exists());
        assert!(executor.pending.is_empty() && executor.running.is_empty());
        assert!(
            executor
                .resolve_action(1, 1, None, monotonic().unwrap())
                .is_err()
        );
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
