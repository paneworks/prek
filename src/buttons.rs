use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, time::Duration};

/// Device identity is assigned by the caller, independently of key codes.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Button {
    pub device: u64,
    pub code: u16,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Gesture {
    /// Emitted after the sequence settles, rather than once per constituent tap.
    Taps { button: Button, count: u32 },
    /// Emitted once when the hold threshold is reached, including during silence.
    Hold { button: Button },
}

#[derive(Clone, Copy, Debug)]
pub struct Timing {
    pub tap_max: Duration,
    pub tap_gap: Duration,
    pub hold_after: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            tap_max: Duration::from_millis(250),
            tap_gap: Duration::from_millis(300),
            hold_after: Duration::from_millis(600),
        }
    }
}

#[derive(Debug)]
struct Press {
    started: Duration,
    held: bool,
}

#[derive(Debug)]
struct Taps {
    released: Duration,
    count: u32,
}

/// Timing recognition for physical keys, mouse buttons and other hardware keys.
///
/// A caller must omit auto-repeat events and cancel a device after event loss or
/// disconnect. Touch contacts require their own spatial recognizer; BTN_TOUCH
/// and touch-tool indicators must not be treated as ordinary buttons.
#[derive(Debug)]
pub struct Buttons {
    timing: Timing,
    pressed: BTreeMap<Button, Press>,
    taps: BTreeMap<Button, Taps>,
}

impl Buttons {
    pub fn new(timing: Timing) -> Result<Self, &'static str> {
        if timing.tap_max.is_zero() || timing.tap_gap.is_zero() {
            return Err("tap duration and gap must be greater than zero");
        }
        if timing.hold_after <= timing.tap_max {
            return Err("hold threshold must be longer than the maximum tap duration");
        }
        Ok(Self {
            timing,
            pressed: BTreeMap::new(),
            taps: BTreeMap::new(),
        })
    }

    pub(crate) fn is_tap_release(&self, button: Button, at: Duration) -> bool {
        self.pressed.get(&button).is_some_and(|press| {
            !press.held && at.saturating_sub(press.started) <= self.timing.tap_max
        })
    }

    /// Process a press or release. Duplicate transitions are harmless.
    pub fn update(&mut self, button: Button, down: bool, at: Duration) -> Vec<Gesture> {
        let mut gestures = self.advance(at);
        if down {
            self.pressed.entry(button).or_insert(Press {
                started: at,
                held: false,
            });
        } else if let Some(press) = self.pressed.remove(&button) {
            if !press.held && at.saturating_sub(press.started) <= self.timing.tap_max {
                let taps = self.taps.entry(button).or_insert(Taps {
                    released: at,
                    count: 0,
                });
                taps.released = at;
                taps.count = taps.count.saturating_add(1);
            } else {
                self.finish_taps(button, &mut gestures);
            }
        }
        gestures
    }

    /// Run due timers even when no input arrives.
    pub fn advance(&mut self, now: Duration) -> Vec<Gesture> {
        let mut gestures = Vec::new();
        let due: Vec<_> = self
            .taps
            .iter()
            .filter(|(button, taps)| {
                !self.pressed.contains_key(button)
                    && now.saturating_sub(taps.released) >= self.timing.tap_gap
            })
            .map(|(&button, _)| button)
            .collect();
        for button in due {
            self.finish_taps(button, &mut gestures);
        }
        for (&button, press) in &mut self.pressed {
            if !press.held && now.saturating_sub(press.started) >= self.timing.hold_after {
                press.held = true;
                if let Some(taps) = self.taps.remove(&button) {
                    gestures.push(Gesture::Taps {
                        button,
                        count: taps.count,
                    });
                }
                gestures.push(Gesture::Hold { button });
            }
        }
        gestures
    }

    /// Earliest wake-up needed to recognize a hold or finish a tap sequence.
    pub fn deadline(&self) -> Option<Duration> {
        let holds = self
            .pressed
            .values()
            .filter(|press| !press.held)
            .map(|press| press.started.saturating_add(self.timing.hold_after));
        let taps = self
            .taps
            .iter()
            .filter(|(button, _)| !self.pressed.contains_key(button))
            .map(|(_, taps)| taps.released.saturating_add(self.timing.tap_gap));
        holds.chain(taps).min()
    }

    /// Discard incomplete input rather than inventing releases after event loss.
    pub fn cancel_device(&mut self, device: u64) {
        self.pressed.retain(|button, _| button.device != device);
        self.taps.retain(|button, _| button.device != device);
    }

    fn finish_taps(&mut self, button: Button, gestures: &mut Vec<Gesture>) {
        if let Some(taps) = self.taps.remove(&button) {
            gestures.push(Gesture::Taps {
                button,
                count: taps.count,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: Button = Button {
        device: 1,
        code: 30,
    };
    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }
    fn engine() -> Buttons {
        Buttons::new(Timing::default()).unwrap()
    }

    #[test]
    fn double_tap_emits_one_gesture_after_settling() {
        let mut buttons = engine();
        for (down, at) in [(true, 0), (false, 50), (true, 150), (false, 200)] {
            assert!(buttons.update(KEY, down, ms(at)).is_empty());
        }
        assert!(buttons.advance(ms(499)).is_empty());
        assert_eq!(
            buttons.advance(ms(500)),
            [Gesture::Taps {
                button: KEY,
                count: 2
            }]
        );
        assert!(buttons.advance(ms(900)).is_empty());
    }

    #[test]
    fn three_taps_form_one_sequence() {
        let mut buttons = engine();
        for (down, at) in [
            (true, 0),
            (false, 50),
            (true, 150),
            (false, 200),
            (true, 300),
            (false, 350),
        ] {
            assert!(buttons.update(KEY, down, ms(at)).is_empty());
        }
        assert_eq!(
            buttons.advance(ms(650)),
            [Gesture::Taps {
                button: KEY,
                count: 3
            }]
        );
    }

    #[test]
    fn a_late_second_press_starts_another_sequence() {
        let mut buttons = engine();
        buttons.update(KEY, true, ms(0));
        buttons.update(KEY, false, ms(50));
        assert_eq!(
            buttons.update(KEY, true, ms(350)),
            [Gesture::Taps {
                button: KEY,
                count: 1
            }]
        );
        buttons.update(KEY, false, ms(400));
        assert_eq!(
            buttons.advance(ms(700)),
            [Gesture::Taps {
                button: KEY,
                count: 1
            }]
        );
    }

    #[test]
    fn a_second_press_keeps_the_tap_sequence_pending() {
        let mut buttons = engine();
        buttons.update(KEY, true, ms(0));
        buttons.update(KEY, false, ms(50));
        buttons.update(KEY, true, ms(300));
        assert!(buttons.advance(ms(400)).is_empty());
        buttons.update(KEY, false, ms(500));
        assert_eq!(
            buttons.advance(ms(800)),
            [Gesture::Taps {
                button: KEY,
                count: 2
            }]
        );
    }

    #[test]
    fn hold_fires_without_another_input_event_and_only_once() {
        let mut buttons = engine();
        buttons.update(KEY, true, ms(10));
        assert_eq!(buttons.deadline(), Some(ms(610)));
        assert!(buttons.advance(ms(609)).is_empty());
        assert_eq!(buttons.advance(ms(610)), [Gesture::Hold { button: KEY }]);
        assert_eq!(buttons.deadline(), None);
        assert!(buttons.advance(ms(2000)).is_empty());
        assert!(buttons.update(KEY, false, ms(2200)).is_empty());
    }

    #[test]
    fn release_can_recognize_a_hold_if_the_timer_was_late() {
        let mut buttons = engine();
        buttons.update(KEY, true, ms(0));
        assert_eq!(
            buttons.update(KEY, false, ms(700)),
            [Gesture::Hold { button: KEY }]
        );
    }

    #[test]
    fn duplicate_press_does_not_restart_a_hold() {
        let mut buttons = engine();
        buttons.update(KEY, true, ms(0));
        buttons.update(KEY, true, ms(500));
        assert_eq!(buttons.advance(ms(600)), [Gesture::Hold { button: KEY }]);
    }

    #[test]
    fn the_same_code_on_two_devices_is_independent() {
        let mut buttons = engine();
        let other = Button { device: 2, ..KEY };
        buttons.update(KEY, true, ms(0));
        buttons.update(other, true, ms(10));
        buttons.update(KEY, false, ms(50));
        buttons.cancel_device(1);
        assert_eq!(buttons.advance(ms(610)), [Gesture::Hold { button: other }]);
    }

    #[test]
    fn cancellation_discards_holds_and_unfinished_taps() {
        let mut buttons = engine();
        buttons.update(KEY, true, ms(0));
        buttons.update(KEY, false, ms(50));
        buttons.update(KEY, true, ms(100));
        buttons.cancel_device(KEY.device);
        assert_eq!(buttons.deadline(), None);
        assert!(buttons.update(KEY, false, ms(1000)).is_empty());
        assert!(buttons.advance(ms(2000)).is_empty());
    }

    #[test]
    fn a_hold_after_a_tap_finishes_the_tap_first() {
        let mut buttons = engine();
        buttons.update(KEY, true, ms(0));
        buttons.update(KEY, false, ms(50));
        buttons.update(KEY, true, ms(100));
        assert_eq!(
            buttons.advance(ms(700)),
            [
                Gesture::Taps {
                    button: KEY,
                    count: 1
                },
                Gesture::Hold { button: KEY },
            ]
        );
    }

    #[test]
    fn ambiguous_medium_press_is_neither_a_tap_nor_a_hold() {
        let mut buttons = engine();
        buttons.update(KEY, true, ms(0));
        assert!(buttons.update(KEY, false, ms(400)).is_empty());
        assert!(buttons.advance(ms(1000)).is_empty());
    }

    #[test]
    fn timing_rejects_overlapping_or_empty_thresholds() {
        assert!(
            Buttons::new(Timing {
                hold_after: ms(100),
                ..Timing::default()
            })
            .is_err()
        );
        assert!(
            Buttons::new(Timing {
                tap_gap: Duration::ZERO,
                ..Timing::default()
            })
            .is_err()
        );
    }
}
