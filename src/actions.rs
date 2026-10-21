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
mod hyprland;
mod morf;
mod power;
pub use hyprland::HyprlandAdapter;
pub use morf::MorfAdapter;
pub use power::PowerAdapter;

pub const MAX_RUNNING: usize = 8;
const MAX_PENDING: usize = 64;
const MAX_GESTURES: usize = 4096;
const MAX_SCROLLS: usize = 4;
struct Pending {
    event: Box<Event>,
    expires: Option<Duration>,
    resolving: bool,
    resolved: Option<Vec<Action>>,
    power_target: Option<crate::context::OutputState>,
    window_target: Option<hyprland::Target>,
    scroll_target: Option<hyprland::ScrollTarget>,
    morf_target: Option<morf::Target>,
    reorder_scope: Option<protocol::ReorderScope>,
    reorder_granted: bool,
}

struct Running {
    fence: Fence,
    id: u64,
    ordinal: u64,
    child: Child,
    pidfd: Option<OwnedFd>,
}
struct Native {
    until: Duration,
    cancelled: bool,
}
/// One continuous workspace scroll and the state its next attempt is
/// guarded against.
struct Scroll {
    target: hyprland::ScrollTarget,
    view: hyprland::ScrollView,
    motion: crate::hyprland::scrolling::Motion,
}
type ExecutionKey = ([u64; 2], u64, u64, u64, u64);
struct ExecutionGroup {
    remaining: usize,
    result: Completion,
    multiple_effects: bool,
}
fn native_scope(
    adapter: Option<&HyprlandAdapter>,
    event: &Event,
    now: Duration,
) -> Option<protocol::ReorderScope> {
    adapter?.native_scope(&crate::routing::native::event_target(event)?, now)
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
    reported_available: Option<BTreeSet<Capability>>,
    lua_ready: bool,
    groups: BTreeMap<ExecutionKey, ExecutionGroup>,
    emitted: VecDeque<(Fence, u64, u64, String)>,
    power: Option<PowerAdapter>,
    hyprland: Option<HyprlandAdapter>,
    morf: Option<MorfAdapter>,
    native: BTreeMap<(u64, u64), Native>,
    native_seen: BTreeMap<u64, u64>,
    scrolls: BTreeMap<u64, Scroll>,
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
            power: None,
            hyprland: None,
            morf: None,
            native: BTreeMap::new(),
            native_seen: BTreeMap::new(),
            scrolls: BTreeMap::new(),
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
        self.power = None;
        self.hyprland = None;
        self.morf = None;
        self.native.clear();
        self.native_seen.clear();
        self.scrolls.clear();
        self.groups.clear();
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
        self.offer_scoped(fence, event, None, now)
    }
    /// Offer a match with its original native claim proof; absent proofs disable reorder.
    pub fn offer_scoped(
        &mut self,
        fence: Fence,
        event: Box<Event>,
        reorder_scope: Option<protocol::ReorderScope>,
        now: Duration,
    ) -> io::Result<()> {
        Reply::Dispatch {
            fence,
            event: event.clone(),
            reorder_scope: reorder_scope.clone(),
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
            || !protocol::supported_session_action(action)
            || !protocol::action_capabilities(action).is_subset(&self.capabilities)
            || source.seat != self.seat
            || *id == 0
            || *ordinal == 0
            || source.generation == 0
            || *at > now
            || now - *at > protocol::MAX_PACKET_AGE
        {
            return Err(protocol::invalid(
                "action offer lacks current session authority",
            ));
        }
        crate::config::validate_action(action).map_err(|error| protocol::invalid(&error))?;
        let power_target =
            if protocol::action_capabilities(action).contains(&Capability::PowerActions) {
                let adapter = self
                    .power
                    .as_ref()
                    .ok_or_else(|| protocol::invalid("power adapter unavailable"))?;
                let Event::Matched {
                    output: Some(output),
                    ..
                } = event.as_ref()
                else {
                    return Err(protocol::invalid("power action requires a frozen output"));
                };
                Some(adapter.target(output, now)?)
            } else {
                None
            };
        let scroll = matches!(
            action,
            Action::Hyprland {
                action: crate::config::WindowAction::ScrollStack
            }
        );
        let scroll_target = if scroll {
            let Event::Matched {
                window: None,
                output: Some(output),
                workspace: Some(workspace),
                ..
            } = event.as_ref()
            else {
                return Err(protocol::invalid(
                    "scrolling requires a frozen output workspace and no window",
                ));
            };
            Some(
                self.hyprland
                    .as_ref()
                    .ok_or_else(|| protocol::invalid("Hyprland adapter unavailable"))?
                    .scroll_target(output, *workspace, now)?,
            )
        } else {
            None
        };
        let window_target = if !scroll
            && protocol::action_capabilities(action).contains(&Capability::HyprlandActions)
        {
            let adapter = self
                .hyprland
                .as_ref()
                .ok_or_else(|| protocol::invalid("Hyprland adapter unavailable"))?;
            let Event::Matched {
                window: Some(window),
                output: Some(output),
                workspace: Some(workspace),
                ..
            } = event.as_ref()
            else {
                return Err(protocol::invalid(
                    "Hyprland action requires a frozen window, output and workspace",
                ));
            };
            let target = adapter.target(window, output, *workspace, now)?;
            if matches!(action,Action::Hyprland{action:action @ (crate::config::WindowAction::ReorderUp|crate::config::WindowAction::ReorderDown)} if target.reorder(*action).is_none())
            {
                return Err(protocol::invalid("reorder has no frozen eligible neighbor"));
            }
            Some(target)
        } else {
            None
        };
        let morf_target =
            if protocol::action_capabilities(action).contains(&Capability::MorfActions) {
                let Event::Matched {
                    output: Some(output),
                    ..
                } = event.as_ref()
                else {
                    return Err(protocol::invalid("Morf action requires its frozen output"));
                };
                Some(
                    self.morf
                        .as_ref()
                        .ok_or_else(|| protocol::invalid("Morf submission adapter unavailable"))?
                        .target(output, now)?,
                )
            } else {
                None
            };
        if self.seen.get(id).is_some_and(|old| ordinal <= old)
            || (!self.seen.contains_key(id) && self.seen.len() >= MAX_GESTURES)
            || self.pending.len() >= MAX_PENDING
        {
            return Err(protocol::invalid(
                "duplicate action or bounded gesture queue exhausted",
            ));
        }
        let (id, ordinal) = (*id, *ordinal);
        self.seen.insert(id, ordinal);
        let reorder_scope = reorder_scope.filter(|scope| {
            native_scope(self.hyprland.as_ref(), &event, now).as_ref() == Some(scope)
        });
        if matches!(event.as_ref(),Event::Matched{action,..} if crate::routing::native::required(action))
            && reorder_scope.is_none()
        {
            return self.complete(id, ordinal, Completion::Unavailable);
        }
        self.queue(Request::Authorize { fence, id, ordinal })?;
        self.pending.insert(
            (id, ordinal),
            Pending {
                event,
                expires: None,
                resolving: false,
                resolved: None,
                power_target,
                window_target,
                scroll_target,
                morf_target,
                reorder_scope,
                reorder_granted: false,
            },
        );
        Ok(())
    }
    /// Explicit host opt-in. Public broker negotiation remains separate.
    pub fn set_hyprland_adapter(&mut self, adapter: HyprlandAdapter) -> io::Result<()> {
        if !self.capabilities.contains(&Capability::HyprlandActions)
            || self.hyprland.is_some()
            || adapter.fence() != self.fence
            || adapter.seat() != Some(self.seat.as_str())
        {
            return Err(protocol::invalid(
                "Hyprland adapter exceeds executor authority",
            ));
        }
        self.hyprland = Some(adapter);
        self.reported_available = None;
        Ok(())
    }
    pub fn sync_morf_context(&mut self, view: crate::context_transfer::View) -> io::Result<()> {
        if !self.capabilities.contains(&Capability::MorfActions) || view.fence != self.fence {
            return Err(protocol::invalid("Morf context exceeds executor authority"));
        }
        self.reported_available = None;
        if let Some(adapter) = self.morf.as_mut() {
            return adapter.update_context(view);
        }
        let adapter = MorfAdapter::new(view)?;
        if adapter.seat() != Some(self.seat.as_str()) || adapter.fence() != self.fence {
            return Err(protocol::invalid(
                "Morf adapter belongs to another seat/fence",
            ));
        }
        self.morf = Some(adapter);
        Ok(())
    }
    pub fn update_hyprland_context(
        &mut self,
        view: crate::context_transfer::View,
    ) -> io::Result<()> {
        self.reported_available = None;
        let result = self
            .hyprland
            .as_mut()
            .ok_or_else(|| protocol::invalid("Hyprland adapter unavailable"))?
            .update_context(view);
        let now = monotonic()?;
        for pending in self.pending.values_mut() {
            if let Some(scope) = pending.reorder_scope.as_ref() {
                let current = result
                    .is_ok()
                    .then(|| native_scope(self.hyprland.as_ref(), &pending.event, now))
                    .flatten();
                if current.as_ref() != Some(scope) {
                    pending.reorder_scope = None;
                    pending.reorder_granted = false;
                }
            }
        }
        result
    }
    /// Explicit host opt-in. Public broker negotiation remains separate.
    pub fn set_power_adapter(&mut self, adapter: PowerAdapter) -> io::Result<()> {
        if !self.capabilities.contains(&Capability::PowerActions)
            || self.power.is_some()
            || adapter.fence() != self.fence
            || adapter.seat() != Some(self.seat.as_str())
        {
            return Err(protocol::invalid(
                "power adapter exceeds executor authority",
            ));
        }
        self.power = Some(adapter);
        self.reported_available = None;
        Ok(())
    }
    pub fn update_power_context(&mut self, view: crate::context_transfer::View) -> io::Result<()> {
        self.reported_available = None;
        self.power
            .as_mut()
            .ok_or_else(|| protocol::invalid("power adapter unavailable"))?
            .update_context(view)
    }
    /// Install from the exact broker-owned compositor binding, never ambient focus.
    pub fn sync_hyprland_context(
        &mut self,
        runtime: &std::path::Path,
        instance: &str,
        view: crate::context_transfer::View,
    ) -> io::Result<()> {
        if self.hyprland.is_some() {
            return self.update_hyprland_context(view);
        }
        let Some(client) =
            self.compositor_client(runtime, instance, &view, Capability::HyprlandActions)?
        else {
            return Ok(());
        };
        self.set_hyprland_adapter(HyprlandAdapter::new(client, view)?)
    }
    /// Install only from a fresh broker-owned binding; never trust a Lua target
    /// or discover a compositor independently of that binding.
    pub fn sync_power_context(
        &mut self,
        runtime: &std::path::Path,
        instance: &str,
        view: crate::context_transfer::View,
    ) -> io::Result<()> {
        if self.power.is_some() {
            return self.update_power_context(view);
        }
        let Some(client) =
            self.compositor_client(runtime, instance, &view, Capability::PowerActions)?
        else {
            return Ok(());
        };
        self.set_power_adapter(PowerAdapter::new(client, view)?)
    }
    fn compositor_client(
        &self,
        runtime: &std::path::Path,
        instance: &str,
        view: &crate::context_transfer::View,
        capability: Capability,
    ) -> io::Result<Option<crate::hyprland::Client>> {
        view.validate()?;
        if view.fence != self.fence || !self.capabilities.contains(&capability) {
            return Err(protocol::invalid(
                "compositor context exceeds current executor authority",
            ));
        }
        let Some(context) = view.context.as_ref() else {
            return Ok(None);
        };
        let now = monotonic()?;
        if view.state != crate::context::State::Desktop
            || context.snapshot.secure_overlay
            || now < view.sampled_at
            || now - view.sampled_at >= crate::context_transfer::FRESHNESS
            || now < context.snapshot.captured_at
            || now - context.snapshot.captured_at >= crate::context_transfer::FRESHNESS
        {
            return Ok(None);
        }
        // SAFETY: read-only current process credential query.
        if context.binding.seat != self.seat || context.binding.uid != unsafe { libc::geteuid() } {
            return Err(protocol::invalid(
                "compositor context belongs to another owner",
            ));
        }
        let executable =
            std::path::PathBuf::from(format!("/proc/{}/exe", context.binding.compositor_pid));
        let client =
            crate::hyprland::Client::open(runtime, instance, context.binding.uid, &executable)?;
        Ok(Some(client))
    }
    pub fn grant(
        &mut self,
        fence: Fence,
        id: u64,
        ordinal: u64,
        expires: Duration,
        now: Duration,
    ) -> io::Result<()> {
        self.grant_scoped(fence, id, ordinal, expires, None, now)
    }
    /// Grant one offer with the same native proof and a bounded execution deadline.
    pub fn grant_scoped(
        &mut self,
        fence: Fence,
        id: u64,
        ordinal: u64,
        expires: Duration,
        reorder_scope: Option<protocol::ReorderScope>,
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
        let current = native_scope(self.hyprland.as_ref(), &pending.event, now);
        pending.reorder_granted = reorder_scope.is_some()
            && reorder_scope == pending.reorder_scope
            && current == reorder_scope;
        if !pending.reorder_granted {
            pending.reorder_scope = None;
        }
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
        if self.seen.get(&id).is_none_or(|seen| ordinal > *seen) {
            return Err(protocol::invalid("cancellation has no offered action"));
        }
        self.scrolls.remove(&id);
        if self.native_seen.contains_key(&id) {
            let send = self.cancelled.get(&id).is_none_or(|old| *old < ordinal);
            for ((gesture, _), native) in &mut self.native {
                if *gesture == id {
                    native.cancelled = true;
                }
            }
            if send {
                self.queue(Request::MorfCancel { fence, id, ordinal })?;
            }
            self.remember_cancellation(id, ordinal);
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
    /// One granted ordinal of a continuous workspace scroll. Begin freezes a
    /// fresh viewport; each ordinal then makes at most one paced attempt, and
    /// end keeps attempting toward its last destination within the grant.
    fn execute_scroll(
        &mut self,
        id: u64,
        ordinal: u64,
        pending: &Pending,
    ) -> io::Result<Completion> {
        use crate::{
            gestures::{Space, TouchGesture},
            hyprland::scrolling::{Motion, Phase, State},
            rules::Evidence,
        };
        let Event::Matched {
            stage, evidence, ..
        } = pending.event.as_ref()
        else {
            unreachable!("offers were validated");
        };
        let expires = pending.expires.unwrap();
        let total = match evidence {
            Evidence::Touch {
                space: Space::Logical,
                gesture: TouchGesture::Pan { total, .. },
            } => Some(total.y),
            _ => None,
        };
        let (Some(adapter), Some(target), Some(total), true, true) = (
            self.hyprland.as_mut(),
            pending.scroll_target.as_ref(),
            total,
            pending.reorder_granted,
            hyprland::supported(crate::config::WindowAction::ScrollStack),
        ) else {
            self.scrolls.remove(&id);
            return Ok(Completion::Unavailable);
        };
        let now = monotonic()?;
        match stage {
            Stage::Begin if !self.scrolls.contains_key(&id) && self.scrolls.len() < MAX_SCROLLS => {
                let until = std::time::Instant::now()
                    + expires.saturating_sub(now).min(Duration::from_millis(200));
                let Some(view) = adapter.scroll_view(target, until)? else {
                    return Ok(Completion::Unavailable);
                };
                let motion = Motion::begin(id, view.viewport.clone(), total, now)?;
                self.scrolls.insert(
                    id,
                    Scroll {
                        target: target.clone(),
                        view,
                        motion,
                    },
                );
            }
            Stage::Update | Stage::End => {
                let phase = if *stage == Stage::End {
                    Phase::End
                } else {
                    Phase::Update
                };
                if !self
                    .scrolls
                    .get_mut(&id)
                    .filter(|scroll| scroll.target.same(target))
                    .is_some_and(|scroll| {
                        scroll.motion.progress(ordinal, phase, total, now).is_ok()
                    })
                {
                    self.scrolls.remove(&id);
                    return Ok(Completion::Unavailable);
                }
            }
            _ => return Ok(Completion::Unavailable),
        }
        if *stage == Stage::Update
            && self
                .pending
                .range((id, ordinal + 1)..=(id, u64::MAX))
                .any(|(_, newer)| newer.expires.is_some())
        {
            return Ok(Completion::Cancelled);
        }
        let scroll = self.scrolls.get_mut(&id).unwrap();
        let mut result = Completion::Cancelled;
        while monotonic()? < expires {
            let Some(attempt) = scroll.motion.take(monotonic()?)? else {
                if scroll.motion.state() != State::Ending {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
                continue;
            };
            let step = adapter.scroll_step(&scroll.target, &mut scroll.view, attempt, expires);
            let observed = match step {
                hyprland::Step::Settled(offset) => Some(offset),
                _ => None,
            };
            scroll.motion.settle(attempt, observed, monotonic()?)?;
            match step {
                hyprland::Step::Settled(_) => result = Completion::Applied,
                hyprland::Step::Unsent if result != Completion::Applied => {
                    result = Completion::Unavailable
                }
                hyprland::Step::Unsent => {}
                hyprland::Step::Unconfirmed => result = Completion::Uncertain,
            }
            if scroll.motion.state() != State::Ending {
                break;
            }
        }
        if *stage == Stage::End || !matches!(scroll.motion.state(), State::Active | State::Ending) {
            self.scrolls.remove(&id);
        }
        Ok(result)
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
            if matches!(
                action,
                Action::Hyprland {
                    action: crate::config::WindowAction::ScrollStack
                }
            ) {
                let result = self.execute_scroll(id, ordinal, &pending)?;
                self.complete(id, ordinal, result)?;
                continue;
            }
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
            if matches!(
                action,
                Action::Command { .. } | Action::Power { .. } | Action::Hyprland { .. }
            ) && !matches!(stage, Stage::Discrete | Stage::Begin)
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
            let native = actions
                .iter()
                .filter(|action| matches!(action, Action::Morf { .. }))
                .count();
            if self.running.len() + self.native.len() + commands + native > MAX_RUNNING
                || native > 1
                || self.emitted.len() + actions.len() > MAX_PENDING
                || actions.iter().any(|action| {
                    !matches!(
                        action,
                        Action::Command { .. }
                            | Action::Event { .. }
                            | Action::Morf { .. }
                            | Action::Power {
                                action: crate::config::PowerAction::Wake
                                    | crate::config::PowerAction::Blank
                            }
                    ) && !matches!(action, Action::Hyprland { action } if hyprland::supported(*action))
                })
                || actions
                    .iter()
                    .filter(|action| matches!(action, Action::Power { .. } | Action::Hyprland { .. }))
                    .count()
                    > 1
                || actions
                    .iter()
                    .any(|action| matches!(action, Action::Power { .. }))
                    && (self.power.is_none() || pending.power_target.is_none())
                || actions.iter().any(|action| matches!(action, Action::Hyprland { .. }))
                    && (self.hyprland.is_none() || pending.window_target.is_none())
                || actions.iter().any(crate::routing::native::required) && !pending.reorder_granted
                || native > 0 && (self.morf.is_none() || pending.morf_target.is_none())
                || actions.iter().any(|action| matches!(action,Action::Morf {action}
                    if crate::morf::controls::Request::from_event(self.fence,&pending.event,*action).is_err()))
            {
                self.complete(id, ordinal, Completion::Unavailable)?;
                continue;
            }
            if native > 0
                && self
                    .morf
                    .as_ref()
                    .unwrap()
                    .check(pending.morf_target.as_ref().unwrap(), monotonic()?)
                    .is_err()
            {
                self.complete(id, ordinal, Completion::Cancelled)?;
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
                if let Action::Morf { action } = action {
                    if self
                        .morf
                        .as_ref()
                        .unwrap()
                        .check(pending.morf_target.as_ref().unwrap(), monotonic()?)
                        .is_err()
                    {
                        result = if applied {
                            Completion::Uncertain
                        } else {
                            Completion::Cancelled
                        };
                        break;
                    }
                    self.queue(Request::MorfAction {
                        fence: self.fence,
                        id,
                        ordinal,
                        action,
                    })?;
                    self.native.insert(
                        (id, ordinal),
                        Native {
                            until: pending.expires.unwrap() + Duration::from_secs(2),
                            cancelled: false,
                        },
                    );
                    self.native_seen.insert(id, ordinal);
                    count += 1;
                    applied = true;
                    continue;
                }
                if let Action::Power { action } = action {
                    let completion = self.power.as_mut().unwrap().execute(
                        action,
                        pending.power_target.as_ref().unwrap(),
                        pending.expires.unwrap(),
                    );
                    if completion == Completion::Applied {
                        applied = true;
                        emitted_event = true;
                        continue;
                    }
                    result = if applied {
                        Completion::Uncertain
                    } else {
                        completion
                    };
                    break;
                }
                if let Action::Hyprland { action } = action {
                    let completion = self.hyprland.as_mut().unwrap().execute(
                        action,
                        pending.window_target.as_ref().unwrap(),
                        pending.expires.unwrap(),
                    );
                    if completion == Completion::Applied {
                        applied = true;
                        emitted_event = true;
                        continue;
                    }
                    result = if applied {
                        Completion::Uncertain
                    } else {
                        completion
                    };
                    break;
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
                    self.finish_effect(
                        running.id,
                        running.ordinal,
                        if status.success() {
                            Completion::Applied
                        } else {
                            Completion::Unavailable
                        },
                    )?;
                } else {
                    self.groups
                        .remove(&execution_key(running.fence, running.id, running.ordinal));
                }
            }
        }
        let now = monotonic()?;
        let expired = self
            .native
            .iter()
            .filter(|(_, native)| now >= native.until)
            .map(|(key, _)| *key)
            .collect::<Vec<_>>();
        for (id, ordinal) in expired {
            let native = self.native.remove(&(id, ordinal)).unwrap();
            if !native.cancelled {
                self.queue(Request::MorfCancel {
                    fence: self.fence,
                    id,
                    ordinal,
                })?;
            }
            self.finish_effect(id, ordinal, Completion::Uncertain)?;
        }
        Ok(())
    }
    fn finish_effect(&mut self, id: u64, ordinal: u64, result: Completion) -> io::Result<()> {
        let key = execution_key(self.fence, id, ordinal);
        let group = self
            .groups
            .get_mut(&key)
            .ok_or_else(|| protocol::invalid("missing action execution group"))?;
        group.remaining -= 1;
        if result != Completion::Applied && group.result == Completion::Applied {
            group.result = if group.multiple_effects {
                Completion::Uncertain
            } else {
                result
            };
        }
        if group.remaining == 0 {
            let result = group.result;
            self.groups.remove(&key);
            self.complete(id, ordinal, result)?;
        }
        Ok(())
    }
    pub fn complete_morf(
        &mut self,
        fence: Fence,
        id: u64,
        ordinal: u64,
        mut result: Completion,
    ) -> io::Result<()> {
        fence.validate()?;
        if fence != self.fence {
            return if fence.broker == self.fence.broker
                && fence.configuration <= self.fence.configuration
                && fence.session <= self.fence.session
            {
                Ok(())
            } else {
                Err(protocol::invalid("foreign or future Morf result"))
            };
        }
        let Some(native) = self.native.remove(&(id, ordinal)) else {
            return if self
                .native_seen
                .get(&id)
                .is_some_and(|seen| ordinal > 0 && ordinal <= *seen)
            {
                Ok(())
            } else {
                Err(protocol::invalid("Morf result has no submitted action"))
            };
        };
        if native.cancelled {
            result = if matches!(result, Completion::Cancelled | Completion::Unavailable) {
                Completion::Cancelled
            } else {
                Completion::Uncertain
            };
        }
        self.finish_effect(id, ordinal, result)
    }
    /// Current executor capacity; grants and fresh target authorization are
    /// still required for every effect. Shared by broker and Lua availability.
    pub fn ready_capabilities(&self, now: Duration) -> BTreeSet<Capability> {
        let available = self.mode == Mode::Route
            && self.running.len() + self.pending.len() + self.native.len() < MAX_RUNNING;
        if available {
            [
                Capability::Commands,
                Capability::LuaActions,
                Capability::PowerActions,
                Capability::HyprlandActions,
                Capability::MorfActions,
            ]
            .into_iter()
            .filter(|capability| {
                self.capabilities.contains(capability)
                    && (*capability != Capability::LuaActions || self.lua_ready)
                    && (*capability != Capability::PowerActions
                        || self.power.as_ref().is_some_and(|power| power.ready(now)))
                    && (*capability != Capability::HyprlandActions
                        || self
                            .hyprland
                            .as_ref()
                            .is_some_and(|adapter| adapter.ready(now)))
                    && (*capability != Capability::MorfActions
                        || self.morf.as_ref().is_some_and(|adapter| adapter.ready(now)))
            })
            .collect()
        } else {
            BTreeSet::new()
        }
    }
    pub fn flush(&mut self, channel: &mut Channel) -> io::Result<()> {
        let capabilities = self.ready_capabilities(monotonic()?);
        if self.reported_available.as_ref() != Some(&capabilities) {
            self.queue(Request::Availability {
                fence: self.fence,
                capabilities: capabilities.clone(),
            })?;
            self.reported_available = Some(capabilities);
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
    mod morf_transport;
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
