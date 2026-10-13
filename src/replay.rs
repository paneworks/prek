//! Versioned, deterministic offline recording replay. This module has no
//! hardware access, output injection or desktop action capability.

use crate::{
    Button, Buttons, Gesture, Timing,
    gestures::{Mapping, TouchEvent, TouchRecognizer, TouchSettings},
    input::{
        CancelReason, Change, Contact, Device, Frame, KeyState, NormalizedFrame, Normalizer, Source,
    },
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, BufRead, Write},
    time::Duration,
};

pub const FORMAT_VERSION: u32 = 1;
pub const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_RECORDS: usize = 100_000;
pub const MAX_RECORDING_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "record", rename_all = "snake_case", deny_unknown_fields)]
pub enum Record {
    Header {
        version: u32,
    },
    Device {
        device: Device,
        #[serde(default)]
        keys: BTreeSet<u16>,
        #[serde(default)]
        contacts: Vec<Contact>,
    },
    Frame {
        frame: Frame,
    },
    Tick {
        at: Duration,
    },
    Mapping {
        source: Source,
        at: Duration,
        mapping: Mapping,
    },
    /// Kernel state queried after loss; held inputs are not new gestures.
    Reconcile {
        source: Source,
        at: Duration,
        keys: BTreeSet<u16>,
        contacts: Vec<Contact>,
    },
    Remove {
        source: Source,
        at: Duration,
    },
    Expected {
        observations: Vec<Observation>,
    },
    ExpectedRules {
        events: Vec<crate::rules::Event>,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "observation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Observation {
    Input {
        frame: NormalizedFrame,
    },
    Button {
        at: Duration,
        gesture: Gesture,
    },
    Touch {
        event: TouchEvent,
    },
    Cancelled {
        source: Source,
        at: Duration,
        reason: CancelReason,
    },
}

pub struct Replay {
    devices: BTreeMap<u64, (Device, Normalizer)>,
    history: BTreeMap<u64, (String, u64)>,
    buttons: Buttons,
    touches: BTreeMap<u64, TouchRecognizer>,
    last_at: Option<Duration>,
    header_seen: bool,
    observations: Vec<Observation>,
    expected: Option<Vec<Observation>>,
}

impl Default for Replay {
    fn default() -> Self {
        Self::new(Timing::default()).expect("valid default timing")
    }
}

impl Replay {
    pub fn new(timing: Timing) -> Result<Self, &'static str> {
        Ok(Self {
            devices: BTreeMap::new(),
            history: BTreeMap::new(),
            buttons: Buttons::new(timing)?,
            touches: BTreeMap::new(),
            last_at: None,
            header_seen: false,
            observations: Vec::new(),
            expected: None,
        })
    }

    pub fn push(&mut self, record: Record) -> io::Result<()> {
        if !self.header_seen {
            return match record {
                Record::Header {
                    version: FORMAT_VERSION,
                } => {
                    self.header_seen = true;
                    Ok(())
                }
                Record::Header { version } => {
                    Err(invalid(format!("unsupported recording version {version}")))
                }
                _ => Err(invalid("recording must start with a version header")),
            };
        }
        if self.expected.is_some() {
            return Err(invalid("expected outcomes must be the final record"));
        }
        match record {
            Record::Header { .. } => return Err(invalid("duplicate recording header")),
            Record::ExpectedRules { .. } => {
                return Err(invalid("rule expectations require replay --config"));
            }
            Record::Device {
                device,
                keys,
                contacts,
            } => {
                if self.devices.len() >= 256 && !self.devices.contains_key(&device.source.device) {
                    return Err(invalid("too many recording devices"));
                }
                if device.source.seat.is_empty() || device.identity.is_empty() {
                    return Err(invalid("empty source identity or seat"));
                }
                if let Some((identity, generation)) = self.history.get(&device.source.device) {
                    if identity != &device.identity {
                        return Err(invalid("stable device identity hash collision"));
                    }
                    if device.source.generation <= *generation {
                        return Err(invalid("connected device generation must increase"));
                    }
                } else if self.history.len() >= 1024 {
                    return Err(invalid("device generation history limit exceeded"));
                }
                let mut normalizer = Normalizer::new(&device);
                normalizer.reconcile(keys, contacts).map_err(invalid)?;
                let touch = if device.axes.contains_key(&crate::input::ABS_MT_POSITION_X)
                    && device.axes.contains_key(&crate::input::ABS_MT_POSITION_Y)
                {
                    Some(
                        TouchRecognizer::new(
                            device.source.clone(),
                            Mapping::from_device(&device).map_err(invalid)?,
                            TouchSettings::default(),
                        )
                        .map_err(invalid)?,
                    )
                } else {
                    None
                };
                if let Some((old, _)) = self.devices.get(&device.source.device) {
                    if old.identity != device.identity {
                        return Err(invalid("stable device identity hash collision"));
                    }
                    if old.source.generation >= device.source.generation {
                        return Err(invalid("connected device generation must increase"));
                    }
                    self.buttons.cancel_device(device.source.device);
                    if let Some(touch) = self.touches.get_mut(&device.source.device) {
                        self.observations.extend(
                            touch
                                .cancel(
                                    self.last_at.unwrap_or_default(),
                                    CancelReason::SourceChanged,
                                )
                                .into_iter()
                                .map(|event| Observation::Touch { event }),
                        );
                    }
                    self.observations.push(Observation::Cancelled {
                        source: old.source.clone(),
                        at: self.last_at.unwrap_or_default(),
                        reason: CancelReason::SourceChanged,
                    });
                }
                self.touches.remove(&device.source.device);
                if let Some(touch) = touch {
                    self.touches.insert(device.source.device, touch);
                }
                self.devices
                    .insert(device.source.device, (device.clone(), normalizer));
                self.history.insert(
                    device.source.device,
                    (device.identity, device.source.generation),
                );
            }
            Record::Frame { frame } => {
                let source = frame.source.clone();
                let at = frame.at;
                // A delayed frame from a previous reader must not cancel the
                // replacement device or advance its timeline.
                self.validate_source(&source)?;
                if self.last_at.is_some_and(|previous| at < previous) {
                    self.cancel_all(at, CancelReason::BackwardTime);
                    self.last_at = Some(at);
                    return Ok(());
                }
                self.last_at = Some(at);
                let Some((_, normalizer)) = self.devices.get_mut(&source.device) else {
                    return Err(invalid("frame from an unregistered device"));
                };
                match normalizer.process(frame) {
                    Ok(frame) => {
                        let mut gestures = Vec::new();
                        for change in &frame.changes {
                            if let Change::Key {
                                code,
                                state: KeyState::Press | KeyState::Release,
                            } = *change
                            {
                                gestures.extend(self.buttons.update(
                                    Button {
                                        device: source.device,
                                        code,
                                    },
                                    matches!(
                                        change,
                                        Change::Key {
                                            state: KeyState::Press,
                                            ..
                                        }
                                    ),
                                    at,
                                ));
                            }
                        }
                        let touch = self
                            .touches
                            .get_mut(&source.device)
                            .map(|touch| touch.process(&frame))
                            .unwrap_or_default();
                        self.observations.push(Observation::Input { frame });
                        self.observations.extend(
                            gestures
                                .into_iter()
                                .map(|gesture| Observation::Button { at, gesture }),
                        );
                        self.observations
                            .extend(touch.into_iter().map(|event| Observation::Touch { event }));
                    }
                    Err(reason) => {
                        self.buttons.cancel_device(source.device);
                        if let Some(touch) = self.touches.get_mut(&source.device) {
                            self.observations.extend(
                                touch
                                    .cancel(at, reason)
                                    .into_iter()
                                    .map(|event| Observation::Touch { event }),
                            );
                        }
                        self.observations
                            .push(Observation::Cancelled { source, at, reason });
                    }
                }
            }
            Record::Tick { at } => {
                if self.last_at.is_some_and(|previous| at < previous) {
                    self.cancel_all(at, CancelReason::BackwardTime);
                } else {
                    self.observations.extend(
                        self.buttons
                            .advance(at)
                            .into_iter()
                            .map(|gesture| Observation::Button { at, gesture }),
                    );
                    for touch in self.touches.values_mut() {
                        self.observations.extend(
                            touch
                                .advance(at)
                                .into_iter()
                                .map(|event| Observation::Touch { event }),
                        );
                    }
                }
                self.last_at = Some(at);
            }
            Record::Mapping {
                source,
                at,
                mapping,
            } => {
                self.validate_source(&source)?;
                if self.advance_clock(at) {
                    let touch = self
                        .touches
                        .get_mut(&source.device)
                        .ok_or_else(|| invalid("mapping requires a touch device"))?;
                    self.observations.extend(
                        touch
                            .replace_mapping(mapping, at)
                            .map_err(invalid)?
                            .into_iter()
                            .map(|event| Observation::Touch { event }),
                    );
                }
            }
            Record::Reconcile {
                source,
                at,
                keys,
                contacts,
            } => {
                self.validate_source(&source)?;
                if self.advance_clock(at) {
                    let held = !contacts.is_empty();
                    self.devices
                        .get_mut(&source.device)
                        .expect("registered source")
                        .1
                        .reconcile(keys, contacts)
                        .map_err(invalid)?;
                    self.buttons.cancel_device(source.device);
                    if let Some(touch) = self.touches.get_mut(&source.device) {
                        self.observations.extend(
                            touch
                                .reconcile(at, held)
                                .into_iter()
                                .map(|event| Observation::Touch { event }),
                        );
                    }
                }
            }
            Record::Remove { source, at } => {
                let Some((device, _)) = self.devices.get(&source.device) else {
                    return Err(invalid("removing an unregistered source"));
                };
                if device.source != source {
                    return Err(invalid("removing a stale device generation"));
                }
                self.advance_clock(at);
                self.devices.remove(&source.device);
                if let Some(mut touch) = self.touches.remove(&source.device) {
                    self.observations.extend(
                        touch
                            .cancel(at, CancelReason::Disconnected)
                            .into_iter()
                            .map(|event| Observation::Touch { event }),
                    );
                }
                self.buttons.cancel_device(source.device);
                self.observations.push(Observation::Cancelled {
                    source,
                    at,
                    reason: CancelReason::Disconnected,
                });
            }
            Record::Expected { observations } => self.expected = Some(observations),
        }
        if self.observations.len() > MAX_RECORDS * 4 {
            return Err(invalid("recording outcome limit exceeded"));
        }
        Ok(())
    }

    pub fn observations(&self) -> &[Observation] {
        &self.observations
    }
    pub(crate) fn inherit_history(&mut self, previous: &Replay) {
        self.history = previous.history.clone();
    }
    /// Live observation drains results so memory does not grow with uptime.
    pub fn take_observations(&mut self) -> Vec<Observation> {
        std::mem::take(&mut self.observations)
    }
    pub fn deadline(&self) -> Option<Duration> {
        self.buttons
            .deadline()
            .into_iter()
            .chain(self.touches.values().filter_map(TouchRecognizer::deadline))
            .min()
    }

    pub fn check(&self) -> io::Result<()> {
        let expected = self.expected.as_ref().ok_or_else(|| invalid("--check requires a final expected-outcomes record; recording alone is not a passing test"))?;
        if expected.len() == self.observations.len()
            && expected
                .iter()
                .zip(&self.observations)
                .all(|(a, b)| same_observation(a, b))
        {
            return Ok(());
        }
        let index = expected
            .iter()
            .zip(&self.observations)
            .position(|(a, b)| !same_observation(a, b))
            .unwrap_or(expected.len().min(self.observations.len()));
        Err(invalid(format!(
            "outcome mismatch at observation {index}: expected {} observations, received {}",
            expected.len(),
            self.observations.len()
        )))
    }

    fn cancel_all(&mut self, at: Duration, reason: CancelReason) {
        for (id, (device, normalizer)) in &mut self.devices {
            self.buttons.cancel_device(*id);
            normalizer.cancel();
            if let Some(touch) = self.touches.get_mut(id) {
                self.observations.extend(
                    touch
                        .cancel(at, reason)
                        .into_iter()
                        .map(|event| Observation::Touch { event }),
                );
            }
            self.observations.push(Observation::Cancelled {
                source: device.source.clone(),
                at,
                reason,
            });
        }
    }
    fn validate_source(&self, source: &Source) -> io::Result<()> {
        if self
            .devices
            .get(&source.device)
            .is_none_or(|(device, _)| device.source != *source)
        {
            Err(invalid("unregistered or stale source generation"))
        } else {
            Ok(())
        }
    }
    fn advance_clock(&mut self, at: Duration) -> bool {
        let valid = self.last_at.is_none_or(|last| at >= last);
        if !valid {
            self.cancel_all(at, CancelReason::BackwardTime);
        }
        self.last_at = Some(at);
        valid
    }
}

fn same_observation(a: &Observation, b: &Observation) -> bool {
    if let (Observation::Touch { event: a }, Observation::Touch { event: b }) = (a, b) {
        // Structural fields, counters and timestamps remain exact. Only float
        // geometry permits roundoff across handwritten fixtures and CPUs.
        let a = serde_json::to_value(a).expect("finite touch geometry");
        let b = serde_json::to_value(b).expect("finite touch geometry");
        similar_geometry(&a, &b)
    } else {
        a == b
    }
}
pub(crate) fn similar_geometry(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    use serde_json::Value;
    match (a, b) {
        (Value::Number(a), Value::Number(b)) if a.is_f64() && b.is_f64() => {
            let a = a.as_f64().expect("float");
            let b = b.as_f64().expect("float");
            (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0)
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter().all(|(key, value)| {
                    b.get(key)
                        .is_some_and(|other| similar_geometry(value, other))
                })
        }
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| similar_geometry(a, b))
        }
        _ => a == b,
    }
}

pub fn write_record(output: &mut impl Write, record: &Record) -> io::Result<()> {
    serde_json::to_writer(&mut *output, record).map_err(io::Error::other)?;
    output.write_all(b"\n")
}

pub fn read(input: &mut impl BufRead, check: bool) -> io::Result<Replay> {
    let mut replay = Replay::default();
    visit(input, |record| replay.push(record))?;
    if !replay.header_seen {
        return Err(invalid("empty recording"));
    }
    if check {
        replay.check()?;
    }
    Ok(replay)
}

/// One bounded decoder for generic and configured replay. It performs no I/O
/// beyond this supplied reader and preserves exact line locations on errors.
pub fn visit(
    input: &mut impl BufRead,
    mut receive: impl FnMut(Record) -> io::Result<()>,
) -> io::Result<()> {
    let mut count = 0;
    let mut total = 0usize;
    loop {
        let mut bytes = Vec::new();
        let mut limited = io::Read::take(&mut *input, (MAX_LINE_BYTES + 1) as u64);
        let length = limited.read_until(b'\n', &mut bytes)?;
        if length == 0 {
            break;
        }
        count += 1;
        total = total.saturating_add(length);
        if length > MAX_LINE_BYTES || count > MAX_RECORDS || total > MAX_RECORDING_BYTES {
            return Err(invalid(format!(
                "recording size limit exceeded at line {count}"
            )));
        }
        let record = serde_json::from_slice(&bytes)
            .map_err(|error| invalid(format!("recording line {count}: {error}")))?;
        receive(record).map_err(|error| invalid(format!("recording line {count}: {error}")))?;
    }
    if count == 0 {
        return Err(invalid("empty recording"));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{EV_KEY, EV_SYN, RawEvent, SYN_REPORT};
    fn device() -> Device {
        Device {
            identity: "keyboard".into(),
            source: Source {
                device: 7,
                generation: 1,
                seat: "seat0".into(),
            },
            name: "test".into(),
            path: "test".into(),
            physical: "test".into(),
            unique: String::new(),
            vendor: 1,
            product: 1,
            properties: vec![],
            capabilities: BTreeMap::new(),
            axes: BTreeMap::new(),
            virtual_device: false,
        }
    }
    fn frame(sequence: u64, millis: u64, value: i32) -> Record {
        Record::Frame {
            frame: Frame {
                source: device().source,
                at: Duration::from_millis(millis),
                sequence,
                events: vec![
                    RawEvent {
                        kind: EV_KEY,
                        code: 30,
                        value,
                    },
                    RawEvent {
                        kind: EV_SYN,
                        code: SYN_REPORT,
                        value: 0,
                    },
                ],
            },
        }
    }
    #[test]
    fn t05_replay_is_deterministic_and_checks_exact_outcomes() {
        let records = vec![
            Record::Header { version: 1 },
            Record::Device {
                device: device(),
                keys: BTreeSet::new(),
                contacts: vec![],
            },
            frame(1, 0, 1),
            frame(2, 50, 0),
            Record::Tick {
                at: Duration::from_millis(350),
            },
        ];
        let mut reference = Replay::default();
        for record in &records {
            reference.push(record.clone()).unwrap();
        }
        let mut bytes = Vec::new();
        for record in &records {
            write_record(&mut bytes, record).unwrap();
        }
        write_record(
            &mut bytes,
            &Record::Expected {
                observations: reference.observations().to_vec(),
            },
        )
        .unwrap();
        for _ in 0..3 {
            let replay = read(&mut io::Cursor::new(&bytes), true).unwrap();
            assert_eq!(replay.observations(), reference.observations());
        }
        assert!(matches!(
            reference.observations().last(),
            Some(Observation::Button {
                gesture: Gesture::Taps { count: 1, .. },
                ..
            })
        ));
    }
    #[test]
    fn t05_check_does_not_treat_missing_expectations_as_success() {
        let mut bytes = Vec::new();
        write_record(&mut bytes, &Record::Header { version: 1 }).unwrap();
        assert!(read(&mut io::Cursor::new(bytes), true).is_err());
    }
    #[test]
    fn t14_disconnect_cancels_pending_hold() {
        let mut replay = Replay::default();
        for record in [
            Record::Header { version: 1 },
            Record::Device {
                device: device(),
                keys: BTreeSet::new(),
                contacts: vec![],
            },
            frame(1, 0, 1),
            Record::Remove {
                source: device().source,
                at: Duration::from_millis(500),
            },
            Record::Tick {
                at: Duration::from_secs(2),
            },
        ] {
            replay.push(record).unwrap();
        }
        assert!(
            !replay
                .observations()
                .iter()
                .any(|o| matches!(o, Observation::Button { .. }))
        );
    }
    #[test]
    fn t71_removed_device_generation_is_not_reusable_and_old_frames_do_not_cancel_replacement() {
        let mut replay = Replay::default();
        replay.push(Record::Header { version: 1 }).unwrap();
        let register = |device| Record::Device {
            device,
            keys: BTreeSet::new(),
            contacts: vec![],
        };
        replay.push(register(device())).unwrap();
        replay
            .push(Record::Remove {
                source: device().source,
                at: Duration::ZERO,
            })
            .unwrap();
        assert!(replay.push(register(device())).is_err());
        let mut replacement = device();
        replacement.source.generation = 2;
        replay.push(register(replacement.clone())).unwrap();
        let mut press = frame(1, 0, 1);
        if let Record::Frame { frame } = &mut press {
            frame.source = replacement.source;
        }
        replay.push(press).unwrap();
        assert!(replay.push(frame(2, 2000, 0)).is_err());
        replay
            .push(Record::Tick {
                at: Duration::from_millis(600),
            })
            .unwrap();
        assert!(matches!(
            replay.observations().last(),
            Some(Observation::Button {
                gesture: Gesture::Hold { .. },
                ..
            })
        ));
    }
    #[test]
    fn t13_existing_held_key_does_not_become_a_hold_or_tap() {
        let mut replay = Replay::default();
        for record in [
            Record::Header { version: 1 },
            Record::Device {
                device: device(),
                keys: BTreeSet::from([30]),
                contacts: vec![],
            },
            Record::Tick {
                at: Duration::from_secs(1),
            },
            frame(1, 1001, 0),
            Record::Tick {
                at: Duration::from_secs(2),
            },
        ] {
            replay.push(record).unwrap();
        }
        assert!(
            !replay
                .observations()
                .iter()
                .any(|o| matches!(o, Observation::Button { .. }))
        );
    }
    #[test]
    fn t11_reconnect_has_an_explicit_generation_barrier() {
        let mut replay = Replay::default();
        replay.push(Record::Header { version: 1 }).unwrap();
        replay
            .push(Record::Device {
                device: device(),
                keys: BTreeSet::new(),
                contacts: vec![],
            })
            .unwrap();
        replay.push(frame(1, 0, 1)).unwrap();
        let mut reconnected = device();
        reconnected.source.generation = 2;
        replay
            .push(Record::Device {
                device: reconnected,
                keys: BTreeSet::new(),
                contacts: vec![],
            })
            .unwrap();
        replay
            .push(Record::Tick {
                at: Duration::from_secs(2),
            })
            .unwrap();
        assert!(matches!(
            replay.observations().last(),
            Some(Observation::Cancelled {
                reason: CancelReason::SourceChanged,
                ..
            })
        ));
    }
    fn touch_device() -> Device {
        use crate::input::*;
        let mut device = device();
        device.identity = "touch".into();
        let axis = |maximum, resolution| Axis {
            minimum: 0,
            maximum,
            resolution,
            fuzz: 0,
            flat: 0,
        };
        device.axes = BTreeMap::from([
            (ABS_MT_SLOT, axis(9, 0)),
            (ABS_MT_POSITION_X, axis(1000, 10)),
            (ABS_MT_POSITION_Y, axis(2000, 10)),
        ]);
        device
    }
    fn touch_frame(sequence: u64, millis: u64, down: bool) -> Frame {
        use crate::input::*;
        let mut events = Vec::new();
        for slot in 0..2 {
            events.push(RawEvent {
                kind: EV_ABS,
                code: ABS_MT_SLOT,
                value: slot,
            });
            events.push(RawEvent {
                kind: EV_ABS,
                code: ABS_MT_TRACKING_ID,
                value: if down { 10 + slot } else { -1 },
            });
            if down {
                events.push(RawEvent {
                    kind: EV_ABS,
                    code: ABS_MT_POSITION_X,
                    value: 200 + slot * 200,
                });
                events.push(RawEvent {
                    kind: EV_ABS,
                    code: ABS_MT_POSITION_Y,
                    value: 400,
                });
            }
        }
        events.push(RawEvent {
            kind: EV_SYN,
            code: SYN_REPORT,
            value: 0,
        });
        Frame {
            source: touch_device().source,
            at: Duration::from_millis(millis),
            sequence,
            events,
        }
    }
    #[test]
    fn t05_touch_replay_checks_a_manual_expected_outcome() {
        use crate::{
            gestures::{Point, Space, TouchGesture},
            input::ContactId,
        };
        let device = touch_device();
        let down = touch_frame(1, 0, true);
        let up = touch_frame(2, 50, false);
        let contacts: Vec<_> = (0..2)
            .map(|slot| Contact {
                id: ContactId {
                    slot,
                    tracking: 10 + i32::from(slot),
                    generation: u64::from(slot) + 1,
                },
                x: 200 + i32::from(slot) * 200,
                y: 400,
                pressure: None,
                fresh: true,
            })
            .collect();
        let ids: Vec<_> = contacts.iter().map(|c| c.id).collect();
        // The fixture is specified independently of the recognizer's output.
        let expected = vec![
            Observation::Input {
                frame: NormalizedFrame {
                    raw: down.clone(),
                    changes: down
                        .events
                        .iter()
                        .filter(|event| event.kind == crate::input::EV_ABS)
                        .map(|event| Change::Absolute {
                            code: event.code,
                            value: event.value,
                        })
                        .collect(),
                    contacts: contacts.clone(),
                    contacts_started: ids.clone(),
                    contacts_ended: vec![],
                    ended_contacts: vec![],
                },
            },
            Observation::Input {
                frame: NormalizedFrame {
                    raw: up.clone(),
                    changes: up
                        .events
                        .iter()
                        .filter(|event| event.kind == crate::input::EV_ABS)
                        .map(|event| Change::Absolute {
                            code: event.code,
                            value: event.value,
                        })
                        .collect(),
                    contacts: vec![],
                    contacts_started: vec![],
                    contacts_ended: ids,
                    ended_contacts: contacts,
                },
            },
            Observation::Touch {
                event: TouchEvent {
                    source: device.source.clone(),
                    id: 1,
                    at: Duration::from_millis(350),
                    space: Space::Normalized,
                    gesture: TouchGesture::Taps {
                        fingers: 2,
                        count: 1,
                        position: Point { x: 0.3, y: 0.2 },
                    },
                },
            },
        ];
        let records = [
            Record::Header { version: 1 },
            Record::Device {
                device,
                keys: BTreeSet::new(),
                contacts: vec![],
            },
            Record::Frame { frame: down },
            Record::Frame { frame: up },
            Record::Tick {
                at: Duration::from_millis(350),
            },
            Record::Expected {
                observations: expected,
            },
        ];
        let mut recording = Vec::new();
        for record in &records {
            write_record(&mut recording, record).unwrap();
        }
        let replay = read(&mut io::Cursor::new(&recording), true).unwrap();
        assert_eq!(replay.observations().len(), 3);
    }
    #[test]
    fn t17_touch_loss_reconciliation_does_not_drop_next_fresh_group() {
        let mut replay = Replay::default();
        replay.push(Record::Header { version: 1 }).unwrap();
        replay
            .push(Record::Device {
                device: touch_device(),
                keys: BTreeSet::new(),
                contacts: vec![],
            })
            .unwrap();
        replay
            .push(Record::Frame {
                frame: touch_frame(1, 0, true),
            })
            .unwrap();
        replay
            .push(Record::Reconcile {
                source: touch_device().source,
                at: Duration::from_millis(50),
                keys: BTreeSet::new(),
                contacts: vec![],
            })
            .unwrap();
        replay
            .push(Record::Frame {
                frame: touch_frame(2, 100, true),
            })
            .unwrap();
        replay
            .push(Record::Frame {
                frame: touch_frame(3, 150, false),
            })
            .unwrap();
        replay
            .push(Record::Tick {
                at: Duration::from_millis(450),
            })
            .unwrap();
        assert!(matches!(
            replay.observations().last(),
            Some(Observation::Touch {
                event: TouchEvent {
                    gesture: crate::gestures::TouchGesture::Taps {
                        fingers: 2,
                        count: 1,
                        ..
                    },
                    ..
                }
            })
        ));
    }
    #[test]
    fn t19_mapping_record_rejects_stale_generation() {
        let mut replay = Replay::default();
        replay.push(Record::Header { version: 1 }).unwrap();
        let device = touch_device();
        replay
            .push(Record::Device {
                device: device.clone(),
                keys: BTreeSet::new(),
                contacts: vec![],
            })
            .unwrap();
        let mut source = device.source.clone();
        source.generation = 2;
        assert!(
            replay
                .push(Record::Mapping {
                    source,
                    at: Duration::ZERO,
                    mapping: Mapping::from_device(&device).unwrap()
                })
                .is_err()
        );
    }
    #[test]
    fn t05_geometry_roundoff_does_not_relax_ids_or_counts() {
        use crate::gestures::{Point, Space, TouchGesture};
        let original = Observation::Touch {
            event: TouchEvent {
                source: touch_device().source,
                id: 1,
                at: Duration::ZERO,
                space: Space::Normalized,
                gesture: TouchGesture::Taps {
                    fingers: 2,
                    count: 1,
                    position: Point { x: 0.3, y: 0.2 },
                },
            },
        };
        let mut roundoff = original.clone();
        if let Observation::Touch { event } = &mut roundoff
            && let TouchGesture::Taps { position, .. } = &mut event.gesture
        {
            position.x += 1e-15;
        }
        assert!(same_observation(&original, &roundoff));
        if let Observation::Touch { event } = &mut roundoff {
            event.id = 2;
        }
        assert!(!same_observation(&original, &roundoff));
        let mut different = original.clone();
        if let Observation::Touch { event } = &mut different
            && let TouchGesture::Taps { count, .. } = &mut event.gesture
        {
            *count = 2;
        }
        assert!(!same_observation(&original, &different));
    }
}
