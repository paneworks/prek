use super::*;
use crate::{
    config::{Delivery, Key},
    keys::worker::tests::{fence, frame, ms, push, routing, rule, source, with_fence},
};

#[test]
fn edited_public_observations_cannot_be_reinterpreted_as_native_work() {
    let mut routing = routing(vec![rule("shift", Action::Remap { key: Key(42) })]);
    routing
        .push(Record::Frame {
            frame: frame(1, 1, 10, 30, 1),
        })
        .unwrap();
    let mut dispatch = routing.take_scoped_dispatches(ms(10)).pop().unwrap();
    if let Event::Matched { action, .. } = &mut dispatch.event {
        *action = Action::Keys {
            keys: vec![Key(30)],
        };
    }
    assert!(dispatch.into_key_work().is_err());
}

#[test]
fn an_observer_without_a_coordinator_cannot_mint_key_work() {
    let mut routing = with_fence(vec![rule("shift", Action::Remap { key: Key(42) })], None);
    routing
        .push(Record::Frame {
            frame: frame(1, 1, 10, 30, 1),
        })
        .unwrap();
    let dispatch = routing.take_scoped_dispatches(ms(10)).pop().unwrap();
    assert!(dispatch.into_key_work().is_err());
}

#[test]
fn retained_work_captures_the_original_raw_sequence_not_dequeue_time() {
    let mut routing = routing(vec![rule("shift", Action::Remap { key: Key(42) })]);
    routing
        .push(Record::Frame {
            frame: frame(1, 1, 10, 30, 1),
        })
        .unwrap();
    routing
        .push(Record::Frame {
            frame: frame(1, 2, 20, 31, 1),
        })
        .unwrap();
    let work = routing
        .take_scoped_dispatches(ms(20))
        .pop()
        .unwrap()
        .into_key_work()
        .ok()
        .unwrap();
    assert_eq!(work.inputs().get(&source(1)), Some(&1));
    assert!(work.available(&routing, ms(20)));
    assert_eq!(work.fence(), fence());
}

#[test]
fn epoch_exhaustion_permanently_seals_only_the_key_lane() {
    let mut routing = routing(vec![rule("shift", Action::Remap { key: Key(42) })]);
    let work = push(&mut routing, 1, 1, 10, 1).pop().unwrap();
    routing.key_epoch = u64::MAX;
    routing.restrict_actions(&BTreeSet::new());
    assert_eq!(routing.key_epoch, 0);
    routing.actions_available(1, BTreeSet::from([Capability::SyntheticInput]), ms(20));
    assert_eq!(routing.key_epoch, 0);
    assert!(!routing.key_lane(fence(), ms(20)));
    assert!(!work.available(&routing, ms(20)));
}

#[test]
fn output_availability_never_renews_the_original_capability_lease() {
    let mut routing = routing(vec![rule("shift", Action::Remap { key: Key(42) })]);
    let work = push(&mut routing, 1, 1, 10, 1).pop().unwrap();
    let expires = routing.capability_expires;
    routing.key_output_available(false);
    routing.key_output_available(true);
    assert_eq!(routing.capability_expires, expires);
    assert!(!work.available(&routing, ms(20)));
    assert!(!routing.key_lane(fence(), expires));
}

#[test]
fn expiry_followed_by_regrant_does_not_resurrect_old_work() {
    let mut routing = routing(vec![rule("shift", Action::Remap { key: Key(42) })]);
    let work = push(&mut routing, 1, 1, 10, 1).pop().unwrap();
    assert!(work.available(&routing, ms(20)));
    routing.actions_available(1, BTreeSet::from([Capability::SyntheticInput]), ms(1000));
    assert!(!work.available(&routing, ms(1000)));
    assert_ne!(work.dispatch.key_epoch, routing.key_epoch);
}

#[test]
fn removing_an_unrelated_capability_does_not_revoke_synthetic_owners() {
    let mut routing = routing(vec![rule("shift", Action::Remap { key: Key(42) })]);
    routing.actions_available(
        1,
        BTreeSet::from([Capability::SyntheticInput, Capability::Observations]),
        ms(5),
    );
    let work = push(&mut routing, 1, 1, 10, 1).pop().unwrap();
    routing.restrict_actions(&BTreeSet::from([Capability::SyntheticInput]));
    assert!(work.available(&routing, ms(11)));
}

#[test]
fn source_removal_and_coordinator_replacement_make_work_unavailable() {
    let mut routing = routing(vec![rule("shift", Action::Remap { key: Key(42) })]);
    let work = push(&mut routing, 1, 1, 10, 1).pop().unwrap();
    let mut other = fence();
    other.session += 1;
    let replacement = with_fence(
        vec![rule("shift", Action::Remap { key: Key(42) })],
        Some(other),
    );
    assert!(!work.available(&replacement, ms(10)));
    routing
        .push(Record::Remove {
            source: source(1),
            at: ms(20),
        })
        .unwrap();
    assert!(!work.available(&routing, ms(20)));
}

#[test]
fn restoring_the_same_rule_after_runtime_edits_does_not_restore_old_work() {
    use crate::rule_transactions::{Change, Transaction};
    let mut routing = routing(vec![rule("shift", Action::Remap { key: Key(42) })]);
    let work = push(&mut routing, 1, 1, 10, 1).pop().unwrap();
    assert!(work.available(&routing, ms(10)));
    for enabled in [false, true] {
        routing
            .transact_rules(
                &Transaction {
                    revision: routing.rule_revision(),
                    changes: vec![Change::Enabled {
                        id: "shift".into(),
                        enabled,
                    }],
                },
                ms(11),
            )
            .unwrap();
    }
    assert!(!work.available(&routing, ms(11)));
}

#[test]
fn consumed_work_waits_for_prior_raw_ack_and_blocks_its_own_cutpoint() {
    let mut remap = rule("shift", Action::Remap { key: Key(42) });
    remap.delivery = Delivery::Reserve;
    let mut routing = routing(vec![remap]);
    let mut prior = frame(1, 1, 10, 31, 1);
    prior.events.insert(
        1,
        crate::input::RawEvent {
            kind: crate::input::EV_KEY,
            code: 31,
            value: 0,
        },
    );
    routing.push(Record::Frame { frame: prior }).unwrap();
    let work = push(&mut routing, 1, 2, 20, 1).pop().unwrap();
    assert!(!work.input_prefix_ready(&routing));
    assert!(!work.blocks_raw(&source(1), 1));
    assert!(work.blocks_raw(&source(1), 2));
    let ticket = routing.due(&source(1), ms(20)).unwrap().unwrap().ticket;
    assert!(!work.input_prefix_ready(&routing));
    routing.delivered(&source(1), ticket, ms(20)).unwrap();
    assert!(work.input_prefix_ready(&routing));
}

#[test]
fn retiring_a_keyboard_releases_only_the_keys_its_consumer_saw() {
    use crate::keys::worker::tests::{projected, scoped};
    for scoped_mode in [false, true] {
        for trigger in ["reload", "remove", "lock"] {
            let label = format!("scoped={scoped_mode}, {trigger}");
            let mut consumed = rule(
                "consumed-s",
                Action::Keys {
                    keys: vec![Key(48)],
                },
            );
            consumed.delivery = Delivery::Buffer;
            consumed.trigger = Trigger::Key { key: Key(31) };
            let rules = vec![consumed];
            let mut routing = if scoped_mode {
                scoped(rules.clone())
            } else {
                projected(rules.clone())
            };
            let mut seen = Vec::new();
            for (sequence, at, code) in [(1, 10, 30), (2, 20, 31)] {
                routing
                    .push(Record::Frame {
                        frame: frame(1, sequence, at, code, 1),
                    })
                    .unwrap();
                routing.take_scoped_dispatches(ms(at));
                while let Some(scheduled) = routing.due(&source(1), ms(at)).unwrap().cloned() {
                    routing
                        .delivered(&source(1), scheduled.ticket, ms(at))
                        .unwrap();
                    seen.extend(scheduled.frame.events.iter().map(|event| event.code));
                }
            }
            assert!(
                seen.contains(&30) && !seen.contains(&31),
                "{label}: {seen:?}"
            );
            match trigger {
                "reload" => {
                    let config = routing.engine.config().clone();
                    routing.reload(config, ms(30)).unwrap();
                }
                "remove" => routing
                    .push(Record::Remove {
                        source: source(1),
                        at: ms(30),
                    })
                    .unwrap(),
                _ => {
                    routing.authority_mut().lock_requested();
                    routing.push(Record::Tick { at: ms(30) }).unwrap();
                }
            }
            let recovery = routing
                .take_recoveries()
                .into_iter()
                .find(|recovery| recovery.source == source(1))
                .unwrap_or_else(|| panic!("{label}: no recovery"));
            let released: Vec<_> = recovery
                .neutral_events
                .iter()
                .filter(|event| event.kind == crate::input::EV_KEY && event.value == 0)
                .map(|event| event.code)
                .collect();
            assert_eq!(released, [30], "{label}");
            assert!(routing.due(&source(1), ms(30)).is_err(), "{label}");
        }
    }
}
