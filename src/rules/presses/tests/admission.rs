use super::*;
use crate::config::{Key, Layer, Millis};

fn triggers() -> [Trigger; 3] {
    [
        Trigger::ButtonTaps {
            key: Key(30),
            count: 1,
        },
        Trigger::ButtonHold {
            key: Key(30),
            after: Millis(20),
            repeat: None,
        },
        Trigger::Chord {
            keys: vec![Key(30), Key(31)],
            ordered: false,
            timeout: Millis(100),
        },
    ]
}
fn engine(trigger: Trigger, throughout: bool) -> Engine {
    let mut engine = configured_required(trigger, MAX_BYTES, vec![Key(42)]);
    let mut config = engine.config.clone();
    config.rules[0].keys_throughout = throughout;
    engine.reload(config, Duration::ZERO).unwrap();
    engine.take_events();
    engine
}
fn tick(engine: &mut Engine, at: u64) -> Vec<RouteOutcome> {
    engine
        .push(Record::Tick {
            at: Duration::from_millis(at),
        })
        .unwrap();
    engine.take_route_outcomes()
}
fn completed(trigger: &Trigger) -> Vec<(u16, i32)> {
    match trigger {
        Trigger::ButtonTaps { .. } => vec![(30, 1), (30, 0)],
        Trigger::ButtonHold { .. } => vec![(30, 1)],
        Trigger::Chord { .. } => vec![(30, 1), (31, 1)],
        _ => unreachable!(),
    }
}

#[test]
fn a_later_modifier_cannot_authorize_an_earlier_timed_or_chord_start() {
    for trigger in triggers() {
        let mut engine = engine(trigger.clone(), false);
        let mut edges = completed(&trigger);
        edges.push((42, 1));
        assert!(
            frame(&mut engine, 1, 1, 10, &edges).is_empty(),
            "{trigger:?}"
        );
        assert!(tick(&mut engine, 31).is_empty(), "{trigger:?}");
        assert!(engine.route_intents().is_empty(), "{trigger:?}");
    }
}

#[test]
fn begin_only_conditions_survive_a_later_modifier_release_in_the_same_report() {
    for trigger in triggers() {
        let mut engine = engine(trigger.clone(), false);
        let mut edges = vec![(42, 1)];
        edges.extend(completed(&trigger));
        edges.push((42, 0));
        let mut outcomes = frame(&mut engine, 1, 1, 10, &edges);
        outcomes.extend(tick(&mut engine, 31));
        assert_eq!(outcomes.len(), 1, "{trigger:?}: {:?}", engine.take_events());
        assert!(
            engine
                .validate_route(&outcomes[0].intent, Duration::from_millis(31))
                .is_ok()
        );
    }
}

#[test]
fn a_transient_modifier_loss_revokes_active_timed_and_chord_candidates() {
    for trigger in triggers() {
        for source in [1, 2] {
            let mut engine = engine(trigger.clone(), true);
            frame(&mut engine, source, 1, 5, &[(42, 1)]);
            frame(
                &mut engine,
                1,
                if source == 1 { 2 } else { 1 },
                10,
                &[(30, 1)],
            );
            let saved = engine.route_intents().pop().unwrap();
            frame(
                &mut engine,
                source,
                if source == 1 { 3 } else { 2 },
                15,
                &[(42, 0), (42, 1)],
            );
            assert!(
                engine
                    .validate_route(&saved, Duration::from_millis(15))
                    .is_err(),
                "{trigger:?}, source={source}"
            );
            assert!(
                frame(
                    &mut engine,
                    1,
                    if source == 1 { 4 } else { 2 },
                    20,
                    &[(30, 0), (31, 1)]
                )
                .is_empty()
            );
            assert!(tick(&mut engine, 41).is_empty(), "{trigger:?}");
        }
    }
}

#[test]
fn a_same_report_modifier_pulse_cannot_resurrect_an_old_trigger_press() {
    for trigger in triggers() {
        let mut engine = engine(trigger.clone(), true);
        let mut edges = vec![(42, 1), (30, 1), (42, 0), (42, 1)];
        if matches!(trigger, Trigger::Chord { .. }) {
            edges.push((31, 1));
        }
        assert!(frame(&mut engine, 1, 1, 10, &edges).is_empty());
        assert!(tick(&mut engine, 31).is_empty(), "{trigger:?}");
        assert!(frame(&mut engine, 1, 2, 40, &[(30, 0), (31, 0)]).is_empty());
        assert!(tick(&mut engine, 61).is_empty(), "{trigger:?}");
    }
}

#[test]
fn a_new_trigger_after_modifier_reentry_in_the_same_report_is_eligible() {
    for trigger in triggers() {
        let mut engine = engine(trigger.clone(), true);
        let mut edges = vec![(42, 1), (42, 0), (42, 1)];
        edges.extend(completed(&trigger));
        let mut outcomes = frame(&mut engine, 1, 1, 10, &edges);
        outcomes.extend(tick(&mut engine, 31));
        assert_eq!(outcomes.len(), 1, "{trigger:?}");
        assert!(
            engine
                .validate_route(&outcomes[0].intent, Duration::from_millis(31))
                .is_ok()
        );
    }
}

#[test]
fn momentary_and_toggled_layers_are_sampled_at_the_actual_timed_start() {
    for trigger in triggers() {
        for toggle in [false, true] {
            for early in [false, true] {
                let mut engine = engine(trigger.clone(), false);
                let mut config = engine.config.clone();
                config.rules[0].require_keys.clear();
                config.rules[0].layer = Some("timed".into());
                config.layers.push(Layer {
                    id: "timed".into(),
                    key: Key(42),
                    toggle,
                    activation: Default::default(),
                });
                engine.reload(config, Duration::ZERO).unwrap();
                let mut edges = completed(&trigger);
                if early {
                    edges.insert(0, (42, 1));
                } else {
                    edges.push((42, 1));
                }
                let mut outcomes = frame(&mut engine, 1, 1, 10, &edges);
                outcomes.extend(tick(&mut engine, 31));
                assert_eq!(
                    outcomes.len(),
                    usize::from(early),
                    "{trigger:?}, toggle={toggle}, early={early}"
                );
            }
        }
    }
}

#[test]
fn cancelling_a_cross_source_chord_discards_every_participants_old_pressed_state() {
    let mut engine = engine(
        Trigger::Chord {
            keys: vec![Key(30), Key(31), Key(32)],
            ordered: false,
            timeout: Millis(100),
        },
        true,
    );
    frame(&mut engine, 1, 1, 5, &[(42, 1), (30, 1)]);
    frame(&mut engine, 2, 1, 10, &[(31, 1)]);
    let old = engine.route_intents().pop().unwrap();
    let early = frame(
        &mut engine,
        1,
        2,
        15,
        &[(42, 0), (42, 1), (30, 0), (30, 1), (32, 1)],
    );
    assert!(
        engine
            .validate_route(&old, Duration::from_millis(15))
            .is_err()
    );
    assert!(early.is_empty());
    assert!(
        engine
            .take_events()
            .into_iter()
            .all(|event| !matches!(event, Event::Matched { .. })),
        "an old participant completed a replacement chord"
    );
    let outcomes = frame(&mut engine, 2, 2, 20, &[(31, 0), (31, 1)]);
    assert_eq!(outcomes.len(), 1);
    assert_eq!(
        positions(&outcomes[0].intent),
        BTreeSet::from([(1, 3, 3, 30), (1, 3, 4, 32), (2, 4, 1, 31)])
    );
}

#[test]
fn a_modifier_pulse_revokes_a_completed_prefix_even_without_an_active_step() {
    let mut engine = engine(
        Trigger::Sequence {
            steps: vec![
                Trigger::Key { key: Key(30) },
                Trigger::ButtonHold {
                    key: Key(31),
                    after: Millis(20),
                    repeat: None,
                },
            ],
            timeout: Millis(100),
        },
        true,
    );
    frame(&mut engine, 1, 1, 10, &[(42, 1), (30, 1), (30, 0)]);
    let old = engine.route_intents().pop().unwrap();
    frame(&mut engine, 1, 2, 15, &[(42, 0), (42, 1), (31, 1)]);
    assert!(
        engine
            .validate_route(&old, Duration::from_millis(15))
            .is_err()
    );
    assert!(tick(&mut engine, 36).is_empty());
}

#[test]
fn a_same_report_prefix_before_modifier_loss_cannot_survive_reentry() {
    let mut engine = engine(
        Trigger::Sequence {
            steps: vec![
                Trigger::Key { key: Key(30) },
                Trigger::ButtonHold {
                    key: Key(31),
                    after: Millis(20),
                    repeat: None,
                },
            ],
            timeout: Millis(100),
        },
        true,
    );
    frame(
        &mut engine,
        1,
        1,
        10,
        &[(42, 1), (30, 1), (30, 0), (42, 0), (42, 1), (31, 1)],
    );
    assert!(tick(&mut engine, 31).is_empty());
    assert!(engine.route_intents().is_empty());
}

#[test]
fn old_step_cancellation_does_not_revoke_a_fresh_prefix_after_reentry() {
    let mut engine = engine(
        Trigger::Sequence {
            steps: vec![
                Trigger::Key { key: Key(30) },
                Trigger::ButtonHold {
                    key: Key(31),
                    after: Millis(20),
                    repeat: None,
                },
            ],
            timeout: Millis(100),
        },
        true,
    );
    frame(&mut engine, 1, 1, 10, &[(42, 1), (30, 1), (30, 0), (31, 1)]);
    let old = engine.route_intents().pop().unwrap();
    frame(
        &mut engine,
        1,
        2,
        15,
        &[(42, 0), (42, 1), (31, 0), (30, 1), (30, 0), (31, 1)],
    );
    assert!(
        engine
            .validate_route(&old, Duration::from_millis(15))
            .is_err()
    );
    let outcomes = tick(&mut engine, 36);
    assert_eq!(outcomes.len(), 1, "{:?}", engine.take_events());
    assert_eq!(
        positions(&outcomes[0].intent),
        BTreeSet::from([(1, 2, 3, 30), (1, 2, 5, 31)])
    );
}

#[test]
fn repeated_short_holds_retain_only_the_latest_admitted_identity_per_source() {
    let trigger = Trigger::Sequence {
        steps: vec![
            Trigger::Key { key: Key(30) },
            Trigger::ButtonHold {
                key: Key(31),
                after: Millis(20),
                repeat: None,
            },
        ],
        timeout: Millis(100),
    };
    let mut engine = configured_mode(trigger, MAX_BYTES, vec![Key(42)], false);
    frame(&mut engine, 1, 1, 10, &[(42, 1), (30, 1), (30, 0)]);
    for index in 0..64 {
        frame(&mut engine, 1, 2 + index * 2, 20, &[(31, 1)]);
        frame(&mut engine, 1, 3 + index * 2, 20, &[(31, 0)]);
        engine.take_events();
    }
    assert_eq!(
        engine.sequences[0].pending["seat0"].step_candidates.len(),
        1
    );
    assert_eq!(engine.press_budget.used.load(Ordering::Acquire), 0);
}
