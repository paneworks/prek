//! Geometry and authority fences for a single active compositor session.
//! This is a state model, not peer authentication: the broker registration
//! layer must verify a binding before supplying it here.
use crate::{
    config::{Action, Context, Delivery, MorfAction, Rule, Scope, Screen},
    gestures::{Mapping, Output, Point, Position},
    input::Source,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, time::Duration};

pub const PROVIDER_LEASE: Duration = Duration::from_secs(1);
pub const SNAPSHOT_MAX_AGE: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Rect {
    pub origin: Point,
    pub size: Point,
}
impl Rect {
    pub fn contains(self, point: Point) -> bool {
        point.x >= self.origin.x
            && point.y >= self.origin.y
            && point.x < self.origin.x + self.size.x
            && point.y < self.origin.y + self.size.y
    }
    pub fn contains_rect(self, other: Rect) -> bool {
        other.origin.x >= self.origin.x
            && other.origin.y >= self.origin.y
            && other.origin.x + other.size.x <= self.origin.x + self.size.x
            && other.origin.y + other.size.y <= self.origin.y + self.size.y
    }
    pub fn valid(self) -> bool {
        [self.origin.x, self.origin.y, self.size.x, self.size.y]
            .iter()
            .all(|n| n.is_finite() && n.abs() <= 1_000_000.0)
            && self.size.x > 0.0
            && self.size.y > 0.0
    }
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    User,
    Greeter,
}
/// Trusted authority input. PID/start identity and seat/session ownership must
/// come from peer/process/logind verification, never a client's claimed fields.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub seat: String,
    pub session: String,
    pub uid: u32,
    pub compositor_pid: u32,
    pub compositor_start: u64,
    pub kind: SessionKind,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Console,
    Desktop,
    Locking,
    Locked,
    Greeter,
    Inactive,
    Unknown,
}
impl State {
    fn configured(self) -> Option<Context> {
        match self {
            Self::Console => Some(Context::Console),
            Self::Desktop => Some(Context::Desktop),
            Self::Locking => Some(Context::Locking),
            Self::Locked => Some(Context::Locked),
            Self::Greeter => Some(Context::Greeter),
            Self::Inactive | Self::Unknown => None,
        }
    }
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Keyboard {
    pub rect: Rect,
    pub layout: String,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OutputState {
    pub geometry: Output,
    pub geometry_generation: u64,
    /// Required for workspace/viewport actions; never guess from current focus.
    pub active_workspace: Option<i64>,
    pub screen: Screen,
    /// True only when a bound provider explicitly establishes keyboard
    /// visibility. Missing geometry alone cannot establish a hidden keyboard.
    #[serde(default)]
    pub keyboard_known: bool,
    pub keyboard: Option<Keyboard>,
}
impl OutputState {
    pub fn rect(&self) -> Rect {
        Rect {
            origin: self.geometry.origin,
            size: self.geometry.logical_size(),
        }
    }
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Window {
    pub id: String,
    pub output: String,
    pub workspace: i64,
    pub rect: Rect,
    pub content: Rect,
    pub mapped: bool,
    /// Provider-proven visibility, distinct from a mapped surface on another workspace.
    pub visible: bool,
    pub focused: bool,
    /// None when the compositor cannot establish stacking order. Overlapping
    /// targets then remain ambiguous instead of guessing from client-list order.
    pub z_order: Option<i64>,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Region {
    pub id: String,
    /// None describes a local widget/overlay rather than a global gesture area.
    pub scope: Option<Scope>,
    pub output: String,
    pub rect: Rect,
    pub interactive: bool,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub generation: u64,
    pub revision: u64,
    pub captured_at: Duration,
    pub state: State,
    pub outputs: Vec<OutputState>,
    pub windows: Vec<Window>,
    pub regions: Vec<Region>,
    pub secure_overlay: bool,
}
impl Snapshot {
    /// Pure eligibility check shared by routing preclaim and broker dispatch.
    /// Spatial scopes require provider-proven rendered visibility. Everywhere
    /// rules retain the mapped window selected by focus when their permit began.
    pub(crate) fn hyprland_target_available(
        &self,
        action: &Action,
        scope: Scope,
        window: Option<&str>,
        output: Option<&str>,
        workspace: Option<i64>,
    ) -> bool {
        if !action.requires_hyprland()
            || self.state != State::Desktop
            || self.secure_overlay
            || matches!(action, Action::Hyprland { action } if !action.supported_session())
        {
            return false;
        }
        let Some(window) = self
            .windows
            .iter()
            .find(|candidate| Some(candidate.id.as_str()) == window)
        else {
            return false;
        };
        if !window.mapped
            || scope != Scope::Everywhere && !window.visible
            || window.workspace <= 0
            || workspace != Some(window.workspace)
            || crate::config::hyprland_window_address(&window.id).is_none()
            || output != Some(window.output.as_str())
        {
            return false;
        }
        let Some(output) = self
            .outputs
            .iter()
            .find(|candidate| candidate.geometry.name == window.output)
        else {
            return false;
        };
        if output.screen != Screen::On || output.active_workspace != Some(window.workspace) {
            return false;
        }
        match action {
            Action::Hyprland { action } if *action != crate::config::WindowAction::Close => {
                self.outputs.len() == 1 && action.workspace_destination(window.workspace).is_some()
            }
            _ => true,
        }
    }
    pub fn validate(&self) -> Result<(), Rejection> {
        if self.outputs.len() > 256 || self.windows.len() > 4096 || self.regions.len() > 4096 {
            return Err(Rejection::InvalidGeometry);
        }
        let mut outputs = BTreeSet::new();
        for output in &self.outputs {
            if !outputs.insert(&output.geometry.name)
                || !output.rect().valid()
                || output.geometry_generation == 0
                || !output.geometry.scale.is_finite()
                || output.geometry.scale <= 0.0
                || !output.geometry.pixel_width.is_finite()
                || output.geometry.pixel_width <= 0.0
                || !output.geometry.pixel_height.is_finite()
                || output.geometry.pixel_height <= 0.0
                || output.geometry.name.is_empty()
                || output.geometry.name.len() > 256
                || !output.keyboard_known && output.keyboard.is_some()
                || output.keyboard.as_ref().is_some_and(|keyboard| {
                    !keyboard.rect.valid()
                        || !output.rect().contains_rect(keyboard.rect)
                        || keyboard.layout.len() > 128
                })
            {
                return Err(Rejection::InvalidGeometry);
            }
        }
        let mut ids = BTreeSet::new();
        for window in &self.windows {
            if window.id.is_empty()
                || window.id.len() > 256
                || !ids.insert(&window.id)
                || !outputs.contains(&window.output)
                || !window.rect.valid()
                || !window.content.valid()
                || !window.rect.contains_rect(window.content)
            {
                return Err(Rejection::InvalidGeometry);
            }
        }
        ids.clear();
        for region in &self.regions {
            let output = self
                .outputs
                .iter()
                .find(|output| output.geometry.name == region.output)
                .ok_or(Rejection::InvalidGeometry)?;
            if region.id.is_empty()
                || region.id.len() > 256
                || !ids.insert(&region.id)
                || !region.rect.valid()
                || !output.rect().contains_rect(region.rect)
                || matches!(
                    region.scope,
                    Some(Scope::Everywhere | Scope::AppContent | Scope::Keyboard)
                )
            {
                return Err(Rejection::InvalidGeometry);
            }
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Rejection {
    Unbound,
    Inactive,
    Unknown,
    StaleProvider,
    StaleSnapshot,
    StaleGeneration,
    StaleRevision,
    FutureTime,
    InvalidGeometry,
    ContextDenied,
    Locked,
    SecureOverlay,
    UnmappedDevice,
    OutputChanged,
    TargetMissing,
    TargetChanged,
    OutsideRegion,
    MixedTargets,
    InteractiveReservation,
    KeyboardVisible,
    KeyboardUnavailable,
    ScreenState,
    SourceChanged,
}
impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Rejection {}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LockBarrier {
    Open,
    Locking,
    Locked,
}
/// A frozen first-input target. It cannot be deserialized from a client request.
#[derive(Clone, Debug)]
pub struct Permit {
    source: Source,
    generation: u64,
    state: State,
    output: Option<OutputState>,
    window: Option<Window>,
    region: Option<Region>,
    scope: Scope,
    mapping_generation: Option<u64>,
    secure_overlay: bool,
    starts: Vec<Point>,
}
impl Permit {
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn source(&self) -> &Source {
        &self.source
    }
    pub fn window(&self) -> Option<&Window> {
        self.window.as_ref()
    }
    pub fn output(&self) -> Option<&OutputState> {
        self.output.as_ref()
    }
    pub(crate) fn desktop_output(&self) -> Option<&OutputState> {
        (self.state == State::Desktop && !self.secure_overlay)
            .then_some(self.output.as_ref())
            .flatten()
    }
    pub fn scope(&self) -> Scope {
        self.scope
    }
    pub(crate) fn same_target(&self, later: &Self) -> bool {
        self.generation == later.generation
            && self.state == later.state
            && self.scope == later.scope
            && self.output == later.output
            && self.region == later.region
            && self.secure_overlay == later.secure_overlay
            && match (&self.window, &later.window) {
                (None, None) => true,
                (Some(first), Some(later)) => {
                    first.id == later.id
                        && first.output == later.output
                        && first.workspace == later.workspace
                        && first.rect == later.rect
                        && first.content == later.content
                        && first.mapped == later.mapped
                        && first.visible == later.visible
                }
                _ => false,
            }
    }
}
#[derive(Clone)]
pub struct Authority {
    binding: Option<Binding>,
    generation: u64,
    active: bool,
    console: Option<String>,
    barrier: LockBarrier,
    snapshot: Option<Snapshot>,
    acknowledged_at: Option<Duration>,
    lease: Option<Duration>,
    revision: u64,
}
#[derive(Clone, Copy)]
pub struct InputContext<'a> {
    pub source: &'a Source,
    pub mapping: Option<&'a Mapping>,
    pub at: Duration,
}
impl Default for Authority {
    fn default() -> Self {
        Self {
            binding: None,
            generation: 0,
            active: false,
            console: None,
            barrier: LockBarrier::Open,
            snapshot: None,
            acknowledged_at: None,
            lease: None,
            revision: 0,
        }
    }
}
impl Authority {
    pub fn generation(&self) -> u64 {
        self.generation
    }
    /// Private worker bootstrap only: this installs a broker-verified binding
    /// on a freshly reconciled engine, never over an existing authority.
    pub(crate) fn adopt(&mut self, binding: Binding, generation: u64) -> Result<(), Rejection> {
        if self.binding.is_some() || self.snapshot.is_some() || generation == 0 {
            return Err(Rejection::StaleGeneration);
        }
        self.bind(binding, true, false)?;
        self.generation = generation;
        Ok(())
    }
    /// A trusted worker can fence context-free observations without inventing
    /// desktop, lock or provider authority from session membership.
    pub(crate) fn observation_generation(&mut self, generation: u64) {
        debug_assert!(self.binding.is_none() && generation > 0);
        self.generation = generation;
    }
    fn revoke(&mut self) {
        self.generation = self
            .generation
            .checked_add(1)
            .expect("session generation exhausted");
        self.snapshot = None;
        self.acknowledged_at = None;
        self.lease = None;
        self.revision = 0;
    }
    pub fn bind(
        &mut self,
        verified_binding: Binding,
        active: bool,
        locked_hint: bool,
    ) -> Result<u64, Rejection> {
        if verified_binding.seat.is_empty()
            || verified_binding.session.is_empty()
            || verified_binding.compositor_pid == 0
            || verified_binding.compositor_start == 0
        {
            return Err(Rejection::Unbound);
        }
        let retain_lock = self
            .binding
            .as_ref()
            .is_some_and(|old| old == &verified_binding)
            && self.barrier != LockBarrier::Open;
        self.revoke();
        self.binding = Some(verified_binding);
        self.active = active;
        self.console = None;
        self.barrier = if locked_hint || retain_lock {
            LockBarrier::Locking
        } else {
            LockBarrier::Open
        };
        Ok(self.generation)
    }
    /// Console authority must be established through active-seat/logind evidence.
    pub fn enter_console(&mut self, verified_seat: String) -> Result<(), Rejection> {
        if verified_seat.is_empty() {
            return Err(Rejection::Unbound);
        }
        self.revoke();
        self.binding = None;
        self.console = Some(verified_seat);
        self.active = true;
        self.barrier = LockBarrier::Open;
        Ok(())
    }
    pub fn active_changed(&mut self, active: bool) {
        if self.active != active {
            self.revoke();
            self.active = active;
        }
    }
    /// Lock request/hint inhibits immediately; it is not a lock acknowledgement.
    pub fn lock_requested(&mut self) {
        self.revoke();
        self.barrier = LockBarrier::Locking;
    }
    /// Called only after the bound lock provider's session-lock acknowledgement.
    pub fn lock_confirmed(&mut self, generation: u64) -> Result<(), Rejection> {
        if generation != self.generation {
            return Err(Rejection::StaleGeneration);
        }
        if self.binding.is_none() || self.barrier != LockBarrier::Locking {
            return Err(Rejection::ContextDenied);
        }
        self.barrier = LockBarrier::Locked;
        Ok(())
    }
    /// Only the bound authentication/lock provider can complete this transition.
    /// A fresh Desktop snapshot is still required before actions become eligible.
    pub fn unlock_completed(&mut self, generation: u64) -> Result<(), Rejection> {
        if generation != self.generation {
            return Err(Rejection::StaleGeneration);
        }
        if self.binding.is_none() || self.barrier != LockBarrier::Locked {
            return Err(Rejection::ContextDenied);
        }
        self.revoke();
        self.barrier = LockBarrier::Open;
        Ok(())
    }
    pub fn update(
        &mut self,
        snapshot: Snapshot,
        acknowledged_at: Duration,
    ) -> Result<(), Rejection> {
        self.tick(acknowledged_at);
        let binding = self.binding.as_ref().ok_or(Rejection::Unbound)?;
        if !self.active {
            return Err(Rejection::Inactive);
        }
        if snapshot.generation != self.generation {
            return Err(Rejection::StaleGeneration);
        }
        if snapshot.revision <= self.revision {
            return Err(Rejection::StaleRevision);
        }
        if snapshot.captured_at > acknowledged_at {
            return Err(Rejection::FutureTime);
        }
        if acknowledged_at.saturating_sub(snapshot.captured_at) > SNAPSHOT_MAX_AGE {
            return Err(Rejection::StaleSnapshot);
        }
        if self
            .acknowledged_at
            .is_some_and(|last| acknowledged_at < last)
        {
            return Err(Rejection::FutureTime);
        }
        let allowed = match (binding.kind, self.barrier) {
            (SessionKind::Greeter, LockBarrier::Open) => snapshot.state == State::Greeter,
            (SessionKind::User, LockBarrier::Open) => snapshot.state == State::Desktop,
            (_, LockBarrier::Locked) => snapshot.state == State::Locked,
            (_, LockBarrier::Locking) => false,
        };
        if !allowed {
            return Err(Rejection::Locked);
        }
        snapshot.validate()?;
        self.revision = snapshot.revision;
        self.lease = Some(acknowledged_at.saturating_add(PROVIDER_LEASE));
        self.acknowledged_at = Some(acknowledged_at);
        self.snapshot = Some(snapshot);
        Ok(())
    }
    pub fn heartbeat(&mut self, generation: u64, at: Duration) -> Result<(), Rejection> {
        self.tick(at);
        if generation != self.generation {
            return Err(Rejection::StaleGeneration);
        }
        if self.binding.is_none() || !self.active {
            return Err(Rejection::Inactive);
        }
        if self.acknowledged_at.is_some_and(|ack| at < ack) {
            return Err(Rejection::FutureTime);
        }
        self.lease = Some(at.saturating_add(PROVIDER_LEASE));
        Ok(())
    }
    pub fn tick(&mut self, at: Duration) {
        if self.lease.is_some_and(|deadline| at >= deadline)
            || self.snapshot.as_ref().is_some_and(|snapshot| {
                at >= snapshot.captured_at
                    && at.saturating_sub(snapshot.captured_at) > SNAPSHOT_MAX_AGE
            })
        {
            self.revoke();
        }
    }
    pub fn state(&self, at: Duration) -> State {
        if !self.active {
            State::Inactive
        } else if self.console.is_some() {
            State::Console
        } else if self.barrier == LockBarrier::Locking {
            State::Locking
        } else {
            self.fresh(at)
                .map_or(State::Unknown, |snapshot| snapshot.state)
        }
    }
    pub(crate) fn fresh(&self, at: Duration) -> Result<&Snapshot, Rejection> {
        if !self.active {
            return Err(Rejection::Inactive);
        }
        if self.binding.is_none() {
            return Err(Rejection::Unbound);
        }
        if self.lease.is_none_or(|deadline| at >= deadline) {
            return Err(Rejection::StaleProvider);
        }
        let snapshot = self.snapshot.as_ref().ok_or(Rejection::Unknown)?;
        if snapshot.captured_at > at || self.acknowledged_at.is_some_and(|ack| ack > at) {
            return Err(Rejection::FutureTime);
        }
        if at.saturating_sub(snapshot.captured_at) > SNAPSHOT_MAX_AGE {
            return Err(Rejection::StaleSnapshot);
        }
        Ok(snapshot)
    }
    pub fn start(
        &self,
        rule: &Rule,
        input: InputContext<'_>,
        positions: &[Position],
    ) -> Result<Permit, Rejection> {
        let InputContext {
            source,
            mapping,
            at,
        } = input;
        let state = self.state(at);
        if matches!(state, State::Unknown | State::Inactive) {
            self.fresh(at)?;
            return Err(Rejection::Unknown);
        }
        let context = state.configured().ok_or(Rejection::Unknown)?;
        if !rule.contexts.is_empty() && !rule.contexts.contains(&context) {
            return Err(Rejection::ContextDenied);
        }
        if !rule.seats.is_empty() && !rule.seats.contains(&source.seat) {
            return Err(Rejection::ContextDenied);
        }
        if state == State::Locking {
            return Err(Rejection::Locked);
        }
        if state == State::Console {
            if self.console.as_ref() != Some(&source.seat)
                || !positions.is_empty()
                || rule.scope != Scope::Everywhere
                || rule.output.is_some()
                || !rule.contexts.contains(&Context::Console)
                || matches!(
                    rule.action,
                    Action::Morf { .. } | Action::Hyprland { .. } | Action::Power { .. }
                )
                || rule.screen.is_some()
                || rule.keyboard_visible.is_some()
            {
                return Err(Rejection::ContextDenied);
            }
            return Ok(Permit {
                source: source.clone(),
                generation: self.generation,
                state,
                output: None,
                window: None,
                region: None,
                scope: rule.scope,
                mapping_generation: None,
                secure_overlay: false,
                starts: Vec::new(),
            });
        }
        let snapshot = self.fresh(at)?;
        if self
            .binding
            .as_ref()
            .is_none_or(|binding| binding.seat != source.seat)
        {
            return Err(Rejection::ContextDenied);
        }
        if state != State::Desktop
            && matches!(
                rule.action,
                Action::Hyprland { .. }
                    | Action::Lua { .. }
                    | Action::Command { .. }
                    | Action::Keys { .. }
                    | Action::Remap { .. }
            )
        {
            return Err(Rejection::Locked);
        }
        let keyboard_action = matches!(
            rule.action,
            Action::Morf {
                action: MorfAction::KeyboardShow
                    | MorfAction::KeyboardHide
                    | MorfAction::KeyboardToggleLayout
                    | MorfAction::RevealClockForm
            }
        );
        if state != State::Desktop && matches!(rule.action, Action::Morf { .. }) && !keyboard_action
        {
            return Err(Rejection::Locked);
        }
        if snapshot.secure_overlay
            && !keyboard_action
            && !matches!(rule.action, Action::Power { .. })
        {
            return Err(Rejection::SecureOverlay);
        }
        let output = if let Some(mapping) = mapping {
            let mapped = mapping.output.as_ref().ok_or(Rejection::UnmappedDevice)?;
            let output = snapshot
                .outputs
                .iter()
                .find(|output| output.geometry.name == mapped.name)
                .ok_or(Rejection::OutputChanged)?;
            if output.geometry != *mapped || output.geometry_generation != mapping.generation {
                return Err(Rejection::OutputChanged);
            }
            if rule
                .output
                .as_ref()
                .is_some_and(|name| *name != output.geometry.name)
            {
                return Err(Rejection::OutputChanged);
            }
            Some(output.clone())
        } else if let Some(name) = &rule.output {
            Some(
                snapshot
                    .outputs
                    .iter()
                    .find(|output| output.geometry.name == *name)
                    .ok_or(Rejection::TargetMissing)?
                    .clone(),
            )
        } else {
            None
        };
        let points: Vec<_> = positions
            .iter()
            .map(|position| position.global.ok_or(Rejection::UnmappedDevice))
            .collect::<Result<_, _>>()?;
        if !points.is_empty() && output.is_none() {
            return Err(Rejection::UnmappedDevice);
        }
        if let Some(output) = &output {
            if points.iter().any(|point| !output.rect().contains(*point)) {
                return Err(Rejection::OutsideRegion);
            }
            if rule.screen.is_some_and(|screen| screen != output.screen) {
                return Err(Rejection::ScreenState);
            }
            if !output.keyboard_known
                && (rule.keyboard_visible.is_some()
                    || rule.scope != Scope::Everywhere
                        && !(output.screen == Screen::Off && rule.screen == Some(Screen::Off)))
            {
                return Err(Rejection::KeyboardUnavailable);
            }
            if rule
                .keyboard_visible
                .is_some_and(|visible| visible != output.keyboard.is_some())
            {
                return Err(Rejection::KeyboardVisible);
            }
            if output.screen == Screen::Off && rule.screen != Some(Screen::Off) {
                return Err(Rejection::ScreenState);
            }
        } else if rule.screen.is_some() || rule.keyboard_visible.is_some() {
            return Err(Rejection::UnmappedDevice);
        }
        let mut window = None;
        let mut region = None;
        if rule.scope == Scope::AppContent {
            if state != State::Desktop || points.is_empty() || snapshot.secure_overlay {
                return Err(Rejection::ContextDenied);
            }
            if output
                .as_ref()
                .and_then(|output| output.keyboard.as_ref())
                .is_some_and(|keyboard| points.iter().any(|point| keyboard.rect.contains(*point)))
                || snapshot
                    .regions
                    .iter()
                    .any(|region| points.iter().any(|point| region.rect.contains(*point)))
            {
                return Err(Rejection::OutsideRegion);
            }
            let output_name = &output.as_ref().expect("mapped points").geometry.name;
            let target = hit(snapshot, output_name, points[0])?.ok_or(Rejection::TargetMissing)?;
            for point in &points {
                if hit(snapshot, output_name, *point)?.is_none_or(|window| window.id != target.id) {
                    return Err(Rejection::MixedTargets);
                }
            }
            window = Some(target.clone());
        } else if rule.scope == Scope::Keyboard {
            let keyboard = output
                .as_ref()
                .and_then(|output| output.keyboard.as_ref())
                .ok_or(Rejection::OutsideRegion)?;
            if points.is_empty() || points.iter().any(|point| !keyboard.rect.contains(*point)) {
                return Err(Rejection::OutsideRegion);
            }
        } else if rule.scope != Scope::Everywhere {
            if points.is_empty() {
                return Err(Rejection::OutsideRegion);
            }
            let output = output.as_ref().ok_or(Rejection::UnmappedDevice)?;
            if let Some(keyboard) = &output.keyboard
                && points.iter().any(|point| keyboard.rect.contains(*point))
            {
                return Err(Rejection::OutsideRegion);
            }
            let target = snapshot
                .regions
                .iter()
                .find(|region| {
                    region.scope == Some(rule.scope)
                        && region.output == output.geometry.name
                        && points.iter().all(|point| region.rect.contains(*point))
                })
                .ok_or(Rejection::OutsideRegion)?;
            if rule.delivery == Delivery::Reserve
                && (target.interactive
                    || snapshot.regions.iter().any(|other| {
                        other.interactive
                            && other.id != target.id
                            && points.iter().any(|point| other.rect.contains(*point))
                    }))
            {
                return Err(Rejection::InteractiveReservation);
            }
            if rule.scope == Scope::ClockBackground
                && snapshot.regions.iter().any(|other| {
                    other.interactive && points.iter().any(|point| other.rect.contains(*point))
                })
            {
                return Err(Rejection::OutsideRegion);
            }
            region = Some(target.clone());
        }
        if rule.action.requires_hyprland()
            && window.is_none()
            && !matches!(rule.scope, Scope::SideLeft | Scope::SideRight)
        {
            let mut focused = snapshot.windows.iter().filter(|window| {
                window.mapped
                    && (window.visible || rule.scope == Scope::Everywhere)
                    && window.focused
                    && output
                        .as_ref()
                        .is_none_or(|output| window.output == output.geometry.name)
            });
            window = Some(focused.next().ok_or(Rejection::TargetMissing)?.clone());
            if focused.next().is_some() {
                return Err(Rejection::MixedTargets);
            }
        }
        if matches!(
            rule.action,
            Action::Hyprland {
                action: crate::config::WindowAction::ScrollStack
            } | Action::Morf {
                action: MorfAction::WorkspacePreview
            }
        ) && output
            .as_ref()
            .is_none_or(|output| output.active_workspace.is_none())
        {
            return Err(Rejection::TargetMissing);
        }
        Ok(Permit {
            source: source.clone(),
            generation: self.generation,
            state,
            output,
            window,
            region,
            scope: rule.scope,
            mapping_generation: mapping.map(|mapping| mapping.generation),
            secure_overlay: snapshot.secure_overlay,
            starts: points,
        })
    }
    /// Check again immediately before dispatch, and whenever another finger joins.
    /// Focus changes do not retarget a permit; geometry/workspace changes revoke it.
    pub fn validate(&self, permit: &Permit, input: InputContext<'_>) -> Result<(), Rejection> {
        let InputContext {
            source,
            mapping,
            at,
        } = input;
        if permit.source != *source {
            return Err(Rejection::SourceChanged);
        }
        if permit.generation != self.generation {
            return Err(Rejection::StaleGeneration);
        }
        if self.state(at) != permit.state {
            return Err(Rejection::ContextDenied);
        }
        if permit.state == State::Console {
            return if self.console.as_ref() == Some(&source.seat) && self.active {
                Ok(())
            } else {
                Err(Rejection::Inactive)
            };
        }
        let snapshot = self.fresh(at)?;
        if snapshot.secure_overlay != permit.secure_overlay {
            return Err(Rejection::SecureOverlay);
        }
        if permit.mapping_generation != mapping.map(|mapping| mapping.generation) {
            return Err(Rejection::OutputChanged);
        }
        if let Some(output) = &permit.output {
            let current = snapshot
                .outputs
                .iter()
                .find(|current| current.geometry.name == output.geometry.name)
                .ok_or(Rejection::OutputChanged)?;
            if current != output
                || mapping.is_some_and(|mapping| mapping.output.as_ref() != Some(&current.geometry))
            {
                return Err(Rejection::OutputChanged);
            }
        }
        if let Some(window) = &permit.window {
            let current = snapshot
                .windows
                .iter()
                .find(|current| {
                    current.id == window.id
                        && current.mapped
                        && (current.visible || permit.scope == Scope::Everywhere)
                })
                .ok_or(Rejection::TargetMissing)?;
            if current.output != window.output
                || current.workspace != window.workspace
                || current.rect != window.rect
                || current.content != window.content
            {
                return Err(Rejection::TargetChanged);
            }
            if permit.scope == Scope::AppContent {
                if snapshot.regions.iter().any(|region| {
                    permit
                        .starts
                        .iter()
                        .any(|point| region.rect.contains(*point))
                }) {
                    return Err(Rejection::TargetChanged);
                }
                for point in &permit.starts {
                    if hit(snapshot, &window.output, *point)?.is_none_or(|hit| hit.id != window.id)
                    {
                        return Err(Rejection::TargetChanged);
                    }
                }
            }
        }
        if let Some(region) = &permit.region
            && (!snapshot.regions.contains(region)
                || !region.interactive
                    && snapshot.regions.iter().any(|other| {
                        other.interactive
                            && other.id != region.id
                            && permit
                                .starts
                                .iter()
                                .any(|point| other.rect.contains(*point))
                    }))
        {
            return Err(Rejection::TargetChanged);
        }
        Ok(())
    }
    pub fn join(
        &self,
        permit: &mut Permit,
        rule: &Rule,
        input: InputContext<'_>,
        position: Position,
    ) -> Result<(), Rejection> {
        self.validate(permit, input)?;
        let joined = self.start(rule, input, &[position])?;
        if joined.window.as_ref().map(|window| &window.id)
            != permit.window.as_ref().map(|window| &window.id)
            || joined.region != permit.region
            || joined.scope != permit.scope
        {
            return Err(Rejection::MixedTargets);
        }
        if permit.starts.len() >= 64 {
            return Err(Rejection::InvalidGeometry);
        }
        permit
            .starts
            .push(position.global.ok_or(Rejection::UnmappedDevice)?);
        Ok(())
    }
}

fn hit<'a>(
    snapshot: &'a Snapshot,
    output: &str,
    point: Point,
) -> Result<Option<&'a Window>, Rejection> {
    let windows: Vec<_> = snapshot
        .windows
        .iter()
        .filter(|window| {
            window.mapped
                && window.visible
                && window.output == output
                && window.content.contains(point)
        })
        .collect();
    if windows.len() <= 1 {
        return Ok(windows.first().copied());
    }
    if windows.iter().any(|window| window.z_order.is_none()) {
        return Err(Rejection::MixedTargets);
    }
    let top = windows
        .iter()
        .max_by_key(|window| window.z_order)
        .expect("overlap");
    if windows
        .iter()
        .filter(|window| window.z_order == top.z_order)
        .count()
        != 1
    {
        return Err(Rejection::MixedTargets);
    }
    Ok(Some(top))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{Config, PowerAction},
        gestures::Transform,
        input::Axis,
    };
    #[test]
    fn unknown_keyboard_is_neither_hidden_nor_eligible_spatial_context() {
        let mut authority = authority();
        let source = source();
        let map = mapping();
        let mut unknown = snapshot(authority.generation(), 2, 0);
        unknown.outputs[0].keyboard = None;
        unknown.outputs[0].keyboard_known = false;
        authority.update(unknown.clone(), ms(0)).unwrap();
        for name in ["G01", "G05", "G06", "G08_up"] {
            error(
                authority.start(&rule(name), input(&source, &map, 0), &[point(200, 200)]),
                Rejection::KeyboardUnavailable,
            );
        }
        let mut selected = rule("G08_up");
        selected.action = Action::Event {
            name: "keyboard-context".into(),
        };
        selected.scope = Scope::Everywhere;
        selected.keyboard_visible = Some(false);
        error(
            authority.start(&selected, input(&source, &map, 0), &[]),
            Rejection::KeyboardUnavailable,
        );
        selected.keyboard_visible = Some(true);
        error(
            authority.start(&selected, input(&source, &map, 0), &[]),
            Rejection::KeyboardUnavailable,
        );
        // Explicit global rules can remain independent of keyboard knowledge.
        selected.keyboard_visible = None;
        authority
            .start(&selected, input(&source, &map, 0), &[])
            .unwrap();
        let mut hidden = unknown;
        hidden.revision = 3;
        hidden.outputs[0].keyboard_known = true;
        authority.update(hidden.clone(), ms(0)).unwrap();
        selected.keyboard_visible = Some(false);
        let permit = authority
            .start(&selected, input(&source, &map, 0), &[])
            .unwrap();
        hidden.revision = 4;
        hidden.outputs[0].keyboard_known = false;
        authority.update(hidden, ms(0)).unwrap();
        assert_eq!(
            authority.validate(&permit, input(&source, &map, 0)),
            Err(Rejection::OutputChanged)
        );
    }
    #[test]
    fn geometry_cannot_appear_under_unknown_keyboard_knowledge() {
        let mut value = snapshot(1, 1, 0);
        value.outputs[0].keyboard_known = false;
        assert_eq!(value.validate(), Err(Rejection::InvalidGeometry));
        value.outputs[0].keyboard = None;
        value.validate().unwrap();
        let mut encoded = serde_json::to_value(&value).unwrap();
        encoded["outputs"][0]
            .as_object_mut()
            .unwrap()
            .remove("keyboard_known");
        let legacy: Snapshot = serde_json::from_value(encoded).unwrap();
        assert!(!legacy.outputs[0].keyboard_known);
    }
    #[test]
    fn explicit_output_freezes_keyboard_targets_without_inventing_a_mapping() {
        let mut authority = authority();
        let mut candidate = Config::preset("desktop").unwrap().rules.remove(0);
        candidate.output = Some("phone".into());
        candidate.screen = Some(Screen::On);
        let source = source();
        let input = InputContext {
            source: &source,
            mapping: None,
            at: ms(0),
        };
        let permit = authority.start(&candidate, input, &[]).unwrap();
        assert_eq!(permit.output().unwrap().geometry.name, "phone");
        authority.validate(&permit, input).unwrap();
        let mut changed = snapshot(authority.generation(), 2, 10);
        changed.outputs[0].geometry_generation += 1;
        authority.update(changed, ms(10)).unwrap();
        error(
            authority.validate(
                &permit,
                InputContext {
                    at: ms(10),
                    ..input
                },
            ),
            Rejection::OutputChanged,
        );
        candidate.output = Some("missing".into());
        error(
            authority.start(
                &candidate,
                InputContext {
                    at: ms(10),
                    ..input
                },
                &[],
            ),
            Rejection::TargetMissing,
        );
        candidate.output = Some("other".into());
        let mapping = mapping();
        error(
            authority.start(
                &candidate,
                InputContext {
                    source: &source,
                    mapping: Some(&mapping),
                    at: ms(10),
                },
                &[],
            ),
            Rejection::OutputChanged,
        );
    }
    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }
    fn rect(x: f64, y: f64, width: f64, height: f64) -> Rect {
        Rect {
            origin: Point { x, y },
            size: Point {
                x: width,
                y: height,
            },
        }
    }
    fn source() -> Source {
        Source {
            device: 7,
            generation: 1,
            seat: "seat0".into(),
        }
    }
    fn binding() -> Binding {
        Binding {
            seat: "seat0".into(),
            session: "session1".into(),
            uid: 1000,
            compositor_pid: 100,
            compositor_start: 1000,
            kind: SessionKind::User,
        }
    }
    fn mapping() -> Mapping {
        let axis = |maximum| Axis {
            minimum: 0,
            maximum,
            resolution: 10,
            fuzz: 0,
            flat: 0,
        };
        Mapping {
            generation: 1,
            x: axis(1000),
            y: axis(2000),
            calibration: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            output: Some(Output {
                name: "phone".into(),
                pixel_width: 1000.0,
                pixel_height: 2000.0,
                scale: 2.0,
                origin: Point::default(),
                transform: Transform::Normal,
            }),
        }
    }
    fn snapshot(generation: u64, revision: u64, at: u64) -> Snapshot {
        Snapshot {
            generation,
            revision,
            captured_at: ms(at),
            state: State::Desktop,
            secure_overlay: false,
            outputs: vec![OutputState {
                geometry: mapping().output.unwrap(),
                geometry_generation: 1,
                active_workspace: Some(1),
                screen: Screen::On,
                keyboard_known: true,
                keyboard: Some(Keyboard {
                    rect: rect(0.0, 800.0, 500.0, 200.0),
                    layout: "normal".into(),
                }),
            }],
            windows: vec![
                Window {
                    id: "a".into(),
                    output: "phone".into(),
                    workspace: 1,
                    rect: rect(0.0, 30.0, 250.0, 700.0),
                    content: rect(8.0, 38.0, 234.0, 684.0),
                    mapped: true,
                    visible: true,
                    focused: true,
                    z_order: Some(1),
                },
                Window {
                    id: "b".into(),
                    output: "phone".into(),
                    workspace: 1,
                    rect: rect(250.0, 30.0, 250.0, 700.0),
                    content: rect(258.0, 38.0, 234.0, 684.0),
                    mapped: true,
                    visible: true,
                    focused: false,
                    z_order: Some(2),
                },
            ],
            regions: vec![
                Region {
                    id: "top_left".into(),
                    scope: Some(Scope::TopLeft),
                    output: "phone".into(),
                    rect: rect(0.0, 0.0, 250.0, 30.0),
                    interactive: true,
                },
                Region {
                    id: "top_right".into(),
                    scope: Some(Scope::TopRight),
                    output: "phone".into(),
                    rect: rect(250.0, 0.0, 250.0, 30.0),
                    interactive: true,
                },
                Region {
                    id: "bottom".into(),
                    scope: Some(Scope::ContentBottom),
                    output: "phone".into(),
                    rect: rect(0.0, 770.0, 500.0, 30.0),
                    interactive: true,
                },
                Region {
                    id: "side_left".into(),
                    scope: Some(Scope::SideLeft),
                    output: "phone".into(),
                    rect: rect(0.0, 30.0, 8.0, 740.0),
                    interactive: false,
                },
            ],
        }
    }
    fn authority() -> Authority {
        let mut authority = Authority::default();
        let generation = authority.bind(binding(), true, false).unwrap();
        authority.update(snapshot(generation, 1, 0), ms(0)).unwrap();
        authority
    }
    fn rule(id: &str) -> Rule {
        Config::preset("phone")
            .unwrap()
            .rules
            .into_iter()
            .find(|rule| rule.id == id)
            .unwrap()
    }
    fn point(x: i32, y: i32) -> Position {
        mapping().map(x, y).unwrap()
    }
    fn input<'a>(source: &'a Source, mapping: &'a Mapping, at: u64) -> InputContext<'a> {
        InputContext {
            source,
            mapping: Some(mapping),
            at: ms(at),
        }
    }
    fn error<T: std::fmt::Debug>(result: Result<T, Rejection>, expected: Rejection) {
        assert_eq!(result.unwrap_err(), expected);
    }

    #[test]
    fn t18_half_open_rectangles_and_scaled_mapping_agree() {
        let r = rect(-500.0, 0.0, 500.0, 1000.0);
        assert!(r.contains(Point { x: -500.0, y: 0.0 }));
        assert!(!r.contains(Point { x: 0.0, y: 100.0 }));
        let authority = authority();
        let source = source();
        let map = mapping();
        assert!(
            authority
                .start(
                    &rule("G08_up"),
                    input(&source, &map, 0),
                    &[point(200, 200), point(400, 200)]
                )
                .is_ok()
        );
        error(
            authority.start(
                &rule("G08_up"),
                input(&source, &map, 0),
                &[point(200, 200), point(800, 200)],
            ),
            Rejection::MixedTargets,
        );
    }
    #[test]
    fn t62_keyboard_rectangle_excludes_apps_and_bottom_is_above_keyboard() {
        let authority = authority();
        let source = source();
        let map = mapping();
        assert!(
            authority
                .start(
                    &rule("G06"),
                    input(&source, &map, 0),
                    &[point(200, 1800), point(400, 1800)]
                )
                .is_ok()
        );
        error(
            authority.start(
                &rule("G08_up"),
                input(&source, &map, 0),
                &[point(200, 1800)],
            ),
            Rejection::OutsideRegion,
        );
        assert!(
            authority
                .start(&rule("G01"), input(&source, &map, 0), &[point(200, 1580)])
                .is_ok()
        );
        error(
            authority.start(&rule("G01"), input(&source, &map, 0), &[point(200, 1800)]),
            Rejection::OutsideRegion,
        );
    }
    #[test]
    fn t58_focus_changes_do_not_retarget_a_frozen_window() {
        let mut authority = authority();
        let source = source();
        let map = mapping();
        let permit = authority
            .start(&rule("G08_up"), input(&source, &map, 0), &[point(200, 200)])
            .unwrap();
        let mut next = snapshot(authority.generation(), 2, 100);
        next.windows[0].focused = false;
        next.windows[1].focused = true;
        authority.update(next, ms(100)).unwrap();
        authority
            .validate(&permit, input(&source, &map, 100))
            .unwrap();
        assert_eq!(permit.window().unwrap().id, "a");
    }
    #[test]
    fn global_named_window_rules_freeze_stock_focus_without_inventing_spatial_visibility() {
        let mut authority = authority();
        let source = source();
        let mut next = snapshot(authority.generation(), 2, 0);
        next.windows[0].id = "0x1234:abc".into();
        next.windows[1].id = "0x5678:def".into();
        for window in &mut next.windows {
            window.visible = false;
        }
        authority.update(next.clone(), ms(0)).unwrap();
        let mut rule = rule("G08_up");
        rule.scope = Scope::Everywhere;
        rule.output = Some("phone".into());
        rule.trigger = crate::config::Trigger::Key {
            key: crate::config::Key(30),
        };
        rule.action = Action::Lua {
            handler: "window".into(),
            requires: BTreeSet::from([crate::config::EffectKind::HyprlandActions]),
        };
        let permit = authority
            .start(
                &rule,
                InputContext {
                    source: &source,
                    mapping: None,
                    at: ms(0),
                },
                &[],
            )
            .unwrap();
        assert_eq!(permit.window().unwrap().id, "0x1234:abc");
        next.revision = 3;
        next.captured_at = ms(100);
        next.windows[0].focused = false;
        next.windows[1].focused = true;
        authority.update(next.clone(), ms(100)).unwrap();
        authority
            .validate(
                &permit,
                InputContext {
                    source: &source,
                    mapping: None,
                    at: ms(100),
                },
            )
            .unwrap();
        assert!(next.hyprland_target_available(
            &rule.action,
            Scope::Everywhere,
            Some("0x1234:abc"),
            Some("phone"),
            Some(1)
        ));
        assert!(!next.hyprland_target_available(
            &rule.action,
            Scope::AppContent,
            Some("0x1234:abc"),
            Some("phone"),
            Some(1)
        ));
    }
    #[test]
    fn t58_window_geometry_workspace_and_removal_cancel() {
        for change in 0..3 {
            let mut authority = authority();
            let source = source();
            let map = mapping();
            let permit = authority
                .start(&rule("G08_up"), input(&source, &map, 0), &[point(200, 200)])
                .unwrap();
            let mut next = snapshot(authority.generation(), 2, 100);
            match change {
                0 => {
                    next.windows[0].rect.origin.x += 1.0;
                    next.windows[0].content.origin.x += 1.0;
                }
                1 => next.windows[0].workspace = 2,
                _ => next.windows[0].mapped = false,
            }
            authority.update(next, ms(100)).unwrap();
            assert!(
                authority
                    .validate(&permit, input(&source, &map, 100))
                    .is_err()
            );
        }
    }
    #[test]
    fn t58_unknown_stacking_order_does_not_guess_overlap_target() {
        let mut authority = authority();
        let source = source();
        let map = mapping();
        let mut next = snapshot(authority.generation(), 2, 100);
        next.windows[1].rect = next.windows[0].rect;
        next.windows[1].content = next.windows[0].content;
        next.windows[1].z_order = None;
        authority.update(next, ms(100)).unwrap();
        error(
            authority.start(
                &rule("G08_up"),
                input(&source, &map, 100),
                &[point(200, 200)],
            ),
            Rejection::MixedTargets,
        );
    }
    #[test]
    fn t19_output_geometry_change_cancels_even_if_window_stays() {
        let mut authority = authority();
        let source = source();
        let map = mapping();
        let permit = authority
            .start(&rule("G08_up"), input(&source, &map, 0), &[point(200, 200)])
            .unwrap();
        let mut next = snapshot(authority.generation(), 2, 100);
        next.outputs[0].geometry_generation = 2;
        authority.update(next, ms(100)).unwrap();
        error(
            authority.validate(&permit, input(&source, &map, 100)),
            Rejection::OutputChanged,
        );
    }
    #[test]
    fn t28_additional_finger_must_start_in_the_original_target() {
        let authority = authority();
        let source = source();
        let map = mapping();
        let rule = rule("G08_up");
        let mut permit = authority
            .start(&rule, input(&source, &map, 0), &[point(200, 200)])
            .unwrap();
        authority
            .join(
                &mut permit,
                &rule,
                input(&source, &map, 50),
                point(400, 200),
            )
            .unwrap();
        error(
            authority.join(
                &mut permit,
                &rule,
                input(&source, &map, 100),
                point(800, 200),
            ),
            Rejection::MixedTargets,
        );
    }
    #[test]
    fn t58_new_overlay_over_a_joined_finger_revokes_the_permit() {
        let mut authority = authority();
        let source = source();
        let map = mapping();
        let rule = rule("G08_up");
        let mut permit = authority
            .start(&rule, input(&source, &map, 0), &[point(200, 200)])
            .unwrap();
        authority
            .join(
                &mut permit,
                &rule,
                input(&source, &map, 50),
                point(400, 200),
            )
            .unwrap();
        let mut next = snapshot(authority.generation(), 2, 100);
        next.regions.push(Region {
            id: "overlay".into(),
            scope: None,
            output: "phone".into(),
            rect: rect(180.0, 80.0, 50.0, 50.0),
            interactive: true,
        });
        authority.update(next, ms(100)).unwrap();
        error(
            authority.validate(&permit, input(&source, &map, 100)),
            Rejection::TargetChanged,
        );
    }
    #[test]
    fn t61_snapshot_age_and_provider_lease_do_not_revive_queued_actions() {
        let mut authority = authority();
        let source = source();
        let map = mapping();
        let permit = authority
            .start(&rule("G08_up"), input(&source, &map, 0), &[point(200, 200)])
            .unwrap();
        authority
            .validate(&permit, input(&source, &map, 250))
            .unwrap();
        assert!(
            authority
                .validate(&permit, input(&source, &map, 251))
                .is_err()
        );
        let old = authority.generation();
        authority.tick(ms(251));
        assert_ne!(authority.generation(), old);
        let fresh = snapshot(authority.generation(), 1, 300);
        authority.update(fresh, ms(300)).unwrap();
        error(
            authority.validate(&permit, input(&source, &map, 300)),
            Rejection::StaleGeneration,
        );
        let generation = authority.generation();
        authority.heartbeat(generation, ms(500)).unwrap();
        assert_eq!(authority.state(ms(551)), State::Unknown);
        authority.tick(ms(1500));
        assert_ne!(authority.generation(), generation);
    }
    #[test]
    fn t61_late_refresh_cannot_bypass_expiration_without_tick() {
        let mut authority = authority();
        let old = authority.generation();
        error(
            authority.update(snapshot(old, 2, 300), ms(300)),
            Rejection::StaleGeneration,
        );
        assert_ne!(authority.generation(), old);
    }
    #[test]
    fn t60_lock_hint_inhibits_and_only_completed_unlock_restores_desktop() {
        let mut authority = authority();
        let source = source();
        let map = mapping();
        let old = authority.generation();
        authority.lock_requested();
        assert_eq!(authority.state(ms(0)), State::Locking);
        error(authority.lock_confirmed(old), Rejection::StaleGeneration);
        let generation = authority.generation();
        error(
            authority.update(snapshot(generation, 1, 0), ms(0)),
            Rejection::Locked,
        );
        authority.lock_confirmed(generation).unwrap();
        let mut locked = snapshot(generation, 1, 0);
        locked.state = State::Locked;
        authority.update(locked, ms(0)).unwrap();
        error(
            authority.start(&rule("G08_up"), input(&source, &map, 0), &[point(200, 200)]),
            Rejection::ContextDenied,
        );
        error(
            authority.update(snapshot(generation, 2, 100), ms(100)),
            Rejection::Locked,
        );
        authority.unlock_completed(generation).unwrap();
        assert_eq!(authority.state(ms(100)), State::Unknown);
        authority
            .update(snapshot(authority.generation(), 1, 100), ms(100))
            .unwrap();
        assert_eq!(authority.state(ms(100)), State::Desktop);
    }
    #[test]
    fn t60_reconnecting_same_locked_session_does_not_imply_unlock() {
        let mut authority = authority();
        authority.lock_requested();
        let generation = authority.bind(binding(), true, false).unwrap();
        assert_eq!(authority.state(ms(0)), State::Locking);
        error(
            authority.update(snapshot(generation, 1, 0), ms(0)),
            Rejection::Locked,
        );
    }
    #[test]
    fn t59_seat_session_and_source_generations_are_fenced() {
        let mut authority = authority();
        let source = source();
        let map = mapping();
        let permit = authority
            .start(&rule("G08_up"), input(&source, &map, 0), &[point(200, 200)])
            .unwrap();
        let mut other = source.clone();
        other.seat = "seat1".into();
        error(
            authority.start(&rule("G08_up"), input(&other, &map, 0), &[point(200, 200)]),
            Rejection::ContextDenied,
        );
        other = source.clone();
        other.generation += 1;
        error(
            authority.validate(&permit, input(&other, &map, 0)),
            Rejection::SourceChanged,
        );
        authority.active_changed(false);
        error(
            authority.validate(&permit, input(&source, &map, 0)),
            Rejection::StaleGeneration,
        );
    }
    #[test]
    fn t60_greeter_cannot_enable_application_actions() {
        let mut authority = Authority::default();
        let mut binding = binding();
        binding.kind = SessionKind::Greeter;
        let generation = authority.bind(binding, true, false).unwrap();
        let mut snap = snapshot(generation, 1, 0);
        snap.state = State::Greeter;
        authority.update(snap, ms(0)).unwrap();
        let mut rule = rule("G08_up");
        rule.contexts = vec![Context::Greeter];
        let source = source();
        let map = mapping();
        error(
            authority.start(&rule, input(&source, &map, 0), &[point(200, 200)]),
            Rejection::Locked,
        );
    }
    #[test]
    fn t51_empty_strip_reservation_rejects_interactive_controls() {
        let mut authority = authority();
        let source = source();
        let map = mapping();
        let mut rule = rule("G11_left");
        rule.delivery = Delivery::Reserve;
        assert!(
            authority
                .start(&rule, input(&source, &map, 0), &[point(4, 200)])
                .is_ok()
        );
        let mut next = snapshot(authority.generation(), 2, 100);
        next.regions.push(Region {
            id: "button".into(),
            scope: None,
            output: "phone".into(),
            rect: rect(0.0, 80.0, 8.0, 40.0),
            interactive: true,
        });
        authority.update(next, ms(100)).unwrap();
        error(
            authority.start(&rule, input(&source, &map, 100), &[point(4, 200)]),
            Rejection::InteractiveReservation,
        );
    }
    #[test]
    fn t68_sleeping_display_requires_explicit_screen_off_rule() {
        let mut authority = authority();
        authority.lock_requested();
        let generation = authority.generation();
        authority.lock_confirmed(generation).unwrap();
        let mut snap = snapshot(generation, 1, 0);
        snap.state = State::Locked;
        snap.outputs[0].screen = Screen::Off;
        authority.update(snap, ms(0)).unwrap();
        let source = source();
        let map = mapping();
        assert!(
            authority
                .start(&rule("G16"), input(&source, &map, 0), &[point(200, 200)])
                .is_ok()
        );
        let mut candidate = rule("G16");
        candidate.screen = None;
        error(
            authority.start(&candidate, input(&source, &map, 0), &[point(200, 200)]),
            Rejection::ScreenState,
        );
        candidate.action = Action::Power {
            action: PowerAction::Blank,
        };
        candidate.screen = Some(Screen::On);
        error(
            authority.start(&candidate, input(&source, &map, 0), &[point(200, 200)]),
            Rejection::ScreenState,
        );
    }
    #[test]
    fn t70_console_rules_are_explicit_and_do_not_have_desktop_authority() {
        let mut authority = Authority::default();
        authority.enter_console("seat0".into()).unwrap();
        let mut candidate = Config::preset("desktop").unwrap().rules.remove(0);
        candidate.contexts = vec![Context::Console];
        let source = source();
        let input = InputContext {
            source: &source,
            mapping: None,
            at: ms(0),
        };
        assert!(authority.start(&candidate, input, &[]).is_ok());
        candidate.contexts.clear();
        error(
            authority.start(&candidate, input, &[]),
            Rejection::ContextDenied,
        );
    }
    #[test]
    fn t61_future_and_reordered_snapshots_are_rejected() {
        let mut authority = authority();
        let generation = authority.generation();
        error(
            authority.update(snapshot(generation, 1, 0), ms(0)),
            Rejection::StaleRevision,
        );
        error(
            authority.update(snapshot(generation, 2, 200), ms(100)),
            Rejection::FutureTime,
        );
        let mut invalid = snapshot(generation, 2, 100);
        invalid.outputs[0].keyboard.as_mut().unwrap().rect.size.y = f64::NAN;
        error(
            authority.update(invalid, ms(100)),
            Rejection::InvalidGeometry,
        );
        assert_eq!(authority.state(ms(100)), State::Desktop);
    }
    #[test]
    fn t58_viewport_actions_freeze_the_start_workspace() {
        let mut authority = authority();
        let source = source();
        let map = mapping();
        let permit = authority
            .start(&rule("G11_left"), input(&source, &map, 0), &[point(4, 100)])
            .unwrap();
        let mut snap = snapshot(authority.generation(), 2, 100);
        snap.outputs[0].active_workspace = Some(2);
        authority.update(snap, ms(100)).unwrap();
        error(
            authority.validate(&permit, input(&source, &map, 100)),
            Rejection::OutputChanged,
        );
        let mut snap = snapshot(authority.generation(), 3, 110);
        snap.outputs[0].active_workspace = None;
        authority.update(snap, ms(110)).unwrap();
        error(
            authority.start(
                &rule("G11_left"),
                input(&source, &map, 110),
                &[point(4, 100)],
            ),
            Rejection::TargetMissing,
        );
    }
    #[test]
    fn t51_new_interactive_overlay_revokes_a_reserved_strip() {
        let mut authority = authority();
        let source = source();
        let map = mapping();
        let mut rule = rule("G11_left");
        rule.delivery = Delivery::Reserve;
        let permit = authority
            .start(&rule, input(&source, &map, 0), &[point(4, 100)])
            .unwrap();
        let mut snap = snapshot(authority.generation(), 2, 100);
        snap.regions.push(Region {
            id: "popup".into(),
            scope: None,
            output: "phone".into(),
            rect: rect(0.0, 30.0, 100.0, 100.0),
            interactive: true,
        });
        authority.update(snap, ms(100)).unwrap();
        error(
            authority.validate(&permit, input(&source, &map, 100)),
            Rejection::TargetChanged,
        );
    }
    #[test]
    fn t58_mapped_but_hidden_workspace_windows_cannot_take_a_touch_target() {
        let mut authority = authority();
        let mut snap = snapshot(authority.generation(), 2, 100);
        let mut hidden = snap.windows[0].clone();
        hidden.id = "hidden".into();
        hidden.visible = false;
        hidden.workspace = 2;
        hidden.z_order = Some(100);
        snap.windows.push(hidden);
        authority.update(snap, ms(100)).unwrap();
        let source = source();
        let map = mapping();
        let permit = authority
            .start(
                &rule("G08_up"),
                input(&source, &map, 100),
                &[point(100, 100)],
            )
            .unwrap();
        assert_eq!(permit.window().unwrap().id, "a");
        let mut snap = snapshot(authority.generation(), 3, 110);
        snap.windows[0].visible = false;
        authority.update(snap, ms(110)).unwrap();
        error(
            authority.validate(&permit, input(&source, &map, 110)),
            Rejection::TargetMissing,
        );
    }
}
