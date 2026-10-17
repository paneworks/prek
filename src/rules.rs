//! Configured rule evaluation. This observation engine never owns devices or
//! executes actions. The same candidates/permits will feed the routing layer;
//! a match here is not permission to consume already-forwarded input.
use crate::{
    Button, Buttons, Gesture,
    config::{Action, Config, Direction, Rule, Scope, Trigger},
    context::{Authority, InputContext, Permit, Rejection},
    gestures::{Mapping, Phase, Space, TouchEvent, TouchGesture, TouchRecognizer, TouchSettings},
    input::{Change, Contact, Device, KeyState, NormalizedFrame, Source},
    keys::{Chord, Hold, KeyObservation, Keys, Repeat},
    paths::{CaptureIdentity, MatchOutcome, Matcher, PathCapture, ShapeMatch, Stroke},
    replay::{Observation, Record, Replay},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    time::Duration,
};
mod continuation;
mod custom;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Discrete,
    Begin,
    Update,
    End,
    Cancel,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Evidence {
    Custom {
        recognizer: String,
        data: serde_json::Value,
    },
    Key {
        down: bool,
    },
    Taps {
        count: u32,
    },
    Hold {
        repeat: bool,
    },
    Chord,
    Touch {
        space: Space,
        gesture: TouchGesture,
    },
    Wheel {
        horizontal: bool,
        amount: f64,
    },
    Shape {
        space: Space,
        fingers: Option<u16>,
        shape: ShapeMatch,
    },
    Sequence {
        steps: Vec<Evidence>,
    },
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum Event {
    Matched {
        version: u32,
        rule: String,
        id: u64,
        /// Deduplicate (gesture ID, ordinal); repeats share a gesture, not an action.
        ordinal: u64,
        source: Source,
        at: Duration,
        config_generation: u64,
        session_generation: u64,
        stage: Stage,
        action: Action,
        evidence: Evidence,
        window: Option<String>,
        output: Option<String>,
        workspace: Option<i64>,
    },
    Skipped {
        version: u32,
        rule: String,
        source: Source,
        at: Duration,
        reason: String,
    },
    Cancelled {
        version: u32,
        rule: String,
        id: u64,
        source: Source,
        at: Duration,
        reason: String,
    },
}

impl Event {
    pub(crate) fn supersedes_progress(&self, old: &Self) -> bool {
        matches!((self, old), (
            Self::Matched { id, ordinal, source, rule, config_generation, session_generation, stage: Stage::Update, .. },
            Self::Matched { id: old_id, ordinal: old_ordinal, source: old_source, rule: old_rule,
                config_generation: old_config, session_generation: old_session, stage: Stage::Update, .. }
        ) if id == old_id && ordinal > old_ordinal && source == old_source && rule == old_rule
            && config_generation == old_config && session_generation == old_session)
    }
}

#[derive(Clone)]
struct DeviceState {
    device: Device,
    input_watermark: u64,
    last_input: Option<(u64, Duration)>,
    mapping: Option<Mapping>,
    keys: BTreeSet<u16>,
    contacts: Vec<Contact>,
}
#[derive(Clone)]
struct Candidate {
    id: u64,
    ordinal: u64,
    source: Source,
    session_generation: u64,
    permit: Option<Permit>,
    progress: bool,
    expires: Option<Duration>,
    started: Duration,
    started_input: u64,
    continued_from: Option<u64>,
    contacts: BTreeSet<crate::input::ContactId>,
    sources: BTreeSet<Source>,
    source_inputs: BTreeMap<Source, u64>,
    peak_contacts: usize,
    peak_at: Duration,
}
#[derive(Clone)]
pub(crate) struct RouteIntent {
    pub rule: Rule,
    candidate: Candidate,
}
#[derive(Default)]
pub(crate) struct RecognitionOwnership {
    pub ownership: Option<crate::router::Ownership>,
    pub winner: Option<u64>,
    pub deadline: Option<Duration>,
}
impl RouteIntent {
    pub fn hyprland_target_available(&self, authority: &Authority, at: Duration) -> bool {
        let Some(permit) = self.candidate.permit.as_ref() else {
            return false;
        };
        let (Some(window), Some(output)) = (permit.window(), permit.desktop_output()) else {
            return false;
        };
        authority.fresh(at).is_ok_and(|snapshot| {
            snapshot.hyprland_target_available(
                &self.rule.action,
                self.rule.scope,
                Some(&window.id),
                Some(&output.geometry.name),
                Some(window.workspace),
            )
        })
    }
    pub fn power_target_available(&self) -> bool {
        self.candidate
            .permit
            .as_ref()
            .and_then(|permit| permit.desktop_output())
            .is_some_and(|output| crate::config::valid_power_output(&output.geometry.name))
    }
    pub fn id(&self) -> u64 {
        self.candidate.id
    }
    pub fn source(&self) -> &Source {
        &self.candidate.source
    }
    pub fn started(&self) -> Duration {
        self.candidate.started
    }
    pub fn contacts(&self) -> usize {
        self.candidate.peak_contacts
    }
    pub fn joined_at(&self) -> Duration {
        self.candidate.peak_at
    }
    pub fn sources(&self) -> &BTreeSet<Source> {
        &self.candidate.sources
    }
    pub(crate) fn admits_input(&self, source: &Source, watermark: u64) -> bool {
        self.candidate.source_inputs.get(source) == Some(&watermark)
    }
}
pub(crate) struct RouteOutcome {
    pub event: Event,
    pub intent: RouteIntent,
    pub expires: Option<Duration>,
}
#[derive(Clone)]
struct StepMatch {
    event: Event,
    candidate: Candidate,
    finished: Duration,
    expires: Option<Duration>,
    after_input: Option<u64>,
}
enum Recognizer {
    Custom {
        blocked: BTreeSet<Source>,
        wakes: BTreeMap<Source, Duration>,
        closed: BTreeSet<Source>,
    },
    Key,
    Taps(Buttons),
    Keys(Keys),
    Touch(BTreeMap<u64, TouchRecognizer>),
    Wheel(BTreeMap<Source, f64>),
    Shape {
        captures: BTreeMap<Source, ShapeCapture>,
        matcher: Matcher,
    },
}
struct ShapeCapture {
    started: Duration,
    path: Option<PathCapture>,
    identity: CaptureIdentity,
}
struct Worker {
    rule: Rule,
    recognizer: Recognizer,
    candidates: BTreeMap<Source, Candidate>,
    touch_active: BTreeSet<Source>,
    trace: bool,
    matches: Vec<StepMatch>,
    released: BTreeMap<Source, Duration>,
    routing: bool,
    claims: Vec<Candidate>,
}

/// Custom triggers additionally require the trusted ticket coordinator.
pub fn unsupported(trigger: &Trigger) -> Option<&'static str> {
    trigger
        .recognizers()
        .next()
        .is_some()
        .then_some("custom recognizers require the trusted asynchronous coordinator")
}
impl Worker {
    fn new(rule: Rule, config: &Config) -> io::Result<Self> {
        if let Some(reason) = unsupported(&rule.trigger)
            && !matches!(rule.trigger, Trigger::Custom { .. })
        {
            return Err(invalid(format!("rules.{}: {reason}", rule.id)));
        }
        let recognizer = match &rule.trigger {
            Trigger::Custom { .. } => Recognizer::Custom {
                blocked: BTreeSet::new(),
                wakes: BTreeMap::new(),
                closed: BTreeSet::new(),
            },
            Trigger::Key { .. } => Recognizer::Key,
            Trigger::ButtonTaps { .. } => {
                Recognizer::Taps(Buttons::new(config.timing.buttons()).map_err(invalid)?)
            }
            Trigger::ButtonHold { key, after, repeat } => Recognizer::Keys(
                Keys::new(
                    Vec::new(),
                    vec![Hold {
                        id: rule.id.clone(),
                        key: key.0,
                        after: after.duration(),
                        repeat: repeat.as_ref().map(|r| Repeat {
                            delay: r.delay.duration(),
                            interval: r.interval.duration(),
                        }),
                    }],
                    Vec::new(),
                )
                .map_err(invalid)?,
            ),
            Trigger::Chord {
                keys,
                ordered,
                timeout,
            } => Recognizer::Keys(
                Keys::new(
                    vec![Chord {
                        id: rule.id.clone(),
                        keys: keys.iter().map(|key| key.0).collect(),
                        ordered: *ordered,
                        timeout: timeout.duration(),
                    }],
                    Vec::new(),
                    Vec::new(),
                )
                .map_err(invalid)?,
            ),
            Trigger::Wheel { .. } => Recognizer::Wheel(BTreeMap::new()),
            Trigger::Shape { .. } => Recognizer::Shape {
                captures: BTreeMap::new(),
                matcher: Matcher::new(&config.shapes).map_err(invalid)?,
            },
            _ => Recognizer::Touch(BTreeMap::new()),
        };
        Ok(Self {
            rule,
            recognizer,
            candidates: BTreeMap::new(),
            touch_active: BTreeSet::new(),
            trace: false,
            matches: Vec::new(),
            released: BTreeMap::new(),
            routing: false,
            claims: Vec::new(),
        })
    }
    fn register(&mut self, state: &DeviceState, config: &Config) -> io::Result<()> {
        if !selected(config, &self.rule, &state.device) {
            return Ok(());
        }
        match &mut self.recognizer {
            Recognizer::Custom {
                blocked,
                wakes,
                closed,
            } => {
                closed.retain(|source| source.device != state.device.source.device);
                blocked.retain(|source| source.device != state.device.source.device);
                wakes.retain(|source, _| source.device != state.device.source.device);
                if !state.keys.is_empty() || !state.contacts.is_empty() {
                    blocked.insert(state.device.source.clone());
                }
            }
            Recognizer::Shape { .. }
                if !state.contacts.is_empty()
                    || matches!(self.rule.trigger, Trigger::Shape { fingers: None, button, .. } if state.keys.contains(&button.map_or(0x110, |key| key.0))) =>
            {
                self.touch_active.insert(state.device.source.clone());
            }
            Recognizer::Keys(keys) => {
                keys.reconcile(&state.device.source, state.keys.iter().copied())
                    .map_err(invalid)?;
            }
            Recognizer::Touch(touches) => {
                let Some(mapping) = state.mapping.clone() else {
                    return Ok(());
                };
                let mut settings = TouchSettings {
                    timing: config.timing.clone(),
                    ..TouchSettings::default()
                };
                match self.rule.trigger {
                    Trigger::TouchHold { after, .. } => settings.timing.hold_after = after,
                    Trigger::Swipe {
                        travel,
                        fallback_travel,
                        ..
                    } => {
                        settings.swipe_travel = travel;
                        settings.fallback_travel = fallback_travel;
                    }
                    Trigger::Pinch { ratio, .. } => settings.pinch_slop = ratio,
                    Trigger::Rotate { degrees, .. } => settings.rotation_slop = degrees,
                    _ => {}
                }
                let mut touch =
                    TouchRecognizer::new(state.device.source.clone(), mapping, settings)
                        .map_err(invalid)?;
                touch.reconcile(Duration::ZERO, !state.contacts.is_empty());
                if !state.contacts.is_empty() {
                    self.touch_active.insert(state.device.source.clone());
                }
                touches.insert(state.device.source.device, touch);
            }
            _ => {}
        }
        Ok(())
    }
    fn deadline(&self) -> Option<Duration> {
        let recognizer = match &self.recognizer {
            Recognizer::Taps(buttons) => buttons.deadline(),
            Recognizer::Keys(keys) => keys.deadline(),
            Recognizer::Touch(touches) => {
                touches.values().filter_map(TouchRecognizer::deadline).min()
            }
            Recognizer::Shape { captures, .. } => captures
                .values()
                .filter_map(|capture| capture.path.as_ref().map(PathCapture::deadline))
                .min(),
            _ => None,
        };
        recognizer
            .into_iter()
            .chain(self.candidates.values().filter_map(|c| c.expires))
            .min()
    }
}

pub struct Engine {
    validation: crate::config::Validation,
    custom: Option<custom::State>,
    coordinated_step: bool,
    custom_owned: BTreeSet<u64>,
    custom_seats: BTreeSet<String>,
    input_watermark: u64,
    config: Config,
    generation: u64,
    rule_revision: u64,
    next_id: u64,
    replay: Replay,
    devices: BTreeMap<u64, DeviceState>,
    conditions: Keys,
    workers: Vec<Worker>,
    authority: Authority,
    events: Vec<Event>,
    expected: Option<Vec<Event>>,
    last_at: Option<Duration>,
    capture_rule: Option<String>,
    captured: Vec<Stroke>,
    capture_input: bool,
    captured_input: Option<NormalizedFrame>,
    sequences: Vec<Sequence>,
    routing: bool,
}
impl Engine {
    pub fn new(config: Config) -> io::Result<Self> {
        Self::build(config, None)
    }
    /// The embedding owns authorization, transport and helper supervision.
    pub fn with_recognition(config: Config, fence: crate::protocol::Fence) -> io::Result<Self> {
        Self::build(config, Some(fence))
    }
    fn build(config: Config, fence: Option<crate::protocol::Fence>) -> io::Result<Self> {
        let validation = if fence.is_some() {
            crate::config::Validation::Coordinated
        } else {
            crate::config::Validation::Public
        };
        Self::compile(config, fence, validation)
    }
    pub(crate) fn baseline(config: Config) -> io::Result<Self> {
        Self::compile(config, None, crate::config::Validation::Coordinated)
    }
    fn compile(
        config: Config,
        fence: Option<crate::protocol::Fence>,
        validation: crate::config::Validation,
    ) -> io::Result<Self> {
        config.validate_for(validation).map_err(invalid)?;
        let custom = custom::State::new(&config, fence)?;
        let mut workers = config
            .rules
            .iter()
            .filter(|r| r.enabled && config.mode_active(r))
            .filter(|rule| !matches!(rule.trigger, Trigger::Sequence { .. }))
            .cloned()
            .map(|r| Worker::new(r, &config))
            .collect::<io::Result<Vec<_>>>()?;
        workers.sort_by(|a, b| {
            b.rule
                .priority
                .cmp(&a.rule.priority)
                .then_with(|| a.rule.id.cmp(&b.rule.id))
        });
        let conditions =
            Keys::new(Vec::new(), Vec::new(), config.layers.clone()).map_err(invalid)?;
        let sequences = config
            .rules
            .iter()
            .filter(|rule| {
                rule.enabled
                    && config.mode_active(rule)
                    && matches!(rule.trigger, Trigger::Sequence { .. })
            })
            .map(|rule| Sequence::new(rule.clone(), &config, fence))
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Self {
            validation,
            custom,
            coordinated_step: false,
            custom_owned: BTreeSet::new(),
            custom_seats: BTreeSet::new(),
            input_watermark: 0,
            config,
            generation: fence.map_or(1, |fence| fence.configuration),
            rule_revision: 1,
            next_id: 0,
            replay: Replay::default(),
            devices: BTreeMap::new(),
            conditions,
            workers,
            authority: Authority::default(),
            events: Vec::new(),
            expected: None,
            last_at: None,
            capture_rule: None,
            captured: Vec::new(),
            capture_input: false,
            captured_input: None,
            sequences,
            routing: false,
        })
    }
    /// Explicit diagnostic capture only. Ordinary engines never retain or
    /// expose trajectories. The capture rule must be the sole enabled,
    /// context-free observational shape rule; it cannot execute an action.
    pub fn new_capture(config: Config, rule: &str) -> io::Result<Self> {
        let enabled: Vec<_> = config.rules.iter().filter(|rule| rule.enabled).collect();
        if config.mode != crate::config::Mode::Observe
            || enabled.len() != 1
            || enabled[0].id != rule
            || !context_free(enabled[0])
            || enabled[0].delivery != crate::config::Delivery::Observe
            || !matches!(enabled[0].trigger, Trigger::Shape { .. })
        {
            return Err(invalid(
                "capture requires one observational, context-free shape rule",
            ));
        }
        let mut engine = Self::new(config)?;
        engine.capture_rule = Some(rule.into());
        Ok(engine)
    }
    pub fn take_strokes(&mut self) -> Vec<Stroke> {
        std::mem::take(&mut self.captured)
    }
    /// Trusted registration layer only. IPC must verify peer/logind binding
    /// before calling this model; a caller-supplied session name is insufficient.
    pub fn authority_mut(&mut self) -> &mut Authority {
        &mut self.authority
    }
    pub(crate) fn authority(&self) -> &Authority {
        &self.authority
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub(crate) fn configuration_generation(&mut self, generation: u64) {
        self.generation = generation;
    }
    pub fn config(&self) -> &Config {
        &self.config
    }
    pub fn rule_revision(&self) -> u64 {
        self.rule_revision
    }
    pub(crate) fn recognition_fence(&self) -> Option<crate::protocol::Fence> {
        self.custom.as_ref().map(|custom| custom.fence)
    }
    pub(crate) fn input_at(&self) -> Duration {
        self.last_at.unwrap_or_default()
    }
    pub(crate) fn input_watermark(&self) -> u64 {
        self.input_watermark
    }
    pub(crate) fn cancel_sequence(&mut self, id: u64, at: Duration, reason: &str) {
        for sequence in &mut self.sequences {
            let seat = sequence
                .pending
                .iter()
                .find_map(|(seat, pending)| (pending.id == id).then(|| seat.clone()));
            if let Some(seat) = seat {
                sequence.cancel(&seat, at, reason, &mut self.events);
                sequence.reset.insert(seat);
            }
        }
        self.custom_sync(at, crate::recognition::Retirement::Cancelled);
    }
    pub(crate) fn enable_routing(&mut self) {
        self.routing = true;
        for worker in &mut self.workers {
            worker.routing = true;
            worker.trace = true;
        }
        for sequence in &mut self.sequences {
            sequence.routing = true;
            for engine in &mut sequence.steps {
                engine.enable_routing();
            }
        }
    }
    pub(crate) fn route_intents(&self) -> Vec<RouteIntent> {
        let mut result = Vec::new();
        for worker in &self.workers {
            result.extend(
                worker
                    .candidates
                    .values()
                    .cloned()
                    .map(|candidate| RouteIntent {
                        rule: worker.rule.clone(),
                        candidate,
                    }),
            );
        }
        for sequence in &self.sequences {
            result.extend(
                sequence
                    .pending
                    .values()
                    .map(|pending| pending.route_intent(&sequence.rule)),
            );
        }
        result
    }
    pub(crate) fn take_route_outcomes(&mut self) -> Vec<RouteOutcome> {
        let mut result = Vec::new();
        for worker in &mut self.workers {
            result.extend(
                std::mem::take(&mut worker.matches)
                    .into_iter()
                    .map(|trace| RouteOutcome {
                        event: trace.event,
                        expires: trace.expires,
                        intent: RouteIntent {
                            rule: worker.rule.clone(),
                            candidate: trace.candidate,
                        },
                    }),
            );
        }
        for sequence in &mut self.sequences {
            result.append(&mut sequence.outcomes);
        }
        result
    }
    pub(crate) fn take_route_claims(&mut self) -> Vec<RouteIntent> {
        let mut result = Vec::new();
        for worker in &mut self.workers {
            result.extend(
                std::mem::take(&mut worker.claims)
                    .into_iter()
                    .map(|candidate| RouteIntent {
                        rule: worker.rule.clone(),
                        candidate,
                    }),
            );
        }
        result
    }
    pub(crate) fn validate_route(
        &self,
        intent: &RouteIntent,
        at: Duration,
    ) -> Result<(), Rejection> {
        if intent.candidate.session_generation != self.authority.generation() {
            return Err(Rejection::StaleGeneration);
        }
        let state = self
            .devices
            .get(&intent.source().device)
            .filter(|state| &state.device.source == intent.source())
            .ok_or(Rejection::SourceChanged)?;
        let permit = intent.candidate.permit.as_ref().ok_or(Rejection::Unbound)?;
        self.authority.validate(permit, input(state, at))
    }
    pub(crate) fn fork_observer(
        &self,
        config: Config,
        seat: &str,
        fence: crate::protocol::Fence,
        at: Duration,
    ) -> io::Result<Self> {
        let mut next = Self::with_recognition(config, fence)?;
        next.recognition_ready(false, at)?;
        next.push(Record::Header {
            version: crate::replay::FORMAT_VERSION,
        })?;
        for state in self
            .devices
            .values()
            .filter(|state| state.device.source.seat == seat)
        {
            next.push(Record::Device {
                device: state.device.clone(),
                keys: state.keys.clone(),
                contacts: state.contacts.clone(),
            })?;
            if let Some(absolute) = self.replay.absolute_state(&state.device.source) {
                next.push(Record::Absolute {
                    source: state.device.source.clone(),
                    state: absolute.clone(),
                })?;
            }
            if let Some(mapping) = &state.mapping {
                next.push(Record::Mapping {
                    source: state.device.source.clone(),
                    at,
                    mapping: mapping.clone(),
                })?;
            }
        }
        next.generation = self.generation;
        next.authority.observation_generation(fence.session);
        next.last_at = Some(at);
        Ok(next)
    }
    pub(crate) fn reconciliation(&self, source: &Source, at: Duration) -> Option<Record> {
        self.devices
            .get(&source.device)
            .filter(|state| &state.device.source == source)
            .map(|state| Record::Reconcile {
                source: source.clone(),
                at,
                keys: state.keys.clone(),
                contacts: state.contacts.clone(),
            })
    }
    pub fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }
    pub(crate) fn events(&self) -> &[Event] {
        &self.events
    }
    pub(crate) fn capture_input(&mut self, enabled: bool) {
        self.capture_input = enabled;
        self.captured_input = None;
    }
    pub(crate) fn take_input(&mut self) -> Option<crate::input_events::Event> {
        let frame = self.captured_input.take()?;
        let device = &self.devices.get(&frame.raw.source.device)?.device;
        crate::input_events::Event::frame(&self.config, device, frame)
    }
    pub(crate) fn input_states(&self, at: Duration) -> Vec<crate::input_events::Event> {
        self.devices
            .values()
            .filter_map(|state| {
                crate::input_events::Event::reset(
                    &self.config,
                    &state.device,
                    state.mapping.as_ref(),
                    at,
                    crate::input_events::ResetReason::Attached,
                    &state.keys,
                    &state.contacts,
                )
            })
            .collect()
    }
    pub(crate) fn input_reset(
        &self,
        source: &Source,
        at: Duration,
        reason: crate::input_events::ResetReason,
    ) -> Option<crate::input_events::Event> {
        let state = self
            .devices
            .get(&source.device)
            .filter(|state| &state.device.source == source)?;
        crate::input_events::Event::reset(
            &self.config,
            &state.device,
            state.mapping.as_ref(),
            at,
            reason,
            &state.keys,
            &state.contacts,
        )
    }
    pub fn deadline(&self) -> Option<Duration> {
        self.workers
            .iter()
            .filter_map(Worker::deadline)
            .chain(self.sequences.iter().filter_map(Sequence::deadline))
            .chain(self.conditions.deadline())
            .chain(self.custom_deadline())
            .min()
    }
    pub fn push(&mut self, record: Record) -> io::Result<()> {
        if matches!(record, Record::Context { .. }) {
            return Err(invalid(
                "recorded context requires configured offline replay",
            ));
        }
        if self.expected.is_some() {
            return Err(invalid("expected rule outcomes must be the final record"));
        }
        if let Record::ExpectedRules { events } = record {
            if self.devices.is_empty() {
                return Err(invalid("rule expectations require registered input"));
            }
            if events.len() > 16384 {
                return Err(invalid("too many expected rule outcomes"));
            }
            self.expected = Some(events);
            return Ok(());
        }
        if self.events.len() >= 16384 {
            return Err(invalid(
                "rule observation queue full; drain events before continuing",
            ));
        }
        // Validate metadata/normalization first. An invalid registration cannot
        // replace the current generation or a working recognizer.
        self.prepare_registration(&record)?;
        let input_watermark = if !self.coordinated_step && matches!(record, Record::Frame { .. }) {
            self.input_watermark
                .checked_add(1)
                .ok_or_else(|| invalid("input watermark exhausted"))?
        } else {
            self.input_watermark
        };
        self.replay.push(record.clone())?;
        self.input_watermark = input_watermark;
        let observations = self.replay.take_observations();
        match &record {
            Record::Device {
                device,
                keys,
                contacts,
            } => {
                if let Some(old) = self.devices.get(&device.source.device) {
                    let source = old.device.source.clone();
                    self.cancel_source(
                        &source,
                        self.last_at.unwrap_or_default(),
                        "device replaced",
                    );
                }
                let state = DeviceState {
                    device: device.clone(),
                    input_watermark: self.input_watermark,
                    last_input: None,
                    keys: keys.clone(),
                    contacts: contacts.clone(),
                    mapping: Mapping::from_device(device).ok(),
                };
                if policy_selected(&self.config, device) {
                    self.conditions
                        .reconcile(&device.source, keys.iter().copied())
                        .map_err(invalid)?;
                }
                for worker in &mut self.workers {
                    worker.register(&state, &self.config)?;
                }
                self.devices.insert(device.source.device, state);
            }
            Record::Mapping {
                source,
                at,
                mapping,
            } => {
                let changed = self
                    .devices
                    .get(&source.device)
                    .is_some_and(|state| state.mapping.as_ref() != Some(mapping));
                if !changed {
                    return Ok(());
                }
                self.cancel_source(source, *at, "output mapping changed");
                let state = self
                    .devices
                    .get_mut(&source.device)
                    .ok_or_else(|| invalid("unregistered mapping source"))?;
                state.mapping = Some(mapping.clone());
                if policy_selected(&self.config, &state.device) {
                    self.conditions
                        .reconcile(source, state.keys.iter().copied())
                        .map_err(invalid)?;
                }
                for worker in &mut self.workers {
                    if matches!(worker.recognizer, Recognizer::Shape { .. }) {
                        worker.register(state, &self.config)?;
                    }
                    if let Recognizer::Touch(touches) = &mut worker.recognizer
                        && let Some(touch) = touches.get_mut(&source.device)
                    {
                        touch
                            .replace_mapping(mapping.clone(), *at)
                            .map_err(invalid)?;
                        touch.reconcile(*at, !state.contacts.is_empty());
                    }
                }
            }
            Record::Reconcile {
                source,
                at,
                keys,
                contacts,
            } => {
                self.cancel_source(source, *at, "input reconciled");
                let state = self
                    .devices
                    .get_mut(&source.device)
                    .ok_or_else(|| invalid("unregistered reconciliation source"))?;
                state.keys = keys.clone();
                state.contacts = contacts.clone();
                if policy_selected(&self.config, &state.device) {
                    self.conditions
                        .reconcile(source, keys.iter().copied())
                        .map_err(invalid)?;
                }
                for worker in &mut self.workers {
                    if matches!(worker.recognizer, Recognizer::Shape { .. }) {
                        worker.register(state, &self.config)?;
                    }
                    if let Recognizer::Keys(core) = &mut worker.recognizer
                        && selected(&self.config, &worker.rule, &state.device)
                    {
                        core.reconcile(source, keys.iter().copied())
                            .map_err(invalid)?;
                    }
                    if let Recognizer::Touch(touches) = &mut worker.recognizer
                        && let Some(touch) = touches.get_mut(&source.device)
                    {
                        touch.reconcile(*at, !contacts.is_empty());
                    }
                }
            }
            Record::Remove { source, .. } => {
                self.devices.remove(&source.device);
            }
            _ => {}
        }
        let mut sequence_input = None;
        let project_sequence_input = self.sequences.iter().any(|sequence| sequence.has_custom);
        for observation in observations {
            match observation {
                Observation::Input { frame } => {
                    if project_sequence_input {
                        sequence_input = Some(frame.clone());
                    }
                    self.frame(frame)?;
                }
                Observation::Cancelled { source, at, reason } => {
                    self.cancel_source(&source, at, &format!("{reason:?}"))
                }
                _ => {}
            }
        }
        if let Record::Tick { at } = &record {
            self.advance(*at)?;
        }
        let context = SequenceContext {
            devices: &self.devices,
            authority: &self.authority,
            keys: &self.conditions,
            generation: self.generation,
            at: self.last_at.unwrap_or_default(),
            admitted_at: self.last_at.unwrap_or_default(),
            input_watermark: self.input_watermark,
            normalized: sequence_input.as_ref(),
        };
        for sequence in &mut self.sequences {
            sequence.push(
                &record,
                &context,
                &mut self.next_id,
                &mut self.events,
                &mut self.custom,
            )?;
        }
        if self.shape_samples() > 65_536 {
            return Err(invalid("total shape sample budget exceeded"));
        }
        if self.events.len() > 16384 {
            return Err(invalid("rule observation queue limit exceeded"));
        }
        self.custom_sync(
            self.last_at.unwrap_or_default(),
            match record {
                Record::Remove { .. } | Record::Reconcile { .. } | Record::Mapping { .. } => {
                    crate::recognition::Retirement::SourceLost
                }
                _ => crate::recognition::Retirement::Cancelled,
            },
        );
        Ok(())
    }
    pub fn check(&self) -> io::Result<()> {
        let expected = self
            .expected
            .as_ref()
            .ok_or_else(|| invalid("recording has no expected rule outcomes"))?;
        let actual = serde_json::to_value(&self.events).map_err(invalid)?;
        let expected_json = serde_json::to_value(expected).map_err(invalid)?;
        if crate::replay::similar_geometry(&actual, &expected_json) {
            Ok(())
        } else {
            Err(invalid(format!(
                "rule outcome mismatch: expected {} events, received {}",
                expected.len(),
                self.events.len()
            )))
        }
    }
    fn shape_samples(&self) -> usize {
        self.workers
            .iter()
            .filter_map(|worker| match &worker.recognizer {
                Recognizer::Shape { captures, .. } => Some(
                    captures
                        .values()
                        .filter_map(|capture| capture.path.as_ref())
                        .map(PathCapture::sample_count)
                        .sum::<usize>(),
                ),
                _ => None,
            })
            .sum::<usize>()
            + self
                .sequences
                .iter()
                .flat_map(|sequence| sequence.steps.iter())
                .map(Engine::shape_samples)
                .sum::<usize>()
    }
    fn prepare_registration(&self, record: &Record) -> io::Result<()> {
        let state = match record {
            Record::Device {
                device,
                keys,
                contacts,
            } => DeviceState {
                device: device.clone(),
                input_watermark: self.input_watermark,
                last_input: None,
                mapping: Mapping::from_device(device).ok(),
                keys: keys.clone(),
                contacts: contacts.clone(),
            },
            Record::Mapping { source, .. } | Record::Reconcile { source, .. } => {
                let mut state = self
                    .devices
                    .get(&source.device)
                    .filter(|state| state.device.source == *source)
                    .ok_or_else(|| invalid("unregistered or stale input source"))?
                    .clone();
                match record {
                    Record::Mapping { mapping, .. } => state.mapping = Some(mapping.clone()),
                    Record::Reconcile { keys, contacts, .. } => {
                        state.keys = keys.clone();
                        state.contacts = contacts.clone();
                    }
                    _ => unreachable!(),
                }
                state
            }
            _ => return Ok(()),
        };
        if policy_selected(&self.config, &state.device) {
            let replacement = self
                .devices
                .get(&state.device.source.device)
                .map_or(&state.device.source, |old| &old.device.source);
            self.conditions
                .validate_reconciliation(replacement, &state.keys)
                .map_err(invalid)?;
        }
        // Compile the prospective per-rule state before normalization commits
        // the device/mapping. A failed timing/mapping check keeps the old stream.
        for worker in &self.workers {
            let mut prepared = Worker::new(worker.rule.clone(), &self.config)?;
            prepared.register(&state, &self.config)?;
        }
        Ok(())
    }
    pub(crate) fn prepare_replacement(&self, config: Config, at: Duration) -> io::Result<Self> {
        let generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| invalid("configuration generation exhausted"))?;
        self.prepare_generation(config, generation, at)
    }
    fn prepare_generation(
        &self,
        config: Config,
        generation: u64,
        at: Duration,
    ) -> io::Result<Self> {
        let mut next = if let Some(custom) = &self.custom {
            let mut fence = custom.fence;
            fence.configuration = generation;
            let mut next = Self::with_recognition(config, fence)?;
            next.custom.as_mut().unwrap().gate = custom
                .gate
                .replacement(fence, custom::definitions(&next.config))?;
            if generation == self.generation {
                next.custom.as_mut().unwrap().approved = custom.approved.clone();
            }
            next.recognition_ready(custom.ready(), at)?;
            next
        } else {
            Self::compile(config, None, self.validation)?
        };
        next.push(Record::Header {
            version: crate::replay::FORMAT_VERSION,
        })?;
        for state in self.devices.values() {
            next.push(Record::Device {
                device: state.device.clone(),
                keys: state.keys.clone(),
                contacts: state.contacts.clone(),
            })?;
            if let Some(absolute) = self.replay.absolute_state(&state.device.source) {
                next.push(Record::Absolute {
                    source: state.device.source.clone(),
                    state: absolute.clone(),
                })?;
            }
            if let Some(mapping) = &state.mapping {
                next.push(Record::Mapping {
                    source: state.device.source.clone(),
                    at,
                    mapping: mapping.clone(),
                })?;
            }
        }
        next.replay.inherit_history(&self.replay);
        next.generation = generation;
        next.next_id = self.next_id;
        next.authority = self.authority.clone();
        if self.routing {
            next.enable_routing();
        }
        next.last_at = Some(at);
        Ok(next)
    }
    /// Compile before replacing; reconcile held state rather than inventing
    /// presses. Failed reload leaves the previous engine/configuration intact.
    pub fn reload(&mut self, config: Config, at: Duration) -> io::Result<()> {
        let next = self.prepare_replacement(config, at)?;
        self.commit_replacement(next, at, "configuration replaced");
        Ok(())
    }
    /// Validated runtime edits do not replace the approved source/file fence.
    /// The caller must cancel queued grants/outcomes before activating them.
    pub fn transact_rules(
        &mut self,
        transaction: &crate::rule_transactions::Transaction,
        at: Duration,
    ) -> io::Result<u64> {
        let config = self.prepare_rules(transaction)?;
        if self.custom.is_some()
            && custom::definitions(&config)
                .iter()
                .any(|definition| !self.custom.as_ref().unwrap().approved.contains(definition))
        {
            return Err(invalid(
                "runtime rules cannot add unapproved custom recognizer declarations",
            ));
        }
        let mut next = self.prepare_generation(config, self.generation, at)?;
        next.conditions = self
            .conditions
            .with_layer_activations(&next.config.layers)
            .map_err(invalid)?;
        next.rule_revision = self
            .rule_revision
            .checked_add(1)
            .ok_or_else(|| invalid("runtime rule revision exhausted"))?;
        self.commit_replacement(next, at, "runtime rules changed");
        Ok(self.rule_revision)
    }
    pub(crate) fn prepare_rules(
        &self,
        transaction: &crate::rule_transactions::Transaction,
    ) -> io::Result<Config> {
        transaction
            .prepare_for(&self.config, self.rule_revision, self.validation)
            .map_err(invalid)
    }
    fn commit_replacement(&mut self, mut next: Self, at: Duration, reason: &str) {
        self.cancel_all(at, reason);
        for sequence in &mut self.sequences {
            sequence.cancel_all(at, reason, &mut self.events);
        }
        next.events = std::mem::take(&mut self.events);
        *self = next;
    }
    fn advance(&mut self, at: Duration) -> io::Result<()> {
        if self.last_at.is_some_and(|last| at < last) {
            self.cancel_all(at, "time moved backwards");
            self.conditions.reset();
        }
        self.last_at = Some(at);
        self.authority.tick(at);
        self.conditions.advance(at).map_err(invalid)?;
        for worker in &mut self.workers {
            validate_candidates(
                worker,
                &self.authority,
                &self.devices,
                &self.conditions,
                at,
                &mut self.events,
            );
            let signals = match &mut worker.recognizer {
                Recognizer::Taps(buttons) => buttons
                    .advance(at)
                    .into_iter()
                    .map(Signal::Button)
                    .collect(),
                Recognizer::Keys(keys) => keys
                    .advance(at)
                    .map_err(invalid)?
                    .into_iter()
                    .map(Signal::Key)
                    .collect(),
                Recognizer::Touch(touches) => touches
                    .values_mut()
                    .flat_map(|touch| touch.advance(at))
                    .map(Signal::Touch)
                    .collect(),
                _ => Vec::new(),
            };
            report_signals(worker, signals, self.generation, at, &mut self.events);
            let expired: Vec<_> = worker
                .candidates
                .iter()
                .filter(|(_, c)| c.expires.is_some_and(|deadline| at >= deadline))
                .map(|(source, _)| source.clone())
                .collect();
            for source in expired {
                cancel_candidate(
                    worker,
                    &source,
                    at,
                    "recognition deadline elapsed",
                    &mut self.events,
                );
            }
        }
        self.custom_advance(at);
        Ok(())
    }
    fn frame(&mut self, frame: NormalizedFrame) -> io::Result<()> {
        if self.capture_input
            && policy_selected(&self.config, &self.devices[&frame.raw.source.device].device)
        {
            self.captured_input = Some(frame.clone());
        }
        let source = &frame.raw.source;
        let at = frame.raw.at;
        self.advance(at)?;
        let allowed = policy_selected(&self.config, &self.devices[&source.device].device);
        for change in &frame.changes {
            if !allowed {
                break;
            }
            if let Change::Key { code, state } = *change {
                self.conditions
                    .update(source, code, state, at)
                    .map_err(invalid)?;
            }
        }
        let state = self
            .devices
            .get_mut(&source.device)
            .ok_or_else(|| invalid("unregistered frame source"))?;
        state.input_watermark = self.input_watermark;
        state.last_input = Some((frame.raw.sequence, at));
        for change in &frame.changes {
            if let Change::Key {
                code,
                state: key_state,
            } = *change
            {
                match key_state {
                    KeyState::Press => {
                        state.keys.insert(code);
                    }
                    KeyState::Release => {
                        state.keys.remove(&code);
                    }
                    _ => {}
                }
            }
        }
        state.contacts = frame.contacts.clone();
        let state = &self.devices[&source.device];
        for worker in &mut self.workers {
            let finish = match worker.rule.trigger {
                Trigger::Shape {
                    fingers: None,
                    button,
                    ..
                } => {
                    let button = button.map_or(0x110, |key| key.0);
                    frame.changes.iter().any(|change| matches!(change, Change::Key { code, state: KeyState::Release } if *code == button)).then_some((source, button))
                }
                _ => None,
            };
            validate_candidates_except(
                worker,
                &self.authority,
                &self.devices,
                &self.conditions,
                at,
                &mut self.events,
                finish,
            );
            if !selected(&self.config, &worker.rule, &state.device) {
                continue;
            }
            let state = &self.devices[&source.device];
            if worker.trace {
                if match &worker.rule.trigger {
                    Trigger::ButtonTaps { key, .. } => frame.changes.iter().any(|change| matches!(change, Change::Key { code, state:KeyState::Release } if *code == key.0)),
                    Trigger::TouchTaps { .. } => frame.contacts.is_empty() && !frame.contacts_ended.is_empty(),
                    _ => false,
                } { worker.released.insert(source.clone(), at); }
                if !matches!(worker.rule.trigger, Trigger::Custom { .. })
                    && let Some(candidate) = worker.candidates.get_mut(source)
                    && !frame.contacts.is_empty()
                {
                    candidate.contacts = frame.contacts.iter().map(|contact| contact.id).collect();
                    if candidate.contacts.len() > candidate.peak_contacts {
                        candidate.peak_contacts = candidate.contacts.len();
                        candidate.peak_at = at;
                    }
                }
            }
            match &worker.rule.trigger {
                Trigger::Custom { .. } => {
                    if self.coordinated_step && !self.custom_seats.contains(&source.seat) {
                        continue;
                    }
                    custom::frame(
                        worker,
                        state,
                        &frame,
                        custom::FrameContext {
                            config: &self.config,
                            authority: &self.authority,
                            keys: &self.conditions,
                            custom: self.custom.as_mut().unwrap(),
                            next_id: &mut self.next_id,
                            events: &mut self.events,
                        },
                    );
                }
                Trigger::Shape { .. } => {
                    shape_frame(
                        worker,
                        state,
                        &frame,
                        ShapeContext {
                            config: &self.config,
                            authority: &self.authority,
                            keys: &self.conditions,
                            generation: self.generation,
                            captured: (self.capture_rule.as_deref()
                                == Some(worker.rule.id.as_str()))
                            .then_some(&mut self.captured),
                        },
                        &mut self.next_id,
                        &mut self.events,
                    )?;
                }
                Trigger::Key { key }
                | Trigger::ButtonTaps { key, .. }
                | Trigger::ButtonHold { key, .. } => {
                    let key = key.0;
                    for change in &frame.changes {
                        if let Change::Key {
                            code,
                            state: key_state,
                        } = *change
                            && code == key
                            && key_state != KeyState::Repeat
                        {
                            if key_state == KeyState::Press
                                && !worker.candidates.contains_key(source)
                            {
                                start_candidate(
                                    worker,
                                    state,
                                    &self.authority,
                                    &self.conditions,
                                    (at, &[]),
                                    &mut self.next_id,
                                    &mut self.events,
                                );
                            }
                            if key_state == KeyState::Press
                                && let Some(candidate) = worker.candidates.get_mut(source)
                            {
                                candidate
                                    .source_inputs
                                    .insert(source.clone(), state.input_watermark);
                            }
                            let signals = match &mut worker.recognizer {
                                Recognizer::Key => vec![Signal::Edge {
                                    source: source.clone(),
                                    down: key_state == KeyState::Press,
                                }],
                                Recognizer::Taps(buttons)
                                    if worker.candidates.contains_key(source) =>
                                {
                                    buttons
                                        .update(
                                            Button {
                                                device: source.device,
                                                code,
                                            },
                                            key_state == KeyState::Press,
                                            at,
                                        )
                                        .into_iter()
                                        .map(Signal::Button)
                                        .collect()
                                }
                                Recognizer::Taps(_) => Vec::new(),
                                Recognizer::Keys(keys)
                                    if worker.candidates.contains_key(source) =>
                                {
                                    keys.update(source, code, key_state, at)
                                        .map_err(invalid)?
                                        .into_iter()
                                        .map(Signal::Key)
                                        .collect()
                                }
                                Recognizer::Keys(keys) => {
                                    keys.reconcile(source, state.keys.iter().copied())
                                        .map_err(invalid)?;
                                    Vec::new()
                                }
                                _ => unreachable!(),
                            };
                            report_signals(worker, signals, self.generation, at, &mut self.events);
                            if key_state == KeyState::Release {
                                if matches!(worker.rule.trigger, Trigger::ButtonTaps { .. }) {
                                    if let Some(candidate) = worker.candidates.get_mut(source) {
                                        candidate.expires =
                                            Some(at.saturating_add(
                                                self.config.timing.tap_gap.duration(),
                                            ));
                                    }
                                } else {
                                    worker.candidates.remove(source);
                                }
                            } else if let Some(candidate) = worker.candidates.get_mut(source) {
                                candidate.expires = None;
                            }
                        }
                    }
                }
                Trigger::Chord {
                    keys: chord_keys,
                    timeout,
                    ..
                } => {
                    let timeout = timeout.duration();
                    let chord_keys: Vec<_> = chord_keys.iter().map(|key| key.0).collect();
                    for change in &frame.changes {
                        if let Change::Key {
                            code,
                            state: key_state,
                        } = *change
                            && chord_keys.contains(&code)
                        {
                            if key_state == KeyState::Press
                                && !worker.candidates.keys().any(|key| key.seat == source.seat)
                            {
                                start_candidate(
                                    worker,
                                    state,
                                    &self.authority,
                                    &self.conditions,
                                    (at, &[]),
                                    &mut self.next_id,
                                    &mut self.events,
                                );
                                if let Some(c) = worker.candidates.get_mut(source) {
                                    c.expires = Some(at.saturating_add(timeout));
                                }
                            }
                            if key_state == KeyState::Press {
                                for candidate in worker
                                    .candidates
                                    .values_mut()
                                    .filter(|candidate| candidate.source.seat == source.seat)
                                {
                                    candidate.sources.insert(source.clone());
                                    candidate
                                        .source_inputs
                                        .insert(source.clone(), state.input_watermark);
                                }
                            }
                            let Recognizer::Keys(keys) = &mut worker.recognizer else {
                                unreachable!()
                            };
                            if !worker.candidates.keys().any(|key| key.seat == source.seat) {
                                keys.reconcile(source, state.keys.iter().copied())
                                    .map_err(invalid)?;
                                continue;
                            }
                            let signals = keys
                                .update(source, code, key_state, at)
                                .map_err(invalid)?
                                .into_iter()
                                .map(Signal::Key)
                                .collect();
                            report_signals(worker, signals, self.generation, at, &mut self.events);
                            let Recognizer::Keys(keys) = &worker.recognizer else {
                                unreachable!()
                            };
                            if chord_keys
                                .iter()
                                .all(|code| !keys.down(&source.seat, *code))
                            {
                                worker.candidates.retain(|key, _| key.seat != source.seat);
                            }
                        }
                    }
                }
                Trigger::Wheel {
                    horizontal,
                    minimum,
                } => {
                    let horizontal = *horizontal;
                    let minimum = *minimum;
                    for change in &frame.changes {
                        if let Change::Wheel {
                            horizontal: axis,
                            steps: amount,
                            ..
                        } = *change
                            && axis == horizontal
                        {
                            if !worker.candidates.contains_key(source) {
                                start_candidate(
                                    worker,
                                    state,
                                    &self.authority,
                                    &self.conditions,
                                    (at, &[]),
                                    &mut self.next_id,
                                    &mut self.events,
                                );
                            }
                            let Recognizer::Wheel(sums) = &mut worker.recognizer else {
                                unreachable!()
                            };
                            if !worker.candidates.contains_key(source) {
                                sums.remove(source);
                                continue;
                            }
                            let sum = sums.entry(source.clone()).or_default();
                            *sum += amount;
                            if sum.abs() >= minimum {
                                let amount = *sum;
                                *sum %= minimum;
                                report_signals(
                                    worker,
                                    vec![Signal::Wheel {
                                        source: source.clone(),
                                        horizontal,
                                        amount,
                                    }],
                                    self.generation,
                                    at,
                                    &mut self.events,
                                );
                            }
                            if let Some(candidate) = worker.candidates.get_mut(source) {
                                candidate.expires =
                                    Some(at.saturating_add(self.config.timing.tap_gap.duration()));
                            }
                        }
                    }
                }
                _ => {
                    let new_group =
                        !worker.touch_active.contains(source) && !frame.contacts.is_empty();
                    if new_group {
                        worker.touch_active.insert(source.clone());
                    }
                    let positions = state
                        .mapping
                        .as_ref()
                        .map(|mapping| {
                            frame
                                .contacts
                                .iter()
                                .filter(|c| frame.contacts_started.contains(&c.id))
                                .map(|c| mapping.map(c.x, c.y))
                                .collect::<Result<Vec<_>, _>>()
                        })
                        .transpose()
                        .map_err(invalid)?
                        .unwrap_or_default();
                    if new_group && !worker.candidates.contains_key(source) {
                        start_candidate(
                            worker,
                            state,
                            &self.authority,
                            &self.conditions,
                            (at, &positions),
                            &mut self.next_id,
                            &mut self.events,
                        );
                    } else if let Some(candidate) = worker.candidates.get_mut(source)
                        && let Some(permit) = candidate.permit.as_mut()
                    {
                        for position in positions {
                            if let Err(reason) = self.authority.join(
                                permit,
                                &worker.rule,
                                input(state, at),
                                position,
                            ) {
                                cancel_candidate(
                                    worker,
                                    source,
                                    at,
                                    &reason.to_string(),
                                    &mut self.events,
                                );
                                break;
                            }
                        }
                    }
                    if let Some(candidate) = worker.candidates.get_mut(source) {
                        if new_group {
                            candidate
                                .source_inputs
                                .insert(source.clone(), state.input_watermark);
                        }
                        candidate.expires = None;
                    }
                    let Recognizer::Touch(touches) = &mut worker.recognizer else {
                        unreachable!()
                    };
                    if let Some(touch) = touches.get_mut(&source.device) {
                        let signals = if worker.candidates.contains_key(source) {
                            touch
                                .process(&frame)
                                .into_iter()
                                .map(Signal::Touch)
                                .collect()
                        } else {
                            touch.reconcile(at, !frame.contacts.is_empty());
                            Vec::new()
                        };
                        report_signals(worker, signals, self.generation, at, &mut self.events);
                    }
                    if frame.contacts.is_empty() {
                        worker.touch_active.remove(source);
                        if matches!(worker.rule.trigger, Trigger::TouchTaps { .. }) {
                            if let Some(candidate) = worker.candidates.get_mut(source) {
                                candidate.expires =
                                    Some(at.saturating_add(self.config.timing.tap_gap.duration()));
                            }
                        } else {
                            worker.candidates.remove(source);
                        }
                    }
                }
            }
        }
        let samples: usize = self
            .workers
            .iter()
            .filter_map(|worker| match &worker.recognizer {
                Recognizer::Shape { captures, .. } => Some(
                    captures
                        .values()
                        .filter_map(|capture| capture.path.as_ref())
                        .map(PathCapture::sample_count)
                        .sum::<usize>(),
                ),
                _ => None,
            })
            .sum();
        if samples > 65_536 {
            return Err(invalid("total shape sample budget exceeded"));
        }
        Ok(())
    }
    fn cancel_source(&mut self, source: &Source, at: Duration, reason: &str) {
        if let Some(state) = self.devices.get_mut(&source.device)
            && state.device.source == *source
        {
            state.last_input = None;
        }
        self.conditions.cancel_source(source);
        for sequence in &mut self.sequences {
            if sequence
                .pending
                .get(&source.seat)
                .is_some_and(|pending| pending.sources.contains(source))
            {
                sequence.cancel(&source.seat, at, reason, &mut self.events);
                sequence.reset.insert(source.seat.clone());
            }
        }
        for worker in &mut self.workers {
            let sources: Vec<_> = worker
                .candidates
                .keys()
                .filter(|key| {
                    *key == source
                        || matches!(worker.rule.trigger, Trigger::Chord { .. })
                            && key.seat == source.seat
                })
                .cloned()
                .collect();
            for key in sources {
                cancel_candidate(worker, &key, at, reason, &mut self.events);
            }
            match &mut worker.recognizer {
                Recognizer::Taps(buttons) => buttons.cancel_device(source.device),
                Recognizer::Keys(keys) => {
                    keys.cancel_source(source);
                }
                Recognizer::Touch(touches) => {
                    if let Some(touch) = touches.get_mut(&source.device) {
                        touch.cancel(at, crate::input::CancelReason::SourceChanged);
                    }
                }
                Recognizer::Wheel(sums) => {
                    sums.remove(source);
                }
                Recognizer::Shape { captures, .. } => {
                    captures.remove(source);
                }
                _ => {}
            }
            worker.touch_active.remove(source);
            validate_candidates(
                worker,
                &self.authority,
                &self.devices,
                &self.conditions,
                at,
                &mut self.events,
            );
        }
    }
    fn cancel_all(&mut self, at: Duration, reason: &str) {
        let sources: Vec<_> = self
            .devices
            .values()
            .map(|state| state.device.source.clone())
            .collect();
        for source in sources {
            self.cancel_source(&source, at, reason);
        }
    }
}

struct SequenceContext<'a> {
    devices: &'a BTreeMap<u64, DeviceState>,
    authority: &'a Authority,
    keys: &'a Keys,
    generation: u64,
    at: Duration,
    admitted_at: Duration,
    input_watermark: u64,
    normalized: Option<&'a NormalizedFrame>,
}
struct SequenceGroup {
    first: usize,
    end: usize,
    timeout: Duration,
}
struct PendingSequence {
    id: u64,
    ordinal: u64,
    first: Candidate,
    step: usize,
    finished: Duration,
    evidence: Vec<Evidence>,
    sources: BTreeSet<Source>,
    source_inputs: BTreeMap<Source, u64>,
    groups: BTreeMap<usize, Duration>,
    progress: bool,
    previous: Option<Candidate>,
    after_input: Option<u64>,
}
impl PendingSequence {
    fn route_intent(&self, rule: &Rule) -> RouteIntent {
        let mut candidate = self.first.clone();
        candidate.id = self.id;
        candidate.sources = self.sources.clone();
        candidate.source_inputs = self.source_inputs.clone();
        RouteIntent {
            rule: rule.clone(),
            candidate,
        }
    }
    fn admits_input(&self, candidate: &Candidate) -> bool {
        self.after_input.is_none_or(|watermark| {
            candidate.started_input > watermark
                || candidate.started_input == watermark
                    && self.previous.as_ref().is_some_and(|previous| {
                        candidate.continued_from == Some(previous.id)
                            && candidate.source == previous.source
                    })
        })
    }
}
struct Sequence {
    rule: Rule,
    steps: Vec<Engine>,
    triggers: Vec<Trigger>,
    groups: Vec<SequenceGroup>,
    pending: BTreeMap<String, PendingSequence>,
    reset: BTreeSet<String>,
    routing: bool,
    outcomes: Vec<RouteOutcome>,
    has_custom: bool,
}
fn flatten_sequence(
    trigger: &Trigger,
    leaves: &mut Vec<Trigger>,
    groups: &mut Vec<SequenceGroup>,
) -> io::Result<()> {
    if let Trigger::Sequence { steps, timeout } = trigger {
        let index = groups.len();
        if index >= 64 {
            return Err(invalid("sequence exceeds 64 bounded groups"));
        }
        groups.push(SequenceGroup {
            first: leaves.len(),
            end: 0,
            timeout: timeout.duration(),
        });
        for step in steps {
            flatten_sequence(step, leaves, groups)?;
        }
        groups[index].end = leaves.len();
    } else {
        if leaves.len() >= 64 {
            return Err(invalid("sequence exceeds 64 recognizer steps"));
        }
        leaves.push(trigger.clone());
    }
    Ok(())
}
impl Sequence {
    fn new(rule: Rule, config: &Config, fence: Option<crate::protocol::Fence>) -> io::Result<Self> {
        let mut triggers = Vec::new();
        let mut groups = Vec::new();
        flatten_sequence(&rule.trigger, &mut triggers, &mut groups)?;
        let has_custom = triggers
            .iter()
            .any(|trigger| matches!(trigger, Trigger::Custom { .. }));
        let mut steps = Vec::new();
        for (index, trigger) in triggers.iter().enumerate() {
            let mut child = config.clone();
            let mut leaf = rule.clone();
            leaf.trigger = trigger.clone();
            if rule.trigger.recognizers().next().is_some() && leaf.contexts.is_empty() {
                leaf.contexts.push(crate::config::Context::Desktop);
            }
            if index != 0 {
                leaf.require_keys.clear();
                leaf.layer = None;
                leaf.keys_throughout = false;
            }
            if let Trigger::Shape {
                fingers: None,
                button,
                ..
            } = trigger
                && leaf.require_keys.is_empty()
                && leaf.layer.is_none()
            {
                leaf.require_keys = vec![button.unwrap_or(crate::config::Key(0x110))];
            }
            child.rules = vec![leaf];
            let mut engine = Engine::build(child, fence)?;
            engine.custom = None;
            engine.coordinated_step = true;
            engine.workers[0].trace = true;
            steps.push(engine);
        }
        Ok(Self {
            rule,
            steps,
            triggers,
            groups,
            pending: BTreeMap::new(),
            reset: BTreeSet::new(),
            routing: false,
            outcomes: Vec::new(),
            has_custom,
        })
    }
    fn deadline(&self) -> Option<Duration> {
        self.steps
            .iter()
            .filter_map(Engine::deadline)
            .chain(self.pending.values().flat_map(|pending| {
                pending
                    .groups
                    .iter()
                    .map(|(index, started)| *started + self.groups[*index].timeout)
            }))
            .min()
    }
    fn cancel_all(&mut self, at: Duration, reason: &str, events: &mut Vec<Event>) {
        let seats: Vec<_> = self.pending.keys().cloned().collect();
        for seat in seats {
            self.cancel(&seat, at, reason, events);
        }
    }
    fn cancel(&mut self, seat: &str, at: Duration, reason: &str, events: &mut Vec<Event>) {
        if let Some(pending) = self.pending.remove(seat) {
            events.push(Event::Cancelled {
                version: 1,
                rule: self.rule.id.clone(),
                id: pending.id,
                source: pending.first.source,
                at,
                reason: reason.into(),
            });
            for engine in &mut self.steps {
                for worker in &mut engine.workers {
                    let sources: Vec<_> = worker
                        .candidates
                        .keys()
                        .filter(|source| source.seat == seat)
                        .cloned()
                        .collect();
                    for source in sources {
                        cancel_candidate(worker, &source, at, reason, &mut engine.events);
                    }
                }
            }
        }
    }
    fn reset_seat(&mut self, seat: &str, context: &SequenceContext<'_>) -> io::Result<()> {
        for engine in &mut self.steps {
            let states: Vec<_> = context
                .devices
                .values()
                .filter(|state| state.device.source.seat == seat)
                .cloned()
                .collect();
            for state in states {
                engine.cancel_source(&state.device.source, context.at, "sequence reset");
                if policy_selected(&engine.config, &state.device) {
                    engine
                        .conditions
                        .reconcile(&state.device.source, state.keys.iter().copied())
                        .map_err(invalid)?;
                }
                engine
                    .devices
                    .insert(state.device.source.device, state.clone());
                for worker in &mut engine.workers {
                    worker.register(&state, &engine.config)?;
                    if let Recognizer::Touch(touches) = &mut worker.recognizer
                        && let Some(touch) = touches.get_mut(&state.device.source.device)
                    {
                        touch.reconcile(context.at, !state.contacts.is_empty());
                    }
                    worker.matches.clear();
                }
            }
            engine.events.clear();
        }
        Ok(())
    }
    fn begin(&mut self, candidate: Candidate, next_id: &mut u64) -> io::Result<()> {
        let seat = candidate.source.seat.clone();
        if self.pending.contains_key(&seat) {
            return Ok(());
        }
        *next_id = next_id
            .checked_add(1)
            .ok_or_else(|| invalid("sequence gesture identity exhausted"))?;
        let groups = self
            .groups
            .iter()
            .enumerate()
            .filter(|(_, group)| group.first == 0)
            .map(|(index, _)| (index, candidate.started))
            .collect();
        self.pending.insert(
            seat,
            PendingSequence {
                id: *next_id,
                ordinal: 0,
                finished: candidate.started,
                sources: candidate.sources.clone(),
                source_inputs: candidate.source_inputs.clone(),
                first: candidate,
                step: 0,
                evidence: Vec::new(),
                groups,
                progress: false,
                previous: None,
                after_input: None,
            },
        );
        Ok(())
    }
    fn push(
        &mut self,
        record: &Record,
        context: &SequenceContext<'_>,
        next_id: &mut u64,
        events: &mut Vec<Event>,
        coordinator: &mut Option<custom::State>,
    ) -> io::Result<()> {
        let queued = std::mem::take(&mut self.reset);
        for seat in queued {
            self.reset_seat(&seat, context)?;
        }
        for (index, engine) in self.steps.iter_mut().enumerate() {
            engine.authority = context.authority.clone();
            engine.input_watermark = context.input_watermark;
            let custom = matches!(engine.workers[0].rule.trigger, Trigger::Custom { .. });
            if custom {
                if coordinator.is_none() {
                    return Err(invalid("custom sequence coordinator is unavailable"));
                }
                engine.custom_seats = context
                    .devices
                    .values()
                    .map(|state| &state.device.source.seat)
                    .filter(|seat| {
                        self.pending
                            .get(*seat)
                            .map_or(index == 0, |pending| pending.step == index)
                    })
                    .cloned()
                    .collect();
                engine.next_id = *next_id;
            }
            if custom {
                std::mem::swap(&mut engine.custom, coordinator);
            }
            let result = engine.push(record.clone());
            if custom {
                std::mem::swap(&mut engine.custom, coordinator);
            }
            if custom {
                *next_id = engine.next_id;
            }
            result?;
        }
        self.collect(Some(record), context, next_id, events, coordinator)
    }
    fn collect(
        &mut self,
        record: Option<&Record>,
        context: &SequenceContext<'_>,
        next_id: &mut u64,
        events: &mut Vec<Event>,
        coordinator: &mut Option<custom::State>,
    ) -> io::Result<()> {
        let mut traces = Vec::new();
        let mut cancelled = Vec::new();
        let mut skipped = Vec::new();
        for (index, engine) in self.steps.iter_mut().enumerate() {
            for worker in &mut engine.workers {
                traces.extend(worker.matches.drain(..).map(|trace| (index, trace)));
                worker.claims.clear();
            }
            for event in engine.take_events() {
                match event {
                    Event::Cancelled { source, reason, .. } => {
                        cancelled.push((index, source, reason))
                    }
                    Event::Skipped { ref source, .. }
                        if index == 0 && !self.pending.contains_key(&source.seat) =>
                    {
                        skipped.push(event)
                    }
                    _ => {}
                }
            }
        }
        let starters: Vec<_> = self.steps[0].workers[0]
            .candidates
            .values()
            .cloned()
            .collect();
        for candidate in starters {
            self.begin(candidate, next_id)?;
        }
        for (index, trace) in &traces {
            if *index == 0 {
                self.begin(trace.candidate.clone(), next_id)?;
            }
        }
        let mut changed_targets = BTreeSet::new();
        for (seat, pending) in &mut self.pending {
            if let Some(candidate) = self.steps[pending.step].workers[0]
                .candidates
                .values()
                .find(|candidate| {
                    candidate.source.seat == *seat
                        && pending.admits_input(candidate)
                        && (pending.step == 0
                            || candidate.started >= pending.finished
                            || pending.previous.as_ref().is_some_and(|previous| {
                                same_contact_continuation(
                                    &self.triggers[pending.step - 1],
                                    &self.triggers[pending.step],
                                    previous,
                                    candidate,
                                )
                            }))
                })
            {
                if let (Some(first), Some(later)) = (&pending.first.permit, &candidate.permit)
                    && !same_target(first, later)
                {
                    changed_targets.insert(seat.clone());
                    continue;
                }
                pending.sources.extend(candidate.sources.iter().cloned());
                pending.source_inputs.extend(
                    candidate
                        .source_inputs
                        .iter()
                        .map(|(source, input)| (source.clone(), *input)),
                );
                for (index, _) in self
                    .groups
                    .iter()
                    .enumerate()
                    .filter(|(_, group)| group.first == pending.step)
                {
                    pending.groups.entry(index).or_insert(candidate.started);
                }
            }
        }
        let invalidated: Vec<_> = self
            .pending
            .iter()
            .filter_map(|(seat, pending)| {
                let changed = matches!(record, Some(Record::Mapping {source,..}|Record::Reconcile {source,..}) if pending.sources.contains(source));
                let reason = if changed_targets.contains(seat) {Some("sequence target changed".into())}
                else if changed {Some("sequence input mapping/state changed".into())}
                else if pending.first.session_generation != context.authority.generation() {
                    Some("session generation changed".into())
                } else if self.rule.keys_throughout && !conditions(&self.rule, context.keys, seat) {
                    Some("activation condition released".into())
                } else if pending
                    .groups
                    .iter()
                    .any(|(index, started)| context.at >= *started + self.groups[*index].timeout)
                {
                    Some("sequence timeout".into())
                } else if pending.sources.iter().any(|source| {
                    context
                        .devices
                        .get(&source.device)
                        .is_none_or(|state| state.device.source != *source)
                }) {
                    Some("sequence source changed".into())
                } else if let Some(state) = context.devices.get(&pending.first.source.device) {
                    pending.first.permit.as_ref().and_then(|permit| {
                        context
                            .authority
                            .validate(permit, input(state, context.at))
                            .err()
                            .map(|reason| reason.to_string())
                    })
                } else {
                    Some("sequence source changed".into())
                };
                reason.map(|reason| (seat.clone(), reason))
            })
            .collect();
        let mut reset = BTreeSet::new();
        for (seat, reason) in invalidated {
            self.cancel(&seat, context.at, &reason, events);
            reset.insert(seat);
        }
        // Group entry clocks are anchored to candidate input, not delayed match
        // notifications. Future recognizers can never lend an old press/path.
        traces.sort_by(|(ai, a), (bi, b)| ai.cmp(bi).then_with(|| a.finished.cmp(&b.finished)));
        let mut activated = BTreeMap::new();
        for (index, trace) in traces {
            let seat = trace.candidate.source.seat.clone();
            if reset.contains(&seat) {
                continue;
            }
            let Some(pending) = self.pending.get_mut(&seat) else {
                continue;
            };
            if pending.step != index {
                continue;
            }
            if !pending.admits_input(&trace.candidate) {
                continue;
            }
            let Event::Matched {
                stage,
                evidence,
                at,
                ..
            } = &trace.event
            else {
                continue;
            };
            if matches!(stage, Stage::Update | Stage::End)
                && !pending.progress
                && index + 1 == self.steps.len()
            {
                continue;
            }
            let final_step = index + 1 == self.steps.len();
            if !final_step
                && !matches!(stage, Stage::Discrete | Stage::End)
                && !matches!(self.triggers[index], Trigger::Key { .. })
            {
                continue;
            }
            if matches!(evidence, Evidence::Hold { repeat: true }) {
                continue;
            }
            let continuation = index > 0
                && pending.previous.as_ref().is_some_and(|previous| {
                    same_contact_continuation(
                        &self.triggers[index - 1],
                        &self.triggers[index],
                        previous,
                        &trace.candidate,
                    )
                });
            if index > 0 && trace.candidate.started < pending.finished && !continuation {
                continue;
            }
            if let (Some(first), Some(later)) = (&pending.first.permit, &trace.candidate.permit)
                && !same_target(first, later)
            {
                self.cancel(&seat, context.at, "sequence target changed", events);
                reset.insert(seat);
                continue;
            }
            for (group_index, _) in self
                .groups
                .iter()
                .enumerate()
                .filter(|(_, group)| group.first == index)
            {
                pending
                    .groups
                    .entry(group_index)
                    .or_insert(trace.candidate.started);
            }
            if pending
                .groups
                .iter()
                .any(|(index, started)| *at >= *started + self.groups[*index].timeout)
            {
                self.cancel(&seat, context.at, "sequence timeout", events);
                reset.insert(seat);
                continue;
            }
            pending.sources.extend(trace.candidate.sources.clone());
            pending
                .source_inputs
                .extend(trace.candidate.source_inputs.clone());
            if final_step {
                pending.ordinal = pending
                    .ordinal
                    .checked_add(1)
                    .ok_or_else(|| invalid("sequence event ordinal exhausted"))?;
                let mut steps = pending.evidence.clone();
                steps.push(evidence.clone());
                let source = pending.first.source.clone();
                let permit = pending.first.permit.as_ref();
                events.push(Event::Matched {
                    version: 1,
                    rule: self.rule.id.clone(),
                    id: pending.id,
                    ordinal: pending.ordinal,
                    source,
                    at: *at,
                    config_generation: context.generation,
                    session_generation: pending.first.session_generation,
                    stage: if matches!(self.triggers[index], Trigger::Key { .. }) {
                        Stage::Discrete
                    } else {
                        *stage
                    },
                    action: self.rule.action.clone(),
                    evidence: Evidence::Sequence { steps },
                    window: permit
                        .and_then(Permit::window)
                        .map(|window| window.id.clone()),
                    output: permit
                        .and_then(Permit::output)
                        .map(|output| output.geometry.name.clone()),
                    workspace: permit
                        .and_then(Permit::output)
                        .and_then(|output| output.active_workspace),
                });
                if self.routing {
                    let mut candidate = pending.first.clone();
                    candidate.id = pending.id;
                    candidate.sources = pending.sources.clone();
                    candidate.source_inputs = pending.source_inputs.clone();
                    self.outcomes.push(RouteOutcome {
                        expires: pending
                            .groups
                            .iter()
                            .map(|(index, started)| *started + self.groups[*index].timeout)
                            .chain(trace.expires)
                            .min(),
                        event: events.last().unwrap().clone(),
                        intent: RouteIntent {
                            rule: self.rule.clone(),
                            candidate,
                        },
                    });
                }
                if *stage == Stage::Begin && !matches!(self.triggers[index], Trigger::Key { .. }) {
                    pending.progress = true;
                } else if matches!(stage, Stage::Discrete | Stage::End)
                    || matches!(self.triggers[index], Trigger::Key { .. })
                {
                    self.pending.remove(&seat);
                    reset.insert(seat);
                }
            } else {
                pending.evidence.push(evidence.clone());
                pending.finished = trace.finished;
                pending.previous = Some(trace.candidate.clone());
                pending.after_input = trace.after_input.or(pending.after_input);
                pending.step += 1;
                if matches!(self.triggers[pending.step], Trigger::Custom { .. })
                    || matches!(&self.triggers[index], Trigger::Custom { recognizer, .. }
                        if recognizer.inputs.contains(&crate::recognition::InputKind::Touch))
                {
                    activated.insert(seat.clone(), (pending.step, trace.after_input.is_none()));
                }
                let completed_groups: Vec<_> = pending
                    .groups
                    .keys()
                    .filter(|index| self.groups[**index].end == pending.step)
                    .copied()
                    .collect();
                // The outer group remains active until the entire sequence ends.
                for group in completed_groups {
                    if self.groups[group].end < self.steps.len() {
                        pending.groups.remove(&group);
                    }
                }
            }
        }
        for (index, source, reason) in cancelled {
            if self
                .pending
                .get(&source.seat)
                .is_some_and(|pending| pending.step == index && pending.sources.contains(&source))
            {
                self.cancel(&source.seat, context.at, &reason, events);
                reset.insert(source.seat);
            }
        }
        for (seat, (step, same_record)) in activated {
            if reset.contains(&seat)
                || self
                    .pending
                    .get(&seat)
                    .is_none_or(|pending| pending.step != step)
            {
                continue;
            }
            let reason = if matches!(self.triggers[step], Trigger::Custom { .. }) {
                self.continue_custom(&seat, step, context, next_id, coordinator)?
            } else {
                self.continue_motion(&seat, step, context, coordinator)?
            };
            let reason = if reason.is_none()
                && same_record
                && let Some(frame) = context.normalized
                && frame.raw.source.seat == seat
                && matches!(self.triggers[step], Trigger::Custom { .. })
            {
                self.project_custom(step, frame, context, next_id, coordinator)?
            } else {
                reason
            };
            if let Some(reason) = reason {
                self.cancel(&seat, context.at, reason, events);
                reset.insert(seat);
            }
        }
        for seat in reset {
            self.reset_seat(&seat, context)?;
        }
        events.extend(skipped);
        Ok(())
    }

    fn continue_custom(
        &mut self,
        seat: &str,
        step: usize,
        context: &SequenceContext<'_>,
        next_id: &mut u64,
        coordinator: &mut Option<custom::State>,
    ) -> io::Result<Option<&'static str>> {
        let previous_touch = matches!(self.triggers[step - 1], Trigger::TouchHold { .. })
            || matches!(&self.triggers[step - 1], Trigger::Custom { recognizer, .. }
                if recognizer.inputs.contains(&crate::recognition::InputKind::Touch));
        if !previous_touch
            || !matches!(&self.triggers[step], Trigger::Custom { recognizer, .. }
                if recognizer.inputs.contains(&crate::recognition::InputKind::Touch))
        {
            return Ok(None);
        }
        let pending = &self.pending[seat];
        let previous = pending.previous.as_ref().unwrap();
        if previous.contacts.is_empty() {
            return Ok(None);
        }
        let Some(state) = context
            .devices
            .get(&previous.source.device)
            .filter(|state| state.device.source == previous.source)
        else {
            return Ok(Some("touch continuation source changed"));
        };
        if state.contacts.is_empty() && matches!(self.triggers[step - 1], Trigger::Custom { .. }) {
            return Ok(None);
        }
        let engine = &mut self.steps[step];
        let worker = &mut engine.workers[0];
        if !selected(&engine.config, &worker.rule, &state.device) {
            return Ok(None);
        }
        let custom = coordinator
            .as_mut()
            .ok_or_else(|| invalid("custom sequence coordinator is unavailable"))?;
        if let Err(reason) = custom::continue_touch(
            worker,
            state,
            previous,
            context.admitted_at,
            context.input_watermark,
            custom::FrameContext {
                config: &engine.config,
                authority: context.authority,
                keys: context.keys,
                custom,
                next_id,
                events: &mut engine.events,
            },
        ) {
            return Ok(Some(reason));
        }
        let pending = self.pending.get_mut(seat).unwrap();
        for (index, _) in self
            .groups
            .iter()
            .enumerate()
            .filter(|(_, group)| group.first == step)
        {
            pending.groups.entry(index).or_insert(context.admitted_at);
        }
        Ok(None)
    }

    fn continue_motion(
        &mut self,
        seat: &str,
        step: usize,
        context: &SequenceContext<'_>,
        coordinator: &Option<custom::State>,
    ) -> io::Result<Option<&'static str>> {
        if !matches!(
            self.triggers[step],
            Trigger::Swipe { .. }
                | Trigger::Pinch { .. }
                | Trigger::Rotate { .. }
                | Trigger::Shape {
                    fingers: Some(_),
                    ..
                }
        ) {
            return Ok(None);
        }
        let previous = self.pending[seat].previous.as_ref().unwrap();
        let Some(state) = context
            .devices
            .get(&previous.source.device)
            .filter(|state| state.device.source == previous.source)
        else {
            return Ok(Some("touch continuation source changed"));
        };
        let engine = &mut self.steps[step];
        let worker = &mut engine.workers[0];
        if previous.contacts.is_empty()
            || state.contacts.is_empty()
            || state.contacts.len() != usize::from(worker.rule.trigger.fingers().unwrap())
            || !selected(&engine.config, &worker.rule, &state.device)
        {
            return Ok(None);
        }
        let custom = coordinator
            .as_ref()
            .ok_or_else(|| invalid("custom sequence coordinator is unavailable"))?;
        if !custom.ready() || context.authority.generation() != custom.fence.session {
            return Ok(Some("touch continuation is no longer eligible"));
        }
        if let Err(reason) = continuation::motion(
            worker,
            state,
            previous,
            context,
            &engine.config,
            &mut engine.next_id,
        ) {
            return Ok(Some(reason));
        }
        let pending = self.pending.get_mut(seat).unwrap();
        for (index, _) in self
            .groups
            .iter()
            .enumerate()
            .filter(|(_, group)| group.first == step)
        {
            pending.groups.entry(index).or_insert(context.admitted_at);
        }
        Ok(None)
    }

    fn project_custom(
        &mut self,
        step: usize,
        frame: &NormalizedFrame,
        context: &SequenceContext<'_>,
        next_id: &mut u64,
        coordinator: &mut Option<custom::State>,
    ) -> io::Result<Option<&'static str>> {
        let source = &frame.raw.source;
        let pending = &self.pending[&source.seat];
        if pending
            .after_input
            .is_some_and(|watermark| context.input_watermark <= watermark)
        {
            return Ok(None);
        }
        let engine = &mut self.steps[step];
        let Some(state) = engine
            .devices
            .get(&source.device)
            .filter(|state| state.device.source == *source)
        else {
            return Ok(None);
        };
        let worker = &mut engine.workers[0];
        if worker.candidates.contains_key(source)
            || !selected(&engine.config, &worker.rule, &state.device)
        {
            return Ok(None);
        }
        engine.next_id = *next_id;
        custom::frame(
            worker,
            state,
            frame,
            custom::FrameContext {
                config: &engine.config,
                authority: context.authority,
                keys: &engine.conditions,
                custom: coordinator
                    .as_mut()
                    .ok_or_else(|| invalid("custom sequence coordinator is unavailable"))?,
                next_id: &mut engine.next_id,
                events: &mut engine.events,
            },
        );
        *next_id = engine.next_id;
        let Some(candidate) = worker.candidates.get(source) else {
            return Ok(None);
        };
        let same = match (&pending.first.permit, &candidate.permit) {
            (Some(first), Some(later)) => same_target(first, later),
            (None, None) => true,
            _ => false,
        };
        if !same {
            return Ok(Some("sequence target changed"));
        }
        let pending = self.pending.get_mut(&source.seat).unwrap();
        for source in &candidate.sources {
            if !pending.sources.contains(source) {
                pending.sources.insert(source.clone());
            }
        }
        pending
            .source_inputs
            .extend(candidate.source_inputs.clone());
        for (index, _) in self
            .groups
            .iter()
            .enumerate()
            .filter(|(_, group)| group.first == step)
        {
            pending.groups.entry(index).or_insert(candidate.started);
        }
        Ok(None)
    }
}
fn same_target(first: &Permit, later: &Permit) -> bool {
    first.same_target(later)
}
fn same_contact_continuation(
    previous: &Trigger,
    current: &Trigger,
    first: &Candidate,
    later: &Candidate,
) -> bool {
    matches!(previous, Trigger::TouchHold { .. })
        && matches!(
            current,
            Trigger::Swipe { .. }
                | Trigger::Shape {
                    fingers: Some(_),
                    ..
                }
                | Trigger::Pinch { .. }
                | Trigger::Rotate { .. }
        )
        && first.source == later.source
        && !first.contacts.is_empty()
        && first.contacts == later.contacts
}

fn selected(config: &Config, rule: &Rule, device: &Device) -> bool {
    !device.virtual_device
        && config.policy.allowed_seats.contains(&device.source.seat)
        && (rule.seats.is_empty() || rule.seats.contains(&device.source.seat))
        && config.devices.iter().any(|(id, selector)| {
            config.policy.allowed_devices.contains(id)
                && (rule.devices.is_empty() || rule.devices.contains(id))
                && !matches!(&rule.trigger, Trigger::Custom { recognizer, .. } if !recognizer.selectors.contains(id))
                && selector.matches(device)
        })
}

struct ShapeContext<'a> {
    config: &'a Config,
    authority: &'a Authority,
    keys: &'a Keys,
    generation: u64,
    captured: Option<&'a mut Vec<Stroke>>,
}
fn shape_frame(
    worker: &mut Worker,
    state: &DeviceState,
    frame: &NormalizedFrame,
    mut context: ShapeContext<'_>,
    next_id: &mut u64,
    events: &mut Vec<Event>,
) -> io::Result<()> {
    let Trigger::Shape {
        fingers, button, ..
    } = worker.rule.trigger
    else {
        unreachable!()
    };
    let source = &frame.raw.source;
    let at = frame.raw.at;
    let button = button.map_or(0x110, |key| key.0);
    let mouse = fingers.is_none();
    if mouse
        && !(state.device.has(crate::input::EV_REL, 0)
            && state.device.has(crate::input::EV_REL, 1)
            && state.device.has(crate::input::EV_KEY, button))
    {
        return Ok(());
    }
    if !mouse && state.mapping.is_none() {
        return Ok(());
    }
    let released = mouse && frame.changes.iter().any(|change| matches!(change, Change::Key { code, state: KeyState::Release } if *code == button));
    let neutral = if mouse {
        !state.keys.contains(&button)
    } else {
        frame.contacts.is_empty()
    };
    let fresh_begin = if mouse {
        frame.changes.iter().any(|change| matches!(change, Change::Key { code, state: KeyState::Press } if *code == button))
    } else {
        !frame.contacts.is_empty() && frame.contacts.iter().all(|contact| contact.fresh)
    };
    let new_group = !worker.touch_active.contains(source) && fresh_begin;
    if new_group {
        worker.touch_active.insert(source.clone());
        let positions = if let Some(mapping) = &state.mapping {
            frame
                .contacts
                .iter()
                .map(|contact| mapping.map(contact.x, contact.y))
                .collect::<Result<Vec<_>, _>>()
                .map_err(invalid)?
        } else {
            Vec::new()
        };
        start_candidate(
            worker,
            state,
            context.authority,
            context.keys,
            (at, &positions),
            next_id,
            events,
        );
        if worker.candidates.contains_key(source) {
            let identity = CaptureIdentity {
                source: source.clone(),
                mapping_generation: state
                    .mapping
                    .as_ref()
                    .map_or(0, |mapping| mapping.generation),
                contacts: BTreeSet::new(),
            };
            let path = if mouse {
                let path = PathCapture::new_relative(
                    identity.clone(),
                    at,
                    context
                        .config
                        .shape_capture
                        .mouse_limits(worker.rule.keys_throughout),
                    true,
                )
                .map_err(invalid)?;
                Some(path)
            } else {
                None
            };
            let Recognizer::Shape { captures, .. } = &mut worker.recognizer else {
                unreachable!()
            };
            captures.insert(
                source.clone(),
                ShapeCapture {
                    started: at,
                    path,
                    identity,
                },
            );
            worker.candidates.get_mut(source).unwrap().expires = Some(
                at + if mouse {
                    context.config.shape_capture.timeout.duration()
                } else {
                    context
                        .config
                        .timing
                        .join_window
                        .duration()
                        .min(context.config.shape_capture.timeout.duration())
                },
            );
        }
    }
    // Later fingers must still start in the frozen target. This happens before
    // matching or changing any capture identity.
    if !new_group
        && let Some(candidate) = worker.candidates.get_mut(source)
        && let Some(permit) = candidate.permit.as_mut()
    {
        for contact in frame
            .contacts
            .iter()
            .filter(|contact| frame.contacts_started.contains(&contact.id))
        {
            let position = state
                .mapping
                .as_ref()
                .unwrap()
                .map(contact.x, contact.y)
                .map_err(invalid)?;
            if let Err(reason) =
                context
                    .authority
                    .join(permit, &worker.rule, input(state, at), position)
            {
                cancel_candidate(worker, source, at, &reason.to_string(), events);
                break;
            }
        }
    }
    let Recognizer::Shape { captures, matcher } = &mut worker.recognizer else {
        unreachable!()
    };
    if let Some(mut capture) = captures.remove(source) {
        let armed = !worker.rule.keys_throughout
            || conditions_except(
                &worker.rule,
                context.keys,
                &source.seat,
                released.then_some(button),
            );
        let result = if mouse {
            let mut delta = crate::gestures::Point::default();
            for change in &frame.changes {
                if let Change::Relative { code, value } = change {
                    if *code == 0 {
                        delta.x += f64::from(*value);
                    }
                    if *code == 1 {
                        delta.y += f64::from(*value);
                    }
                }
            }
            capture
                .path
                .as_mut()
                .unwrap()
                .sample_relative(&capture.identity, at, delta, armed)
        } else {
            let expected = usize::from(fingers.unwrap());
            let positions = frame
                .contacts
                .iter()
                .chain(frame.ended_contacts.iter())
                .map(|contact| {
                    state
                        .mapping
                        .as_ref()
                        .unwrap()
                        .map(contact.x, contact.y)
                        .map(|position| (contact.id, position))
                })
                .collect::<Result<BTreeMap<_, _>, _>>()
                .map_err(invalid)?;
            if capture.path.is_none()
                && frame.contacts.len() == expected
                && frame.contacts_ended.is_empty()
            {
                capture.identity.contacts = positions.keys().copied().collect();
                capture.path = Some(
                    PathCapture::new(
                        capture.identity.clone(),
                        capture.started,
                        context
                            .config
                            .shape_capture
                            .touch_limits(worker.rule.keys_throughout),
                        true,
                    )
                    .map_err(invalid)?,
                );
                worker.candidates.get_mut(source).unwrap().expires =
                    Some(capture.started + context.config.shape_capture.timeout.duration());
            }
            if !frame.contacts_ended.is_empty() && !neutral || frame.contacts.len() > expected {
                Err(crate::paths::CaptureError::ContactsChanged)
            } else if let Some(path) = capture.path.as_mut() {
                path.sample_contacts(
                    source,
                    capture.identity.mapping_generation,
                    at,
                    &positions,
                    armed,
                )
            } else if neutral {
                Err(crate::paths::CaptureError::ContactsChanged)
            } else {
                Ok(())
            }
        };
        let completed = released || !mouse && neutral;
        if let Err(reason) = result {
            cancel_candidate(worker, source, at, &reason.to_string(), events);
        } else if completed {
            match capture
                .path
                .take()
                .ok_or(crate::paths::CaptureError::TooSmall)
                .and_then(|path| path.finish(at))
            {
                Ok(stroke) => {
                    if let Some(captured) = context.captured.as_mut() {
                        if captured.len() >= 16
                            || captured
                                .iter()
                                .map(|stroke| stroke.points().len())
                                .sum::<usize>()
                                + stroke.points().len()
                                > 65_536
                        {
                            return Err(invalid(
                                "diagnostic stroke queue is full; drain before continuing",
                            ));
                        }
                        captured.push(stroke.clone());
                    }
                    match matcher.classify(&stroke) {
                        MatchOutcome::Matched { shape } if matches!(&worker.rule.trigger, Trigger::Shape { name, .. } if *name == shape.name) => {
                            report_signals(
                                worker,
                                vec![Signal::Shape {
                                    source: source.clone(),
                                    space: stroke.space,
                                    fingers,
                                    shape,
                                }],
                                context.generation,
                                at,
                                events,
                            )
                        }
                        MatchOutcome::Matched { .. } => cancel_candidate(
                            worker,
                            source,
                            at,
                            "path matched a different template",
                            events,
                        ),
                        MatchOutcome::Ambiguous { .. } => {
                            cancel_candidate(worker, source, at, "ambiguous shape", events)
                        }
                        MatchOutcome::Rejected { .. } => cancel_candidate(
                            worker,
                            source,
                            at,
                            "shape confidence below threshold",
                            events,
                        ),
                    }
                }
                Err(reason) => cancel_candidate(worker, source, at, &reason.to_string(), events),
            }
            worker.candidates.remove(source);
        } else {
            let Recognizer::Shape { captures, .. } = &mut worker.recognizer else {
                unreachable!()
            };
            captures.insert(source.clone(), capture);
        }
    }
    if neutral {
        worker.touch_active.remove(source);
    }
    Ok(())
}
fn policy_selected(config: &Config, device: &Device) -> bool {
    !device.virtual_device
        && config.policy.allowed_seats.contains(&device.source.seat)
        && config.devices.iter().any(|(id, selector)| {
            config.policy.allowed_devices.contains(id) && selector.matches(device)
        })
}
fn conditions(rule: &Rule, keys: &Keys, seat: &str) -> bool {
    conditions_except(rule, keys, seat, None)
}
fn conditions_except(rule: &Rule, keys: &Keys, seat: &str, completed_button: Option<u16>) -> bool {
    rule.require_keys
        .iter()
        .all(|key| Some(key.0) == completed_button || keys.down(seat, key.0))
        && rule
            .layer
            .as_ref()
            .is_none_or(|layer| keys.layer_active(seat, layer))
}
pub fn requires_context(rule: &Rule) -> bool {
    !context_free(rule)
}
fn context_free(rule: &Rule) -> bool {
    rule.trigger.recognizers().next().is_none()
        && rule.contexts.is_empty()
        && rule.scope == Scope::Everywhere
        && rule.screen.is_none()
        && rule.keyboard_visible.is_none()
        && matches!(rule.action, Action::Event { .. })
}
fn input(state: &DeviceState, at: Duration) -> InputContext<'_> {
    InputContext {
        source: &state.device.source,
        mapping: state.mapping.as_ref(),
        at,
    }
}
fn start_candidate(
    worker: &mut Worker,
    state: &DeviceState,
    authority: &Authority,
    keys: &Keys,
    start: (Duration, &[crate::gestures::Position]),
    next_id: &mut u64,
    events: &mut Vec<Event>,
) {
    let (at, positions) = start;
    let source = &state.device.source;
    let permit = if !conditions(&worker.rule, keys, &source.seat) {
        Err("activation condition not met".into())
    } else if context_free(&worker.rule) && !worker.routing {
        Ok(None)
    } else {
        authority
            .start(&worker.rule, input(state, at), positions)
            .map(Some)
            .map_err(|reason| reason.to_string())
    };
    match permit {
        Ok(permit) => {
            let Some(id) = next_id.checked_add(1) else {
                return;
            };
            *next_id = id;
            worker.candidates.insert(
                source.clone(),
                Candidate {
                    id,
                    ordinal: 0,
                    source: source.clone(),
                    session_generation: authority.generation(),
                    permit,
                    progress: false,
                    expires: None,
                    started: at,
                    started_input: state.input_watermark,
                    continued_from: None,
                    contacts: state.contacts.iter().map(|contact| contact.id).collect(),
                    sources: BTreeSet::from([source.clone()]),
                    source_inputs: BTreeMap::from([(source.clone(), state.input_watermark)]),
                    peak_contacts: state.contacts.len(),
                    peak_at: at,
                },
            );
        }
        Err(reason) => {
            events.push(Event::Skipped {
                version: 1,
                rule: worker.rule.id.clone(),
                source: source.clone(),
                at,
                reason,
            });
            // Flush rejected tap history; a later eligible group cannot inherit it.
            if let Recognizer::Touch(touches) = &mut worker.recognizer
                && let Some(touch) = touches.get_mut(&source.device)
            {
                touch.cancel(at, crate::input::CancelReason::SourceChanged);
            }
            if let Recognizer::Taps(buttons) = &mut worker.recognizer {
                buttons.cancel_device(source.device);
            }
        }
    }
}
fn validate_candidates(
    worker: &mut Worker,
    authority: &Authority,
    devices: &BTreeMap<u64, DeviceState>,
    keys: &Keys,
    at: Duration,
    events: &mut Vec<Event>,
) {
    validate_candidates_except(worker, authority, devices, keys, at, events, None)
}
fn validate_candidates_except(
    worker: &mut Worker,
    authority: &Authority,
    devices: &BTreeMap<u64, DeviceState>,
    keys: &Keys,
    at: Duration,
    events: &mut Vec<Event>,
    completed_button: Option<(&Source, u16)>,
) {
    let invalids: Vec<_> = worker
        .candidates
        .iter()
        .filter_map(|(source, candidate)| {
            let reason = if worker.rule.keys_throughout
                && !conditions_except(
                    &worker.rule,
                    keys,
                    &source.seat,
                    completed_button
                        .filter(|(completed, _)| *completed == source)
                        .map(|(_, button)| button),
                ) {
                Some("activation condition released".into())
            } else if candidate.session_generation != authority.generation() {
                Some(Rejection::StaleGeneration.to_string())
            } else if let Some(state) = devices.get(&source.device) {
                if state.device.source != *source {
                    Some(Rejection::SourceChanged.to_string())
                } else {
                    candidate.permit.as_ref().and_then(|permit| {
                        authority
                            .validate(permit, input(state, at))
                            .err()
                            .map(|r| r.to_string())
                    })
                }
            } else {
                Some(Rejection::SourceChanged.to_string())
            };
            reason.map(|reason| (source.clone(), reason))
        })
        .collect();
    for (source, reason) in invalids {
        cancel_candidate(worker, &source, at, &reason, events);
    }
}
fn cancel_candidate(
    worker: &mut Worker,
    source: &Source,
    at: Duration,
    reason: &str,
    events: &mut Vec<Event>,
) {
    if let Some(candidate) = worker.candidates.remove(source) {
        events.push(Event::Cancelled {
            version: 1,
            rule: worker.rule.id.clone(),
            id: candidate.id,
            source: candidate.source,
            at,
            reason: reason.into(),
        });
    }
    match &mut worker.recognizer {
        Recognizer::Custom {
            blocked,
            wakes,
            closed,
        } => {
            blocked.insert(source.clone());
            wakes.remove(source);
            closed.remove(source);
        }
        Recognizer::Taps(buttons) => buttons.cancel_device(source.device),
        Recognizer::Keys(keys) => {
            keys.cancel_source(source);
        }
        Recognizer::Touch(touches) => {
            if let Some(touch) = touches.get_mut(&source.device) {
                touch.cancel(at, crate::input::CancelReason::SourceChanged);
            }
        }
        Recognizer::Wheel(sums) => {
            sums.remove(source);
        }
        Recognizer::Shape { captures, .. } => {
            captures.remove(source);
        }
        _ => {}
    }
}
enum Signal {
    Custom {
        source: Source,
        stage: Stage,
        data: serde_json::Value,
    },
    Shape {
        source: Source,
        space: Space,
        fingers: Option<u16>,
        shape: ShapeMatch,
    },
    Edge {
        source: Source,
        down: bool,
    },
    Button(Gesture),
    Key(KeyObservation),
    Touch(TouchEvent),
    Wheel {
        source: Source,
        horizontal: bool,
        amount: f64,
    },
}
fn direction_matches(wanted: Direction, actual: Direction) -> bool {
    wanted == Direction::Any
        || wanted == actual
        || wanted == Direction::Horizontal && matches!(actual, Direction::Left | Direction::Right)
        || wanted == Direction::Vertical && matches!(actual, Direction::Up | Direction::Down)
}
fn report_signals(
    worker: &mut Worker,
    signals: Vec<Signal>,
    generation: u64,
    at: Duration,
    events: &mut Vec<Event>,
) {
    for signal in signals {
        if worker.routing
            && let (
                Trigger::Swipe {
                    fingers: wanted,
                    direction: wanted_direction,
                    ..
                },
                Signal::Touch(event),
            ) = (&worker.rule.trigger, &signal)
            && let TouchGesture::Pan {
                fingers,
                direction,
                phase: Phase::Begin | Phase::Update,
                ..
            } = &event.gesture
            && wanted == fingers
            && direction_matches(*wanted_direction, *direction)
            && let Some(candidate) = worker.candidates.get(&event.source)
        {
            worker.claims.push(candidate.clone());
        }
        let proposal = match (&worker.rule.trigger, signal) {
            (
                Trigger::Custom { recognizer, .. },
                Signal::Custom {
                    source,
                    stage,
                    data,
                },
            ) => Some((
                source,
                stage,
                Evidence::Custom {
                    recognizer: recognizer.name.clone(),
                    data,
                },
            )),
            (
                Trigger::Shape { name, .. },
                Signal::Shape {
                    source,
                    space,
                    fingers,
                    shape,
                },
            ) if *name == shape.name => Some((
                source,
                Stage::Discrete,
                Evidence::Shape {
                    space,
                    fingers,
                    shape,
                },
            )),
            (Trigger::Key { .. }, Signal::Edge { source, down }) => Some((
                source,
                if down { Stage::Begin } else { Stage::End },
                Evidence::Key { down },
            )),
            (
                Trigger::ButtonTaps { count: wanted, .. },
                Signal::Button(Gesture::Taps { button, count }),
            ) if *wanted == count => worker
                .candidates
                .keys()
                .find(|source| source.device == button.device)
                .cloned()
                .map(|source| (source, Stage::Discrete, Evidence::Taps { count })),
            (Trigger::ButtonHold { .. }, Signal::Key(KeyObservation::Hold { key, repeat, .. })) => {
                Some((key.source, Stage::Discrete, Evidence::Hold { repeat }))
            }
            (Trigger::Chord { .. }, Signal::Key(KeyObservation::Chord { seat, .. })) => worker
                .candidates
                .keys()
                .find(|source| source.seat == seat)
                .cloned()
                .map(|source| (source, Stage::Discrete, Evidence::Chord)),
            (
                Trigger::Wheel { .. },
                Signal::Wheel {
                    source,
                    horizontal,
                    amount,
                },
            ) => Some((
                source,
                Stage::Discrete,
                Evidence::Wheel { horizontal, amount },
            )),
            (trigger, Signal::Touch(event)) => {
                let stage = match (trigger, &event.gesture) {
                    (
                        Trigger::TouchTaps {
                            fingers: f,
                            count: c,
                        },
                        TouchGesture::Taps { fingers, count, .. },
                    ) if f == fingers && c == count => Some(Stage::Discrete),
                    (Trigger::TouchHold { fingers: f, .. }, TouchGesture::Hold { fingers, .. })
                        if f == fingers =>
                    {
                        Some(Stage::Discrete)
                    }
                    (
                        Trigger::Swipe {
                            fingers: f,
                            direction: wanted,
                            continuous,
                            ..
                        },
                        TouchGesture::Pan {
                            fingers,
                            phase,
                            direction,
                            swipe,
                            ..
                        },
                    ) if f == fingers && direction_matches(*wanted, *direction) => {
                        if *continuous {
                            Some(stage(*phase))
                        } else if *phase == Phase::End && *swipe {
                            Some(Stage::Discrete)
                        } else {
                            None
                        }
                    }
                    (
                        Trigger::Pinch { fingers: f, .. },
                        TouchGesture::Pinch { fingers, phase, .. },
                    )
                    | (
                        Trigger::Rotate { fingers: f, .. },
                        TouchGesture::Rotate { fingers, phase, .. },
                    ) if f == fingers => Some(stage(*phase)),
                    (_, TouchGesture::Cancelled { .. }) => Some(Stage::Cancel),
                    _ => None,
                };
                stage.map(|stage| {
                    (
                        event.source,
                        stage,
                        Evidence::Touch {
                            space: event.space,
                            gesture: event.gesture,
                        },
                    )
                })
            }
            _ => None,
        };
        if let Some((source, stage, evidence)) = proposal
            && let Some(candidate) = worker.candidates.get_mut(&source)
        {
            if stage == Stage::Cancel {
                cancel_candidate(worker, &source, at, "recognizer cancelled", events);
                continue;
            }
            if stage == Stage::Begin {
                candidate.progress = true;
            }
            if matches!(stage, Stage::Update | Stage::End) && !candidate.progress {
                continue;
            }
            events.push(Event::Matched {
                version: 1,
                rule: worker.rule.id.clone(),
                id: candidate.id,
                ordinal: {
                    let Some(ordinal) = candidate.ordinal.checked_add(1) else {
                        cancel_candidate(
                            worker,
                            &source,
                            at,
                            "rule event sequence exhausted",
                            events,
                        );
                        continue;
                    };
                    candidate.ordinal = ordinal;
                    ordinal
                },
                source: source.clone(),
                at,
                config_generation: generation,
                session_generation: candidate.session_generation,
                stage,
                action: worker.rule.action.clone(),
                evidence,
                window: candidate
                    .permit
                    .as_ref()
                    .and_then(|p| p.window())
                    .map(|w| w.id.clone()),
                output: candidate
                    .permit
                    .as_ref()
                    .and_then(|p| p.output())
                    .map(|o| o.geometry.name.clone()),
                workspace: candidate
                    .permit
                    .as_ref()
                    .and_then(|p| p.output())
                    .and_then(|o| o.active_workspace),
            });
            if worker.trace {
                worker.matches.push(StepMatch {
                    expires: None,
                    after_input: None,
                    event: events.last().unwrap().clone(),
                    candidate: candidate.clone(),
                    finished: if matches!(
                        worker.rule.trigger,
                        Trigger::ButtonTaps { .. } | Trigger::TouchTaps { .. }
                    ) {
                        worker.released.get(&source).copied().unwrap_or(at)
                    } else {
                        at
                    },
                });
            }
            if stage == Stage::End {
                candidate.progress = false;
            }
            if matches!(
                worker.rule.trigger,
                Trigger::ButtonTaps { .. }
                    | Trigger::TouchTaps { .. }
                    | Trigger::Chord { .. }
                    | Trigger::Wheel { .. }
                    | Trigger::Shape { .. }
            ) {
                worker.candidates.remove(&source);
            }
            if let Recognizer::Wheel(sums) = &mut worker.recognizer {
                sums.remove(&source);
            }
        }
    }
}
fn stage(phase: Phase) -> Stage {
    match phase {
        Phase::Begin => Stage::Begin,
        Phase::Update => Stage::Update,
        Phase::End => Stage::End,
    }
}
fn invalid(message: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

#[cfg(test)]
mod tests {
    mod composition;
    use super::*;
    use crate::{
        config::{
            Context, Delivery, Key, Layer, Millis, Screen, Selector, ShapeTemplate, WindowAction,
        },
        context::{Binding, OutputState, Rect, SessionKind, Snapshot, State, Window},
        gestures::{Output, Point, Transform},
        input::*,
    };
    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }
    fn device(id: u64) -> Device {
        Device {
            identity: format!("test-{id}"),
            source: Source {
                device: id,
                generation: 1,
                seat: "seat0".into(),
            },
            name: "test".into(),
            path: format!("test-{id}"),
            physical: format!("test-{id}"),
            unique: "".into(),
            vendor: 1,
            product: 1,
            properties: vec![1],
            virtual_device: false,
            capabilities: BTreeMap::from([
                (EV_KEY, vec![30, 115, 125]),
                (
                    EV_ABS,
                    vec![
                        ABS_MT_SLOT,
                        ABS_MT_TRACKING_ID,
                        ABS_MT_POSITION_X,
                        ABS_MT_POSITION_Y,
                    ],
                ),
            ]),
            axes: BTreeMap::from([
                (
                    ABS_MT_SLOT,
                    Axis {
                        minimum: 0,
                        maximum: 4,
                        resolution: 0,
                        fuzz: 0,
                        flat: 0,
                    },
                ),
                (
                    ABS_MT_POSITION_X,
                    Axis {
                        minimum: 0,
                        maximum: 1000,
                        resolution: 10,
                        fuzz: 0,
                        flat: 0,
                    },
                ),
                (
                    ABS_MT_POSITION_Y,
                    Axis {
                        minimum: 0,
                        maximum: 1000,
                        resolution: 10,
                        fuzz: 0,
                        flat: 0,
                    },
                ),
            ]),
        }
    }
    fn config(trigger: Trigger) -> Config {
        let mut config = Config::preset("desktop").unwrap();
        config.rules[0].enabled = true;
        config.rules[0].trigger = trigger;
        config.devices.insert(
            "keyboard".into(),
            Selector {
                keys: vec![Key(30)],
                ..Selector::default()
            },
        );
        config
    }
    fn engine(config: Config) -> Engine {
        let mut engine = Engine::new(config).unwrap();
        engine.push(Record::Header { version: 1 }).unwrap();
        add(&mut engine, device(1), &[]);
        engine
    }
    fn add(engine: &mut Engine, device: Device, held: &[u16]) {
        engine
            .push(Record::Device {
                device,
                keys: held.iter().copied().collect(),
                contacts: vec![],
            })
            .unwrap();
    }
    fn frame(
        engine: &mut Engine,
        source: Source,
        at: u64,
        sequence: u64,
        events: &[(u16, u16, i32)],
    ) {
        let mut events: Vec<_> = events
            .iter()
            .map(|&(kind, code, value)| RawEvent { kind, code, value })
            .collect();
        events.push(RawEvent {
            kind: EV_SYN,
            code: SYN_REPORT,
            value: 0,
        });
        engine
            .push(Record::Frame {
                frame: Frame {
                    source,
                    at: ms(at),
                    sequence,
                    events,
                },
            })
            .unwrap();
    }
    fn key(engine: &mut Engine, id: u64, at: u64, sequence: u64, code: u16, down: bool) {
        frame(
            engine,
            device(id).source,
            at,
            sequence,
            &[(EV_KEY, code, i32::from(down))],
        );
    }
    fn tick(engine: &mut Engine, at: u64) {
        engine.push(Record::Tick { at: ms(at) }).unwrap();
    }
    fn matches(events: &[Event]) -> Vec<&Event> {
        events
            .iter()
            .filter(|e| matches!(e, Event::Matched { .. }))
            .collect()
    }
    fn hold(after: u64) -> Trigger {
        Trigger::ButtonHold {
            key: Key(115),
            after: Millis(after),
            repeat: None,
        }
    }
    fn taps(count: u32) -> Trigger {
        Trigger::ButtonTaps {
            key: Key(115),
            count,
        }
    }
    #[test]
    fn t20_configured_taps_settle_once_with_explicit_expected_outcome() {
        let mut engine = engine(config(taps(2)));
        key(&mut engine, 1, 0, 1, 115, true);
        key(&mut engine, 1, 20, 2, 115, false);
        key(&mut engine, 1, 100, 3, 115, true);
        key(&mut engine, 1, 120, 4, 115, false);
        assert!(engine.events.is_empty());
        assert_eq!(engine.deadline(), Some(ms(420)));
        tick(&mut engine, 420);
        engine
            .push(Record::ExpectedRules {
                events: vec![Event::Matched {
                    version: 1,
                    rule: "sample_double_tap".into(),
                    id: 1,
                    ordinal: 1,
                    source: device(1).source,
                    at: ms(420),
                    config_generation: 1,
                    session_generation: 0,
                    stage: Stage::Discrete,
                    action: Action::Event {
                        name: "volume_double_tap".into(),
                    },
                    evidence: Evidence::Taps { count: 2 },
                    window: None,
                    output: None,
                    workspace: None,
                }],
            })
            .unwrap();
        engine.check().unwrap();
        assert!(engine.push(Record::Tick { at: ms(421) }).is_err());
    }
    #[test]
    fn t23_custom_holds_fire_during_silence_and_coalesce_repeats() {
        let mut trigger = hold(2000);
        if let Trigger::ButtonHold { repeat, .. } = &mut trigger {
            *repeat = Some(crate::config::Repeat {
                delay: Millis(200),
                interval: Millis(100),
            });
        }
        let mut engine = engine(config(trigger));
        key(&mut engine, 1, 0, 1, 115, true);
        assert_eq!(engine.deadline(), Some(ms(2000)));
        tick(&mut engine, 1999);
        assert!(engine.events.is_empty());
        tick(&mut engine, 2000);
        tick(&mut engine, 2200);
        tick(&mut engine, 10000);
        assert_eq!(matches(&engine.events).len(), 3);
        let ordinals: Vec<_> = matches(&engine.events)
            .into_iter()
            .map(|event| match event {
                Event::Matched { ordinal, .. } => *ordinal,
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(ordinals, vec![1, 2, 3]);
        key(&mut engine, 1, 10001, 2, 115, false);
        tick(&mut engine, 11000);
        assert_eq!(matches(&engine.events).len(), 3);
        assert!(engine.deadline().is_none());
    }
    #[test]
    fn t13_reconciled_held_keys_never_start_a_rule() {
        let mut engine = Engine::new(config(hold(600))).unwrap();
        engine.push(Record::Header { version: 1 }).unwrap();
        add(&mut engine, device(1), &[115]);
        tick(&mut engine, 1000);
        key(&mut engine, 1, 1100, 1, 115, false);
        assert!(engine.events.is_empty());
        key(&mut engine, 1, 1200, 2, 115, true);
        tick(&mut engine, 1800);
        assert_eq!(matches(&engine.events).len(), 1);
    }
    #[test]
    fn t40_rejected_input_cannot_join_a_later_eligible_tap_series() {
        let mut config = config(taps(1));
        config.rules[0].require_keys = vec![Key(125)];
        let mut engine = engine(config);
        key(&mut engine, 1, 0, 1, 115, true);
        key(&mut engine, 1, 20, 2, 115, false);
        key(&mut engine, 1, 50, 3, 125, true);
        key(&mut engine, 1, 100, 4, 115, true);
        key(&mut engine, 1, 120, 5, 115, false);
        tick(&mut engine, 420);
        assert_eq!(matches(&engine.events).len(), 1);
        assert!(matches!(
            matches(&engine.events)[0],
            Event::Matched {
                evidence: Evidence::Taps { count: 1 },
                ..
            }
        ));
    }
    #[test]
    fn t40_throughout_conditions_cancel_on_modifier_release() {
        let mut config = config(hold(600));
        config.rules[0].require_keys = vec![Key(125)];
        config.rules[0].keys_throughout = true;
        let mut engine = engine(config);
        key(&mut engine, 1, 0, 1, 125, true);
        key(&mut engine, 1, 10, 2, 115, true);
        key(&mut engine, 1, 200, 3, 125, false);
        tick(&mut engine, 1000);
        assert!(matches(&engine.events).is_empty());
        assert!(
            engine.events.iter().any(
                |e| matches!(e, Event::Cancelled { reason, .. } if reason.contains("released"))
            )
        );
    }
    #[test]
    fn t41_chords_cross_devices_but_not_seats_or_unallowed_sources() {
        let mut engine = engine(config(Trigger::Chord {
            keys: vec![Key(125), Key(115)],
            ordered: true,
            timeout: Millis(300),
        }));
        add(&mut engine, device(2), &[]);
        key(&mut engine, 1, 0, 1, 125, true);
        key(&mut engine, 2, 100, 1, 115, true);
        assert_eq!(matches(&engine.events).len(), 1);
        key(&mut engine, 2, 101, 2, 115, true);
        assert_eq!(matches(&engine.events).len(), 1);
        key(&mut engine, 2, 110, 3, 115, false);
        key(&mut engine, 2, 120, 4, 115, true);
        let ids: BTreeSet<_> = matches(&engine.events)
            .into_iter()
            .filter_map(|event| {
                if let Event::Matched { id, .. } = event {
                    Some(*id)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(ids.len(), 2);
        let mut config = config(hold(600));
        config.rules[0].require_keys = vec![Key(125)];
        config.policy.allowed_seats.push("seat1".into());
        let mut engine = super::tests::engine(config);
        let mut other = device(2);
        other.source.seat = "seat1".into();
        add(&mut engine, other.clone(), &[]);
        frame(&mut engine, other.source, 0, 1, &[(EV_KEY, 125, 1)]);
        key(&mut engine, 1, 10, 1, 115, true);
        tick(&mut engine, 1000);
        assert!(matches(&engine.events).is_empty());
    }
    #[test]
    fn t41_policy_excludes_virtual_modifier_sources() {
        let mut config = config(hold(600));
        config.rules[0].require_keys = vec![Key(125)];
        let mut engine = engine(config);
        let mut virtual_device = device(2);
        virtual_device.virtual_device = true;
        add(&mut engine, virtual_device.clone(), &[]);
        frame(
            &mut engine,
            virtual_device.source,
            0,
            1,
            &[(EV_KEY, 125, 1)],
        );
        key(&mut engine, 1, 10, 1, 115, true);
        tick(&mut engine, 1000);
        assert!(matches(&engine.events).is_empty());
    }
    #[test]
    fn t06_reload_is_last_good_and_does_not_reinvent_held_input() {
        let mut engine = engine(config(hold(600)));
        key(&mut engine, 1, 0, 1, 115, true);
        let mut invalid_config = engine.config.clone();
        invalid_config.timing.tap_max = Millis(0);
        assert!(engine.reload(invalid_config, ms(100)).is_err());
        assert_eq!(engine.generation(), 1);
        tick(&mut engine, 600);
        assert_eq!(matches(&engine.events).len(), 1);
        engine.take_events();
        engine.reload(config(hold(1000)), ms(700)).unwrap();
        assert_eq!(engine.generation(), 2);
        tick(&mut engine, 2000);
        assert!(matches(&engine.events).is_empty());
        key(&mut engine, 1, 2100, 2, 115, false);
        key(&mut engine, 1, 2200, 3, 115, true);
        tick(&mut engine, 3200);
        assert_eq!(matches(&engine.events).len(), 1);
    }
    #[test]
    fn runtime_rules_keep_file_generation_ids_and_neutral_input_boundaries() {
        use crate::rule_transactions::{Change, Transaction};
        let mut engine = engine(config(hold(600)));
        let rule = engine.config.rules[0].id.clone();
        key(&mut engine, 1, 0, 1, 115, true);
        tick(&mut engine, 600);
        let first = engine
            .take_events()
            .into_iter()
            .find_map(|event| match event {
                Event::Matched { id, .. } => Some(id),
                _ => None,
            })
            .unwrap();
        let disable = Transaction {
            revision: 1,
            changes: vec![Change::Enabled {
                id: rule.clone(),
                enabled: false,
            }],
        };
        assert_eq!(engine.transact_rules(&disable, ms(650)).unwrap(), 2);
        assert_eq!(engine.generation(), 1);
        assert!(!engine.config.rules[0].enabled);
        let cancelled = engine.take_events();
        assert!(
            cancelled
                .iter()
                .any(|event| matches!(event,Event::Cancelled {id,..} if *id==first))
        );
        let enable = Transaction {
            revision: 2,
            changes: vec![Change::Enabled {
                id: rule,
                enabled: true,
            }],
        };
        assert_eq!(engine.transact_rules(&enable, ms(700)).unwrap(), 3);
        engine.take_events();
        tick(&mut engine, 1400);
        assert!(
            matches(&engine.events).is_empty(),
            "held key invented a fresh hold"
        );
        key(&mut engine, 1, 1500, 2, 115, false);
        key(&mut engine, 1, 1600, 3, 115, true);
        tick(&mut engine, 2200);
        let next = engine
            .take_events()
            .into_iter()
            .find_map(|event| match event {
                Event::Matched {
                    id,
                    config_generation,
                    ..
                } => {
                    assert_eq!(config_generation, 1);
                    Some(id)
                }
                _ => None,
            })
            .unwrap();
        assert!(
            next > first,
            "runtime changes reused a retired gesture identity"
        );
        assert!(engine.transact_rules(&disable, ms(2201)).is_err());
        assert_eq!(engine.rule_revision(), 3);
        engine.reload(config(hold(600)), ms(2300)).unwrap();
        assert_eq!(engine.generation(), 2);
        assert_eq!(engine.rule_revision(), 1);
    }
    #[test]
    fn invalid_runtime_rules_keep_the_pending_recognizer_and_current_config() {
        use crate::rule_transactions::{Change, Transaction};
        let mut engine = engine(config(hold(600)));
        key(&mut engine, 1, 0, 1, 115, true);
        let old = engine.config.clone();
        let mut invalid = old.rules[0].clone();
        invalid.action = Action::Event {
            name: String::new(),
        };
        let transaction = Transaction {
            revision: 1,
            changes: vec![Change::Replace {
                rule: Box::new(invalid),
            }],
        };
        assert!(engine.transact_rules(&transaction, ms(100)).is_err());
        assert_eq!(engine.config, old);
        assert_eq!(engine.rule_revision(), 1);
        tick(&mut engine, 600);
        assert_eq!(matches(&engine.events).len(), 1);
    }
    #[test]
    fn t71_retired_generation_fence_survives_configuration_reload() {
        let mut engine = engine(config(hold(600)));
        engine
            .push(Record::Remove {
                source: device(1).source,
                at: ms(100),
            })
            .unwrap();
        engine.reload(config(hold(600)), ms(200)).unwrap();
        let register = |device| Record::Device {
            device,
            keys: BTreeSet::new(),
            contacts: vec![],
        };
        assert!(engine.push(register(device(1))).is_err());
        let mut replacement = device(1);
        replacement.source.generation = 2;
        engine.push(register(replacement.clone())).unwrap();
        frame(
            &mut engine,
            replacement.source.clone(),
            400,
            1,
            &[(EV_KEY, 115, 1)],
        );
        let old = Record::Frame {
            frame: Frame {
                source: device(1).source,
                at: ms(4000),
                sequence: 2,
                events: vec![RawEvent {
                    kind: EV_SYN,
                    code: SYN_REPORT,
                    value: 0,
                }],
            },
        };
        assert!(engine.push(old).is_err());
        tick(&mut engine, 1000);
        assert!(
            matches!(matches(&engine.events)[0],Event::Matched { source,config_generation:2,.. } if source==&replacement.source)
        );
    }
    #[test]
    fn t13_bad_registration_cannot_replace_a_working_source() {
        let mut engine = engine(config(hold(600)));
        key(&mut engine, 1, 0, 1, 115, true);
        let mut replacement = device(1);
        replacement.source.generation = 2;
        assert!(
            engine
                .push(Record::Device {
                    device: replacement,
                    keys: (0..769).collect(),
                    contacts: vec![]
                })
                .is_err()
        );
        tick(&mut engine, 600);
        assert_eq!(matches(&engine.events).len(), 1);
        assert!(
            matches!(matches(&engine.events)[0],Event::Matched { source,.. } if source.generation==1)
        );
    }
    #[test]
    fn t14_device_loss_cancels_one_source_and_preserves_others() {
        let mut engine = engine(config(hold(600)));
        add(&mut engine, device(2), &[]);
        key(&mut engine, 1, 0, 1, 115, true);
        key(&mut engine, 2, 10, 1, 115, true);
        engine
            .push(Record::Remove {
                source: device(1).source,
                at: ms(200),
            })
            .unwrap();
        tick(&mut engine, 610);
        assert_eq!(matches(&engine.events).len(), 1);
        assert!(
            matches!(matches(&engine.events)[0], Event::Matched { source, .. } if source.device == 2)
        );
    }
    #[test]
    fn t25_layers_arm_only_fresh_input_and_cancel_when_released() {
        let mut config = config(hold(600));
        config.layers = vec![Layer {
            id: "touch".into(),
            key: Key(125),
            toggle: false,
            activation: Default::default(),
        }];
        config.rules[0].layer = Some("touch".into());
        config.rules[0].keys_throughout = true;
        let mut engine = engine(config);
        key(&mut engine, 1, 0, 1, 115, true);
        key(&mut engine, 1, 10, 2, 125, true);
        tick(&mut engine, 700);
        assert!(matches(&engine.events).is_empty());
        key(&mut engine, 1, 710, 3, 115, false);
        key(&mut engine, 1, 720, 4, 115, true);
        tick(&mut engine, 1320);
        assert_eq!(matches(&engine.events).len(), 1);
    }

    #[test]
    fn runtime_layer_activation_requires_fresh_input_and_deactivation_cancels_progress() {
        use crate::{
            config::LayerActivation,
            rule_transactions::{Change, Transaction},
        };
        let mut config = config(hold(600));
        config.layers = vec![Layer {
            id: "touch".into(),
            key: Key(125),
            toggle: false,
            activation: LayerActivation::Physical,
        }];
        config.rules[0].layer = Some("touch".into());
        config.rules[0].keys_throughout = true;
        let mut engine = engine(config);
        key(&mut engine, 1, 0, 1, 115, true);
        engine
            .transact_rules(
                &Transaction {
                    revision: 1,
                    changes: vec![Change::Layer {
                        id: "touch".into(),
                        activation: LayerActivation::Active,
                    }],
                },
                ms(100),
            )
            .unwrap();
        tick(&mut engine, 700);
        assert!(
            matches(&engine.events).is_empty(),
            "activation reused input that was already held"
        );
        key(&mut engine, 1, 710, 2, 115, false);
        key(&mut engine, 1, 720, 3, 115, true);
        tick(&mut engine, 1320);
        let matched = engine
            .take_events()
            .into_iter()
            .find_map(|event| match event {
                Event::Matched { id, .. } => Some(id),
                _ => None,
            })
            .unwrap();
        let invalid = Transaction {
            revision: 2,
            changes: vec![Change::Layer {
                id: "missing".into(),
                activation: LayerActivation::Inactive,
            }],
        };
        assert!(engine.transact_rules(&invalid, ms(1330)).is_err());
        assert_eq!(engine.rule_revision(), 2);
        assert_eq!(
            engine.config().layers[0].activation,
            LayerActivation::Active
        );
        engine
            .transact_rules(
                &Transaction {
                    revision: 2,
                    changes: vec![Change::Layer {
                        id: "touch".into(),
                        activation: LayerActivation::Inactive,
                    }],
                },
                ms(1340),
            )
            .unwrap();
        assert!(
            engine
                .take_events()
                .iter()
                .any(|event| matches!(event,Event::Cancelled{id,..} if *id==matched))
        );
        assert_eq!(engine.generation(), 1);
        key(&mut engine, 1, 1350, 4, 115, false);
        key(&mut engine, 1, 1360, 5, 115, true);
        tick(&mut engine, 2000);
        assert!(matches(&engine.events).is_empty());
    }
    #[test]
    fn named_mode_transitions_require_fresh_input_and_cancel_pending_and_active_rules() {
        use crate::{
            config::NamedMode,
            rule_transactions::{Change, Transaction},
        };
        let mut config = config(hold(600));
        config.modes = vec![NamedMode {
            id: "navigate".into(),
            active: false,
        }];
        config.rules[0].when_mode = Some("navigate".into());
        let mut engine = engine(config);
        key(&mut engine, 1, 0, 1, 115, true);
        let select = |revision, selected: bool| Transaction {
            revision,
            changes: vec![Change::Mode {
                id: selected.then(|| "navigate".into()),
            }],
        };
        engine.transact_rules(&select(1, true), ms(100)).unwrap();
        tick(&mut engine, 700);
        assert!(
            matches(&engine.events).is_empty(),
            "mode selection reused a held key"
        );
        key(&mut engine, 1, 710, 2, 115, false);
        key(&mut engine, 1, 720, 3, 115, true);
        engine.transact_rules(&select(2, false), ms(900)).unwrap();
        tick(&mut engine, 1400);
        assert!(
            matches(&engine.events).is_empty(),
            "retired mode completed a pending hold"
        );
        key(&mut engine, 1, 1410, 4, 115, false);
        engine.transact_rules(&select(3, true), ms(1420)).unwrap();
        key(&mut engine, 1, 1430, 5, 115, true);
        tick(&mut engine, 2030);
        let matched = engine
            .take_events()
            .into_iter()
            .find_map(|event| match event {
                Event::Matched { id, .. } => Some(id),
                _ => None,
            })
            .unwrap();
        engine.transact_rules(&select(4, false), ms(2040)).unwrap();
        assert!(
            engine
                .take_events()
                .iter()
                .any(|event| matches!(event,Event::Cancelled{id,..} if *id==matched))
        );
        assert_eq!(engine.generation(), 1);
        assert_eq!(engine.config().mode, crate::config::Mode::Observe);
    }
    #[test]
    fn named_modes_gate_sequences_and_do_not_reuse_steps_across_transitions() {
        use crate::{
            config::NamedMode,
            rule_transactions::{Change, Transaction},
        };
        let mut config = config(Trigger::Sequence {
            steps: vec![
                Trigger::Key { key: Key(115) },
                Trigger::Key { key: Key(30) },
            ],
            timeout: Millis(1000),
        });
        config.modes = vec![NamedMode {
            id: "navigate".into(),
            active: false,
        }];
        config.rules[0].when_mode = Some("navigate".into());
        let mut engine = engine(config);
        for (at, sequence, code, down) in [
            (0, 1, 115, true),
            (10, 2, 115, false),
            (20, 3, 30, true),
            (30, 4, 30, false),
        ] {
            key(&mut engine, 1, at, sequence, code, down);
        }
        assert!(matches(&engine.events).is_empty());
        let select = |revision, active: bool| Transaction {
            revision,
            changes: vec![Change::Mode {
                id: active.then(|| "navigate".into()),
            }],
        };
        engine.transact_rules(&select(1, true), ms(40)).unwrap();
        key(&mut engine, 1, 50, 5, 115, true);
        key(&mut engine, 1, 60, 6, 115, false);
        engine.transact_rules(&select(2, false), ms(70)).unwrap();
        assert!(
            engine
                .events
                .iter()
                .any(|event| matches!(event, Event::Cancelled { .. }))
        );
        engine.transact_rules(&select(3, true), ms(80)).unwrap();
        key(&mut engine, 1, 90, 7, 30, true);
        key(&mut engine, 1, 100, 8, 30, false);
        assert!(matches(&engine.events).is_empty());
        for (at, sequence, code, down) in [
            (110, 9, 115, true),
            (120, 10, 115, false),
            (130, 11, 30, true),
            (140, 12, 30, false),
        ] {
            key(&mut engine, 1, at, sequence, code, down);
        }
        assert_eq!(matches(&engine.events).len(), 1);
    }
    #[test]
    fn t17_wheel_high_resolution_is_counted_once_with_modifier_conditions() {
        let mut config = config(Trigger::Wheel {
            horizontal: false,
            minimum: 1.0,
        });
        config.rules[0].require_keys = vec![Key(125)];
        let mut engine = engine(config);
        key(&mut engine, 1, 0, 1, 125, true);
        frame(
            &mut engine,
            device(1).source,
            10,
            2,
            &[(EV_REL, REL_WHEEL_HI_RES, 60), (EV_REL, REL_WHEEL, 1)],
        );
        assert!(matches(&engine.events).is_empty());
        frame(
            &mut engine,
            device(1).source,
            20,
            3,
            &[(EV_REL, REL_WHEEL_HI_RES, 60)],
        );
        assert!(matches!(
            matches(&engine.events)[0],
            Event::Matched {
                evidence: Evidence::Wheel { amount: 1.0, .. },
                ..
            }
        ));
    }
    #[test]
    fn t40_rejected_wheel_motion_and_expired_residue_cannot_trigger_a_later_rule() {
        let mut config = config(Trigger::Wheel {
            horizontal: false,
            minimum: 1.0,
        });
        config.rules[0].require_keys = vec![Key(125)];
        let mut engine = engine(config);
        frame(
            &mut engine,
            device(1).source,
            0,
            1,
            &[(EV_REL, REL_WHEEL_HI_RES, 60)],
        );
        key(&mut engine, 1, 10, 2, 125, true);
        frame(
            &mut engine,
            device(1).source,
            20,
            3,
            &[(EV_REL, REL_WHEEL_HI_RES, 60)],
        );
        assert!(matches(&engine.events).is_empty());
        tick(&mut engine, 320);
        frame(
            &mut engine,
            device(1).source,
            330,
            4,
            &[(EV_REL, REL_WHEEL_HI_RES, 60)],
        );
        assert!(matches(&engine.events).is_empty());
        frame(
            &mut engine,
            device(1).source,
            340,
            5,
            &[(EV_REL, REL_WHEEL_HI_RES, 60)],
        );
        assert_eq!(matches(&engine.events).len(), 1);
    }
    fn rect(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect {
            origin: Point { x, y },
            size: Point { x: w, y: h },
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
                geometry: Output {
                    name: "phone".into(),
                    pixel_width: 1000.0,
                    pixel_height: 1000.0,
                    scale: 1.0,
                    origin: Point::default(),
                    transform: Transform::Normal,
                },
                geometry_generation: 2,
                active_workspace: Some(1),
                screen: Screen::On,
                keyboard_known: true,
                keyboard: None,
            }],
            windows: vec![
                Window {
                    id: "a".into(),
                    output: "phone".into(),
                    workspace: 1,
                    rect: rect(0.0, 0.0, 500.0, 800.0),
                    content: rect(0.0, 0.0, 500.0, 800.0),
                    mapped: true,
                    visible: true,
                    focused: true,
                    z_order: Some(1),
                },
                Window {
                    id: "b".into(),
                    output: "phone".into(),
                    workspace: 1,
                    rect: rect(500.0, 0.0, 500.0, 800.0),
                    content: rect(500.0, 0.0, 500.0, 800.0),
                    mapped: true,
                    visible: true,
                    focused: false,
                    z_order: Some(2),
                },
            ],
            regions: vec![],
        }
    }
    fn phone() -> Engine {
        let mut config = Config::preset("phone").unwrap();
        config.rules.retain(|rule| rule.id == "G08_up");
        assert_eq!(config.rules.len(), 1);
        let mut engine = engine(config);
        let authority = engine.authority_mut();
        let generation = authority
            .bind(
                Binding {
                    seat: "seat0".into(),
                    session: "test".into(),
                    uid: 1000,
                    compositor_pid: 100,
                    compositor_start: 1,
                    kind: SessionKind::User,
                },
                true,
                false,
            )
            .unwrap();
        let snap = snapshot(generation, 1, 0);
        let geometry = snap.outputs[0].geometry.clone();
        authority.update(snap, ms(0)).unwrap();
        let mut mapping = Mapping::from_device(&device(1)).unwrap();
        mapping.generation = 2;
        mapping.output = Some(geometry);
        engine
            .push(Record::Mapping {
                source: device(1).source,
                at: ms(0),
                mapping,
            })
            .unwrap();
        engine
    }
    fn touch_start(engine: &mut Engine, second_x: i32) {
        touch_start_at(engine, second_x, 0, 1);
    }
    fn touch_start_at(engine: &mut Engine, second_x: i32, at: u64, sequence: u64) {
        frame(
            engine,
            device(1).source,
            at,
            sequence,
            &[
                (EV_ABS, ABS_MT_SLOT, 0),
                (EV_ABS, ABS_MT_TRACKING_ID, 1),
                (EV_ABS, ABS_MT_POSITION_X, 100),
                (EV_ABS, ABS_MT_POSITION_Y, 500),
                (EV_ABS, ABS_MT_SLOT, 1),
                (EV_ABS, ABS_MT_TRACKING_ID, 2),
                (EV_ABS, ABS_MT_POSITION_X, second_x),
                (EV_ABS, ABS_MT_POSITION_Y, 500),
            ],
        );
    }
    #[test]
    fn t27_configured_two_finger_double_tap_emits_only_its_settled_count() {
        let mut engine = engine(config(Trigger::TouchTaps {
            fingers: 2,
            count: 2,
        }));
        touch_start(&mut engine, 200);
        let release = [
            (EV_ABS, ABS_MT_SLOT, 0),
            (EV_ABS, ABS_MT_TRACKING_ID, -1),
            (EV_ABS, ABS_MT_SLOT, 1),
            (EV_ABS, ABS_MT_TRACKING_ID, -1),
        ];
        frame(&mut engine, device(1).source, 50, 2, &release);
        touch_start_at(&mut engine, 200, 150, 3);
        frame(&mut engine, device(1).source, 200, 4, &release);
        assert!(matches(&engine.events).is_empty());
        tick(&mut engine, 500);
        assert_eq!(matches(&engine.events).len(), 1);
        assert!(matches!(
            matches(&engine.events)[0],
            Event::Matched {
                evidence: Evidence::Touch {
                    space: Space::Normalized,
                    gesture: TouchGesture::Taps {
                        fingers: 2,
                        count: 2,
                        ..
                    }
                },
                ..
            }
        ));
    }
    #[test]
    fn t28_configured_touch_hold_uses_its_custom_duration() {
        let mut engine = engine(config(Trigger::TouchHold {
            fingers: 2,
            after: Millis(2000),
        }));
        touch_start(&mut engine, 200);
        tick(&mut engine, 1999);
        assert!(matches(&engine.events).is_empty());
        tick(&mut engine, 2000);
        assert_eq!(matches(&engine.events).len(), 1);
        assert!(
            matches!(matches(&engine.events)[0],Event::Matched { at, evidence:Evidence::Touch { gesture:TouchGesture::Hold { fingers:2,.. },.. },.. } if *at==ms(2000))
        );
    }
    #[test]
    fn t28_short_touch_hold_waits_for_finger_join_without_a_past_deadline() {
        let mut engine = engine(config(Trigger::TouchHold {
            fingers: 2,
            after: Millis(100),
        }));
        touch_start(&mut engine, 200);
        assert_eq!(engine.deadline(), Some(ms(120)));
        tick(&mut engine, 100);
        assert!(matches(&engine.events).is_empty());
        assert_eq!(engine.deadline(), Some(ms(120)));
        tick(&mut engine, 120);
        assert_eq!(matches(&engine.events).len(), 1);
    }
    #[test]
    fn t58_configured_app_swipe_keeps_original_window_across_focus_change() {
        let mut engine = phone();
        touch_start(&mut engine, 200);
        frame(
            &mut engine,
            device(1).source,
            150,
            2,
            &[
                (EV_ABS, ABS_MT_SLOT, 0),
                (EV_ABS, ABS_MT_POSITION_Y, 300),
                (EV_ABS, ABS_MT_SLOT, 1),
                (EV_ABS, ABS_MT_POSITION_Y, 300),
            ],
        );
        let generation = engine.authority.generation();
        let mut snap = snapshot(generation, 2, 180);
        snap.windows[0].focused = false;
        snap.windows[1].focused = true;
        engine.authority.update(snap, ms(180)).unwrap();
        frame(
            &mut engine,
            device(1).source,
            200,
            3,
            &[
                (EV_ABS, ABS_MT_SLOT, 0),
                (EV_ABS, ABS_MT_TRACKING_ID, -1),
                (EV_ABS, ABS_MT_SLOT, 1),
                (EV_ABS, ABS_MT_TRACKING_ID, -1),
            ],
        );
        assert_eq!(matches(&engine.events).len(), 1);
        assert!(
            matches!(matches(&engine.events)[0], Event::Matched { window:Some(window), action:Action::Hyprland { action:WindowAction::ReorderUp }, .. } if window=="a")
        );
    }
    #[test]
    fn t58_different_start_windows_are_rejected_before_recognition() {
        let mut engine = phone();
        touch_start(&mut engine, 700);
        tick(&mut engine, 120);
        assert!(matches(&engine.events).is_empty());
        assert!(
            engine
                .events
                .iter()
                .any(|e| matches!(e,Event::Skipped { reason, .. } if reason=="MixedTargets"))
        );
    }
    #[test]
    fn t60_lock_revokes_an_in_progress_app_candidate() {
        let mut engine = phone();
        touch_start(&mut engine, 200);
        engine.authority.lock_requested();
        frame(
            &mut engine,
            device(1).source,
            150,
            2,
            &[
                (EV_ABS, ABS_MT_SLOT, 0),
                (EV_ABS, ABS_MT_POSITION_Y, 300),
                (EV_ABS, ABS_MT_SLOT, 1),
                (EV_ABS, ABS_MT_POSITION_Y, 300),
            ],
        );
        assert!(matches(&engine.events).is_empty());
        assert!(
            engine
                .events
                .iter()
                .any(|e| matches!(e, Event::Cancelled { .. }))
        );
    }
    #[test]
    fn t61_stale_provider_cancels_before_rule_completion() {
        let mut engine = phone();
        touch_start(&mut engine, 200);
        tick(&mut engine, 251);
        assert!(matches(&engine.events).is_empty());
        assert!(
            engine
                .events
                .iter()
                .any(|e| matches!(e,Event::Cancelled { reason, .. } if reason=="StaleGeneration"))
        );
    }
    #[test]
    fn t02_runtime_compiles_sequences_and_configured_shapes() {
        let trigger = Trigger::Sequence {
            steps: vec![taps(2), hold(2000)],
            timeout: Millis(5000),
        };
        assert!(Engine::new(config(trigger)).is_ok());
        let mut config = config(Trigger::Shape {
            name: "v".into(),
            fingers: Some(1),
            button: None,
        });
        let shape: ShapeTemplate = crate::paths::demo_templates().remove(0);
        config.shapes = vec![shape];
        assert!(Engine::new(config).is_ok());
    }
    #[test]
    fn t61_missing_context_skips_without_emitting_an_action() {
        let mut config = config(hold(600));
        config.rules[0].contexts = vec![Context::Desktop];
        config.rules[0].delivery = Delivery::Observe;
        config.rules[0].scope = Scope::Everywhere;
        let mut engine = engine(config);
        key(&mut engine, 1, 0, 1, 115, true);
        tick(&mut engine, 600);
        assert!(matches(&engine.events).is_empty());
        assert!(matches!(&engine.events[0], Event::Skipped { .. }));
    }

    fn shape_engine(fingers: Option<u16>, throughout: bool) -> Engine {
        let mut config = config(Trigger::Shape {
            name: "v".into(),
            fingers,
            button: None,
        });
        config.rules[0].require_keys = vec![Key(115)];
        config.rules[0].keys_throughout = throughout;
        let mut engine = Engine::new(config).unwrap();
        engine.push(Record::Header { version: 1 }).unwrap();
        if fingers.is_none() {
            let mut mouse = device(1);
            mouse.capabilities.insert(EV_REL, vec![0, 1]);
            mouse.capabilities.get_mut(&EV_KEY).unwrap().push(0x110);
            add(&mut engine, mouse, &[]);
        } else {
            add(&mut engine, device(1), &[]);
        }
        add(&mut engine, device(2), &[]);
        key(&mut engine, 2, 0, 1, 115, true);
        engine
    }
    fn shape_touch(
        engine: &mut Engine,
        at: u64,
        sequence: u64,
        points: &[(u16, i32, i32)],
        start: bool,
        end: bool,
    ) {
        let mut events = Vec::new();
        for &(slot, x, y) in points {
            events.push((EV_ABS, ABS_MT_SLOT, i32::from(slot)));
            if start {
                events.push((EV_ABS, ABS_MT_TRACKING_ID, 10 + i32::from(slot)));
            }
            events.push((EV_ABS, ABS_MT_POSITION_X, x));
            events.push((EV_ABS, ABS_MT_POSITION_Y, y));
            if end {
                events.push((EV_ABS, ABS_MT_TRACKING_ID, -1));
            }
        }
        frame(engine, device(1).source, at, sequence, &events);
    }
    #[test]
    fn t33_configured_touch_shape_matches_once_and_keeps_release_frame_motion() {
        let mut engine = shape_engine(Some(1), true);
        shape_touch(&mut engine, 10, 1, &[(0, 100, 100)], true, false);
        shape_touch(&mut engine, 50, 2, &[(0, 500, 900)], false, false);
        shape_touch(&mut engine, 80, 3, &[(0, 900, 100)], false, true);
        assert_eq!(matches(&engine.events).len(), 1);
        assert!(
            engine.take_strokes().is_empty(),
            "ordinary engines never expose captured paths"
        );
        assert!(
            matches!(&matches(&engine.events)[0], Event::Matched { evidence: Evidence::Shape { space: Space::Millimetres, fingers: Some(1), shape }, stage: Stage::Discrete, .. } if shape.name == "v" && shape.confidence >= 0.94)
        );
    }
    #[test]
    fn t33_configured_armed_mouse_integrates_counts_and_includes_release_delta() {
        let mut engine = shape_engine(None, true);
        frame(&mut engine, device(1).source, 10, 1, &[(EV_KEY, 0x110, 1)]);
        frame(
            &mut engine,
            device(1).source,
            50,
            2,
            &[(EV_REL, 0, 50), (EV_REL, 1, 100)],
        );
        frame(
            &mut engine,
            device(1).source,
            80,
            3,
            &[(EV_REL, 0, 50), (EV_REL, 1, -100), (EV_KEY, 0x110, 0)],
        );
        assert_eq!(matches(&engine.events).len(), 1);
        assert!(matches!(
            &matches(&engine.events)[0],
            Event::Matched {
                evidence: Evidence::Shape {
                    space: Space::Relative,
                    fingers: None,
                    ..
                },
                ..
            }
        ));
    }
    #[test]
    fn t38_cross_device_release_cancels_throughout_capture_immediately() {
        for fingers in [None, Some(1)] {
            let mut engine = shape_engine(fingers, true);
            if fingers.is_some() {
                shape_touch(&mut engine, 10, 1, &[(0, 100, 100)], true, false);
            } else {
                frame(&mut engine, device(1).source, 10, 1, &[(EV_KEY, 0x110, 1)]);
            }
            key(&mut engine, 2, 20, 2, 115, false);
            assert!(engine.events.iter().any(|event| matches!(event, Event::Cancelled { source, reason, .. } if source.device == 1 && reason == "activation condition released")));
            assert!(matches(&engine.events).is_empty());
        }
    }
    #[test]
    fn t38_sampled_arming_survives_release_but_unarmed_touch_never_rearms_mid_path() {
        let mut engine = shape_engine(Some(1), false);
        shape_touch(&mut engine, 10, 1, &[(0, 100, 100)], true, false);
        key(&mut engine, 2, 20, 2, 115, false);
        shape_touch(&mut engine, 50, 2, &[(0, 500, 900)], false, false);
        shape_touch(&mut engine, 80, 3, &[(0, 900, 100)], false, true);
        assert_eq!(matches(&engine.events).len(), 1);
        engine.take_events();
        shape_touch(&mut engine, 100, 4, &[(0, 100, 100)], true, false);
        key(&mut engine, 2, 110, 3, 115, true);
        shape_touch(&mut engine, 140, 5, &[(0, 500, 900)], false, false);
        shape_touch(&mut engine, 180, 6, &[(0, 900, 100)], false, true);
        assert!(matches(&engine.events).is_empty());
    }
    #[test]
    fn t34_tiny_inverted_and_ambiguous_configured_paths_produce_no_match() {
        for (points, ambiguous) in [
            ([(100, 100), (110, 120), (120, 100)], false),
            ([(100, 900), (500, 100), (900, 900)], false),
            ([(100, 100), (500, 900), (900, 100)], true),
        ] {
            let mut engine = shape_engine(Some(1), true);
            if ambiguous {
                let mut config = engine.config.clone();
                let mut duplicate = config.shapes[0].clone();
                duplicate.name = "other-v".into();
                config.shapes.push(duplicate);
                engine.reload(config, ms(1)).unwrap();
            }
            for (index, (x, y)) in points.into_iter().enumerate() {
                shape_touch(
                    &mut engine,
                    10 + index as u64 * 30,
                    index as u64 + 1,
                    &[(0, x, y)],
                    index == 0,
                    index == 2,
                );
            }
            assert!(matches(&engine.events).is_empty());
            assert!(
                engine
                    .events
                    .iter()
                    .any(|event| matches!(event, Event::Cancelled { .. }))
            );
        }
    }
    #[test]
    fn t36_two_finger_shapes_follow_a_fixed_centroid_and_reject_partial_lifts() {
        for partial in [false, true] {
            let mut engine = shape_engine(Some(2), true);
            shape_touch(
                &mut engine,
                10,
                1,
                &[(0, 100, 100), (1, 200, 100)],
                true,
                false,
            );
            shape_touch(
                &mut engine,
                50,
                2,
                &[(0, 450, 800), (1, 550, 800)],
                false,
                false,
            );
            if partial {
                shape_touch(&mut engine, 80, 3, &[(0, 800, 100)], false, true);
                assert!(matches(&engine.events).is_empty());
                assert!(engine.events.iter().any(|event| matches!(event, Event::Cancelled { reason, .. } if reason == "ContactsChanged")));
            } else {
                shape_touch(
                    &mut engine,
                    80,
                    3,
                    &[(0, 800, 100), (1, 900, 100)],
                    false,
                    true,
                );
                assert_eq!(matches(&engine.events).len(), 1);
            }
        }
    }
    #[test]
    fn t36_shape_join_sample_and_idle_time_limits_cancel_without_action() {
        let mut engine = shape_engine(Some(2), true);
        shape_touch(&mut engine, 10, 1, &[(0, 100, 100)], true, false);
        tick(&mut engine, 130);
        assert!(
            engine
                .events
                .iter()
                .any(|event| matches!(event, Event::Cancelled { .. }))
        );
        for samples in [2, 2048] {
            let mut engine = shape_engine(Some(1), true);
            let mut config = engine.config.clone();
            config.shape_capture.max_samples = samples;
            engine.reload(config, ms(1)).unwrap();
            shape_touch(&mut engine, 10, 1, &[(0, 100, 100)], true, false);
            shape_touch(&mut engine, 50, 2, &[(0, 500, 900)], false, false);
            if samples == 2 {
                shape_touch(&mut engine, 80, 3, &[(0, 900, 100)], false, true);
            } else {
                tick(&mut engine, 5010);
            }
            assert!(matches(&engine.events).is_empty());
            assert!(
                engine
                    .events
                    .iter()
                    .any(|event| matches!(event, Event::Cancelled { .. }))
            );
        }
    }
    #[test]
    fn t39_mapping_replacement_waits_for_neutral_before_a_new_shape() {
        let mut engine = shape_engine(Some(1), true);
        shape_touch(&mut engine, 10, 1, &[(0, 100, 100)], true, false);
        let mut mapping = Mapping::from_device(&device(1)).unwrap();
        mapping.generation = 2;
        engine
            .push(Record::Mapping {
                source: device(1).source,
                at: ms(20),
                mapping,
            })
            .unwrap();
        shape_touch(&mut engine, 50, 2, &[(0, 500, 900)], false, false);
        shape_touch(&mut engine, 80, 3, &[(0, 900, 100)], false, true);
        assert!(matches(&engine.events).is_empty());
        shape_touch(&mut engine, 100, 4, &[(0, 100, 100)], true, false);
        shape_touch(&mut engine, 130, 5, &[(0, 500, 900)], false, false);
        shape_touch(&mut engine, 160, 6, &[(0, 900, 100)], false, true);
        assert_eq!(matches(&engine.events).len(), 1);
    }
    #[test]
    fn t33_circle_and_zigzag_work_from_configuration_with_different_timing() {
        for name in ["circle", "zigzag"] {
            let mut engine = shape_engine(Some(1), true);
            let mut config = engine.config.clone();
            config.rules[0].trigger = Trigger::Shape {
                name: name.into(),
                fingers: Some(1),
                button: None,
            };
            engine.reload(config, ms(1)).unwrap();
            let points = if name == "circle" {
                (0..=32)
                    .map(|index| {
                        let angle = index as f64 * std::f64::consts::TAU / 32.0;
                        (
                            500 + (400.0 * angle.cos()) as i32,
                            500 + (400.0 * angle.sin()) as i32,
                        )
                    })
                    .collect::<Vec<_>>()
            } else {
                vec![(100, 100), (300, 900), (500, 100), (700, 900), (900, 100)]
            };
            for (index, &(x, y)) in points.iter().enumerate() {
                shape_touch(
                    &mut engine,
                    10 + (index * index) as u64,
                    index as u64 + 1,
                    &[(0, x, y)],
                    index == 0,
                    index + 1 == points.len(),
                );
            }
            assert_eq!(matches(&engine.events).len(), 1, "{name}");
        }
    }
    #[test]
    fn t39_reused_slots_and_different_touchscreens_cannot_complete_a_group() {
        let mut engine = shape_engine(Some(2), true);
        shape_touch(&mut engine, 10, 1, &[(0, 100, 100)], true, false);
        frame(
            &mut engine,
            device(2).source,
            20,
            2,
            &[
                (EV_ABS, ABS_MT_TRACKING_ID, 99),
                (EV_ABS, ABS_MT_POSITION_X, 200),
                (EV_ABS, ABS_MT_POSITION_Y, 100),
            ],
        );
        tick(&mut engine, 130);
        assert!(matches(&engine.events).is_empty());
        let mut engine = shape_engine(Some(1), true);
        shape_touch(&mut engine, 10, 1, &[(0, 100, 100)], true, false);
        shape_touch(&mut engine, 50, 2, &[(0, 500, 900)], false, false);
        frame(
            &mut engine,
            device(1).source,
            60,
            3,
            &[
                (EV_ABS, ABS_MT_TRACKING_ID, -1),
                (EV_ABS, ABS_MT_TRACKING_ID, 77),
                (EV_ABS, ABS_MT_POSITION_X, 900),
                (EV_ABS, ABS_MT_POSITION_Y, 100),
            ],
        );
        assert!(matches(&engine.events).is_empty());
        assert!(engine.events.iter().any(
            |event| matches!(event, Event::Cancelled { reason, .. } if reason == "ContactsChanged")
        ));
    }
    #[test]
    fn t38_mouse_capture_button_can_arm_and_complete_its_own_stroke() {
        let mut engine = shape_engine(None, true);
        let mut config = engine.config.clone();
        config.rules[0].require_keys = vec![Key(0x110)];
        engine.reload(config, ms(1)).unwrap();
        frame(&mut engine, device(1).source, 10, 1, &[(EV_KEY, 0x110, 1)]);
        frame(
            &mut engine,
            device(1).source,
            50,
            2,
            &[(EV_REL, 0, 50), (EV_REL, 1, 100)],
        );
        frame(
            &mut engine,
            device(1).source,
            80,
            3,
            &[(EV_REL, 0, 50), (EV_REL, 1, -100), (EV_KEY, 0x110, 0)],
        );
        assert_eq!(matches(&engine.events).len(), 1);
    }
    fn sequence_engine(steps: Vec<Trigger>, timeout: u64) -> Engine {
        let mut engine = engine(config(Trigger::Sequence {
            steps,
            timeout: Millis(timeout),
        }));
        add(&mut engine, device(2), &[]);
        engine
    }
    #[test]
    fn t42_tap_twice_then_hold_emits_only_the_complete_sequence() {
        let mut engine = sequence_engine(vec![taps(2), hold(600)], 5000);
        for (at, seq, down) in [
            (0, 1, true),
            (50, 2, false),
            (150, 3, true),
            (200, 4, false),
            (300, 5, true),
        ] {
            key(&mut engine, 1, at, seq, 115, down);
        }
        assert!(matches(&engine.events).is_empty());
        tick(&mut engine, 900);
        assert_eq!(matches(&engine.events).len(), 1);
        assert!(
            matches!(matches(&engine.events)[0],Event::Matched {evidence:Evidence::Sequence {steps},stage:Stage::Discrete,..} if steps.len()==2 && matches!(steps[0],Evidence::Taps {count:2}) && matches!(steps[1],Evidence::Hold {repeat:false}))
        );
    }
    #[test]
    fn t42_hold_then_shape_combines_permitted_sources_and_retains_one_identity() {
        let mut engine = sequence_engine(
            vec![
                hold(600),
                Trigger::Shape {
                    name: "v".into(),
                    fingers: Some(1),
                    button: None,
                },
            ],
            5000,
        );
        key(&mut engine, 2, 0, 1, 115, true);
        tick(&mut engine, 600);
        assert!(matches(&engine.events).is_empty());
        shape_touch(&mut engine, 610, 1, &[(0, 100, 100)], true, false);
        shape_touch(&mut engine, 650, 2, &[(0, 500, 900)], false, false);
        shape_touch(&mut engine, 680, 3, &[(0, 900, 100)], false, true);
        assert_eq!(matches(&engine.events).len(), 1);
        assert!(
            matches!(matches(&engine.events)[0],Event::Matched {source,evidence:Evidence::Sequence {steps},..} if source.device==2 && steps.len()==2 && matches!(steps[1],Evidence::Shape {..}))
        );
    }
    #[test]
    fn t41_a_future_hold_started_before_the_previous_step_cannot_complete() {
        let mut engine = sequence_engine(
            vec![
                taps(2),
                Trigger::ButtonHold {
                    key: Key(30),
                    after: Millis(600),
                    repeat: None,
                },
            ],
            5000,
        );
        key(&mut engine, 2, 0, 1, 30, true);
        for (at, seq, down) in [
            (10, 1, true),
            (50, 2, false),
            (150, 3, true),
            (200, 4, false),
        ] {
            key(&mut engine, 1, at, seq, 115, down);
        }
        tick(&mut engine, 600);
        assert!(matches(&engine.events).is_empty());
        key(&mut engine, 2, 650, 2, 30, false);
        key(&mut engine, 2, 700, 3, 30, true);
        tick(&mut engine, 1300);
        assert_eq!(matches(&engine.events).len(), 1);
    }
    #[test]
    fn t42_sequence_timeout_and_source_loss_cancel_the_chain() {
        for loss in [false, true] {
            let mut engine = sequence_engine(vec![hold(600), taps(2)], 1000);
            key(&mut engine, 1, 0, 1, 115, true);
            tick(&mut engine, 600);
            if loss {
                engine
                    .push(Record::Remove {
                        source: device(1).source,
                        at: ms(700),
                    })
                    .unwrap();
            } else {
                tick(&mut engine, 1000);
            }
            assert!(matches(&engine.events).is_empty());
            assert!(
                engine
                    .events
                    .iter()
                    .any(|event| matches!(event, Event::Cancelled { .. }))
            );
        }
    }
    #[test]
    fn t40_sequence_steps_do_not_cross_seats() {
        let mut engine = sequence_engine(
            vec![
                hold(600),
                Trigger::ButtonHold {
                    key: Key(30),
                    after: Millis(600),
                    repeat: None,
                },
            ],
            5000,
        );
        let mut other = device(3);
        other.source.seat = "seat1".into();
        let mut config = engine.config.clone();
        config.policy.allowed_seats.push("seat1".into());
        engine.reload(config, ms(0)).unwrap();
        add(&mut engine, other.clone(), &[]);
        key(&mut engine, 1, 0, 1, 115, true);
        tick(&mut engine, 600);
        frame(&mut engine, other.source, 700, 1, &[(EV_KEY, 30, 1)]);
        tick(&mut engine, 1300);
        assert!(matches(&engine.events).is_empty());
    }
    #[test]
    fn t42_touch_hold_then_swipe_continues_only_the_same_contacts() {
        let mut engine = sequence_engine(
            vec![
                Trigger::TouchHold {
                    fingers: 1,
                    after: Millis(600),
                },
                Trigger::Swipe {
                    fingers: 1,
                    direction: Direction::Right,
                    travel: crate::config::Distance::Millimetres(12.0),
                    fallback_travel: crate::config::Distance::Normalized(0.04),
                    continuous: false,
                },
            ],
            5000,
        );
        shape_touch(&mut engine, 0, 1, &[(0, 100, 100)], true, false);
        tick(&mut engine, 600);
        shape_touch(&mut engine, 650, 2, &[(0, 900, 100)], false, false);
        shape_touch(&mut engine, 700, 3, &[(0, 900, 100)], false, true);
        assert_eq!(matches(&engine.events).len(), 1);
    }
    #[test]
    fn final_continuous_step_preserves_begin_update_and_end() {
        let mut engine = sequence_engine(
            vec![
                hold(600),
                Trigger::Swipe {
                    fingers: 1,
                    direction: Direction::Right,
                    travel: crate::config::Distance::Millimetres(12.0),
                    fallback_travel: crate::config::Distance::Normalized(0.04),
                    continuous: true,
                },
            ],
            5000,
        );
        key(&mut engine, 2, 0, 1, 115, true);
        tick(&mut engine, 600);
        shape_touch(&mut engine, 610, 1, &[(0, 100, 100)], true, false);
        shape_touch(&mut engine, 740, 2, &[(0, 400, 100)], false, false);
        shape_touch(&mut engine, 780, 3, &[(0, 700, 100)], false, false);
        shape_touch(&mut engine, 800, 4, &[(0, 900, 100)], false, true);
        let matched = matches(&engine.events);
        assert!(matched.iter().any(|event| matches!(
            event,
            Event::Matched {
                stage: Stage::Begin,
                ..
            }
        )));
        assert!(matched.iter().any(|event| matches!(
            event,
            Event::Matched {
                stage: Stage::Update,
                ..
            }
        )));
        assert!(matched.iter().any(|event| matches!(
            event,
            Event::Matched {
                stage: Stage::End,
                ..
            }
        )));
        let Event::Matched { id: first, .. } = matched[0] else {
            unreachable!()
        };
        assert!(
            matched
                .iter()
                .all(|event| matches!(event,Event::Matched {id,..} if id==first))
        );
    }
    #[test]
    fn sampled_sequence_activation_survives_release_but_throughout_cancels() {
        for throughout in [false, true] {
            let mut engine = sequence_engine(
                vec![
                    hold(600),
                    Trigger::ButtonHold {
                        key: Key(30),
                        after: Millis(600),
                        repeat: None,
                    },
                ],
                5000,
            );
            let mut config = engine.config.clone();
            config.rules[0].require_keys = vec![Key(125)];
            config.rules[0].keys_throughout = throughout;
            engine.reload(config, ms(0)).unwrap();
            key(&mut engine, 2, 0, 1, 125, true);
            key(&mut engine, 1, 10, 1, 115, true);
            tick(&mut engine, 610);
            key(&mut engine, 2, 620, 2, 125, false);
            key(&mut engine, 2, 630, 3, 30, true);
            tick(&mut engine, 1230);
            assert_eq!(matches(&engine.events).len(), usize::from(!throughout));
        }
    }
    #[test]
    fn nested_timeout_starts_at_input_and_reset_does_not_reuse_held_keys() {
        let mut engine = sequence_engine(
            vec![
                hold(600),
                Trigger::Sequence {
                    steps: vec![
                        Trigger::ButtonHold {
                            key: Key(30),
                            after: Millis(600),
                            repeat: None,
                        },
                        Trigger::Key { key: Key(125) },
                    ],
                    timeout: Millis(300),
                },
            ],
            5000,
        );
        key(&mut engine, 1, 0, 1, 115, true);
        tick(&mut engine, 600);
        key(&mut engine, 2, 610, 1, 30, true);
        tick(&mut engine, 910);
        assert!(engine.events.iter().any(
            |event| matches!(event,Event::Cancelled {reason,..} if reason=="sequence timeout")
        ));
        tick(&mut engine, 2000);
        key(&mut engine, 2, 2010, 2, 125, true);
        assert!(matches(&engine.events).is_empty());
    }
    #[test]
    fn sequence_context_keeps_the_first_window_across_focus_change() {
        let mut engine = phone();
        let mut config = engine.config.clone();
        config.rules[0].trigger = Trigger::Sequence {
            steps: vec![
                Trigger::TouchHold {
                    fingers: 2,
                    after: Millis(150),
                },
                Trigger::Swipe {
                    fingers: 2,
                    direction: Direction::Up,
                    travel: crate::config::Distance::Millimetres(12.0),
                    fallback_travel: crate::config::Distance::Normalized(0.04),
                    continuous: false,
                },
            ],
            timeout: Millis(1000),
        };
        engine.reload(config, ms(0)).unwrap();
        touch_start(&mut engine, 200);
        tick(&mut engine, 150);
        let generation = engine.authority.generation();
        let mut snap = snapshot(generation, 2, 160);
        snap.windows[0].focused = false;
        snap.windows[1].focused = true;
        engine.authority.update(snap, ms(160)).unwrap();
        frame(
            &mut engine,
            device(1).source,
            180,
            2,
            &[
                (EV_ABS, ABS_MT_SLOT, 0),
                (EV_ABS, ABS_MT_POSITION_Y, 300),
                (EV_ABS, ABS_MT_SLOT, 1),
                (EV_ABS, ABS_MT_POSITION_Y, 300),
            ],
        );
        frame(
            &mut engine,
            device(1).source,
            200,
            3,
            &[
                (EV_ABS, ABS_MT_SLOT, 0),
                (EV_ABS, ABS_MT_TRACKING_ID, -1),
                (EV_ABS, ABS_MT_SLOT, 1),
                (EV_ABS, ABS_MT_TRACKING_ID, -1),
            ],
        );
        assert_eq!(matches(&engine.events).len(), 1);
        assert!(
            matches!(matches(&engine.events)[0],Event::Matched {window:Some(window),..} if window=="a")
        );
    }
    #[test]
    fn event_loss_on_an_already_completed_step_cancels_the_remaining_chain() {
        let mut engine = sequence_engine(
            vec![
                hold(600),
                Trigger::ButtonHold {
                    key: Key(30),
                    after: Millis(600),
                    repeat: None,
                },
            ],
            5000,
        );
        key(&mut engine, 1, 0, 1, 115, true);
        tick(&mut engine, 600);
        frame(
            &mut engine,
            device(1).source,
            650,
            2,
            &[(EV_SYN, SYN_DROPPED, 0)],
        );
        key(&mut engine, 2, 700, 1, 30, true);
        tick(&mut engine, 1300);
        assert!(matches(&engine.events).is_empty());
        assert!(
            engine
                .events
                .iter()
                .any(|event| matches!(event, Event::Cancelled { .. }))
        );
    }
    #[test]
    fn sequence_missing_context_reports_skipped_instead_of_silently_disappearing() {
        let mut config = config(Trigger::Sequence {
            steps: vec![hold(600), taps(2)],
            timeout: Millis(5000),
        });
        config.rules[0].contexts = vec![Context::Desktop];
        let mut engine = engine(config);
        key(&mut engine, 1, 0, 1, 115, true);
        assert!(
            engine
                .events
                .iter()
                .any(|event| matches!(event, Event::Skipped { .. }))
        );
    }
    #[test]
    fn nested_sequences_keep_their_own_timeout() {
        let mut engine = sequence_engine(
            vec![
                hold(600),
                Trigger::Sequence {
                    steps: vec![
                        Trigger::Key { key: Key(30) },
                        Trigger::ButtonHold {
                            key: Key(125),
                            after: Millis(600),
                            repeat: None,
                        },
                    ],
                    timeout: Millis(300),
                },
            ],
            5000,
        );
        key(&mut engine, 1, 0, 1, 115, true);
        tick(&mut engine, 600);
        key(&mut engine, 2, 610, 1, 30, true);
        key(&mut engine, 2, 620, 2, 125, true);
        tick(&mut engine, 1220);
        assert!(matches(&engine.events).is_empty());
        assert!(engine.events.iter().any(
            |event| matches!(event,Event::Cancelled {reason,..} if reason=="sequence timeout")
        ));
    }
}
