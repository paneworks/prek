use super::*;
use crate::{
    config::{Action, Key},
    input::{Device, EV_SYN, Frame, RawEvent, SYN_REPORT},
    keys::worker::tests::{frame, ms, routing, rule, source},
    routing::{KeyWork, Routing},
};

fn packet(id: u64, sequence: u64, at: u64, edges: &[(u16, i32)]) -> Frame {
    let mut frame = frame(id, sequence, at, 30, 1);
    frame.events = edges
        .iter()
        .map(|&(code, value)| RawEvent {
            kind: EV_KEY,
            code,
            value,
        })
        .collect();
    frame.events.push(RawEvent {
        kind: EV_SYN,
        code: SYN_REPORT,
        value: 0,
    });
    frame
}

fn send(routing: &mut Routing, frame: Frame) -> Vec<KeyWork> {
    let at = frame.at;
    routing.push(Record::Frame { frame }).unwrap();
    routing
        .take_scoped_dispatches(at)
        .into_iter()
        .map(|dispatch| dispatch.into_key_work().ok().expect("canonical key work"))
        .collect()
}

fn conditional(throughout: bool) -> Rule {
    let mut rule = rule("shift", Action::Remap { key: Key(42) });
    rule.require_keys = vec![Key(31)];
    rule.keys_throughout = throughout;
    rule
}

fn stages(work: &[KeyWork]) -> Vec<Stage> {
    work.iter()
        .map(|work| match work.event() {
            Event::Matched { stage, .. } => *stage,
            _ => panic!("not a match"),
        })
        .collect()
}

#[test]
fn same_report_activation_and_normal_completion_keep_original_positions() {
    let mut routing = routing(vec![conditional(true)]);
    let work = send(
        &mut routing,
        packet(1, 1, 10, &[(31, 1), (30, 1), (30, 0), (31, 0)]),
    );
    assert_eq!(stages(&work), [Stage::Begin, Stage::End]);
    assert_eq!(
        work.iter().map(KeyWork::position).collect::<Vec<_>>(),
        [
            Some(InputPosition { frame: 1, event: 1 }),
            Some(InputPosition { frame: 1, event: 2 })
        ]
    );
    assert!(work.iter().all(|work| work.available(&routing, ms(10))));
}

#[test]
fn later_activation_cannot_retroactively_authorize_a_press() {
    let mut routing = routing(vec![conditional(true)]);
    assert!(send(&mut routing, packet(1, 1, 10, &[(30, 1), (31, 1)])).is_empty());
    assert!(send(&mut routing, packet(1, 2, 20, &[(30, 0)])).is_empty());
    assert_eq!(
        stages(&send(&mut routing, packet(1, 3, 30, &[(30, 1)]))),
        [Stage::Begin]
    );
}

#[test]
fn activation_loss_permanently_revokes_retained_work_even_after_repress() {
    let mut routing = routing(vec![conditional(true)]);
    let work = send(&mut routing, packet(1, 1, 10, &[(31, 1), (30, 1)]))
        .pop()
        .unwrap();
    assert!(work.available(&routing, ms(10)));
    assert!(send(&mut routing, packet(1, 2, 20, &[(31, 0), (31, 1)])).is_empty());
    assert!(!work.available(&routing, ms(20)));
    assert!(send(&mut routing, packet(1, 3, 30, &[(30, 0)])).is_empty());
    assert!(!work.available(&routing, ms(30)));
}

#[test]
fn initial_only_activation_survives_release_in_the_same_report() {
    let mut routing = routing(vec![conditional(false)]);
    let work = send(
        &mut routing,
        packet(1, 1, 10, &[(31, 1), (30, 1), (31, 0), (30, 0)]),
    );
    assert_eq!(stages(&work), [Stage::Begin, Stage::End]);
    assert!(work.iter().all(|work| work.available(&routing, ms(10))));
}

#[test]
fn own_trigger_release_is_completion_not_activation_cancellation() {
    for same_report in [false, true] {
        let mut rule = conditional(true);
        rule.require_keys = vec![Key(30)];
        let mut routing = routing(vec![rule]);
        let work = if same_report {
            send(&mut routing, packet(1, 1, 10, &[(30, 1), (30, 0)]))
        } else {
            let mut work = send(&mut routing, packet(1, 1, 10, &[(30, 1)]));
            work.extend(send(&mut routing, packet(1, 2, 20, &[(30, 0)])));
            work
        };
        assert_eq!(stages(&work), [Stage::Begin, Stage::End]);
        assert!(work.iter().all(|work| work.available(&routing, ms(20))));
    }
}

#[test]
fn activation_loss_on_another_source_revokes_only_throughout_scopes() {
    for throughout in [false, true] {
        let mut routing = routing(vec![conditional(throughout)]);
        assert!(send(&mut routing, packet(2, 1, 10, &[(31, 1)])).is_empty());
        let work = send(&mut routing, packet(1, 1, 20, &[(30, 1)]))
            .pop()
            .unwrap();
        assert_eq!(work.position(), Some(InputPosition { frame: 2, event: 0 }));
        send(&mut routing, packet(2, 2, 30, &[(31, 0), (31, 1)]));
        assert_eq!(work.available(&routing, ms(30)), !throughout);
    }
}

#[test]
fn duplicates_and_repeats_do_not_move_or_invent_key_edges() {
    let mut routing = routing(vec![rule("shift", Action::Remap { key: Key(42) })]);
    let work = send(
        &mut routing,
        packet(
            1,
            1,
            10,
            &[
                (30, 1),
                (30, 1),
                (30, 2),
                (30, 0),
                (30, 0),
                (30, 1),
                (30, 0),
            ],
        ),
    );
    assert_eq!(
        stages(&work),
        [Stage::Begin, Stage::End, Stage::Begin, Stage::End]
    );
    assert_eq!(
        work.iter()
            .map(|work| work.position().unwrap().event)
            .collect::<Vec<_>>(),
        [0, 3, 5, 6]
    );
}

#[test]
fn interleaved_rules_dispatch_in_raw_edge_order_not_gesture_id_order() {
    let a = rule("a", Action::Remap { key: Key(42) });
    let mut b = rule("b", Action::Remap { key: Key(29) });
    b.trigger = Trigger::Key { key: Key(31) };
    let mut routing = routing(vec![a, b]);
    let work = send(
        &mut routing,
        packet(1, 1, 10, &[(31, 1), (30, 1), (31, 0), (30, 0)]),
    );
    assert_eq!(
        work.iter()
            .map(|work| match work.event() {
                Event::Matched { rule, .. } => rule.as_str(),
                _ => panic!("not a match"),
            })
            .collect::<Vec<_>>(),
        ["b", "a", "b", "a"]
    );
    assert_eq!(
        work.iter()
            .map(|work| work.position().unwrap().event)
            .collect::<Vec<_>>(),
        [0, 1, 2, 3]
    );
}

#[test]
fn same_report_cancellation_prevents_a_begin_from_escaping_later() {
    let mut routing = routing(vec![conditional(true)]);
    assert!(
        send(
            &mut routing,
            packet(1, 1, 10, &[(31, 1), (30, 1), (31, 0), (31, 1), (30, 0)])
        )
        .is_empty()
    );
}

#[test]
fn a_timer_before_the_report_stays_before_that_reports_key_edge() {
    let mut held = rule(
        "held",
        Action::Keys {
            keys: vec![Key(29)],
        },
    );
    held.trigger = Trigger::ButtonHold {
        key: Key(31),
        after: crate::config::Millis(10),
        repeat: None,
    };
    let mut routing = routing(vec![held, rule("shift", Action::Remap { key: Key(42) })]);
    assert!(send(&mut routing, packet(1, 1, 10, &[(31, 1)])).is_empty());
    let work = send(&mut routing, packet(1, 2, 20, &[(30, 1)]));
    assert_eq!(
        work.iter()
            .map(|work| match work.event() {
                Event::Matched { rule, .. } => rule.as_str(),
                _ => panic!("not a match"),
            })
            .collect::<Vec<_>>(),
        ["held", "shift"]
    );
    assert_eq!(
        work[0].position(),
        Some(InputPosition {
            frame: 1,
            event: usize::MAX
        })
    );
}

#[test]
fn raw_positions_include_nonkey_events_and_reject_a_reordered_subsequence() {
    let mut raw = packet(1, 1, 10, &[(30, 1), (30, 0)]);
    raw.events.insert(
        0,
        RawEvent {
            kind: 4,
            code: 4,
            value: 123,
        },
    );
    let mut frame = NormalizedFrame {
        raw,
        changes: vec![
            Change::Key {
                code: 30,
                state: KeyState::Press,
            },
            Change::Key {
                code: 30,
                state: KeyState::Release,
            },
        ],
        contacts: vec![],
        contacts_started: vec![],
        contacts_ended: vec![],
        ended_contacts: vec![],
    };
    assert_eq!(
        positioned_keys(&frame).unwrap(),
        [(1, 30, KeyState::Press), (2, 30, KeyState::Release)]
    );
    frame.changes.reverse();
    assert!(positioned_keys(&frame).is_err());
}

fn empty_engine() -> Engine {
    let mut engine = Engine::new(Config::preset("minimal").unwrap()).unwrap();
    engine.push(Record::Header { version: 1 }).unwrap();
    engine
        .push(Record::Device {
            device: Device {
                source: source(1),
                identity: "ordered-edge".into(),
                name: "Model keyboard".into(),
                path: String::new(),
                physical: String::new(),
                unique: String::new(),
                vendor: 1,
                product: 1,
                virtual_device: false,
                properties: vec![],
                capabilities: BTreeMap::from([(EV_KEY, vec![30, 31])]),
                axes: BTreeMap::new(),
            },
            keys: BTreeSet::new(),
            contacts: vec![],
        })
        .unwrap();
    engine
}

#[test]
fn replacement_preserves_the_global_input_position_watermark() {
    let mut engine = empty_engine();
    engine
        .push(Record::Frame {
            frame: packet(1, 1, 10, &[(30, 1)]),
        })
        .unwrap();
    assert_eq!(engine.input_watermark(), 1);
    engine.reload(engine.config.clone(), ms(20)).unwrap();
    assert_eq!(engine.input_watermark(), 1);
    engine
        .push(Record::Frame {
            frame: packet(1, 2, 30, &[(30, 0)]),
        })
        .unwrap();
    assert_eq!(engine.input_watermark(), 2);
    engine
        .transact_rules(
            &crate::rule_transactions::Transaction {
                revision: engine.rule_revision(),
                changes: vec![crate::rule_transactions::Change::Mode { id: None }],
            },
            ms(40),
        )
        .unwrap();
    assert_eq!(engine.input_watermark(), 2);
    engine
        .push(Record::Frame {
            frame: packet(1, 3, 50, &[(30, 1)]),
        })
        .unwrap();
    assert_eq!(engine.input_watermark(), 3);
}

fn sequence_rule() -> Rule {
    let mut rule = rule(
        "sequence",
        Action::Keys {
            keys: vec![Key(42)],
        },
    );
    rule.trigger = Trigger::Sequence {
        steps: vec![Trigger::Key { key: Key(30) }, Trigger::Key { key: Key(31) }],
        timeout: crate::config::Millis(1000),
    };
    rule
}

#[test]
fn completed_sequence_uses_the_final_edges_position_and_keeps_its_scope() {
    let mut routing = routing(vec![sequence_rule()]);
    assert!(send(&mut routing, packet(1, 1, 10, &[(30, 1)])).is_empty());
    assert!(send(&mut routing, packet(1, 2, 20, &[(30, 0)])).is_empty());
    let work = send(&mut routing, packet(1, 3, 30, &[(31, 1)]))
        .pop()
        .unwrap();
    assert_eq!(work.position(), Some(InputPosition { frame: 3, event: 0 }));
    assert!(work.available(&routing, ms(30)));
    assert_eq!(stages(&[work]), [Stage::Discrete]);
}

#[test]
fn root_sequence_scope_is_distinct_from_a_normally_completed_child() {
    use crate::context::{Binding, SessionKind, Snapshot, State};
    let mut engine = empty_engine();
    let mut config = engine.config.clone();
    config.devices.insert(
        "keys".into(),
        crate::config::Selector {
            keys: vec![Key(30)],
            ..Default::default()
        },
    );
    config.policy.allowed_devices.push("keys".into());
    config.rules = vec![sequence_rule()];
    engine.reload(config, ms(0)).unwrap();
    engine
        .authority
        .bind(
            Binding {
                seat: "seat0".into(),
                session: "positions".into(),
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
                captured_at: ms(0),
                state: State::Desktop,
                outputs: vec![],
            },
            ms(0),
        )
        .unwrap();
    engine.enable_routing();
    engine
        .push(Record::Frame {
            frame: packet(1, 1, 10, &[(30, 1)]),
        })
        .unwrap();
    let root = engine.route_intents().pop().unwrap();
    let child = engine.sequences[0].pending["seat0"].first.clone();
    assert!(!std::sync::Arc::ptr_eq(
        &root.candidate.cancelled,
        &child.cancelled
    ));
    assert!(engine.validate_route(&root, ms(10)).is_ok());
    engine
        .push(Record::Frame {
            frame: packet(1, 2, 20, &[(30, 0)]),
        })
        .unwrap();
    assert!(engine.validate_route(&root, ms(20)).is_ok());
    engine.cancel_sequence(root.id(), ms(30), "test cancellation");
    assert!(engine.validate_route(&root, ms(30)).is_err());
    assert!(!child.cancelled.load(std::sync::atomic::Ordering::Acquire));
}

#[test]
fn exhausted_positions_reject_before_raw_input_history_changes() {
    let mut engine = empty_engine();
    engine.input_watermark = u64::MAX;
    assert!(
        engine
            .push(Record::Frame {
                frame: packet(1, 1, 10, &[(30, 1)])
            })
            .is_err()
    );
    assert!(engine.devices[&1].keys.is_empty());
    engine.input_watermark = 0;
    engine
        .push(Record::Frame {
            frame: packet(1, 1, 10, &[(30, 1)]),
        })
        .unwrap();
    assert_eq!(engine.input_watermark(), 1);
}
