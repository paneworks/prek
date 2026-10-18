use super::*;
use crate::config::{Key, Millis};

fn tap_key(timeout: u64) -> Engine {
    configured(
        Trigger::Sequence {
            steps: vec![
                Trigger::ButtonTaps {
                    key: Key(30),
                    count: 1,
                },
                Trigger::Key { key: Key(31) },
            ],
            timeout: Millis(timeout),
        },
        MAX_BYTES,
    )
}
fn tick(engine: &mut Engine, at: u64) -> Vec<RouteOutcome> {
    engine
        .push(Record::Tick {
            at: Duration::from_millis(at),
        })
        .unwrap();
    engine.take_route_outcomes()
}

#[test]
fn a_later_completed_key_waits_for_earlier_tap_settlement() {
    let mut engine = tap_key(100);
    frame(&mut engine, 1, 1, 10, &[(30, 1), (30, 0)]);
    assert!(frame(&mut engine, 2, 1, 15, &[(31, 1), (31, 0)]).is_empty());
    let completed = tick(&mut engine, 31);
    assert_eq!(completed.len(), 1);
    assert_eq!(
        positions(&completed[0].intent),
        BTreeSet::from([(1, 1, 0, 30), (2, 2, 0, 31)])
    );
    assert!(
        matches!(completed[0].event, Event::Matched { at, .. } if at == Duration::from_millis(31))
    );
}

#[test]
fn a_later_same_report_key_can_finish_while_its_tap_prefix_settles() {
    let mut engine = tap_key(100);
    frame(&mut engine, 1, 1, 10, &[(30, 1), (30, 0), (31, 1), (31, 0)]);
    let completed = tick(&mut engine, 31);
    assert_eq!(completed.len(), 1);
    assert_eq!(
        positions(&completed[0].intent),
        BTreeSet::from([(1, 1, 0, 30), (1, 1, 2, 31)])
    );
}

#[test]
fn an_earlier_key_cannot_be_borrowed_after_a_tap_settles() {
    let mut engine = tap_key(100);
    frame(&mut engine, 1, 1, 10, &[(31, 1), (31, 0), (30, 1), (30, 0)]);
    assert!(tick(&mut engine, 31).is_empty());
}

#[test]
fn two_complete_sequences_in_one_report_keep_disjoint_presses_and_ids() {
    let mut engine = ordered_keys();
    let completed = frame(
        &mut engine,
        1,
        1,
        10,
        &[
            (30, 1),
            (30, 0),
            (31, 1),
            (31, 0),
            (30, 1),
            (30, 0),
            (31, 1),
            (31, 0),
        ],
    );
    assert_eq!(completed.len(), 2);
    assert_eq!(
        positions(&completed[0].intent),
        BTreeSet::from([(1, 1, 0, 30), (1, 1, 2, 31)])
    );
    assert_eq!(
        positions(&completed[1].intent),
        BTreeSet::from([(1, 1, 4, 30), (1, 1, 6, 31)])
    );
    assert_ne!(completed[0].intent.id(), completed[1].intent.id());
}

#[test]
fn a_new_partial_sequence_after_a_same_report_completion_keeps_its_fresh_press() {
    let mut engine = ordered_keys();
    assert_eq!(
        frame(
            &mut engine,
            1,
            1,
            10,
            &[(30, 1), (30, 0), (31, 1), (31, 0), (30, 1)]
        )
        .len(),
        1
    );
    let pending = engine.route_intents().pop().unwrap();
    assert_eq!(positions(&pending), BTreeSet::from([(1, 1, 4, 30)]));
    let completed = frame(&mut engine, 2, 1, 20, &[(31, 1)]);
    assert_eq!(completed.len(), 1);
    assert_eq!(
        positions(&completed[0].intent),
        BTreeSet::from([(1, 1, 4, 30), (2, 2, 0, 31)])
    );
}

#[test]
fn repeated_key_steps_require_a_fresh_down_not_the_same_press_twice() {
    let mut engine = configured(
        Trigger::Sequence {
            steps: vec![Trigger::Key { key: Key(30) }; 2],
            timeout: Millis(100),
        },
        MAX_BYTES,
    );
    assert!(frame(&mut engine, 1, 1, 10, &[(30, 1)]).is_empty());
    frame(&mut engine, 1, 2, 20, &[(30, 0)]);
    let completed = frame(&mut engine, 1, 3, 30, &[(30, 1)]);
    assert_eq!(completed.len(), 1);
    assert_eq!(
        positions(&completed[0].intent),
        BTreeSet::from([(1, 1, 0, 30), (1, 3, 0, 30)])
    );
}

#[test]
fn a_chords_completing_press_is_not_another_immediate_key_step() {
    let mut engine = configured(
        Trigger::Sequence {
            steps: vec![
                Trigger::Chord {
                    keys: vec![Key(30), Key(31)],
                    ordered: false,
                    timeout: Millis(100),
                },
                Trigger::Key { key: Key(31) },
            ],
            timeout: Millis(100),
        },
        MAX_BYTES,
    );
    assert!(frame(&mut engine, 1, 1, 10, &[(30, 1), (31, 1)]).is_empty());
    frame(&mut engine, 1, 2, 20, &[(31, 0)]);
    let completed = frame(&mut engine, 1, 3, 30, &[(31, 1)]);
    assert_eq!(completed.len(), 1);
    assert_eq!(
        positions(&completed[0].intent),
        BTreeSet::from([(1, 1, 0, 30), (1, 1, 1, 31), (1, 3, 0, 31)])
    );
}

#[test]
fn a_chords_completing_press_can_continue_into_its_own_hold() {
    let mut engine = configured(
        Trigger::Sequence {
            steps: vec![
                Trigger::Chord {
                    keys: vec![Key(30), Key(31)],
                    ordered: false,
                    timeout: Millis(100),
                },
                Trigger::ButtonHold {
                    key: Key(31),
                    after: Millis(20),
                    repeat: None,
                },
            ],
            timeout: Millis(100),
        },
        MAX_BYTES,
    );
    frame(&mut engine, 1, 1, 10, &[(30, 1), (31, 1)]);
    let completed = tick(&mut engine, 31);
    assert_eq!(completed.len(), 1);
    assert_eq!(
        positions(&completed[0].intent),
        BTreeSet::from([(1, 1, 0, 30), (1, 1, 1, 31)])
    );
}

#[test]
fn deferred_completion_does_not_extend_the_declared_group_deadline() {
    let mut engine = tap_key(15);
    frame(&mut engine, 1, 1, 10, &[(30, 1), (30, 0)]);
    frame(&mut engine, 2, 1, 15, &[(31, 1), (31, 0)]);
    assert!(tick(&mut engine, 31).is_empty());
}

#[test]
fn a_deferred_key_cannot_migrate_into_a_later_parent_after_completion() {
    let mut engine = tap_key(100);
    frame(&mut engine, 1, 1, 10, &[(30, 1), (30, 0)]);
    frame(&mut engine, 2, 1, 15, &[(31, 1), (31, 0)]);
    assert_eq!(tick(&mut engine, 31).len(), 1);
    frame(&mut engine, 1, 2, 35, &[(30, 1), (30, 0)]);
    assert!(tick(&mut engine, 56).is_empty());
}

#[test]
fn replacement_of_a_deferred_source_cannot_finish_the_old_sequence() {
    let mut engine = tap_key(100);
    frame(&mut engine, 1, 1, 10, &[(30, 1), (30, 0)]);
    frame(&mut engine, 2, 1, 15, &[(31, 1), (31, 0)]);
    let mut replacement = engine.devices[&2].device.clone();
    replacement.source.generation += 1;
    engine
        .push(Record::Device {
            device: replacement,
            keys: BTreeSet::new(),
            contacts: vec![],
        })
        .unwrap();
    assert!(tick(&mut engine, 31).is_empty());
}

#[test]
fn retained_key_completions_are_charged_once_without_widening_the_active_prefix() {
    let mut engine = tap_key(100);
    let budget = engine.press_budget.clone();
    frame(&mut engine, 1, 1, 10, &[(30, 1), (30, 0)]);
    frame(&mut engine, 2, 1, 15, &[(31, 1), (31, 0)]);
    assert_eq!(engine.sequences[0].pending["seat0"].deferred.len(), 1);
    let intent = engine.route_intents().pop().unwrap();
    assert_eq!(positions(&intent), BTreeSet::from([(1, 1, 0, 30)]));
    assert!(
        !intent
            .sources()
            .contains(&crate::keys::worker::tests::source(2))
    );
    let charged = budget.used.load(Ordering::Acquire);
    assert!(charged > 2 * NODE_BYTES);
    assert!(tick(&mut engine, 20).is_empty());
    assert_eq!(budget.used.load(Ordering::Acquire), charged);
    let completed = tick(&mut engine, 31);
    assert_eq!(completed.len(), 1);
    assert!(engine.sequences[0].pending.is_empty());
    assert!(budget.used.load(Ordering::Acquire) < charged);
    drop((intent, completed, engine));
    assert_eq!(budget.used.load(Ordering::Acquire), 0);
}

#[test]
fn deferred_byte_exhaustion_cancels_the_whole_parent_without_a_partial_action() {
    let mut engine = configured(tap_key(100).config.rules[0].trigger.clone(), 8 * NODE_BYTES);
    let budget = engine.press_budget.clone();
    frame(&mut engine, 1, 1, 10, &[(30, 1), (30, 0)]);
    frame(&mut engine, 2, 1, 15, &[(31, 1), (31, 0)]);
    assert!(engine.sequences[0].pending.is_empty());
    assert!(engine.take_events().iter().any(|event| matches!(event,
        Event::Cancelled { reason, .. } if reason == "sequence deferred completion budget exhausted")));
    assert!(tick(&mut engine, 31).is_empty());
    drop(engine);
    assert_eq!(budget.used.load(Ordering::Acquire), 0);
}

#[test]
fn deferred_entry_exhaustion_is_bounded_across_a_single_original_report() {
    let mut engine = configured(tap_key(100).config.rules[0].trigger.clone(), 64 * MAX_BYTES);
    let budget = engine.press_budget.clone();
    frame(&mut engine, 1, 1, 10, &[(30, 1), (30, 0)]);
    let events: Vec<_> = (0..=sequence::MAX_DEFERRED)
        .flat_map(|_| [(31, 1), (31, 0)])
        .collect();
    assert!(frame(&mut engine, 2, 1, 15, &events).is_empty());
    assert!(engine.sequences[0].pending.is_empty());
    assert_eq!(engine.take_events().iter().filter(|event| matches!(event,
        Event::Cancelled { reason, .. } if reason == "sequence deferred completion budget exhausted")).count(), 1);
    assert!(tick(&mut engine, 31).is_empty());
    drop(engine);
    assert_eq!(budget.used.load(Ordering::Acquire), 0);
}

#[test]
fn a_configuration_reload_releases_deferred_completions_without_reusing_them() {
    let mut engine = tap_key(100);
    let budget = engine.press_budget.clone();
    frame(&mut engine, 1, 1, 10, &[(30, 1), (30, 0)]);
    frame(&mut engine, 2, 1, 15, &[(31, 1), (31, 0)]);
    let charged = budget.used.load(Ordering::Acquire);
    engine
        .reload(engine.config.clone(), Duration::from_millis(20))
        .unwrap();
    assert!(budget.used.load(Ordering::Acquire) < charged);
    frame(&mut engine, 1, 2, 25, &[(30, 1), (30, 0)]);
    assert!(tick(&mut engine, 46).is_empty());
    drop(engine);
    assert_eq!(budget.used.load(Ordering::Acquire), 0);
}

#[test]
fn explicitly_cancelled_deferred_candidates_cannot_complete_a_parent() {
    let mut engine = tap_key(100);
    frame(&mut engine, 1, 1, 10, &[(30, 1), (30, 0)]);
    frame(&mut engine, 2, 1, 15, &[(31, 1), (31, 0)]);
    engine.sequences[0].pending["seat0"].deferred[0]
        .1
        .candidate
        .cancelled
        .store(true, Ordering::Release);
    assert!(tick(&mut engine, 31).is_empty());
    assert!(engine.sequences[0].pending["seat0"].deferred.is_empty());
}

fn three_keys(repeated: bool) -> Engine {
    configured(
        Trigger::Sequence {
            steps: vec![
                Trigger::ButtonTaps {
                    key: Key(30),
                    count: 1,
                },
                Trigger::Key { key: Key(31) },
                Trigger::Key {
                    key: Key(if repeated { 31 } else { 32 }),
                },
            ],
            timeout: Millis(100),
        },
        MAX_BYTES,
    )
}

#[test]
fn several_completed_future_steps_keep_their_original_order_and_positions() {
    let mut engine = three_keys(false);
    frame(&mut engine, 1, 1, 10, &[(30, 1), (30, 0)]);
    frame(&mut engine, 2, 1, 15, &[(31, 1), (31, 0), (32, 1), (32, 0)]);
    let completed = tick(&mut engine, 31);
    assert_eq!(completed.len(), 1);
    assert_eq!(
        positions(&completed[0].intent),
        BTreeSet::from([(1, 1, 0, 30), (2, 2, 0, 31), (2, 2, 2, 32)])
    );
}

#[test]
fn deferred_future_steps_cannot_reverse_the_original_physical_order() {
    let mut engine = three_keys(false);
    frame(&mut engine, 1, 1, 10, &[(30, 1), (30, 0)]);
    frame(&mut engine, 2, 1, 15, &[(32, 1), (32, 0), (31, 1), (31, 0)]);
    assert!(tick(&mut engine, 31).is_empty());
    assert_eq!(frame(&mut engine, 2, 2, 35, &[(32, 1), (32, 0)]).len(), 1);
}

#[test]
fn deferred_repeated_steps_still_need_distinct_physical_presses() {
    let mut engine = three_keys(true);
    frame(&mut engine, 1, 1, 10, &[(30, 1), (30, 0)]);
    frame(&mut engine, 2, 1, 15, &[(31, 1), (31, 0)]);
    assert!(tick(&mut engine, 31).is_empty());
    let completed = frame(&mut engine, 2, 2, 35, &[(31, 1), (31, 0)]);
    assert_eq!(completed.len(), 1);
    assert_eq!(
        positions(&completed[0].intent),
        BTreeSet::from([(1, 1, 0, 30), (2, 2, 0, 31), (2, 3, 0, 31)])
    );
}

fn layered_completions(toggle: bool) {
    let mut engine = ordered_keys();
    let mut config = engine.config.clone();
    config.layers = vec![crate::config::Layer {
        id: "sequence-mode".into(),
        key: Key(42),
        toggle,
        activation: Default::default(),
    }];
    config.rules[0].layer = Some("sequence-mode".into());
    engine.reload(config, Duration::ZERO).unwrap();
    frame(
        &mut engine,
        1,
        1,
        1,
        if toggle {
            &[(42, 1), (42, 0)]
        } else {
            &[(42, 1)]
        },
    );
    let completed = frame(
        &mut engine,
        1,
        2,
        10,
        &[
            (30, 1),
            (30, 0),
            (31, 1),
            (31, 0),
            (30, 1),
            (30, 0),
            (31, 1),
            (31, 0),
        ],
    );
    assert_eq!(completed.len(), 2);
    assert!(engine.conditions.layer_active("seat0", "sequence-mode"));
    assert!(
        engine.sequences[0]
            .steps
            .iter()
            .all(|step| step.conditions.layer_active("seat0", "sequence-mode"))
    );
    frame(
        &mut engine,
        1,
        3,
        20,
        if toggle {
            &[(42, 1), (42, 0)]
        } else {
            &[(42, 0)]
        },
    );
    assert!(frame(&mut engine, 1, 4, 25, &[(30, 1), (30, 0), (31, 1), (31, 0)]).is_empty());
}

#[test]
fn normal_sequence_reset_preserves_the_canonical_toggle_layer() {
    layered_completions(true);
}

#[test]
fn normal_sequence_reset_preserves_the_canonical_momentary_layer() {
    layered_completions(false);
}

#[test]
fn reconciliation_discards_a_completed_future_key_even_when_it_is_already_up() {
    let mut engine = tap_key(100);
    frame(&mut engine, 1, 1, 10, &[(30, 1), (30, 0)]);
    frame(&mut engine, 2, 1, 15, &[(31, 1), (31, 0)]);
    engine
        .push(Record::Reconcile {
            source: crate::keys::worker::tests::source(2),
            at: Duration::from_millis(20),
            keys: BTreeSet::new(),
            contacts: vec![],
        })
        .unwrap();
    assert!(engine.sequences[0].pending["seat0"].deferred.is_empty());
    assert!(tick(&mut engine, 31).is_empty());
}

#[test]
fn deferred_recognition_cannot_extend_a_nested_groups_physical_entry_deadline() {
    let mut engine = configured(
        Trigger::Sequence {
            steps: vec![
                Trigger::ButtonTaps {
                    key: Key(30),
                    count: 1,
                },
                Trigger::Sequence {
                    steps: vec![Trigger::Key { key: Key(31) }, Trigger::Key { key: Key(32) }],
                    timeout: Millis(10),
                },
            ],
            timeout: Millis(100),
        },
        MAX_BYTES,
    );
    frame(&mut engine, 1, 1, 10, &[(30, 1), (30, 0)]);
    frame(&mut engine, 2, 1, 15, &[(31, 1), (31, 0), (32, 1), (32, 0)]);
    assert!(tick(&mut engine, 31).is_empty());
    assert!(engine.sequences[0].pending.is_empty());
    assert!(engine.take_events().iter().any(|event| matches!(event,
        Event::Cancelled { reason, .. } if reason == "sequence timeout")));
}
