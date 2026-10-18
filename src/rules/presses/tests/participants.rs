use super::*;
use crate::config::{Key, Millis};

fn taps(count: u32) -> Engine {
    let mut engine = configured(
        Trigger::ButtonTaps {
            key: Key(30),
            count,
        },
        MAX_BYTES,
    );
    let mut config = engine.config.clone();
    config.timing.tap_max = Millis(10);
    config.timing.hold_after = Millis(30);
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
fn chord() -> Engine {
    configured(
        Trigger::Chord {
            keys: vec![Key(30), Key(31)],
            ordered: false,
            timeout: Millis(100),
        },
        MAX_BYTES,
    )
}

#[test]
fn a_settled_tap_does_not_claim_the_following_held_press() {
    let mut engine = taps(1);
    frame(&mut engine, 1, 1, 10, &[(30, 1), (30, 0)]);
    frame(&mut engine, 1, 2, 15, &[(30, 1)]);
    let observed = engine.route_intents().pop().unwrap();
    assert_eq!(positions(&observed).len(), 2);
    let outcomes = tick(&mut engine, 46);
    assert_eq!(outcomes.len(), 1);
    assert_eq!(
        positions(&outcomes[0].intent),
        BTreeSet::from([(1, 1, 0, 30)])
    );
    assert_eq!(positions(&observed).len(), 2);
}

#[test]
fn a_settled_tap_excludes_an_invalid_release_tail() {
    let mut engine = taps(1);
    frame(&mut engine, 1, 1, 10, &[(30, 1), (30, 0)]);
    frame(&mut engine, 1, 2, 15, &[(30, 1)]);
    let outcomes = frame(&mut engine, 1, 3, 27, &[(30, 0)]);
    assert_eq!(outcomes.len(), 1);
    assert_eq!(
        positions(&outcomes[0].intent),
        BTreeSet::from([(1, 1, 0, 30)])
    );
}

#[test]
fn a_completed_tap_followed_by_a_hold_does_not_poison_the_next_fresh_tap() {
    let mut engine = taps(1);
    frame(&mut engine, 1, 1, 10, &[(30, 1), (30, 0)]);
    frame(&mut engine, 1, 2, 15, &[(30, 1)]);
    assert_eq!(tick(&mut engine, 46).len(), 1);
    frame(&mut engine, 1, 3, 47, &[(30, 0)]);
    frame(&mut engine, 1, 4, 50, &[(30, 1), (30, 0)]);
    let outcomes = tick(&mut engine, 71);
    assert_eq!(outcomes.len(), 1, "{:?}", engine.take_events());
    assert_eq!(
        positions(&outcomes[0].intent),
        BTreeSet::from([(1, 4, 0, 30)])
    );
}

#[test]
fn an_invalid_tap_cannot_lend_its_begin_only_activation_to_a_fresh_tap() {
    let mut engine = taps(1);
    let mut config = engine.config.clone();
    config.rules[0].require_keys = vec![Key(42)];
    engine.reload(config, Duration::ZERO).unwrap();
    frame(&mut engine, 1, 1, 10, &[(42, 1), (30, 1)]);
    frame(&mut engine, 1, 2, 22, &[(30, 0), (42, 0)]);
    frame(&mut engine, 1, 3, 25, &[(30, 1), (30, 0)]);
    assert!(tick(&mut engine, 46).is_empty());
}

#[test]
fn a_chord_selects_the_newest_actual_source_for_duplicate_key_codes() {
    let mut engine = chord();
    frame(&mut engine, 1, 1, 10, &[(30, 1)]);
    frame(&mut engine, 2, 1, 11, &[(30, 1)]);
    let outcomes = frame(&mut engine, 1, 2, 12, &[(31, 1)]);
    assert_eq!(outcomes.len(), 1);
    assert_eq!(
        positions(&outcomes[0].intent),
        BTreeSet::from([(2, 2, 0, 30), (1, 3, 0, 31)])
    );
}

#[test]
fn a_chord_reusing_a_held_member_keeps_its_original_incarnation() {
    let mut engine = chord();
    frame(&mut engine, 1, 1, 10, &[(30, 1)]);
    assert_eq!(frame(&mut engine, 2, 1, 11, &[(31, 1)]).len(), 1);
    frame(&mut engine, 2, 2, 12, &[(31, 0)]);
    let outcomes = frame(&mut engine, 2, 3, 13, &[(31, 1)]);
    assert_eq!(outcomes.len(), 1);
    assert_eq!(
        positions(&outcomes[0].intent),
        BTreeSet::from([(1, 1, 0, 30), (2, 4, 0, 31)])
    );
}

#[test]
fn a_chord_does_not_claim_an_earlier_released_incarnation() {
    let mut engine = chord();
    frame(&mut engine, 1, 1, 10, &[(30, 1)]);
    frame(&mut engine, 1, 2, 11, &[(30, 0)]);
    frame(&mut engine, 1, 3, 12, &[(30, 1)]);
    let outcomes = frame(&mut engine, 2, 1, 13, &[(31, 1)]);
    assert_eq!(outcomes.len(), 1);
    assert_eq!(
        positions(&outcomes[0].intent),
        BTreeSet::from([(1, 3, 0, 30), (2, 4, 0, 31)])
    );
}

#[test]
fn a_later_same_report_first_step_cannot_contaminate_an_earlier_complete_sequence() {
    let mut engine = ordered_keys();
    let outcomes = frame(&mut engine, 1, 1, 10, &[(30, 1), (30, 0), (31, 1), (30, 1)]);
    assert_eq!(outcomes.len(), 1);
    assert_eq!(
        positions(&outcomes[0].intent),
        BTreeSet::from([(1, 1, 0, 30), (1, 1, 2, 31)])
    );
}

#[test]
fn a_completed_tap_keeps_valid_authority_after_its_held_tail_is_abandoned() {
    let mut engine = taps(1);
    frame(&mut engine, 1, 1, 10, &[(30, 1), (30, 0)]);
    frame(&mut engine, 1, 2, 15, &[(30, 1)]);
    let completed = tick(&mut engine, 46).pop().unwrap().intent;
    frame(&mut engine, 1, 3, 47, &[(30, 0)]);
    frame(&mut engine, 1, 4, 50, &[(30, 1), (30, 0)]);
    assert!(
        engine
            .validate_route(&completed, Duration::from_millis(50))
            .is_ok()
    );
    assert_eq!(positions(&completed), BTreeSet::from([(1, 1, 0, 30)]));
}

#[test]
fn an_unmatched_tap_count_ends_its_admission_before_another_group_can_start() {
    let mut engine = taps(2);
    let mut config = engine.config.clone();
    config.rules[0].require_keys = vec![Key(42)];
    engine.reload(config, Duration::ZERO).unwrap();
    frame(&mut engine, 1, 1, 10, &[(42, 1), (30, 1), (30, 0)]);
    frame(&mut engine, 1, 2, 15, &[(30, 1)]);
    assert!(frame(&mut engine, 1, 3, 27, &[(30, 0), (42, 0)]).is_empty());
    frame(&mut engine, 1, 4, 30, &[(30, 1), (30, 0)]);
    frame(&mut engine, 1, 5, 35, &[(30, 1), (30, 0)]);
    assert!(tick(&mut engine, 56).is_empty());
}

#[test]
fn a_carried_chord_member_is_a_real_source_retirement_dependency() {
    let mut engine = chord();
    frame(&mut engine, 1, 1, 10, &[(30, 1)]);
    frame(&mut engine, 2, 1, 11, &[(31, 1)]);
    frame(&mut engine, 2, 2, 12, &[(31, 0)]);
    let completed = frame(&mut engine, 2, 3, 13, &[(31, 1)])
        .pop()
        .unwrap()
        .intent;
    assert!(
        completed
            .sources()
            .contains(&crate::keys::worker::tests::source(1))
    );
    let mut replacement = engine.devices[&1].device.clone();
    replacement.source.generation += 1;
    engine
        .push(Record::Device {
            device: replacement,
            keys: BTreeSet::new(),
            contacts: vec![],
        })
        .unwrap();
    assert!(
        engine
            .validate_route(&completed, Duration::from_millis(14))
            .is_err()
    );
}

#[test]
fn a_superseded_chord_observation_is_not_a_completed_source_dependency() {
    let mut engine = chord();
    let mut third = engine.devices[&2].device.clone();
    third.source = crate::keys::worker::tests::source(3);
    third.identity.push_str("-third");
    engine
        .push(Record::Device {
            device: third,
            keys: BTreeSet::new(),
            contacts: vec![],
        })
        .unwrap();
    frame(&mut engine, 1, 1, 10, &[(30, 1)]);
    frame(&mut engine, 2, 1, 11, &[(30, 1)]);
    frame(&mut engine, 3, 1, 12, &[(30, 1)]);
    let completed = frame(&mut engine, 1, 2, 13, &[(31, 1)])
        .pop()
        .unwrap()
        .intent;
    assert_eq!(
        completed.sources(),
        &BTreeSet::from([
            crate::keys::worker::tests::source(1),
            crate::keys::worker::tests::source(3)
        ])
    );
    let mut replacement = engine.devices[&2].device.clone();
    replacement.source.generation += 1;
    engine
        .push(Record::Device {
            device: replacement,
            keys: BTreeSet::new(),
            contacts: vec![],
        })
        .unwrap();
    assert!(
        engine
            .validate_route(&completed, Duration::from_millis(14))
            .is_ok()
    );
}

#[test]
fn an_already_active_next_step_is_visible_without_an_extra_input_or_tick() {
    let mut trigger = ordered_trigger();
    let Trigger::Sequence { steps, .. } = &mut trigger else {
        unreachable!()
    };
    steps[1] = Trigger::ButtonHold {
        key: Key(31),
        after: Millis(60),
        repeat: None,
    };
    let mut engine = configured(trigger, MAX_BYTES);
    assert!(frame(&mut engine, 1, 1, 10, &[(30, 1), (31, 1)]).is_empty());
    let pending = engine.route_intents().pop().unwrap();
    assert_eq!(
        positions(&pending),
        BTreeSet::from([(1, 1, 0, 30), (1, 1, 1, 31)])
    );
}

#[test]
fn a_sequence_replaces_tap_observations_with_its_completed_prefix_and_current_step() {
    let mut engine = taps(1);
    let mut config = engine.config.clone();
    config.rules[0].trigger = Trigger::Sequence {
        steps: vec![
            Trigger::ButtonTaps {
                key: Key(30),
                count: 1,
            },
            Trigger::ButtonHold {
                key: Key(31),
                after: Millis(60),
                repeat: None,
            },
        ],
        timeout: Millis(200),
    };
    engine.reload(config, Duration::ZERO).unwrap();
    frame(&mut engine, 1, 1, 10, &[(30, 1), (30, 0)]);
    frame(&mut engine, 1, 2, 15, &[(30, 1)]);
    frame(&mut engine, 2, 1, 20, &[(31, 1)]);
    assert!(tick(&mut engine, 46).is_empty());
    assert_eq!(
        positions(&engine.route_intents().pop().unwrap()),
        BTreeSet::from([(1, 1, 0, 30), (2, 3, 0, 31)])
    );
    let completed = tick(&mut engine, 81).pop().unwrap().intent;
    assert_eq!(
        positions(&completed),
        BTreeSet::from([(1, 1, 0, 30), (2, 3, 0, 31)])
    );
}

#[test]
fn an_unfinished_released_step_does_not_remain_in_the_parent_footprint_or_sources() {
    let mut trigger = ordered_trigger();
    let Trigger::Sequence { steps, .. } = &mut trigger else {
        unreachable!()
    };
    steps[1] = Trigger::ButtonHold {
        key: Key(31),
        after: Millis(60),
        repeat: None,
    };
    let mut engine = configured(trigger, MAX_BYTES);
    frame(&mut engine, 1, 1, 10, &[(30, 1)]);
    frame(&mut engine, 2, 1, 15, &[(31, 1)]);
    frame(&mut engine, 2, 2, 20, &[(31, 0)]);
    let pending = engine.route_intents().pop().unwrap();
    assert_eq!(positions(&pending), BTreeSet::from([(1, 1, 0, 30)]));
    assert_eq!(
        pending.sources(),
        &BTreeSet::from([crate::keys::worker::tests::source(1)])
    );
    frame(&mut engine, 1, 2, 25, &[(31, 1)]);
    let completed = tick(&mut engine, 86).pop().unwrap().intent;
    assert_eq!(
        positions(&completed),
        BTreeSet::from([(1, 1, 0, 30), (1, 4, 0, 31)])
    );
}

#[test]
fn exact_selection_reuses_whole_snapshots_and_rejects_duplicate_positions_before_charging() {
    let budget = budget(2 * (NODE_BYTES + 5));
    let mut observed = Set::empty(budget.clone());
    observed.insert(press(1, 0, 30)).unwrap();
    observed.insert(press(2, 0, 31)).unwrap();
    let selected = observed
        .select(vec![press(2, 0, 31), press(1, 0, 30)])
        .unwrap();
    assert!(Arc::ptr_eq(
        observed.root.as_ref().unwrap(),
        selected.root.as_ref().unwrap()
    ));
    assert!(
        observed
            .select(vec![press(1, 0, 30), press(1, 0, 30)])
            .is_err()
    );
    assert!(observed.select(vec![press(1, 0, 30)]).is_err());
    assert_eq!(observed.count, 2);
    assert_eq!(budget.used.load(Ordering::Acquire), 2 * (NODE_BYTES + 5));
    drop((observed, selected));
    assert_eq!(budget.used.load(Ordering::Acquire), 0);
}
