use super::*;

mod admission;

fn press(frame: u64, event: usize, code: u16) -> Press {
    Press {
        source: Source {
            device: 1,
            generation: 1,
            seat: "seat0".into(),
        },
        position: InputPosition { frame, event },
        code,
    }
}
fn budget(limit: usize) -> Arc<Budget> {
    Arc::new(Budget {
        used: AtomicUsize::new(0),
        limit,
    })
}

#[test]
fn snapshots_keep_their_original_members_and_release_shared_storage_exactly_once() {
    let budget = budget(3 * (NODE_BYTES + 5));
    let mut current = Set::empty(budget.clone());
    current.insert(press(1, 0, 30)).unwrap();
    let snapshot = current.clone();
    current.insert(press(2, 0, 31)).unwrap();
    assert_eq!(snapshot.iter().count(), 1);
    assert_eq!(current.iter().count(), 2);
    assert!(!snapshot.contains(&press(2, 0, 31).source, 31, press(2, 0, 31).position));
    assert_eq!(budget.used.load(Ordering::Acquire), 2 * (NODE_BYTES + 5));
    drop(current);
    assert_eq!(budget.used.load(Ordering::Acquire), NODE_BYTES + 5);
    drop(snapshot);
    assert_eq!(budget.used.load(Ordering::Acquire), 0);
}

#[test]
fn byte_budget_failure_and_duplicate_input_do_not_mutate_a_snapshot() {
    let budget = budget(NODE_BYTES + 5);
    let mut set = Set::empty(budget.clone());
    set.insert(press(1, 0, 30)).unwrap();
    set.insert(press(1, 0, 30)).unwrap();
    assert!(set.insert(press(2, 0, 30)).is_err());
    assert_eq!(set.count, 1);
    assert_eq!(budget.used.load(Ordering::Acquire), NODE_BYTES + 5);
    for wrong in [
        press(0, 0, 30),
        press(2, 4096, 30),
        press(2, 0, 0x300),
        press(1, 0, 31),
    ] {
        assert!(set.insert(wrong).is_err());
        assert_eq!(set.count, 1);
    }
    drop(set);
    assert_eq!(budget.used.load(Ordering::Acquire), 0);
}

#[test]
fn unions_deduplicate_shared_prefixes_without_copying_or_recharging_them() {
    let budget = Arc::new(Budget::default());
    let mut left = Set::empty(budget.clone());
    left.insert(press(1, 0, 30)).unwrap();
    let mut right = left.clone();
    right.insert(press(2, 0, 31)).unwrap();
    let bytes = budget.used.load(Ordering::Acquire);
    left.merge(&right).unwrap();
    left.merge(&right).unwrap();
    assert_eq!(left.count, 2);
    assert_eq!(left.iter().count(), 2);
    assert_eq!(budget.used.load(Ordering::Acquire), bytes);
    let mut other = Set::empty(budget.clone());
    other.insert(press(3, 0, 32)).unwrap();
    left.merge(&other).unwrap();
    assert_eq!(left.iter().count(), 3);
    drop((left, right, other));
    assert_eq!(budget.used.load(Ordering::Acquire), 0);
}

#[test]
fn union_rejects_foreign_budgets_conflicting_positions_and_exhausted_storage() {
    let budget = budget(2 * (NODE_BYTES + 5));
    let mut left = Set::empty(budget.clone());
    let mut right = Set::empty(budget.clone());
    left.insert(press(1, 0, 30)).unwrap();
    right.insert(press(2, 0, 31)).unwrap();
    assert!(left.merge(&right).is_err());
    assert_eq!(left.count, 1);
    assert!(
        left.merge(&Set::empty(Arc::new(Budget::default())))
            .is_err()
    );
    drop(right);
    let mut wrong = Set::empty(budget.clone());
    wrong.insert(press(1, 0, 31)).unwrap();
    assert!(left.merge(&wrong).is_err());
    assert_eq!(left.count, 1);
    drop((left, wrong));
    assert_eq!(budget.used.load(Ordering::Acquire), 0);
}

#[test]
fn independently_recorded_shared_members_are_yielded_only_once_after_union() {
    let budget = Arc::new(Budget::default());
    let mut left = Set::empty(budget.clone());
    let mut right = Set::empty(budget.clone());
    left.insert(press(1, 0, 30)).unwrap();
    right.insert(press(1, 0, 30)).unwrap();
    left.insert(press(2, 0, 31)).unwrap();
    right.insert(press(3, 0, 32)).unwrap();
    left.merge(&right).unwrap();
    assert_eq!(left.count, 3);
    assert_eq!(left.iter().count(), 3);
    assert_eq!(left.iter().collect::<BTreeSet<_>>().len(), 3);
    drop((left, right));
    assert_eq!(budget.used.load(Ordering::Acquire), 0);
}

#[test]
fn deep_shared_provenance_retires_without_recursive_chain_teardown() {
    let budget = Arc::new(Budget::default());
    let mut set = Set::empty(budget.clone());
    for frame in 1..=MAX_PRESSES as u64 {
        set.insert(press(frame, 0, 30)).unwrap();
    }
    assert_eq!(set.iter().count(), MAX_PRESSES);
    assert!(set.insert(press(MAX_PRESSES as u64 + 1, 0, 30)).is_err());
    drop(set);
    assert_eq!(budget.used.load(Ordering::Acquire), 0);
}

fn configured(trigger: Trigger, limit: usize) -> Engine {
    configured_required(trigger, limit, Vec::new())
}
fn configured_required(
    trigger: Trigger,
    limit: usize,
    required: Vec<crate::config::Key>,
) -> Engine {
    configured_mode(trigger, limit, required, true)
}
fn configured_mode(
    trigger: Trigger,
    limit: usize,
    required: Vec<crate::config::Key>,
    routing: bool,
) -> Engine {
    use crate::{
        config::{Key, Millis, Mode, Selector},
        context::{Binding, SessionKind, Snapshot, State},
        protocol::{Epoch, Fence},
    };
    let mut config = Config::preset("minimal").unwrap();
    config.mode = Mode::Route;
    config.policy.allowed_devices = vec!["keys".into()];
    config.policy.allow_input_events = true;
    config.devices = BTreeMap::from([(
        "keys".into(),
        Selector {
            keys: vec![Key(30)],
            ..Default::default()
        },
    )]);
    config.timing.tap_gap = Millis(20);
    let mut rule = crate::keys::worker::tests::rule(
        "provenance",
        Action::Keys {
            keys: vec![Key(42)],
        },
    );
    rule.delivery = crate::config::Delivery::Buffer;
    rule.trigger = trigger;
    rule.require_keys = required;
    config.rules = vec![rule];
    let fence = Fence {
        broker: Epoch([1, 2]),
        configuration: 1,
        session: 1,
    };
    let mut engine = Engine::build_with_budget(config, Some(fence), budget(limit)).unwrap();
    if routing {
        engine.enable_routing();
    }
    engine
        .push(Record::Header {
            version: crate::replay::FORMAT_VERSION,
        })
        .unwrap();
    for id in [1, 2] {
        engine
            .push(Record::Device {
                device: Device {
                    source: crate::keys::worker::tests::source(id),
                    identity: format!("press-model-{id}"),
                    name: "Model keyboard".into(),
                    path: String::new(),
                    physical: String::new(),
                    unique: String::new(),
                    vendor: 1,
                    product: 1,
                    properties: vec![],
                    capabilities: BTreeMap::from([(crate::input::EV_KEY, vec![30, 31, 32, 42])]),
                    axes: BTreeMap::new(),
                    virtual_device: false,
                },
                keys: BTreeSet::new(),
                contacts: vec![],
            })
            .unwrap();
    }
    engine
        .authority
        .bind(
            Binding {
                seat: "seat0".into(),
                session: "provenance-model".into(),
                uid: 1000,
                compositor_pid: 42,
                compositor_start: 1,
                kind: SessionKind::User,
            },
            true,
            false,
        )
        .unwrap();
    engine
        .authority
        .update(
            Snapshot {
                generation: 1,
                revision: 1,
                captured_at: Duration::ZERO,
                state: State::Desktop,
                outputs: vec![],
                windows: vec![],
                regions: vec![],
                secure_overlay: false,
            },
            Duration::ZERO,
        )
        .unwrap();
    engine
}
fn frame(
    engine: &mut Engine,
    source: u64,
    sequence: u64,
    at: u64,
    events: &[(u16, i32)],
) -> Vec<RouteOutcome> {
    let frame = crate::input::Frame {
        source: crate::keys::worker::tests::source(source),
        sequence,
        at: Duration::from_millis(at),
        events: events
            .iter()
            .map(|&(code, value)| crate::input::RawEvent {
                kind: crate::input::EV_KEY,
                code,
                value,
            })
            .chain(std::iter::once(crate::input::RawEvent {
                kind: crate::input::EV_SYN,
                code: crate::input::SYN_REPORT,
                value: 0,
            }))
            .collect(),
    };
    engine.push(Record::Frame { frame }).unwrap();
    engine.take_route_outcomes()
}
fn positions(intent: &RouteIntent) -> BTreeSet<(u64, u64, usize, u16)> {
    intent
        .candidate
        .key_presses
        .iter()
        .map(|press| {
            (
                press.source.device,
                press.position.frame,
                press.position.event,
                press.code,
            )
        })
        .collect()
}

#[test]
fn immediate_matches_keep_original_press_identity_through_repeat_release_and_repress() {
    let mut engine = configured_required(
        Trigger::Key {
            key: crate::config::Key(30),
        },
        MAX_BYTES,
        vec![crate::config::Key(31)],
    );
    let outcomes = frame(
        &mut engine,
        1,
        1,
        10,
        &[
            (31, 1),
            (30, 1),
            (30, 2),
            (30, 0),
            (30, 1),
            (30, 0),
            (31, 0),
        ],
    );
    assert_eq!(outcomes.len(), 4);
    for (index, outcome) in outcomes.iter().enumerate() {
        let event = if index < 2 { 1 } else { 4 };
        assert_eq!(
            positions(&outcome.intent),
            BTreeSet::from([(1, 1, event, 30)])
        );
        assert!(outcome.intent.has_key_press(
            &crate::keys::worker::tests::source(1),
            30,
            InputPosition { frame: 1, event }
        ));
        assert!(!outcome.intent.has_key_press(
            &crate::keys::worker::tests::source(1),
            31,
            InputPosition { frame: 1, event: 0 }
        ));
        let mut replaced = crate::keys::worker::tests::source(1);
        replaced.generation = 2;
        assert!(
            !outcome
                .intent
                .has_key_press(&replaced, 30, InputPosition { frame: 1, event })
        );
    }
    assert_eq!(
        outcomes[1].intent.position(),
        Some(InputPosition { frame: 1, event: 3 })
    );
    assert_eq!(
        outcomes[1].intent.sole_key_press().unwrap().position,
        InputPosition { frame: 1, event: 1 }
    );
}

#[test]
fn timed_hold_uses_the_original_down_not_its_timer_or_latest_repeat_boundary() {
    let mut engine = configured(
        Trigger::ButtonHold {
            key: crate::config::Key(30),
            after: crate::config::Millis(20),
            repeat: None,
        },
        MAX_BYTES,
    );
    assert!(frame(&mut engine, 1, 1, 10, &[(31, 1), (30, 1), (31, 0)]).is_empty());
    let saved = engine.route_intents().pop().unwrap();
    assert!(frame(&mut engine, 1, 2, 20, &[(32, 1), (32, 0), (30, 2)]).is_empty());
    engine
        .push(Record::Tick {
            at: Duration::from_millis(31),
        })
        .unwrap();
    let outcome = engine.take_route_outcomes().pop().unwrap();
    assert_eq!(positions(&outcome.intent), BTreeSet::from([(1, 1, 1, 30)]));
    assert_eq!(positions(&saved), positions(&outcome.intent));
    assert_eq!(
        outcome.intent.position(),
        Some(InputPosition::after_frame(2))
    );
}

#[test]
fn settled_taps_retain_both_press_incarnations_without_retroactively_mutating_snapshots() {
    let mut engine = configured(
        Trigger::ButtonTaps {
            key: crate::config::Key(30),
            count: 2,
        },
        MAX_BYTES,
    );
    frame(&mut engine, 1, 1, 10, &[(31, 1), (30, 1), (30, 0), (31, 0)]);
    let saved = engine.route_intents().pop().unwrap();
    frame(&mut engine, 1, 2, 20, &[(32, 1), (30, 1), (30, 0), (32, 0)]);
    engine
        .push(Record::Tick {
            at: Duration::from_millis(50),
        })
        .unwrap();
    let outcome = engine.take_route_outcomes().pop().unwrap();
    assert_eq!(positions(&saved), BTreeSet::from([(1, 1, 1, 30)]));
    assert_eq!(
        positions(&outcome.intent),
        BTreeSet::from([(1, 1, 1, 30), (1, 2, 1, 30)])
    );
    assert_eq!(
        outcome.intent.position(),
        Some(InputPosition::after_frame(2))
    );
}

#[test]
fn a_cross_source_chord_carries_only_its_actual_participating_presses() {
    let mut engine = configured(
        Trigger::Chord {
            keys: vec![crate::config::Key(30), crate::config::Key(31)],
            ordered: false,
            timeout: crate::config::Millis(100),
        },
        MAX_BYTES,
    );
    frame(&mut engine, 1, 1, 10, &[(32, 1), (30, 1)]);
    let saved = engine.route_intents().pop().unwrap();
    let outcome = frame(&mut engine, 2, 1, 20, &[(31, 1), (32, 1), (32, 0)])
        .pop()
        .unwrap();
    assert_eq!(positions(&saved), BTreeSet::from([(1, 1, 1, 30)]));
    assert_eq!(
        positions(&outcome.intent),
        BTreeSet::from([(1, 1, 1, 30), (2, 2, 0, 31)])
    );
    assert_eq!(outcome.intent.sources().len(), 2);
}

#[test]
fn sequence_snapshots_merge_real_press_evidence_from_every_step_and_source() {
    let mut engine = configured(
        Trigger::Sequence {
            steps: vec![
                Trigger::ButtonTaps {
                    key: crate::config::Key(30),
                    count: 1,
                },
                Trigger::Chord {
                    keys: vec![crate::config::Key(31), crate::config::Key(32)],
                    ordered: false,
                    timeout: crate::config::Millis(100),
                },
            ],
            timeout: crate::config::Millis(200),
        },
        MAX_BYTES,
    );
    let budget = engine.press_budget.clone();
    frame(&mut engine, 1, 1, 10, &[(30, 1), (30, 0), (42, 1), (42, 0)]);
    engine
        .push(Record::Tick {
            at: Duration::from_millis(31),
        })
        .unwrap();
    assert!(engine.take_route_outcomes().is_empty());
    let saved = engine.route_intents().pop().unwrap();
    let outcome = frame(&mut engine, 2, 1, 40, &[(32, 1), (31, 1)])
        .pop()
        .unwrap();
    assert_eq!(positions(&saved), BTreeSet::from([(1, 1, 0, 30)]));
    assert_eq!(
        positions(&outcome.intent),
        BTreeSet::from([(1, 1, 0, 30), (2, 2, 0, 32), (2, 2, 1, 31)])
    );
    assert!(
        engine.sequences[0]
            .steps
            .iter()
            .all(|step| Arc::ptr_eq(&step.press_budget, &engine.press_budget))
    );
    drop((saved, outcome, engine));
    assert_eq!(budget.used.load(Ordering::Acquire), 0);
}

#[test]
fn custom_key_candidates_retain_fresh_presses_but_not_repeat_or_release_edges() {
    let mut engine = configured(
        Trigger::Custom {
            recognizer: crate::recognition::Definition {
                name: "presses".into(),
                selectors: BTreeSet::from(["keys".into()]),
                inputs: BTreeSet::from([crate::recognition::InputKind::Key]),
                reply_within: crate::config::Millis(100),
                continuous: true,
            },
            timeout: crate::config::Millis(100),
            wake: None,
        },
        MAX_BYTES,
    );
    frame(&mut engine, 1, 1, 10, &[(30, 1)]);
    let saved = engine.route_intents().pop().unwrap();
    frame(&mut engine, 1, 2, 20, &[(30, 2), (30, 0), (31, 1)]);
    let current = engine.route_intents().pop().unwrap();
    assert_eq!(positions(&saved), BTreeSet::from([(1, 1, 0, 30)]));
    assert_eq!(
        positions(&current),
        BTreeSet::from([(1, 1, 0, 30), (1, 2, 2, 31)])
    );
}

#[test]
fn exhaustion_cancels_the_candidate_and_releases_the_budget_after_old_snapshots_drop() {
    let mut engine = configured(
        Trigger::ButtonTaps {
            key: crate::config::Key(30),
            count: 2,
        },
        NODE_BYTES + 5,
    );
    frame(&mut engine, 1, 1, 10, &[(30, 1), (30, 0)]);
    let saved = engine.route_intents().pop().unwrap();
    frame(&mut engine, 1, 2, 20, &[(30, 1)]);
    assert!(engine.route_intents().is_empty());
    assert!(
        engine
            .validate_route(&saved, Duration::from_millis(20))
            .is_err()
    );
    assert_eq!(
        engine.press_budget.used.load(Ordering::Acquire),
        NODE_BYTES + 5
    );
    assert!(engine.events.iter().any(
        |event| matches!(event,Event::Cancelled {reason,..} if reason.contains("provenance budget"))
    ));
    drop(saved);
    assert_eq!(engine.press_budget.used.load(Ordering::Acquire), 0);
    frame(&mut engine, 1, 3, 30, &[(30, 0), (30, 1), (30, 0)]);
    assert_eq!(engine.route_intents().len(), 1);
}

#[test]
fn reconciled_held_keys_do_not_gain_a_press_identity_from_duplicates_or_repeats() {
    let mut engine = configured(
        Trigger::Key {
            key: crate::config::Key(30),
        },
        MAX_BYTES,
    );
    let source = crate::keys::worker::tests::source(1);
    engine
        .push(Record::Reconcile {
            source,
            at: Duration::ZERO,
            keys: BTreeSet::from([30]),
            contacts: vec![],
        })
        .unwrap();
    assert!(frame(&mut engine, 1, 1, 10, &[(30, 1), (30, 2), (31, 1), (31, 0)]).is_empty());
    assert!(engine.route_intents().is_empty());
    assert_eq!(engine.press_budget.used.load(Ordering::Acquire), 0);
    frame(&mut engine, 1, 2, 20, &[(30, 0)]);
    let outcome = frame(&mut engine, 1, 3, 30, &[(30, 1)]).pop().unwrap();
    assert_eq!(positions(&outcome.intent), BTreeSet::from([(1, 3, 0, 30)]));
}

#[test]
fn replacement_sources_keep_old_provenance_distinct_and_cancel_old_authority() {
    let mut engine = configured(
        Trigger::Key {
            key: crate::config::Key(30),
        },
        MAX_BYTES,
    );
    let old = frame(&mut engine, 1, 1, 10, &[(30, 1)]).pop().unwrap();
    let mut replacement = engine.devices[&1].device.clone();
    engine
        .push(Record::Remove {
            source: replacement.source.clone(),
            at: Duration::from_millis(20),
        })
        .unwrap();
    replacement.source.generation = 2;
    engine
        .push(Record::Device {
            device: replacement.clone(),
            keys: BTreeSet::new(),
            contacts: vec![],
        })
        .unwrap();
    let mut next = crate::keys::worker::tests::frame(1, 1, 30, 30, 1);
    next.source = replacement.source.clone();
    engine.push(Record::Frame { frame: next }).unwrap();
    let current = engine.take_route_outcomes().pop().unwrap();
    assert_eq!(old.intent.sole_key_press().unwrap().source.generation, 1);
    assert_eq!(
        current.intent.sole_key_press().unwrap().source.generation,
        2
    );
    assert!(!old.intent.has_key_press(
        &replacement.source,
        30,
        InputPosition { frame: 1, event: 0 }
    ));
    assert!(
        engine
            .validate_route(&old.intent, Duration::from_millis(30))
            .is_err()
    );
}

#[test]
fn reload_keeps_old_retained_evidence_charged_without_inventing_held_input() {
    let mut engine = configured(
        Trigger::Key {
            key: crate::config::Key(30),
        },
        MAX_BYTES,
    );
    let old = frame(&mut engine, 1, 1, 10, &[(30, 1)]).pop().unwrap();
    let budget = engine.press_budget.clone();
    engine
        .reload(engine.config.clone(), Duration::from_millis(20))
        .unwrap();
    assert!(Arc::ptr_eq(&budget, &engine.press_budget));
    assert_eq!(budget.used.load(Ordering::Acquire), NODE_BYTES + 5);
    assert!(engine.route_intents().is_empty());
    drop(old);
    assert_eq!(budget.used.load(Ordering::Acquire), 0);
}

fn ordered_trigger() -> Trigger {
    Trigger::Sequence {
        steps: vec![
            Trigger::Key {
                key: crate::config::Key(30),
            },
            Trigger::Key {
                key: crate::config::Key(31),
            },
        ],
        timeout: crate::config::Millis(100),
    }
}
fn ordered_keys() -> Engine {
    configured(ordered_trigger(), MAX_BYTES)
}

#[test]
fn ordered_same_report_sequence_steps_retain_both_original_presses() {
    let mut engine = ordered_keys();
    let outcome = frame(&mut engine, 1, 1, 10, &[(30, 1), (31, 1)])
        .pop()
        .unwrap();
    assert_eq!(
        positions(&outcome.intent),
        BTreeSet::from([(1, 1, 0, 30), (1, 1, 1, 31)])
    );
}

#[test]
fn reverse_same_report_sequence_steps_require_a_new_later_press() {
    let mut engine = ordered_keys();
    assert!(frame(&mut engine, 1, 1, 10, &[(31, 1), (30, 1)]).is_empty());
    assert!(frame(&mut engine, 1, 2, 20, &[(31, 0)]).is_empty());
    let outcome = frame(&mut engine, 1, 3, 30, &[(31, 1)]).pop().unwrap();
    assert_eq!(
        positions(&outcome.intent),
        BTreeSet::from([(1, 1, 1, 30), (1, 3, 0, 31)])
    );
}

#[test]
fn equal_timestamps_do_not_make_an_earlier_report_a_later_sequence_step() {
    let mut engine = ordered_keys();
    assert!(frame(&mut engine, 1, 1, 10, &[(31, 1)]).is_empty());
    assert!(frame(&mut engine, 1, 2, 10, &[(30, 1)]).is_empty());
    assert!(frame(&mut engine, 1, 3, 20, &[(31, 0)]).is_empty());
    let outcome = frame(&mut engine, 1, 4, 30, &[(31, 1)]).pop().unwrap();
    assert_eq!(
        positions(&outcome.intent),
        BTreeSet::from([(1, 2, 0, 30), (1, 4, 0, 31)])
    );
}

#[test]
fn observation_only_sequences_enforce_original_event_order_without_retaining_presses() {
    for (events, expected) in [(vec![(30, 1), (31, 1)], 1), (vec![(31, 1), (30, 1)], 0)] {
        let mut engine = configured_mode(ordered_trigger(), MAX_BYTES, vec![], false);
        frame(&mut engine, 1, 1, 10, &events);
        assert_eq!(
            engine
                .take_events()
                .into_iter()
                .filter(|event| matches!(event, Event::Matched { .. }))
                .count(),
            expected
        );
        assert_eq!(engine.press_budget.used.load(Ordering::Acquire), 0);
    }
}

#[test]
fn a_held_later_step_cannot_start_in_an_earlier_report_with_an_equal_timestamp() {
    let mut trigger = ordered_trigger();
    let Trigger::Sequence { steps, .. } = &mut trigger else {
        unreachable!()
    };
    steps[1] = Trigger::ButtonHold {
        key: crate::config::Key(31),
        after: crate::config::Millis(20),
        repeat: None,
    };
    let mut engine = configured(trigger, MAX_BYTES);
    frame(&mut engine, 1, 1, 10, &[(31, 1)]);
    frame(&mut engine, 1, 2, 10, &[(30, 1)]);
    engine
        .push(Record::Tick {
            at: Duration::from_millis(31),
        })
        .unwrap();
    assert!(engine.take_route_outcomes().is_empty());
    frame(&mut engine, 1, 3, 40, &[(31, 0)]);
    frame(&mut engine, 1, 4, 50, &[(31, 1)]);
    engine
        .push(Record::Tick {
            at: Duration::from_millis(71),
        })
        .unwrap();
    let outcome = engine.take_route_outcomes().pop().unwrap();
    assert_eq!(
        positions(&outcome.intent),
        BTreeSet::from([(1, 2, 0, 30), (1, 4, 0, 31)])
    );
}

#[test]
fn delayed_tap_settlement_uses_its_physical_release_not_its_notification_boundary() {
    for ordered in [true, false] {
        let mut engine = configured(
            Trigger::Sequence {
                steps: vec![
                    Trigger::ButtonTaps {
                        key: crate::config::Key(30),
                        count: 1,
                    },
                    Trigger::ButtonHold {
                        key: crate::config::Key(31),
                        after: crate::config::Millis(20),
                        repeat: None,
                    },
                ],
                timeout: crate::config::Millis(100),
            },
            MAX_BYTES,
        );
        let events = if ordered {
            vec![(30, 1), (30, 0), (31, 1)]
        } else {
            vec![(31, 1), (30, 1), (30, 0)]
        };
        frame(&mut engine, 1, 1, 10, &events);
        engine
            .push(Record::Tick {
                at: Duration::from_millis(31),
            })
            .unwrap();
        let outcomes = engine.take_route_outcomes();
        assert_eq!(outcomes.len(), usize::from(ordered));
        if ordered {
            assert_eq!(
                positions(&outcomes[0].intent),
                BTreeSet::from([(1, 1, 0, 30), (1, 1, 2, 31)])
            );
        }
    }
}

#[test]
fn a_key_after_a_chord_cannot_start_before_the_chords_completing_press() {
    for ordered in [true, false] {
        let mut engine = configured(
            Trigger::Sequence {
                steps: vec![
                    Trigger::Chord {
                        keys: vec![crate::config::Key(30), crate::config::Key(31)],
                        ordered: false,
                        timeout: crate::config::Millis(100),
                    },
                    Trigger::Key {
                        key: crate::config::Key(32),
                    },
                ],
                timeout: crate::config::Millis(100),
            },
            MAX_BYTES,
        );
        let events = if ordered {
            vec![(30, 1), (31, 1), (32, 1)]
        } else {
            vec![(30, 1), (32, 1), (31, 1)]
        };
        let outcomes = frame(&mut engine, 1, 1, 10, &events);
        assert_eq!(outcomes.len(), usize::from(ordered));
        if ordered {
            assert_eq!(
                positions(&outcomes[0].intent),
                BTreeSet::from([(1, 1, 0, 30), (1, 1, 1, 31), (1, 1, 2, 32)])
            );
        }
    }
}

#[test]
fn a_key_then_hold_can_share_the_exact_same_original_press() {
    let mut engine = configured(
        Trigger::Sequence {
            steps: vec![
                Trigger::Key {
                    key: crate::config::Key(30),
                },
                Trigger::ButtonHold {
                    key: crate::config::Key(30),
                    after: crate::config::Millis(20),
                    repeat: None,
                },
            ],
            timeout: crate::config::Millis(100),
        },
        MAX_BYTES,
    );
    frame(&mut engine, 1, 1, 10, &[(30, 1)]);
    engine
        .push(Record::Tick {
            at: Duration::from_millis(31),
        })
        .unwrap();
    let outcomes = engine.take_route_outcomes();
    assert_eq!(outcomes.len(), 1);
    assert_eq!(
        positions(&outcomes[0].intent),
        BTreeSet::from([(1, 1, 0, 30)])
    );
}

#[test]
fn a_following_same_key_hold_does_not_move_the_completed_tap_boundary() {
    let mut engine = configured(
        Trigger::Sequence {
            steps: vec![
                Trigger::ButtonTaps {
                    key: crate::config::Key(30),
                    count: 2,
                },
                Trigger::ButtonHold {
                    key: crate::config::Key(30),
                    after: crate::config::Millis(600),
                    repeat: None,
                },
            ],
            timeout: crate::config::Millis(1000),
        },
        MAX_BYTES,
    );
    frame(&mut engine, 1, 1, 10, &[(30, 1), (30, 0)]);
    frame(&mut engine, 1, 2, 20, &[(30, 1), (30, 0)]);
    frame(&mut engine, 1, 3, 25, &[(30, 1)]);
    let mut snapshot = engine
        .authority
        .fresh(Duration::from_millis(25))
        .unwrap()
        .clone();
    for at in [100, 200, 300, 400, 500, 600] {
        snapshot.revision += 1;
        snapshot.captured_at = Duration::from_millis(at);
        engine
            .authority
            .update(snapshot.clone(), snapshot.captured_at)
            .unwrap();
    }
    engine
        .push(Record::Tick {
            at: Duration::from_millis(626),
        })
        .unwrap();
    let outcomes = engine.take_route_outcomes();
    assert_eq!(outcomes.len(), 1, "{:?}", engine.take_events());
    assert_eq!(
        positions(&outcomes[0].intent),
        BTreeSet::from([(1, 1, 0, 30), (1, 2, 0, 30), (1, 3, 0, 30)])
    );
}

#[test]
fn shared_deep_snapshots_release_their_byte_charge_after_concurrent_drop() {
    let budget = Arc::new(Budget::default());
    let mut set = Set::empty(budget.clone());
    let mut snapshots = Vec::new();
    for index in 1..=MAX_PRESSES as u64 {
        set.insert(press(index, 0, 30)).unwrap();
        if index % 1024 == 0 {
            snapshots.push(set.clone());
        }
    }
    let handles: Vec<_> = snapshots
        .into_iter()
        .map(|snapshot| std::thread::spawn(move || drop(snapshot)))
        .collect();
    drop(set);
    for handle in handles {
        handle.join().unwrap();
    }
    assert_eq!(budget.used.load(Ordering::Acquire), 0);
}
