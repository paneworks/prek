//! Seat-scoped key conditions, chords, layers and synthetic-key ownership.
//! Output execution uses an explicitly supplied frame sink in [`output`].
pub mod output;
use crate::{
    config::Layer,
    input::{KeyState, Source},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct SourceKey {
    pub source: Source,
    pub code: u16,
}
#[derive(Clone, Debug)]
pub struct Chord {
    pub id: String,
    pub keys: Vec<u16>,
    pub ordered: bool,
    pub timeout: Duration,
}
#[derive(Clone, Debug)]
pub struct Repeat {
    pub delay: Duration,
    pub interval: Duration,
}
#[derive(Clone, Debug)]
pub struct Hold {
    pub id: String,
    pub key: u16,
    pub after: Duration,
    pub repeat: Option<Repeat>,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum KeyObservation {
    Chord {
        id: String,
        seat: String,
        at: Duration,
    },
    Layer {
        id: String,
        seat: String,
        active: bool,
    },
    Hold {
        id: String,
        key: SourceKey,
        at: Duration,
        repeat: bool,
    },
}
#[derive(Clone)]
struct Pressed {
    at: Option<Duration>,
    order: u64,
    holds: BTreeSet<usize>,
    repeats: BTreeMap<usize, Duration>,
}
#[derive(Clone)]
pub struct Keys {
    chords: Vec<Chord>,
    holds: Vec<Hold>,
    layers: Vec<Layer>,
    pressed: BTreeMap<SourceKey, Pressed>,
    order: u64,
    fired: BTreeSet<(String, usize)>,
    active_layers: BTreeSet<(String, usize)>,
    toggles: BTreeMap<(String, usize), Source>,
    last_at: Option<Duration>,
}
impl Keys {
    pub fn new(
        chords: Vec<Chord>,
        holds: Vec<Hold>,
        layers: Vec<Layer>,
    ) -> Result<Self, &'static str> {
        if chords.len() > 1024 || holds.len() > 1024 || layers.len() > 128 {
            return Err("too many key definitions");
        }
        let valid_time = |time: Duration| !time.is_zero() && time <= Duration::from_secs(3600);
        let mut ids = BTreeSet::new();
        for chord in &chords {
            if chord.id.is_empty()
                || !ids.insert(&chord.id)
                || !(2..=32).contains(&chord.keys.len())
                || chord.keys.iter().collect::<BTreeSet<_>>().len() != chord.keys.len()
                || !valid_time(chord.timeout)
            {
                return Err("invalid chord ID, keys or timeout");
            }
        }
        ids.clear();
        for hold in &holds {
            if hold.id.is_empty()
                || !ids.insert(&hold.id)
                || !valid_time(hold.after)
                || hold
                    .repeat
                    .as_ref()
                    .is_some_and(|r| !valid_time(r.delay) || !valid_time(r.interval))
            {
                return Err("invalid hold ID, timing or repetition");
            }
        }
        ids.clear();
        for layer in &layers {
            if layer.id.is_empty() || !ids.insert(&layer.id) {
                return Err("invalid layer ID");
            }
        }
        Ok(Self {
            chords,
            holds,
            layers,
            pressed: BTreeMap::new(),
            order: 0,
            fired: BTreeSet::new(),
            active_layers: BTreeSet::new(),
            toggles: BTreeMap::new(),
            last_at: None,
        })
    }
    pub fn down(&self, seat: &str, code: u16) -> bool {
        self.pressed
            .keys()
            .any(|key| key.source.seat == seat && key.code == code)
    }
    pub fn layer_active(&self, seat: &str, id: &str) -> bool {
        self.layers
            .iter()
            .position(|layer| layer.id == id)
            .is_some_and(|index| match self.layers[index].activation {
                crate::config::LayerActivation::Active => true,
                crate::config::LayerActivation::Inactive => false,
                crate::config::LayerActivation::Physical => {
                    self.active_layers.contains(&(seat.into(), index))
                }
            })
    }
    /// Runtime edits preserve physical press/toggle state. Layer definitions
    /// stay fixed; a source reload uses ordinary held-state reconciliation.
    pub(crate) fn with_layer_activations(&self, layers: &[Layer]) -> Result<Self, &'static str> {
        if layers.len() != self.layers.len()
            || layers.iter().zip(&self.layers).any(|(new, old)| {
                new.id != old.id || new.key != old.key || new.toggle != old.toggle
            })
        {
            return Err("runtime layer definitions changed");
        }
        let mut next = self.clone();
        next.layers = layers.to_vec();
        next.refresh_layers();
        Ok(next)
    }
    pub fn update(
        &mut self,
        source: &Source,
        code: u16,
        state: KeyState,
        at: Duration,
    ) -> Result<Vec<KeyObservation>, &'static str> {
        if source.seat.is_empty() {
            return Err("key source requires a seat");
        }
        if self.last_at.is_some_and(|last| at < last) {
            self.reset();
            self.last_at = Some(at);
            return Err("key time moved backwards; state cancelled");
        }
        self.last_at = Some(at);
        let key = SourceKey {
            source: source.clone(),
            code,
        };
        let was_down = self.down(&source.seat, code);
        let mut events = self.advance(at)?;
        match state {
            KeyState::Press if !self.pressed.contains_key(&key) => {
                if self.pressed.len() >= 16384 {
                    return Err("pressed-key limit exceeded");
                }
                self.order = self
                    .order
                    .checked_add(1)
                    .ok_or("key press sequence overflow")?;
                self.pressed.insert(
                    key,
                    Pressed {
                        at: Some(at),
                        order: self.order,
                        holds: BTreeSet::new(),
                        repeats: BTreeMap::new(),
                    },
                );
                if !was_down {
                    for (index, layer) in self.layers.iter().enumerate() {
                        if layer.toggle && layer.key.0 == code {
                            let key = (source.seat.clone(), index);
                            if self.toggles.remove(&key).is_none() {
                                self.toggles.insert(key, source.clone());
                            }
                        }
                    }
                }
                for (index, chord) in self.chords.iter().enumerate() {
                    let key = (source.seat.clone(), index);
                    if self.fired.contains(&key) {
                        continue;
                    }
                    // Chords require actual presses after reconciliation. Held
                    // snapshot modifiers still participate in `down` conditions.
                    let presses: Option<Vec<_>> = chord
                        .keys
                        .iter()
                        .map(|code| {
                            self.pressed
                                .iter()
                                .filter(|(key, pressed)| {
                                    key.source.seat == source.seat
                                        && key.code == *code
                                        && pressed.at.is_some()
                                })
                                .max_by_key(|(_, pressed)| pressed.order)
                                .map(|(_, pressed)| pressed)
                        })
                        .collect();
                    if let Some(presses) = presses {
                        let first = presses
                            .iter()
                            .filter_map(|p| p.at)
                            .min()
                            .expect("fresh presses");
                        let ordered =
                            !chord.ordered || presses.windows(2).all(|p| p[0].order < p[1].order);
                        if ordered && at.saturating_sub(first) <= chord.timeout {
                            self.fired.insert(key);
                            events.push(KeyObservation::Chord {
                                id: chord.id.clone(),
                                seat: source.seat.clone(),
                                at,
                            });
                        }
                    }
                }
            }
            KeyState::Release => {
                self.pressed.remove(&key);
                self.fired.retain(|(seat, index)| {
                    self.chords[*index].keys.iter().all(|code| {
                        self.pressed
                            .keys()
                            .any(|key| key.source.seat == *seat && key.code == *code)
                    })
                });
            }
            // Kernel repeat belongs to passthrough; it is not another press.
            _ => {}
        }
        events.extend(self.refresh_layers());
        Ok(events)
    }
    pub fn reconcile(
        &mut self,
        source: &Source,
        keys: impl IntoIterator<Item = u16>,
    ) -> Result<Vec<KeyObservation>, &'static str> {
        let keys: BTreeSet<_> = keys.into_iter().collect();
        self.validate_reconciliation(source, &keys)?;
        let mut events = self.cancel_source(source);
        for code in keys {
            self.pressed.insert(
                SourceKey {
                    source: source.clone(),
                    code,
                },
                Pressed {
                    at: None,
                    order: 0,
                    holds: BTreeSet::new(),
                    repeats: BTreeMap::new(),
                },
            );
        }
        events.extend(self.refresh_layers());
        Ok(events)
    }
    pub fn validate_reconciliation(
        &self,
        source: &Source,
        keys: &BTreeSet<u16>,
    ) -> Result<(), &'static str> {
        let retained = self
            .pressed
            .keys()
            .filter(|key| key.source != *source)
            .count();
        if keys.len() > 768 || retained + keys.len() > 16384 || source.seat.is_empty() {
            Err("invalid initial key state")
        } else {
            Ok(())
        }
    }
    pub fn advance(&mut self, at: Duration) -> Result<Vec<KeyObservation>, &'static str> {
        if self.last_at.is_some_and(|last| at < last) {
            self.reset();
            self.last_at = Some(at);
            return Err("key time moved backwards; state cancelled");
        }
        self.last_at = Some(at);
        let mut events = Vec::new();
        for (key, pressed) in &mut self.pressed {
            let Some(started) = pressed.at else {
                continue;
            };
            for (index, hold) in self
                .holds
                .iter()
                .enumerate()
                .filter(|(_, hold)| hold.key == key.code)
            {
                let threshold = started.saturating_add(hold.after);
                if at < threshold {
                    continue;
                }
                if pressed.holds.insert(index) {
                    events.push(KeyObservation::Hold {
                        id: hold.id.clone(),
                        key: key.clone(),
                        at,
                        repeat: false,
                    });
                    if let Some(repeat) = &hold.repeat {
                        pressed
                            .repeats
                            .insert(index, at.saturating_add(repeat.delay));
                    }
                } else if let Some(next) = pressed.repeats.get_mut(&index)
                    && at >= *next
                {
                    events.push(KeyObservation::Hold {
                        id: hold.id.clone(),
                        key: key.clone(),
                        at,
                        repeat: true,
                    });
                    // Coalesce missed deadlines; never burst queued actions.
                    *next =
                        at.saturating_add(hold.repeat.as_ref().expect("repeat schedule").interval);
                }
            }
        }
        Ok(events)
    }
    pub fn deadline(&self) -> Option<Duration> {
        self.pressed
            .iter()
            .filter_map(|(key, pressed)| pressed.at.map(|at| (key, at, pressed)))
            .flat_map(|(key, at, pressed)| {
                self.holds
                    .iter()
                    .enumerate()
                    .filter_map(move |(index, hold)| {
                        if hold.key != key.code {
                            return None;
                        }
                        if pressed.holds.contains(&index) {
                            pressed.repeats.get(&index).copied()
                        } else {
                            Some(at.saturating_add(hold.after))
                        }
                    })
            })
            .min()
    }
    pub fn cancel_source(&mut self, source: &Source) -> Vec<KeyObservation> {
        self.pressed.retain(|key, _| key.source != *source);
        self.toggles.retain(|_, owner| owner != source);
        self.fired.retain(|(seat, index)| {
            self.chords[*index].keys.iter().all(|code| {
                self.pressed
                    .keys()
                    .any(|key| key.source.seat == *seat && key.code == *code)
            })
        });
        self.refresh_layers()
    }
    pub fn reset(&mut self) -> Vec<KeyObservation> {
        self.pressed.clear();
        self.toggles.clear();
        self.fired.clear();
        self.refresh_layers()
    }
    fn refresh_layers(&mut self) -> Vec<KeyObservation> {
        let seats: BTreeSet<_> = self
            .pressed
            .keys()
            .map(|k| k.source.seat.clone())
            .chain(self.active_layers.iter().map(|(seat, _)| seat.clone()))
            .chain(self.toggles.keys().map(|(seat, _)| seat.clone()))
            .collect();
        let mut next = BTreeSet::new();
        for seat in seats {
            for (index, layer) in self.layers.iter().enumerate() {
                if match layer.activation {
                    crate::config::LayerActivation::Active => true,
                    crate::config::LayerActivation::Inactive => false,
                    crate::config::LayerActivation::Physical => {
                        if layer.toggle {
                            self.toggles.contains_key(&(seat.clone(), index))
                        } else {
                            self.down(&seat, layer.key.0)
                        }
                    }
                } {
                    next.insert((seat.clone(), index));
                }
            }
        }
        let events = self
            .active_layers
            .symmetric_difference(&next)
            .map(|(seat, index)| KeyObservation::Layer {
                id: self.layers[*index].id.clone(),
                seat: seat.clone(),
                active: next.contains(&(seat.clone(), *index)),
            })
            .collect();
        self.active_layers = next;
        events
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Owner {
    Physical(SourceKey),
    Synthetic {
        seat: String,
        generation: u64,
        id: u64,
    },
}
impl Owner {
    fn seat(&self) -> &str {
        match self {
            Self::Physical(key) => &key.source.seat,
            Self::Synthetic { seat, .. } => seat,
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Transition {
    pub seat: String,
    pub code: u16,
    pub down: bool,
}
#[derive(Default)]
pub struct LogicalKeys {
    owners: BTreeMap<Owner, BTreeSet<u16>>,
    counts: BTreeMap<(String, u16), usize>,
}
impl LogicalKeys {
    pub fn update(
        &mut self,
        owner: Owner,
        code: u16,
        down: bool,
    ) -> Result<Option<Transition>, &'static str> {
        if owner.seat().is_empty() {
            return Err("logical key requires a seat");
        }
        let key = (owner.seat().to_owned(), code);
        if down {
            if self.owners.len() >= 16384 && !self.owners.contains_key(&owner) {
                return Err("logical-key owner limit exceeded");
            }
            let keys = self.owners.entry(owner).or_default();
            if keys.len() >= 768 && !keys.contains(&code) {
                return Err("logical keys per owner limit exceeded");
            }
            if !keys.insert(code) {
                return Ok(None);
            }
            let count = self.counts.entry(key.clone()).or_default();
            *count += 1;
            Ok((*count == 1).then_some(Transition {
                seat: key.0,
                code,
                down: true,
            }))
        } else {
            let Some(keys) = self.owners.get_mut(&owner) else {
                return Ok(None);
            };
            if !keys.remove(&code) {
                return Ok(None);
            }
            if keys.is_empty() {
                self.owners.remove(&owner);
            }
            let count = self.counts.get_mut(&key).expect("owned key count");
            *count -= 1;
            if *count == 0 {
                self.counts.remove(&key);
                Ok(Some(Transition {
                    seat: key.0,
                    code,
                    down: false,
                }))
            } else {
                Ok(None)
            }
        }
    }
    pub fn release_owner(&mut self, owner: &Owner) -> Vec<Transition> {
        let keys = self.owners.get(owner).cloned().unwrap_or_default();
        keys.into_iter()
            .filter_map(|code| {
                self.update(owner.clone(), code, false)
                    .expect("existing ownership")
            })
            .collect()
    }
    pub fn cancel_source(&mut self, source: &Source) -> Vec<Transition> {
        let owners: Vec<_> = self
            .owners
            .keys()
            .filter(|owner| matches!(owner, Owner::Physical(key) if key.source == *source))
            .cloned()
            .collect();
        owners
            .iter()
            .flat_map(|owner| self.release_owner(owner))
            .collect()
    }
    pub fn cancel_generation(&mut self, seat: &str, generation: u64) -> Vec<Transition> {
        let owners: Vec<_> = self.owners.keys().filter(|owner| matches!(owner, Owner::Synthetic { seat: owned_seat, generation: owned_generation, .. } if owned_seat == seat && *owned_generation == generation)).cloned().collect();
        owners
            .iter()
            .flat_map(|owner| self.release_owner(owner))
            .collect()
    }
    pub fn down(&self, seat: &str, code: u16) -> bool {
        self.counts.contains_key(&(seat.into(), code))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn t13_invalid_reconciliation_keeps_existing_key_state_and_timer() {
        let source = Source {
            device: 1,
            generation: 1,
            seat: "seat0".into(),
        };
        let mut keys = Keys::new(
            vec![],
            vec![Hold {
                id: "hold".into(),
                key: 115,
                after: Duration::from_millis(600),
                repeat: None,
            }],
            vec![],
        )
        .unwrap();
        keys.update(&source, 115, KeyState::Press, Duration::ZERO)
            .unwrap();
        assert!(keys.reconcile(&source, 0..769).is_err());
        assert!(keys.down("seat0", 115));
        assert_eq!(keys.deadline(), Some(Duration::from_millis(600)));
        assert_eq!(keys.advance(Duration::from_millis(600)).unwrap().len(), 1);
    }
    use crate::config::Key;
    fn source(device: u64) -> Source {
        Source {
            device,
            generation: 1,
            seat: "seat0".into(),
        }
    }
    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }
    fn model(ordered: bool) -> Keys {
        Keys::new(
            vec![Chord {
                id: "launch".into(),
                keys: vec![125, 28],
                ordered,
                timeout: ms(300),
            }],
            vec![],
            vec![],
        )
        .unwrap()
    }
    #[test]
    fn t40_chord_can_span_devices_only_within_one_seat() {
        let mut keys = model(false);
        assert!(
            keys.update(&source(1), 125, KeyState::Press, ms(0))
                .unwrap()
                .is_empty()
        );
        let mut other = source(2);
        other.seat = "seat1".into();
        assert!(
            keys.update(&other, 28, KeyState::Press, ms(50))
                .unwrap()
                .is_empty()
        );
        assert!(
            matches!(keys.update(&source(2), 28, KeyState::Press, ms(100)).unwrap().as_slice(), [KeyObservation::Chord { seat, .. }] if seat == "seat0")
        );
        assert!(
            keys.update(&source(2), 28, KeyState::Repeat, ms(110))
                .unwrap()
                .is_empty()
        );
        assert!(
            keys.update(&source(2), 28, KeyState::Press, ms(120))
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn t41_chord_order_and_timeout_are_explicit() {
        for ordered in [false, true] {
            let mut keys = model(ordered);
            keys.update(&source(1), 28, KeyState::Press, ms(0)).unwrap();
            assert_eq!(
                keys.update(&source(1), 125, KeyState::Press, ms(100))
                    .unwrap()
                    .len(),
                usize::from(!ordered)
            );
        }
        for delay in [300, 301] {
            let mut keys = model(true);
            keys.update(&source(1), 125, KeyState::Press, ms(0))
                .unwrap();
            assert_eq!(
                keys.update(&source(2), 28, KeyState::Press, ms(delay))
                    .unwrap()
                    .len(),
                usize::from(delay == 300)
            );
        }
    }
    #[test]
    fn t22_chord_rearms_after_release_and_repeat_does_not_press() {
        let mut keys = model(false);
        keys.update(&source(1), 125, KeyState::Press, ms(0))
            .unwrap();
        assert_eq!(
            keys.update(&source(1), 28, KeyState::Press, ms(10))
                .unwrap()
                .len(),
            1
        );
        keys.update(&source(1), 28, KeyState::Release, ms(20))
            .unwrap();
        assert!(
            keys.update(&source(1), 28, KeyState::Repeat, ms(30))
                .unwrap()
                .is_empty()
        );
        assert!(!keys.down("seat0", 28));
        assert_eq!(
            keys.update(&source(1), 28, KeyState::Press, ms(40))
                .unwrap()
                .len(),
            1
        );
    }
    #[test]
    fn t13_initial_held_state_is_condition_only() {
        let hold = Hold {
            id: "hold".into(),
            key: 125,
            after: ms(600),
            repeat: None,
        };
        let mut keys = Keys::new(vec![], vec![hold], vec![]).unwrap();
        keys.reconcile(&source(1), [125]).unwrap();
        assert!(keys.down("seat0", 125));
        assert_eq!(keys.deadline(), None);
        assert!(keys.advance(ms(5000)).unwrap().is_empty());
        keys.update(&source(1), 125, KeyState::Release, ms(5001))
            .unwrap();
        keys.update(&source(1), 125, KeyState::Press, ms(5010))
            .unwrap();
        assert_eq!(keys.deadline(), Some(ms(5610)));
        assert_eq!(keys.advance(ms(5610)).unwrap().len(), 1);
    }
    #[test]
    fn t23_configured_hold_and_silence_do_not_repeat() {
        let hold = Hold {
            id: "long".into(),
            key: 115,
            after: ms(2000),
            repeat: None,
        };
        let mut keys = Keys::new(vec![], vec![hold], vec![]).unwrap();
        keys.update(&source(1), 115, KeyState::Press, ms(0))
            .unwrap();
        keys.update(&source(1), 115, KeyState::Repeat, ms(500))
            .unwrap();
        assert!(keys.advance(ms(1999)).unwrap().is_empty());
        assert!(matches!(
            keys.advance(ms(2000)).unwrap().as_slice(),
            [KeyObservation::Hold { repeat: false, .. }]
        ));
        assert!(keys.advance(ms(3000)).unwrap().is_empty());
        assert_eq!(keys.deadline(), None);
    }
    #[test]
    fn t25_repeat_is_explicit_coalesced_and_stops_on_release() {
        let hold = Hold {
            id: "repeat".into(),
            key: 115,
            after: ms(600),
            repeat: Some(Repeat {
                delay: ms(200),
                interval: ms(100),
            }),
        };
        let mut keys = Keys::new(vec![], vec![hold], vec![]).unwrap();
        keys.update(&source(1), 115, KeyState::Press, ms(0))
            .unwrap();
        assert_eq!(keys.advance(ms(600)).unwrap().len(), 1);
        assert_eq!(keys.deadline(), Some(ms(800)));
        assert!(keys.advance(ms(799)).unwrap().is_empty());
        assert!(matches!(
            keys.advance(ms(2000)).unwrap().as_slice(),
            [KeyObservation::Hold { repeat: true, .. }]
        ));
        assert_eq!(keys.deadline(), Some(ms(2100)));
        keys.update(&source(1), 115, KeyState::Release, ms(2050))
            .unwrap();
        assert!(keys.advance(ms(5000)).unwrap().is_empty());
        assert_eq!(keys.deadline(), None);
    }
    #[test]
    fn t23_late_release_can_complete_hold_and_unrelated_keys_do_not_set_timers() {
        let hold = Hold {
            id: "hold".into(),
            key: 115,
            after: ms(600),
            repeat: None,
        };
        let mut keys = Keys::new(vec![], vec![hold], vec![]).unwrap();
        keys.update(&source(1), 30, KeyState::Press, ms(0)).unwrap();
        assert_eq!(keys.deadline(), None);
        keys.update(&source(1), 115, KeyState::Press, ms(100))
            .unwrap();
        assert!(matches!(
            keys.update(&source(1), 115, KeyState::Release, ms(700))
                .unwrap()
                .as_slice(),
            [KeyObservation::Hold { repeat: false, .. }]
        ));
        assert_eq!(keys.deadline(), None);
    }
    #[test]
    fn t41_momentary_and_toggled_layers_are_seat_scoped() {
        let layers = vec![
            Layer {
                id: "momentary".into(),
                key: Key(42),
                toggle: false,
                activation: Default::default(),
            },
            Layer {
                id: "toggle".into(),
                key: Key(125),
                toggle: true,
                activation: Default::default(),
            },
        ];
        let mut keys = Keys::new(vec![], vec![], layers).unwrap();
        keys.update(&source(1), 42, KeyState::Press, ms(0)).unwrap();
        assert!(keys.layer_active("seat0", "momentary"));
        keys.update(&source(2), 42, KeyState::Press, ms(1)).unwrap();
        keys.update(&source(1), 42, KeyState::Release, ms(2))
            .unwrap();
        assert!(keys.layer_active("seat0", "momentary"));
        keys.cancel_source(&source(2));
        assert!(!keys.layer_active("seat0", "momentary"));
        keys.update(&source(1), 125, KeyState::Press, ms(3))
            .unwrap();
        keys.update(&source(2), 125, KeyState::Press, ms(4))
            .unwrap();
        assert!(keys.layer_active("seat0", "toggle"));
        keys.update(&source(1), 125, KeyState::Release, ms(5))
            .unwrap();
        keys.update(&source(2), 125, KeyState::Release, ms(6))
            .unwrap();
        assert!(keys.layer_active("seat0", "toggle"));
        assert!(!keys.layer_active("seat1", "toggle"));
        keys.cancel_source(&source(1));
        assert!(!keys.layer_active("seat0", "toggle"));
    }

    #[test]
    fn layer_overrides_restore_physical_toggles_without_synthesizing_presses() {
        use crate::config::LayerActivation;
        let physical = vec![Layer {
            id: "toggle".into(),
            key: Key(125),
            toggle: true,
            activation: LayerActivation::Physical,
        }];
        let mut keys = Keys::new(vec![], vec![], physical.clone()).unwrap();
        keys.update(&source(1), 125, KeyState::Press, ms(0))
            .unwrap();
        keys.update(&source(1), 125, KeyState::Release, ms(1))
            .unwrap();
        assert!(keys.layer_active("seat0", "toggle"));
        assert!(!keys.layer_active("seat1", "toggle"));
        let mut overrides = physical.clone();
        overrides[0].activation = LayerActivation::Inactive;
        let suppressed = keys.with_layer_activations(&overrides).unwrap();
        assert!(!suppressed.layer_active("seat0", "toggle"));
        let restored = suppressed.with_layer_activations(&physical).unwrap();
        assert!(restored.layer_active("seat0", "toggle"));
        assert!(!restored.layer_active("seat1", "toggle"));
        overrides[0].activation = LayerActivation::Active;
        let mut active = Keys::new(vec![], vec![], overrides).unwrap();
        assert!(active.layer_active("seat0", "toggle"));
        active = active.with_layer_activations(&physical).unwrap();
        assert!(!active.layer_active("seat0", "toggle"));
        let mut changed = physical.clone();
        changed[0].key = Key(42);
        assert!(restored.with_layer_activations(&changed).is_err());
    }
    #[test]
    fn t14_disconnection_cancels_only_affected_hold() {
        let mut keys = Keys::new(
            vec![],
            vec![Hold {
                id: "hold".into(),
                key: 115,
                after: ms(600),
                repeat: None,
            }],
            vec![],
        )
        .unwrap();
        keys.update(&source(1), 115, KeyState::Press, ms(0))
            .unwrap();
        keys.update(&source(2), 115, KeyState::Press, ms(100))
            .unwrap();
        keys.cancel_source(&source(1));
        assert!(
            matches!(keys.advance(ms(700)).unwrap().as_slice(), [KeyObservation::Hold { key, .. }] if key.source.device == 2)
        );
    }
    fn owner(device: u64) -> Owner {
        Owner::Physical(SourceKey {
            source: source(device),
            code: 30,
        })
    }
    #[test]
    fn t46_two_remaps_to_one_modifier_release_only_after_last_owner() {
        let mut keys = LogicalKeys::default();
        assert_eq!(
            keys.update(owner(1), 125, true).unwrap(),
            Some(Transition {
                seat: "seat0".into(),
                code: 125,
                down: true
            })
        );
        assert_eq!(keys.update(owner(1), 125, true).unwrap(), None);
        assert_eq!(keys.update(owner(2), 125, true).unwrap(), None);
        assert!(keys.release_owner(&owner(1)).is_empty());
        assert!(keys.down("seat0", 125));
        assert_eq!(
            keys.cancel_source(&source(2)),
            vec![Transition {
                seat: "seat0".into(),
                code: 125,
                down: false
            }]
        );
        assert!(!keys.down("seat0", 125));
    }
    #[test]
    fn t46_synthetic_and_physical_ownership_are_independent() {
        let mut keys = LogicalKeys::default();
        let synthetic = Owner::Synthetic {
            seat: "seat0".into(),
            generation: 1,
            id: 7,
        };
        keys.update(owner(1), 42, true).unwrap();
        keys.update(synthetic.clone(), 42, true).unwrap();
        assert!(keys.cancel_generation("seat0", 2).is_empty());
        assert!(keys.cancel_generation("seat0", 1).is_empty());
        assert!(keys.down("seat0", 42));
        assert_eq!(keys.release_owner(&owner(1)).len(), 1);
        assert!(keys.release_owner(&synthetic).is_empty());
    }
    #[test]
    fn t17_backward_time_cancels_key_conditions_and_timers() {
        let mut keys = model(false);
        keys.update(&source(1), 125, KeyState::Press, ms(100))
            .unwrap();
        assert!(
            keys.update(&source(1), 28, KeyState::Press, ms(90))
                .is_err()
        );
        assert!(!keys.down("seat0", 125));
        assert_eq!(keys.deadline(), None);
    }
}
