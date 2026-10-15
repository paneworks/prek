//! Hardware-independent frames and state reconciliation.
//!
//! Raw events are retained for eventual forwarding; normalized changes are for
//! recognition. A contact identity is never just its reusable Linux slot.

use serde::{Deserialize, Serialize};
mod absolute;
pub(crate) use absolute::per_slot;
pub use absolute::{AbsoluteSlot, AbsoluteState};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

pub const EV_SYN: u16 = 0;
pub const EV_KEY: u16 = 1;
pub const EV_REL: u16 = 2;
pub const EV_ABS: u16 = 3;
pub const EV_SW: u16 = 5;
pub const SYN_REPORT: u16 = 0;
pub const SYN_DROPPED: u16 = 3;
pub const ABS_MT_SLOT: u16 = 0x2f;
pub const ABS_MT_POSITION_X: u16 = 0x35;
pub const ABS_MT_POSITION_Y: u16 = 0x36;
pub const ABS_MT_TRACKING_ID: u16 = 0x39;
pub const ABS_MT_PRESSURE: u16 = 0x3a;
pub const REL_WHEEL: u16 = 8;
pub const REL_HWHEEL: u16 = 6;
pub const REL_WHEEL_HI_RES: u16 = 11;
pub const REL_HWHEEL_HI_RES: u16 = 12;

/// Identity assigned from stable discovery properties, with a fresh generation
/// on every connection. Seat is part of the identity, not caller action context.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub device: u64,
    pub generation: u64,
    pub seat: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RawEvent {
    pub kind: u16,
    pub code: u16,
    pub value: i32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Frame {
    pub source: Source,
    pub at: Duration,
    pub sequence: u64,
    pub events: Vec<RawEvent>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Axis {
    pub minimum: i32,
    pub maximum: i32,
    pub resolution: i32,
    pub fuzz: i32,
    pub flat: i32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Device {
    pub identity: String,
    pub source: Source,
    pub name: String,
    pub path: String,
    pub physical: String,
    pub unique: String,
    pub vendor: u16,
    pub product: u16,
    pub properties: Vec<u16>,
    #[serde(with = "numeric_map")]
    pub capabilities: BTreeMap<u16, Vec<u16>>,
    #[serde(with = "numeric_map")]
    pub axes: BTreeMap<u16, Axis>,
    pub virtual_device: bool,
}

// JSON object keys are strings. Parse them explicitly so metadata also works
// inside serde's internally tagged recording enum, which buffers map keys.
pub(crate) mod numeric_map {
    use super::*;
    pub fn serialize<S: serde::Serializer, V: Serialize>(
        value: &BTreeMap<u16, V>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value.serialize(serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>, V: Deserialize<'de>>(
        deserializer: D,
    ) -> Result<BTreeMap<u16, V>, D::Error> {
        let values = BTreeMap::<String, V>::deserialize(deserializer)?;
        values
            .into_iter()
            .map(|(key, value)| {
                let code: u16 = key.parse().map_err(serde::de::Error::custom)?;
                if key != code.to_string() {
                    return Err(serde::de::Error::custom(
                        "event-code keys require canonical unsigned decimal integers",
                    ));
                }
                Ok((code, value))
            })
            .collect()
    }
}

impl Device {
    /// Conservative direct-touch capacity; an absent slot axis is unknown.
    pub fn touch_capacity(&self) -> Option<usize> {
        self.axes.get(&ABS_MT_SLOT).and_then(|axis| {
            let capacity = i64::from(axis.maximum) - i64::from(axis.minimum) + 1;
            (capacity > 0 && capacity <= 256).then_some(capacity as usize)
        })
    }

    pub fn has(&self, kind: u16, code: u16) -> bool {
        self.capabilities
            .get(&kind)
            .is_some_and(|codes| codes.contains(&code))
    }
}

/// Deterministic across architectures and program runs; collision detection is
/// still required when registering a device catalog.
pub fn identity_number(identity: &str) -> u64 {
    identity.bytes().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    })
}

pub fn is_button(code: u16) -> bool {
    // Tool-count, pen-tool and contact flags describe state, not timed presses.
    !matches!(code, 0x140..=0x148 | 0x14a | 0x14d..=0x14f)
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyState {
    Press,
    Release,
    Repeat,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Change {
    Key {
        code: u16,
        state: KeyState,
    },
    Relative {
        code: u16,
        value: i32,
    },
    Absolute {
        code: u16,
        value: i32,
    },
    Switch {
        code: u16,
        on: bool,
    },
    Wheel {
        horizontal: bool,
        steps: f64,
        high_resolution: bool,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContactId {
    pub slot: u16,
    pub tracking: i32,
    pub generation: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Contact {
    pub id: ContactId,
    pub x: i32,
    pub y: i32,
    pub pressure: Option<i32>,
    /// False for a contact found already held during reconciliation.
    pub fresh: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NormalizedFrame {
    pub raw: Frame,
    pub changes: Vec<Change>,
    pub contacts: Vec<Contact>,
    pub contacts_started: Vec<ContactId>,
    pub contacts_ended: Vec<ContactId>,
    /// Last positions, including motion in the same frame as a release.
    #[serde(default)]
    pub ended_contacts: Vec<Contact>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelReason {
    EventLoss,
    BackwardTime,
    InvalidFrame,
    SourceChanged,
    Disconnected,
    MappingChanged,
}

pub type FrameResult = Result<NormalizedFrame, CancelReason>;

#[derive(Clone, Debug, Default)]
struct Slot {
    id: Option<ContactId>,
    x: Option<i32>,
    y: Option<i32>,
    pressure: Option<i32>,
    fresh: bool,
}

/// Normalizes one connected event source. After loss, callers must reconcile
/// current kernel state before feeding new frames; silently guessing is unsafe.
#[derive(Debug)]
pub struct Normalizer {
    source: Source,
    last_at: Option<Duration>,
    last_sequence: Option<u64>,
    keys: BTreeSet<u16>,
    switches: BTreeMap<u16, bool>,
    slots: BTreeMap<u16, Slot>,
    active_slot: u16,
    slot_range: Option<(u16, u16)>,
    next_contact: u64,
    synchronized: bool,
    absolute: AbsoluteState,
    axes: BTreeMap<u16, Axis>,
}

impl Normalizer {
    pub fn new(device: &Device) -> Self {
        let slot_range = device.axes.get(&ABS_MT_SLOT).and_then(|axis| {
            let minimum = u16::try_from(axis.minimum).ok()?;
            let maximum = u16::try_from(axis.maximum).ok()?;
            (maximum >= minimum && maximum - minimum < 256).then_some((minimum, maximum))
        });
        let mut absolute = AbsoluteState::default();
        if let Some((minimum, _)) = slot_range {
            absolute.values.insert(ABS_MT_SLOT, i32::from(minimum));
        }
        Self {
            source: device.source.clone(),
            last_at: None,
            last_sequence: None,
            keys: BTreeSet::new(),
            switches: BTreeMap::new(),
            slots: BTreeMap::new(),
            active_slot: slot_range.map_or(0, |(minimum, _)| minimum),
            slot_range,
            next_contact: 0,
            synchronized: true,
            absolute,
            axes: device.axes.clone(),
        }
    }

    /// Seed current state without emitting invented presses or gesture starts.
    pub fn reconcile(
        &mut self,
        keys: BTreeSet<u16>,
        contacts: Vec<Contact>,
    ) -> Result<(), &'static str> {
        // Build the replacement first. An invalid snapshot must not erase a
        // working source or silently overwrite duplicated slots/tracking IDs.
        if keys.len() > 768 || keys.iter().any(|key| *key > 0x2ff) || contacts.len() > 256 {
            return Err("reconciliation exceeds device-state bounds");
        }
        let mut slots = BTreeMap::new();
        let mut tracking = BTreeSet::new();
        let mut generations = BTreeSet::new();
        let mut next_contact = self.next_contact;
        for contact in contacts {
            if !self.valid_slot(contact.id.slot)
                || contact.id.tracking < 0
                || contact.id.generation == 0
                || contact.id.generation == u64::MAX
                || slots.contains_key(&contact.id.slot)
                || !tracking.insert(contact.id.tracking)
                || !generations.insert(contact.id.generation)
            {
                return Err("invalid contact in reconciliation");
            }
            next_contact = next_contact.max(contact.id.generation);
            slots.insert(
                contact.id.slot,
                Slot {
                    id: Some(contact.id),
                    x: Some(contact.x),
                    y: Some(contact.y),
                    pressure: contact.pressure,
                    fresh: false,
                },
            );
        }
        self.clear();
        self.keys = keys;
        self.slots = slots;
        for (&number, slot) in &self.slots {
            for (code, value) in [
                (ABS_MT_POSITION_X, slot.x),
                (ABS_MT_POSITION_Y, slot.y),
                (ABS_MT_PRESSURE, slot.pressure),
            ] {
                if self.axes.contains_key(&code)
                    && let Some(value) = value
                {
                    self.absolute
                        .slots
                        .entry(number)
                        .or_default()
                        .values
                        .insert(code, value);
                }
            }
        }
        self.next_contact = next_contact;
        self.synchronized = true;
        Ok(())
    }

    pub fn cancel(&mut self) {
        self.clear();
        self.synchronized = false;
    }

    pub fn seed_absolute(
        &mut self,
        device: &Device,
        state: AbsoluteState,
    ) -> Result<(), &'static str> {
        if device.source != self.source || self.last_sequence.is_some() || !self.synchronized {
            return Err("absolute state requires a freshly registered or reconciled source");
        }
        state.validate(&device.axes)?;
        for (&number, coordinates) in &state.slots {
            let slot = self.slots.entry(number).or_default();
            if let Some(&value) = coordinates.values.get(&ABS_MT_POSITION_X) {
                slot.x = Some(value);
            }
            if let Some(&value) = coordinates.values.get(&ABS_MT_POSITION_Y) {
                slot.y = Some(value);
            }
            if let Some(&value) = coordinates.values.get(&ABS_MT_PRESSURE) {
                slot.pressure = Some(value);
            }
        }
        self.active_slot = state.selected_slot();
        self.absolute = state;
        Ok(())
    }

    pub fn absolute_state(&self) -> &AbsoluteState {
        &self.absolute
    }

    pub fn process(&mut self, frame: Frame) -> FrameResult {
        if frame.source != self.source {
            return self.reject(CancelReason::SourceChanged);
        }
        if self.last_at.is_some_and(|last| frame.at < last) {
            return self.reject(CancelReason::BackwardTime);
        }
        if self
            .last_sequence
            .is_some_and(|last| frame.sequence <= last)
            || frame.events.len() > 4096
            || frame
                .events
                .last()
                .is_none_or(|event| event.kind != EV_SYN || event.code != SYN_REPORT)
            || frame.events[..frame.events.len().saturating_sub(1)]
                .iter()
                .any(|event| event.kind == EV_SYN && event.code == SYN_REPORT)
        {
            return self.reject(CancelReason::InvalidFrame);
        }
        if !self.synchronized
            || frame
                .events
                .iter()
                .any(|event| event.kind == EV_SYN && event.code == SYN_DROPPED)
        {
            return self.reject(CancelReason::EventLoss);
        }
        self.last_at = Some(frame.at);
        self.last_sequence = Some(frame.sequence);
        let before: BTreeSet<_> = self.slots.values().filter_map(|slot| slot.id).collect();
        let mut changes = Vec::new();
        let mut ended_contacts = BTreeMap::new();
        let mut wheel = BTreeMap::<u16, i64>::new();
        for event in &frame.events {
            match event.kind {
                EV_KEY if is_button(event.code) => match event.value {
                    1 if self.keys.insert(event.code) => changes.push(Change::Key {
                        code: event.code,
                        state: KeyState::Press,
                    }),
                    0 if self.keys.remove(&event.code) => changes.push(Change::Key {
                        code: event.code,
                        state: KeyState::Release,
                    }),
                    2 if self.keys.contains(&event.code) => changes.push(Change::Key {
                        code: event.code,
                        state: KeyState::Repeat,
                    }),
                    0..=2 => {}
                    _ => return self.reject(CancelReason::InvalidFrame),
                },
                EV_REL
                    if matches!(
                        event.code,
                        REL_WHEEL | REL_HWHEEL | REL_WHEEL_HI_RES | REL_HWHEEL_HI_RES
                    ) =>
                {
                    *wheel.entry(event.code).or_default() += i64::from(event.value);
                }
                EV_REL => changes.push(Change::Relative {
                    code: event.code,
                    value: event.value,
                }),
                EV_SW => {
                    if !matches!(event.value, 0 | 1) {
                        return self.reject(CancelReason::InvalidFrame);
                    }
                    let on = event.value == 1;
                    if self.switches.insert(event.code, on) != Some(on) {
                        changes.push(Change::Switch {
                            code: event.code,
                            on,
                        });
                    }
                }
                EV_ABS => {
                    if event.code > 0x3f {
                        return self.reject(CancelReason::InvalidFrame);
                    }
                    changes.push(Change::Absolute {
                        code: event.code,
                        value: event.value,
                    });
                    match event.code {
                        ABS_MT_SLOT => {
                            let Ok(slot) = u16::try_from(event.value) else {
                                return self.reject(CancelReason::InvalidFrame);
                            };
                            if !self.valid_slot(slot) {
                                return self.reject(CancelReason::InvalidFrame);
                            }
                            self.active_slot = slot;
                        }
                        ABS_MT_TRACKING_ID | ABS_MT_POSITION_X | ABS_MT_POSITION_Y
                        | ABS_MT_PRESSURE => {
                            if !self.valid_slot(self.active_slot) {
                                return self.reject(CancelReason::InvalidFrame);
                            }
                            if event.code == ABS_MT_TRACKING_ID
                                && event.value >= 0
                                && self
                                    .slots
                                    .get(&self.active_slot)
                                    .and_then(|slot| slot.id)
                                    .is_some_and(|id| id.tracking == event.value)
                            {
                                continue;
                            }
                            if event.code == ABS_MT_TRACKING_ID && event.value >= 0 {
                                let Some(next) = self.next_contact.checked_add(1) else {
                                    return self.reject(CancelReason::InvalidFrame);
                                };
                                self.next_contact = next;
                            }
                            let slot = self.slots.entry(self.active_slot).or_default();
                            if event.code == ABS_MT_TRACKING_ID
                                && let (Some(id), Some(x), Some(y)) = (slot.id, slot.x, slot.y)
                            {
                                ended_contacts.insert(
                                    id,
                                    Contact {
                                        id,
                                        x,
                                        y,
                                        pressure: slot.pressure,
                                        fresh: slot.fresh,
                                    },
                                );
                            }
                            match event.code {
                                ABS_MT_TRACKING_ID if event.value >= 0 => {
                                    slot.id = Some(ContactId {
                                        slot: self.active_slot,
                                        tracking: event.value,
                                        generation: self.next_contact,
                                    });
                                    slot.fresh = true;
                                }
                                ABS_MT_TRACKING_ID if event.value == -1 => slot.id = None,
                                ABS_MT_TRACKING_ID => {
                                    return self.reject(CancelReason::InvalidFrame);
                                }
                                ABS_MT_POSITION_X => slot.x = Some(event.value),
                                ABS_MT_POSITION_Y => slot.y = Some(event.value),
                                ABS_MT_PRESSURE => slot.pressure = Some(event.value),
                                _ => unreachable!(),
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        for (horizontal, legacy, hires) in [
            (false, REL_WHEEL, REL_WHEEL_HI_RES),
            (true, REL_HWHEEL, REL_HWHEEL_HI_RES),
        ] {
            if let Some(value) = wheel.get(&hires).or_else(|| wheel.get(&legacy)) {
                let high_resolution = wheel.contains_key(&hires);
                changes.push(Change::Wheel {
                    horizontal,
                    steps: *value as f64 / if high_resolution { 120.0 } else { 1.0 },
                    high_resolution,
                });
            }
        }
        for event in &frame.events {
            if self.axes.contains_key(&event.code) {
                self.absolute.update(*event);
            }
        }
        let contacts: Vec<_> = self
            .slots
            .values()
            .filter_map(|slot| {
                Some(Contact {
                    id: slot.id?,
                    x: slot.x?,
                    y: slot.y?,
                    pressure: slot.pressure,
                    fresh: slot.fresh,
                })
            })
            .collect();
        // An active tracking ID without complete coordinates is unsafe for hit testing.
        if self.slots.values().filter(|slot| slot.id.is_some()).count() != contacts.len() {
            return self.reject(CancelReason::InvalidFrame);
        }
        let after: BTreeSet<_> = contacts.iter().map(|contact| contact.id).collect();
        Ok(NormalizedFrame {
            raw: frame,
            changes,
            contacts,
            contacts_started: after.difference(&before).copied().collect(),
            contacts_ended: before.difference(&after).copied().collect(),
            ended_contacts: ended_contacts.into_values().collect(),
        })
    }

    fn valid_slot(&self, slot: u16) -> bool {
        self.slot_range
            .is_some_and(|(minimum, maximum)| (minimum..=maximum).contains(&slot))
    }

    fn reject(&mut self, reason: CancelReason) -> FrameResult {
        self.cancel();
        Err(reason)
    }

    fn clear(&mut self) {
        self.keys.clear();
        self.switches.clear();
        self.slots.clear();
        self.last_at = None;
        self.last_sequence = None;
        self.active_slot = self.slot_range.map_or(0, |(minimum, _)| minimum);
        self.absolute = AbsoluteState::default();
        if self.slot_range.is_some() {
            self.absolute
                .values
                .insert(ABS_MT_SLOT, i32::from(self.active_slot));
        }
    }
}

/// Accumulates kernel batches across reads, discarding loss through SYN_REPORT.
#[derive(Debug)]
pub struct FrameAssembler {
    source: Source,
    pending: Vec<RawEvent>,
    dropping: bool,
    sequence: u64,
}

#[derive(Debug)]
pub enum Assembly {
    Pending,
    Frame(Frame),
    Lost,
}

impl FrameAssembler {
    pub fn new(source: Source) -> Self {
        Self {
            source,
            pending: Vec::new(),
            dropping: false,
            sequence: 0,
        }
    }

    /// True only between complete frames, outside synchronization-loss drain.
    /// This describes our software buffer, not the kernel queue or ownership.
    /// After `Assembly::Lost`, callers must still retire/reconcile the source.
    pub fn at_boundary(&self) -> bool {
        self.pending.is_empty() && !self.dropping
    }

    pub fn push(&mut self, event: RawEvent, at: Duration) -> Assembly {
        if event.kind == EV_SYN && event.code == SYN_DROPPED {
            self.pending.clear();
            self.dropping = true;
            return Assembly::Pending;
        }
        if self.dropping {
            if event.kind == EV_SYN && event.code == SYN_REPORT {
                self.dropping = false;
                return Assembly::Lost;
            }
            return Assembly::Pending;
        }
        if self.pending.len() >= 4096 {
            self.pending.clear();
            self.dropping = true;
            if event.kind == EV_SYN && event.code == SYN_REPORT {
                self.dropping = false;
                return Assembly::Lost;
            }
            return Assembly::Pending;
        }
        self.pending.push(event);
        if event.kind == EV_SYN && event.code == SYN_REPORT {
            let Some(sequence) = self.sequence.checked_add(1) else {
                self.pending.clear();
                return Assembly::Lost;
            };
            self.sequence = sequence;
            return Assembly::Frame(Frame {
                source: self.source.clone(),
                at,
                sequence,
                events: std::mem::take(&mut self.pending),
            });
        }
        Assembly::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    pub fn device() -> Device {
        Device {
            identity: "test-touch".into(),
            source: Source {
                device: 1,
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
            axes: BTreeMap::from([(
                ABS_MT_SLOT,
                Axis {
                    minimum: 0,
                    maximum: 4,
                    resolution: 0,
                    fuzz: 0,
                    flat: 0,
                },
            )]),
            virtual_device: false,
        }
    }
    fn frame(sequence: u64, events: &[(u16, u16, i32)]) -> Frame {
        let mut events: Vec<_> = events
            .iter()
            .map(|&(kind, code, value)| RawEvent { kind, code, value })
            .collect();
        events.push(RawEvent {
            kind: EV_SYN,
            code: SYN_REPORT,
            value: 0,
        });
        Frame {
            source: device().source,
            at: Duration::from_millis(sequence),
            sequence,
            events,
        }
    }
    fn complete(result: FrameResult) -> NormalizedFrame {
        match result {
            Ok(frame) => frame,
            other => panic!("{other:?}"),
        }
    }
    #[test]
    fn t16_slot_reuse_has_a_new_generation() {
        let mut normalizer = Normalizer::new(&device());
        let a = complete(normalizer.process(frame(
            1,
            &[
                (EV_ABS, ABS_MT_TRACKING_ID, 42),
                (EV_ABS, ABS_MT_POSITION_X, 100),
                (EV_ABS, ABS_MT_POSITION_Y, 200),
            ],
        )));
        let b = complete(normalizer.process(frame(2, &[(EV_ABS, ABS_MT_TRACKING_ID, -1)])));
        let c = complete(normalizer.process(frame(3, &[(EV_ABS, ABS_MT_TRACKING_ID, 42)])));
        assert_eq!(b.contacts_ended, [a.contacts[0].id]);
        assert_ne!(a.contacts[0].id, c.contacts[0].id);
    }
    #[test]
    fn t16_slot_order_does_not_change_contact_order() {
        let mut normalizer = Normalizer::new(&device());
        let a = complete(normalizer.process(frame(
            1,
            &[
                (EV_ABS, ABS_MT_SLOT, 1),
                (EV_ABS, ABS_MT_TRACKING_ID, 2),
                (EV_ABS, ABS_MT_POSITION_X, 20),
                (EV_ABS, ABS_MT_POSITION_Y, 30),
                (EV_ABS, ABS_MT_SLOT, 0),
                (EV_ABS, ABS_MT_TRACKING_ID, 1),
                (EV_ABS, ABS_MT_POSITION_X, 10),
                (EV_ABS, ABS_MT_POSITION_Y, 15),
            ],
        )));
        assert_eq!(
            a.contacts.iter().map(|c| c.id.slot).collect::<Vec<_>>(),
            [0, 1]
        );
    }
    #[test]
    fn t17_loss_discards_through_next_frame_boundary() {
        let mut assembler = FrameAssembler::new(device().source);
        let event = |kind, code, value| RawEvent { kind, code, value };
        assembler.push(event(EV_KEY, 30, 1), Duration::ZERO);
        assembler.push(event(EV_SYN, SYN_DROPPED, 0), Duration::ZERO);
        assembler.push(event(EV_KEY, 31, 1), Duration::ZERO);
        assert!(matches!(
            assembler.push(event(EV_SYN, SYN_REPORT, 0), Duration::ZERO),
            Assembly::Lost
        ));
        assembler.push(event(EV_KEY, 32, 1), Duration::ZERO);
        let Assembly::Frame(frame) = assembler.push(event(EV_SYN, SYN_REPORT, 0), Duration::ZERO)
        else {
            panic!()
        };
        assert_eq!(frame.events.len(), 2);
        assert_eq!(frame.events[0].code, 32);
    }
    #[test]
    fn t13_initial_key_state_does_not_invent_a_press() {
        let mut normalizer = Normalizer::new(&device());
        normalizer.reconcile(BTreeSet::from([30]), vec![]).unwrap();
        let result = complete(normalizer.process(frame(
            1,
            &[(EV_KEY, 30, 1), (EV_KEY, 30, 2), (EV_KEY, 30, 0)],
        )));
        assert_eq!(
            result.changes,
            [
                Change::Key {
                    code: 30,
                    state: KeyState::Repeat
                },
                Change::Key {
                    code: 30,
                    state: KeyState::Release
                }
            ]
        );
    }
    #[test]
    fn t22_tool_count_is_not_a_double_tap() {
        let mut normalizer = Normalizer::new(&device());
        assert!(
            complete(normalizer.process(frame(1, &[(EV_KEY, 0x14d, 1), (EV_KEY, 0x14d, 0)])))
                .changes
                .is_empty()
        );
    }
    #[test]
    fn t26_high_resolution_wheel_does_not_double_count() {
        let mut normalizer = Normalizer::new(&device());
        let result = complete(normalizer.process(frame(
            1,
            &[(EV_REL, REL_WHEEL, 1), (EV_REL, REL_WHEEL_HI_RES, 120)],
        )));
        assert_eq!(
            result.changes,
            [Change::Wheel {
                horizontal: false,
                steps: 1.0,
                high_resolution: true
            }]
        );
        assert_eq!(result.raw.events.len(), 3);
    }
    #[test]
    fn t17_backward_time_requires_reconciliation() {
        let mut normalizer = Normalizer::new(&device());
        complete(normalizer.process(frame(2, &[])));
        assert_eq!(
            normalizer.process(frame(1, &[])),
            Err(CancelReason::BackwardTime)
        );
        assert_eq!(
            normalizer.process(frame(3, &[])),
            Err(CancelReason::EventLoss)
        );
        normalizer.reconcile(BTreeSet::new(), vec![]).unwrap();
        complete(normalizer.process(frame(4, &[])));
    }
    #[test]
    fn t16_invalid_slot_fails_without_allocating_arbitrary_state() {
        let mut normalizer = Normalizer::new(&device());
        assert_eq!(
            normalizer.process(frame(1, &[(EV_ABS, ABS_MT_SLOT, 1000)])),
            Err(CancelReason::InvalidFrame)
        );
    }
    #[test]
    fn t15_identity_hash_is_stable() {
        assert_eq!(identity_number(""), 0xcbf29ce484222325);
        assert_eq!(identity_number("hello"), 0xa430d84680aabd0b);
    }
    #[test]
    fn t16_release_preserves_final_position_and_repeated_tracking_is_not_new() {
        let mut normalizer = Normalizer::new(&device());
        let first = complete(normalizer.process(frame(
            1,
            &[
                (EV_ABS, ABS_MT_TRACKING_ID, 42),
                (EV_ABS, ABS_MT_POSITION_X, 100),
                (EV_ABS, ABS_MT_POSITION_Y, 200),
            ],
        )));
        let repeated = complete(normalizer.process(frame(2, &[(EV_ABS, ABS_MT_TRACKING_ID, 42)])));
        assert_eq!(repeated.contacts[0].id, first.contacts[0].id);
        assert!(repeated.contacts_started.is_empty());
        let released = complete(normalizer.process(frame(
            3,
            &[
                (EV_ABS, ABS_MT_POSITION_X, 350),
                (EV_ABS, ABS_MT_TRACKING_ID, -1),
            ],
        )));
        assert!(released.contacts.is_empty());
        assert_eq!(released.ended_contacts[0].id, first.contacts[0].id);
        assert_eq!(released.ended_contacts[0].x, 350);
    }

    #[test]
    fn invalid_reconciliation_preserves_held_state_and_source_timeline() {
        let mut normalizer = Normalizer::new(&device());
        let first = complete(normalizer.process(frame(
            1,
            &[
                (EV_KEY, 30, 1),
                (EV_ABS, ABS_MT_TRACKING_ID, 42),
                (EV_ABS, ABS_MT_POSITION_X, 100),
                (EV_ABS, ABS_MT_POSITION_Y, 200),
            ],
        )));
        let mut contact = first.contacts[0].clone();
        contact.id.slot = 1000;
        assert!(
            normalizer
                .reconcile(BTreeSet::new(), vec![contact])
                .is_err()
        );
        let released = complete(normalizer.process(frame(
            2,
            &[(EV_KEY, 30, 0), (EV_ABS, ABS_MT_TRACKING_ID, -1)],
        )));
        assert_eq!(released.contacts_ended, vec![first.contacts[0].id]);
        assert_eq!(
            released.changes[0],
            Change::Key {
                code: 30,
                state: KeyState::Release
            }
        );
    }

    #[test]
    fn reconciliation_rejects_duplicate_contacts_and_unbounded_key_codes() {
        let mut normalizer = Normalizer::new(&device());
        let contact = Contact {
            id: ContactId {
                slot: 0,
                tracking: 5,
                generation: 1,
            },
            x: 10,
            y: 20,
            pressure: None,
            fresh: true,
        };
        assert!(
            normalizer
                .reconcile(BTreeSet::new(), vec![contact.clone(), contact.clone()])
                .is_err()
        );
        let mut another = contact.clone();
        another.id.slot = 1;
        another.id.generation = 2;
        assert!(
            normalizer
                .reconcile(BTreeSet::new(), vec![contact.clone(), another])
                .is_err()
        );
        assert!(
            normalizer
                .reconcile(BTreeSet::from([u16::MAX]), vec![])
                .is_err()
        );
        normalizer
            .reconcile(BTreeSet::new(), vec![contact])
            .unwrap();
        let held = complete(normalizer.process(frame(1, &[])));
        assert!(!held.contacts[0].fresh);
        assert!(held.contacts_started.is_empty());
    }
}
