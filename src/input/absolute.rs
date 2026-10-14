//! Absolute-axis state includes inactive Type B slots. Tracking IDs are kept
//! in contact reconciliation, never in this coordinate-only seed.
use super::*;

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AbsoluteSlot {
    #[serde(with = "numeric_map")]
    pub values: BTreeMap<u16, i32>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AbsoluteState {
    #[serde(with = "numeric_map")]
    pub values: BTreeMap<u16, i32>,
    #[serde(with = "numeric_map")]
    pub slots: BTreeMap<u16, AbsoluteSlot>,
}

pub(crate) fn per_slot(code: u16) -> bool {
    (0x30..=0x3d).contains(&code) && code != ABS_MT_TRACKING_ID
}

impl AbsoluteState {
    pub fn validate(&self, axes: &BTreeMap<u16, Axis>) -> Result<(), &'static str> {
        if self.values.len() > 64 || self.slots.len() > 256 {
            return Err("absolute state exceeds axis/slot bounds");
        }
        for &code in self.values.keys() {
            if code > 0x3f
                || !axes.contains_key(&code)
                || per_slot(code)
                || code == ABS_MT_TRACKING_ID
            {
                return Err("invalid global absolute-state axis");
            }
        }
        let range = axes.get(&ABS_MT_SLOT);
        if range.is_some_and(|axis| {
            axis.minimum < 0
                || axis.maximum > i32::from(u16::MAX)
                || axis.maximum < axis.minimum
                || i64::from(axis.maximum) - i64::from(axis.minimum) >= 256
        }) {
            return Err("invalid absolute-state slot bounds");
        }
        if !self.slots.is_empty() && !self.values.contains_key(&ABS_MT_SLOT) {
            return Err("per-slot absolute state requires its selected slot");
        }
        if let Some(&slot) = self.values.get(&ABS_MT_SLOT)
            && range.is_none_or(|axis| !(axis.minimum..=axis.maximum).contains(&slot))
        {
            return Err("invalid absolute-state selected slot");
        }
        for (&slot, state) in &self.slots {
            if range.is_none_or(|axis| !(axis.minimum..=axis.maximum).contains(&i32::from(slot)))
                || state.values.len() > 13
                || state
                    .values
                    .keys()
                    .any(|code| !per_slot(*code) || !axes.contains_key(code))
            {
                return Err("invalid per-slot absolute state");
            }
        }
        Ok(())
    }

    pub fn selected_slot(&self) -> u16 {
        self.values.get(&ABS_MT_SLOT).copied().unwrap_or_default() as u16
    }

    pub(crate) fn update(&mut self, event: RawEvent) {
        if event.kind != EV_ABS || event.code > 0x3f || event.code == ABS_MT_TRACKING_ID {
            return;
        }
        if per_slot(event.code) {
            self.slots
                .entry(self.selected_slot())
                .or_default()
                .values
                .insert(event.code, event.value);
        } else {
            self.values.insert(event.code, event.value);
        }
    }

    /// Repair persistent values needed by an ordinary delta frame. Values
    /// explicitly supplied by that frame win. Unused inactive slots remain
    /// private until an ordinary frame actually addresses them.
    pub(crate) fn restore_for(&self, output: &Self, frame: &Frame) -> Vec<RawEvent> {
        let mut cursor = self.selected_slot();
        let mut used = BTreeSet::new();
        let mut globals = BTreeSet::new();
        let mut written = BTreeSet::new();
        let mut first_mt = None;
        let mut has_absolute = false;
        for event in &frame.events {
            if event.kind != EV_ABS {
                continue;
            }
            has_absolute = true;
            if event.code == ABS_MT_SLOT {
                first_mt.get_or_insert(event.code);
                cursor = event.value as u16;
            } else if (0x30..=0x3d).contains(&event.code) {
                first_mt.get_or_insert(event.code);
                used.insert(cursor);
                if per_slot(event.code) {
                    written.insert((cursor, event.code));
                }
            } else {
                globals.insert(event.code);
            }
        }
        let mut events = Vec::new();
        if has_absolute {
            for (&code, &value) in &self.values {
                if code != ABS_MT_SLOT
                    && !globals.contains(&code)
                    && output.values.get(&code) != Some(&value)
                {
                    events.push(RawEvent {
                        kind: EV_ABS,
                        code,
                        value,
                    });
                }
            }
        }
        let mut cursor = output.selected_slot();
        for slot in used {
            let Some(state) = self.slots.get(&slot) else {
                continue;
            };
            for (&code, &value) in &state.values {
                if written.contains(&(slot, code))
                    || output
                        .slots
                        .get(&slot)
                        .and_then(|state| state.values.get(&code))
                        == Some(&value)
                {
                    continue;
                }
                if cursor != slot {
                    events.push(RawEvent {
                        kind: EV_ABS,
                        code: ABS_MT_SLOT,
                        value: i32::from(slot),
                    });
                    cursor = slot;
                }
                events.push(RawEvent {
                    kind: EV_ABS,
                    code,
                    value,
                });
            }
        }
        // An explicit selector in the raw frame supplies its own cursor. An
        // implicit MT update needs the original cursor before it is processed.
        if first_mt.is_some_and(|code| code != ABS_MT_SLOT) && cursor != self.selected_slot() {
            events.push(RawEvent {
                kind: EV_ABS,
                code: ABS_MT_SLOT,
                value: i32::from(self.selected_slot()),
            });
        }
        events
    }

    /// Leave every contact released and restore the selected slot after
    /// visiting the stored coordinate slots. This emits no tracking IDs.
    pub(crate) fn initialization(&self, source: Source, at: Duration) -> Frame {
        let mut events = self
            .values
            .iter()
            .filter(|(code, _)| **code != ABS_MT_SLOT)
            .map(|(&code, &value)| RawEvent {
                kind: EV_ABS,
                code,
                value,
            })
            .collect::<Vec<_>>();
        for (&slot, state) in &self.slots {
            events.push(RawEvent {
                kind: EV_ABS,
                code: ABS_MT_SLOT,
                value: i32::from(slot),
            });
            events.extend(state.values.iter().map(|(&code, &value)| RawEvent {
                kind: EV_ABS,
                code,
                value,
            }));
        }
        if let Some(&value) = self.values.get(&ABS_MT_SLOT) {
            events.push(RawEvent {
                kind: EV_ABS,
                code: ABS_MT_SLOT,
                value,
            });
        }
        events.push(RawEvent {
            kind: EV_SYN,
            code: SYN_REPORT,
            value: 0,
        });
        Frame {
            source,
            sequence: 0,
            at,
            events,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (Device, AbsoluteState) {
        let mut device = crate::input::tests::device();
        let axis = Axis {
            minimum: 0,
            maximum: 1000,
            fuzz: 0,
            flat: 0,
            resolution: 20,
        };
        device.axes.insert(ABS_MT_POSITION_X, axis);
        device.axes.insert(ABS_MT_POSITION_Y, axis);
        let state = AbsoluteState {
            values: BTreeMap::from([(ABS_MT_SLOT, 1)]),
            slots: BTreeMap::from([(
                1,
                AbsoluteSlot {
                    values: BTreeMap::from([(ABS_MT_POSITION_X, 321), (ABS_MT_POSITION_Y, 654)]),
                },
            )]),
        };
        (device, state)
    }

    #[test]
    fn inactive_coordinates_and_selected_slot_survive_delta_only_contact_start() {
        let (device, state) = fixture();
        let mut normalizer = Normalizer::new(&device);
        normalizer.seed_absolute(&device, state.clone()).unwrap();
        let mut invalid = state.clone();
        invalid
            .slots
            .get_mut(&1)
            .unwrap()
            .values
            .insert(ABS_MT_TRACKING_ID, 7);
        assert!(normalizer.seed_absolute(&device, invalid).is_err());
        assert_eq!(normalizer.absolute_state(), &state);
        let result = normalizer
            .process(Frame {
                source: device.source.clone(),
                sequence: 1,
                at: Duration::from_millis(1),
                events: vec![
                    RawEvent {
                        kind: EV_ABS,
                        code: ABS_MT_TRACKING_ID,
                        value: 7,
                    },
                    RawEvent {
                        kind: EV_SYN,
                        code: SYN_REPORT,
                        value: 0,
                    },
                ],
            })
            .unwrap();
        assert_eq!(result.contacts.len(), 1);
        assert_eq!(result.contacts[0].id.slot, 1);
        assert_eq!((result.contacts[0].x, result.contacts[0].y), (321, 654));
        assert_eq!(result.contacts_started.len(), 1);
        assert!(normalizer.seed_absolute(&device, state).is_err());
    }

    #[test]
    fn recording_round_trip_and_virtual_initialization_preserve_only_coordinates() {
        let (device, state) = fixture();
        let record = crate::replay::Record::Absolute {
            source: device.source.clone(),
            state: state.clone(),
        };
        let encoded = serde_json::to_vec(&record).unwrap();
        assert_eq!(
            serde_json::from_slice::<crate::replay::Record>(&encoded).unwrap(),
            record
        );
        let frame = state.initialization(device.source, Duration::ZERO);
        assert_eq!(frame.sequence, 0);
        assert!(
            frame
                .events
                .iter()
                .all(|event| event.kind != EV_KEY && event.code != ABS_MT_TRACKING_ID)
        );
        assert_eq!(
            frame.events[frame.events.len() - 2],
            RawEvent {
                kind: EV_ABS,
                code: ABS_MT_SLOT,
                value: 1
            }
        );
        let mut invalid = state;
        invalid.values.insert(ABS_MT_SLOT, -1);
        assert!(invalid.validate(&device.axes).is_err());
    }
}
