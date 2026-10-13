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
    replay::{Observation, Record, Replay},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    time::Duration,
};

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
    Key { down: bool },
    Taps { count: u32 },
    Hold { repeat: bool },
    Chord,
    Touch { space: Space, gesture: TouchGesture },
    Wheel { horizontal: bool, amount: f64 },
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

#[derive(Clone)]
struct DeviceState {
    device: Device,
    mapping: Option<Mapping>,
    keys: BTreeSet<u16>,
    contacts: Vec<Contact>,
}
struct Candidate {
    id: u64,
    ordinal: u64,
    source: Source,
    session_generation: u64,
    permit: Option<Permit>,
    progress: bool,
    expires: Option<Duration>,
}
enum Recognizer {
    Key,
    Taps(Buttons),
    Keys(Keys),
    Touch(BTreeMap<u64, TouchRecognizer>),
    Wheel(BTreeMap<Source, f64>),
}
struct Worker {
    rule: Rule,
    recognizer: Recognizer,
    candidates: BTreeMap<Source, Candidate>,
    touch_active: BTreeSet<Source>,
}

/// Explicit unsupported requirements are returned before observation starts.
/// In particular, accepting a shape/sequence in the schema doesn't mean the
/// configured composition runner is operational yet.
pub fn unsupported(trigger: &Trigger) -> Option<&'static str> {
    match trigger {
        Trigger::Shape { .. } => Some("configured shape capture is not connected yet"),
        Trigger::Sequence { .. } => Some("configured sequence composition is not connected yet"),
        _ => None,
    }
}
impl Worker {
    fn new(rule: Rule, config: &Config) -> io::Result<Self> {
        if let Some(reason) = unsupported(&rule.trigger) {
            return Err(invalid(format!("rules.{}: {reason}", rule.id)));
        }
        let recognizer = match &rule.trigger {
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
            _ => Recognizer::Touch(BTreeMap::new()),
        };
        Ok(Self {
            rule,
            recognizer,
            candidates: BTreeMap::new(),
            touch_active: BTreeSet::new(),
        })
    }
    fn register(&mut self, state: &DeviceState, config: &Config) -> io::Result<()> {
        if !selected(config, &self.rule, &state.device) {
            return Ok(());
        }
        match &mut self.recognizer {
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
            _ => None,
        };
        recognizer
            .into_iter()
            .chain(self.candidates.values().filter_map(|c| c.expires))
            .min()
    }
}

pub struct Engine {
    config: Config,
    generation: u64,
    next_id: u64,
    replay: Replay,
    devices: BTreeMap<u64, DeviceState>,
    conditions: Keys,
    workers: Vec<Worker>,
    authority: Authority,
    events: Vec<Event>,
    expected: Option<Vec<Event>>,
    last_at: Option<Duration>,
}
impl Engine {
    pub fn new(config: Config) -> io::Result<Self> {
        config.validate().map_err(invalid)?;
        let mut workers = config
            .rules
            .iter()
            .filter(|r| r.enabled)
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
        Ok(Self {
            config,
            generation: 1,
            next_id: 0,
            replay: Replay::default(),
            devices: BTreeMap::new(),
            conditions,
            workers,
            authority: Authority::default(),
            events: Vec::new(),
            expected: None,
            last_at: None,
        })
    }
    /// Trusted registration layer only. IPC must verify peer/logind binding
    /// before calling this model; a caller-supplied session name is insufficient.
    pub fn authority_mut(&mut self) -> &mut Authority {
        &mut self.authority
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }
    pub fn deadline(&self) -> Option<Duration> {
        self.workers
            .iter()
            .filter_map(Worker::deadline)
            .chain(self.conditions.deadline())
            .min()
    }
    pub fn push(&mut self, record: Record) -> io::Result<()> {
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
        self.replay.push(record.clone())?;
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
        for observation in observations {
            match observation {
                Observation::Input { frame } => self.frame(frame)?,
                Observation::Cancelled { source, at, reason } => {
                    self.cancel_source(&source, at, &format!("{reason:?}"))
                }
                _ => {}
            }
        }
        if let Record::Tick { at } = record {
            self.advance(at)?;
        }
        if self.events.len() > 16384 {
            return Err(invalid("rule observation queue limit exceeded"));
        }
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
    fn prepare_registration(&self, record: &Record) -> io::Result<()> {
        let state = match record {
            Record::Device {
                device,
                keys,
                contacts,
            } => DeviceState {
                device: device.clone(),
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
    /// Compile before replacing; reconcile held state rather than inventing
    /// presses. Failed reload leaves the previous engine/configuration intact.
    pub fn reload(&mut self, config: Config, at: Duration) -> io::Result<()> {
        let generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| invalid("configuration generation exhausted"))?;
        let mut next = Self::new(config)?;
        next.push(Record::Header {
            version: crate::replay::FORMAT_VERSION,
        })?;
        for state in self.devices.values() {
            next.push(Record::Device {
                device: state.device.clone(),
                keys: state.keys.clone(),
                contacts: state.contacts.clone(),
            })?;
            if let Some(mapping) = &state.mapping {
                next.push(Record::Mapping {
                    source: state.device.source.clone(),
                    at,
                    mapping: mapping.clone(),
                })?;
            }
        }
        self.cancel_all(at, "configuration replaced");
        next.replay.inherit_history(&self.replay);
        next.generation = generation;
        next.next_id = self.next_id;
        next.authority = std::mem::take(&mut self.authority);
        next.events = std::mem::take(&mut self.events);
        next.last_at = Some(at);
        *self = next;
        Ok(())
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
        Ok(())
    }
    fn frame(&mut self, frame: NormalizedFrame) -> io::Result<()> {
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
            if !selected(&self.config, &worker.rule, &state.device) {
                continue;
            }
            validate_candidates(
                worker,
                &self.authority,
                &self.devices,
                &self.conditions,
                at,
                &mut self.events,
            );
            let state = &self.devices[&source.device];
            match &worker.rule.trigger {
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
        Ok(())
    }
    fn cancel_source(&mut self, source: &Source, at: Duration, reason: &str) {
        self.conditions.cancel_source(source);
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
                _ => {}
            }
            worker.touch_active.remove(source);
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

fn selected(config: &Config, rule: &Rule, device: &Device) -> bool {
    !device.virtual_device
        && config.policy.allowed_seats.contains(&device.source.seat)
        && (rule.seats.is_empty() || rule.seats.contains(&device.source.seat))
        && config.devices.iter().any(|(id, selector)| {
            config.policy.allowed_devices.contains(id)
                && (rule.devices.is_empty() || rule.devices.contains(id))
                && selector.matches(device)
        })
}
fn policy_selected(config: &Config, device: &Device) -> bool {
    !device.virtual_device
        && config.policy.allowed_seats.contains(&device.source.seat)
        && config.devices.iter().any(|(id, selector)| {
            config.policy.allowed_devices.contains(id) && selector.matches(device)
        })
}
fn conditions(rule: &Rule, keys: &Keys, seat: &str) -> bool {
    rule.require_keys.iter().all(|key| keys.down(seat, key.0))
        && rule
            .layer
            .as_ref()
            .is_none_or(|layer| keys.layer_active(seat, layer))
}
pub fn requires_context(rule: &Rule) -> bool {
    !context_free(rule)
}
fn context_free(rule: &Rule) -> bool {
    rule.contexts.is_empty()
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
    } else if context_free(&worker.rule) {
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
    let invalids: Vec<_> = worker
        .candidates
        .iter()
        .filter_map(|(source, candidate)| {
            let reason =
                if worker.rule.keys_throughout && !conditions(&worker.rule, keys, &source.seat) {
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
        _ => {}
    }
}
enum Signal {
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
        let proposal = match (&worker.rule.trigger, signal) {
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
            if stage == Stage::End {
                candidate.progress = false;
            }
            if matches!(
                worker.rule.trigger,
                Trigger::ButtonTaps { .. }
                    | Trigger::TouchTaps { .. }
                    | Trigger::Chord { .. }
                    | Trigger::Wheel { .. }
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
    fn t02_runtime_reports_unconnected_composition_instead_of_accepting_it() {
        let trigger = Trigger::Sequence {
            steps: vec![taps(2), hold(2000)],
            timeout: Millis(5000),
        };
        assert!(
            Engine::new(config(trigger))
                .err()
                .unwrap()
                .to_string()
                .contains("composition")
        );
        let mut config = config(Trigger::Shape {
            name: "v".into(),
            fingers: Some(1),
        });
        let shape: ShapeTemplate = crate::paths::demo_templates().remove(0);
        config.shapes = vec![shape];
        assert!(
            Engine::new(config)
                .err()
                .unwrap()
                .to_string()
                .contains("capture")
        );
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
}
