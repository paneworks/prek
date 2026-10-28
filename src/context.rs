//! Geometry and authority fences for a single active compositor session.
//! This is a state model, not peer authentication: the broker registration
//! layer must verify a binding before supplying it here.
use crate::{
    config::{Action, Context, Rule, Scope, Screen},
    gestures::{Mapping, Output, Point, Position},
    input::Source,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, time::Duration};

pub const PROVIDER_LEASE: Duration = crate::protocol::LEASE;
/// How long a context snapshot stays valid without a newer revision; it ends
/// before every [`crate::protocol::LEASE`].
pub const SNAPSHOT_MAX_AGE: Duration = Duration::from_millis(2500);
/// Oldest input event, or context of an action decision, that may still be
/// acted on.
pub const DECISION_MAX_AGE: Duration = Duration::from_millis(250);

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
pub struct OutputState {
    pub geometry: Output,
    pub geometry_generation: u64,
    pub screen: Screen,
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
pub struct Snapshot {
    pub generation: u64,
    pub revision: u64,
    pub captured_at: Duration,
    pub state: State,
    pub outputs: Vec<OutputState>,
}
impl Snapshot {
    pub fn validate(&self) -> Result<(), Rejection> {
        if self.outputs.len() > 256 {
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
    UnmappedDevice,
    OutputChanged,
    TargetMissing,
    OutsideRegion,
    MixedTargets,
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
    scope: Scope,
    mapping_generation: Option<u64>,
    starts: Vec<Point>,
}
impl Permit {
    pub(crate) fn retained_heap_bytes(&self) -> usize {
        self.source.seat.capacity()
            + self.starts.capacity() * std::mem::size_of::<Point>()
            + self
                .output
                .as_ref()
                .map_or(0, |output| output.geometry.name.capacity())
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn source(&self) -> &Source {
        &self.source
    }
    pub fn output(&self) -> Option<&OutputState> {
        self.output.as_ref()
    }
    pub fn scope(&self) -> Scope {
        self.scope
    }
    pub(crate) fn same_target(&self, later: &Self) -> bool {
        self.generation == later.generation
            && self.state == later.state
            && self.scope == later.scope
            && self.output == later.output
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
    #[cfg(target_os = "linux")]
    pub(crate) fn recorded(
        binding: Binding,
        snapshot: Snapshot,
        at: Duration,
    ) -> Result<Self, Rejection> {
        snapshot.validate()?;
        if snapshot.generation == 0
            || snapshot.generation == u64::MAX
            || snapshot.revision == 0
            || snapshot.captured_at > at
            || at - snapshot.captured_at > SNAPSHOT_MAX_AGE
        {
            return Err(Rejection::StaleSnapshot);
        }
        if binding.uid == 0
            || [&binding.seat, &binding.session]
                .iter()
                .any(|name| name.len() > 1024 || name.chars().any(char::is_control))
            || snapshot.state == State::Greeter && binding.kind != SessionKind::Greeter
            || snapshot.state == State::Desktop && binding.kind != SessionKind::User
        {
            return Err(Rejection::Unbound);
        }
        let mut authority = Self::default();
        authority.adopt(binding.clone(), snapshot.generation)?;
        match snapshot.state {
            State::Locked => authority.barrier = LockBarrier::Locked,
            State::Locking => authority.barrier = LockBarrier::Locking,
            State::Inactive | State::Unknown => authority.active = false,
            State::Console => authority.console = Some(binding.seat),
            _ => {}
        }
        authority.revision = snapshot.revision;
        authority.acknowledged_at = Some(at);
        authority.lease = Some(at.saturating_add(PROVIDER_LEASE));
        authority.snapshot = Some(snapshot);
        Ok(authority)
    }
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
    /// Called only once the session lock is confirmed.
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
    /// Called only once the session unlock is confirmed. A fresh Desktop
    /// snapshot is still required before actions become eligible.
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
                || rule.screen.is_some()
            {
                return Err(Rejection::ContextDenied);
            }
            return Ok(Permit {
                source: source.clone(),
                generation: self.generation,
                state,
                output: None,
                scope: rule.scope,
                mapping_generation: None,
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
                Action::Lua { .. }
                    | Action::Command { .. }
                    | Action::Keys { .. }
                    | Action::Remap { .. }
            )
        {
            return Err(Rejection::Locked);
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
            if output.screen == Screen::Off && rule.screen != Some(Screen::Off) {
                return Err(Rejection::ScreenState);
            }
        } else if rule.screen.is_some() {
            return Err(Rejection::UnmappedDevice);
        }
        Ok(Permit {
            source: source.clone(),
            generation: self.generation,
            state,
            output,
            scope: rule.scope,
            mapping_generation: mapping.map(|mapping| mapping.generation),
            starts: points,
        })
    }
    /// Check again immediately before dispatch, and whenever another finger joins.
    /// Output geometry changes revoke a permit.
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
        if joined.scope != permit.scope || joined.output != permit.output {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{Config, Delivery, Direction, Distance, Trigger},
        gestures::Transform,
        input::Axis,
    };
    #[test]
    fn explicit_output_freezes_targets_without_inventing_a_mapping() {
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
            outputs: vec![OutputState {
                geometry: mapping().output.unwrap(),
                geometry_generation: 1,
                screen: Screen::On,
            }],
        }
    }
    fn authority() -> Authority {
        let mut authority = Authority::default();
        let generation = authority.bind(binding(), true, false).unwrap();
        authority.update(snapshot(generation, 1, 0), ms(0)).unwrap();
        authority
    }
    fn rule() -> Rule {
        Rule {
            id: "swipe".into(),
            enabled: true,
            priority: 0,
            devices: vec![],
            seats: vec![],
            contexts: vec![Context::Desktop],
            scope: Scope::Everywhere,
            output: None,
            screen: None,
            require_keys: vec![],
            keys_throughout: false,
            layer: None,
            when_mode: None,
            delivery: Delivery::Observe,
            trigger: Trigger::Swipe {
                fingers: 2,
                direction: Direction::Up,
                travel: Distance::Millimetres(12.0),
                fallback_travel: Distance::Normalized(0.04),
                continuous: false,
            },
            action: Action::Event {
                name: "swipe".into(),
            },
        }
    }
    fn screen_off_rule() -> Rule {
        let mut rule = rule();
        rule.contexts = vec![Context::Locked, Context::Greeter];
        rule.screen = Some(Screen::Off);
        rule.trigger = Trigger::TouchTaps {
            fingers: 1,
            count: 2,
        };
        rule
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
        let permit = authority
            .start(
                &rule(),
                input(&source, &map, 0),
                &[point(200, 200), point(1000, 2000)],
            )
            .unwrap();
        assert_eq!(permit.output().unwrap().geometry.name, "phone");
        assert!(point(1000, 2000).global.unwrap().x < 500.0);
    }
    #[test]
    fn t19_output_geometry_change_cancels_the_permit() {
        let mut authority = authority();
        let source = source();
        let map = mapping();
        let permit = authority
            .start(&rule(), input(&source, &map, 0), &[point(200, 200)])
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
    fn t28_additional_finger_must_start_on_the_original_output() {
        let mut authority = authority();
        let source = source();
        let map = mapping();
        let rule = rule();
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
        next.outputs[0].geometry.origin.x = 10.0;
        authority.update(next, ms(100)).unwrap();
        error(
            authority.join(
                &mut permit,
                &rule,
                input(&source, &map, 100),
                point(800, 200),
            ),
            Rejection::OutputChanged,
        );
    }
    #[test]
    fn t61_snapshot_age_and_provider_lease_do_not_revive_queued_actions() {
        let mut authority = authority();
        let source = source();
        let map = mapping();
        let permit = authority
            .start(&rule(), input(&source, &map, 0), &[point(200, 200)])
            .unwrap();
        let age = SNAPSHOT_MAX_AGE.as_millis() as u64;
        authority
            .validate(&permit, input(&source, &map, age))
            .unwrap();
        assert!(
            authority
                .validate(&permit, input(&source, &map, age + 1))
                .is_err()
        );
        let old = authority.generation();
        authority.tick(ms(age + 1));
        assert_ne!(authority.generation(), old);
        let captured = age + 50;
        let fresh = snapshot(authority.generation(), 1, captured);
        authority.update(fresh, ms(captured)).unwrap();
        error(
            authority.validate(&permit, input(&source, &map, captured)),
            Rejection::StaleGeneration,
        );
        let generation = authority.generation();
        let renewed = captured + 200;
        authority.heartbeat(generation, ms(renewed)).unwrap();
        assert_eq!(authority.state(ms(captured + age + 1)), State::Unknown);
        authority.tick(ms(renewed) + PROVIDER_LEASE);
        assert_ne!(authority.generation(), generation);
    }
    #[test]
    fn t61_late_refresh_cannot_bypass_expiration_without_tick() {
        let mut authority = authority();
        let old = authority.generation();
        let late = SNAPSHOT_MAX_AGE.as_millis() as u64 + 50;
        error(
            authority.update(snapshot(old, 2, late), ms(late)),
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
            authority.start(&rule(), input(&source, &map, 0), &[point(200, 200)]),
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
            .start(&rule(), input(&source, &map, 0), &[point(200, 200)])
            .unwrap();
        let mut other = source.clone();
        other.seat = "seat1".into();
        error(
            authority.start(&rule(), input(&other, &map, 0), &[point(200, 200)]),
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
        let mut rule = rule();
        rule.contexts = vec![Context::Greeter];
        rule.action = Action::Command {
            argv: vec!["true".into()],
        };
        let source = source();
        let map = mapping();
        error(
            authority.start(&rule, input(&source, &map, 0), &[point(200, 200)]),
            Rejection::Locked,
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
                .start(
                    &screen_off_rule(),
                    input(&source, &map, 0),
                    &[point(200, 200)]
                )
                .is_ok()
        );
        let mut candidate = screen_off_rule();
        candidate.screen = None;
        error(
            authority.start(&candidate, input(&source, &map, 0), &[point(200, 200)]),
            Rejection::ScreenState,
        );
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
        invalid.outputs[0].geometry.scale = f64::NAN;
        error(
            authority.update(invalid, ms(100)),
            Rejection::InvalidGeometry,
        );
        assert_eq!(authority.state(ms(100)), State::Desktop);
    }
}
