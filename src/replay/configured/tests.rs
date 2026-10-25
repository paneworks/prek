use super::*;
pub(crate) mod composition;
use crate::{
    config::{Delivery, Millis, Trigger},
    context::SessionKind,
    input::{Device, EV_KEY, EV_SYN, Frame, RawEvent, SYN_REPORT},
    rules::{Evidence, Stage},
};

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}
fn definition(body: &str, continuous: bool, wake: bool) -> Definition {
    Definition::compile(&format!(r#"
local p=require('prek');local calls=0
p.events.on('input',function()error('ordinary input callback ran')end)
p.scheduler.after('1ms',function()error('ordinary scheduler ran')end)
local trigger=p.recognizers.define('offline',{{selectors={{'keys'}},inputs={{'key'}},
 reply_within='100ms',continuous={continuous},{wake}}},function(ticket)
 {body}
end)
return p.config.new{{policy={{allow_input_events=true,allowed_devices={{'keys'}}}},
 devices={{keys={{keys={{'KEY_A'}}}}}},rules={{{{id='offline',devices={{'keys'}},contexts={{'desktop'}},
 scope='everywhere',delivery='observe',trigger=trigger,action=p.actions.event{{name='offline'}}}}}}}}
"#,wake=if wake {"wake='10ms',"} else {""}), "offline.lua").unwrap()
}
pub(crate) fn context(seat: &str, generation: u64, revision: u64, at: u64, state: State) -> Record {
    Record::Context {
        at: ms(at),
        binding: Binding {
            seat: seat.into(),
            session: "recorded".into(),
            uid: 1000,
            compositor_pid: 42,
            compositor_start: 1,
            kind: SessionKind::User,
        },
        snapshot: Box::new(Snapshot {
            generation,
            revision,
            captured_at: ms(at),
            state,
            outputs: vec![],
        }),
    }
}
pub(crate) fn device(seat: &str, id: u64) -> Record {
    Record::Device {
        device: Device {
            source: Source {
                device: id,
                generation: u64::MAX,
                seat: seat.into(),
            },
            identity: format!("fixture-{id}"),
            name: "Offline fixture".into(),
            path: String::new(),
            physical: String::new(),
            unique: String::new(),
            vendor: 1,
            product: 1,
            properties: vec![],
            capabilities: BTreeMap::from([(EV_KEY, vec![30])]),
            axes: BTreeMap::new(),
            virtual_device: false,
        },
        keys: BTreeSet::new(),
        contacts: vec![],
    }
}
pub(crate) fn frame(seat: &str, id: u64, sequence: u64, at: u64, value: i32) -> Record {
    Record::Frame {
        frame: Frame {
            source: Source {
                device: id,
                generation: u64::MAX,
                seat: seat.into(),
            },
            sequence,
            at: ms(at),
            events: vec![
                RawEvent {
                    kind: EV_KEY,
                    code: 30,
                    value,
                },
                RawEvent {
                    kind: EV_SYN,
                    code: SYN_REPORT,
                    value: 0,
                },
            ],
        },
    }
}
fn start(replay: &mut Configured, id: u64) {
    for record in [
        Record::Header { version: 1 },
        context("seat0", 1, 1, 0, State::Desktop),
        device("seat0", id),
    ] {
        replay.push(record).unwrap();
    }
}
fn matched(replay: &Configured) -> Vec<&Event> {
    replay
        .events()
        .iter()
        .filter(|e| matches!(e, Event::Matched { .. }))
        .collect()
}

#[test]
fn actual_callbacks_and_stationary_wakes_are_identical_after_wall_clock_delay() {
    let definition = definition(
        r#"
if ticket.cause=='cancel' then assert(ticket.reason=='completed' and calls==2);return nil end
calls=calls+1
assert(ticket.input.source.device=='18446744073709551615')
assert(ticket.input.source.generation=='18446744073709551615')
assert(p.devices.get(ticket.input.source).device.name=='Offline fixture')
assert(not p.capabilities.ready('commands') and p.status.current()==nil)
if calls==1 then assert(ticket.cause=='frame');return nil end
assert(ticket.cause=='timer' and ticket.input.sequence=='1' and #ticket.input.changes==0)
return {phase='discrete',data={calls=calls,at=ticket.requested_at}}
"#,
        false,
        true,
    );
    let config = definition.validate().unwrap();
    let mut reference = None;
    for delayed in [false, true, false] {
        let mut replay = Configured::new(config.clone(), Some(definition.clone())).unwrap();
        start(&mut replay, u64::MAX);
        replay.push(frame("seat0", u64::MAX, 1, 10, 1)).unwrap();
        assert!(matched(&replay).is_empty());
        if delayed {
            std::thread::sleep(ms(280));
        }
        replay.push(Record::Tick { at: ms(20) }).unwrap();
        assert!(
            matches!(&matched(&replay)[..],[Event::Matched {at,evidence:Evidence::Custom {data,..},..}] if *at==ms(20)&&data["calls"]==2)
        );
        let bytes = serde_json::to_vec(replay.events()).unwrap();
        if let Some(expected) = &reference {
            assert_eq!(&bytes, expected);
        } else {
            reference = Some(bytes);
        }
        replay
            .push(Record::ExpectedRules {
                events: replay.events().to_vec(),
            })
            .unwrap();
        replay.finish(true).unwrap();
        assert_eq!(replay.budget.0.get(), 0);
        assert!(replay.push(Record::Tick { at: ms(21) }).is_err());
    }
}

#[test]
fn the_last_recorded_fresh_nanosecond_is_not_spent_on_host_vm_work() {
    let definition = definition(
        "if ticket.cause=='cancel' then return nil end;return {phase='discrete'}",
        false,
        false,
    );
    let mut replay = Configured::new(definition.validate().unwrap(), Some(definition)).unwrap();
    start(&mut replay, 1);
    let mut record = frame("seat0", 1, 1, 249, 1);
    let Record::Frame { frame } = &mut record else {
        unreachable!()
    };
    frame.at = ms(250) - Duration::from_nanos(1);
    replay.push(record).unwrap();
    assert_eq!(matched(&replay).len(), 1);
    replay.finish(false).unwrap();
}

#[test]
fn continuous_phases_keep_one_candidate_and_cleanup_at_end() {
    let definition = definition(
        r#"
if ticket.cause=='cancel' then assert(ticket.reason=='completed' and calls==3);return nil end
calls=calls+1
return {phase=({'begin','update','end'})[calls],data={calls=calls}}
"#,
        true,
        false,
    );
    let mut replay = Configured::new(definition.validate().unwrap(), Some(definition)).unwrap();
    start(&mut replay, 1);
    for (sequence, value) in [(1, 1), (2, 2), (3, 0)] {
        replay
            .push(frame("seat0", 1, sequence, sequence * 10, value))
            .unwrap();
    }
    let events = matched(&replay);
    assert_eq!(events.len(), 3);
    let mut ids = BTreeSet::new();
    for (event, stage) in events
        .into_iter()
        .zip([Stage::Begin, Stage::Update, Stage::End])
    {
        let Event::Matched {
            id, stage: actual, ..
        } = event
        else {
            unreachable!()
        };
        ids.insert(*id);
        assert_eq!(*actual, stage);
    }
    assert_eq!(ids.len(), 1);
    replay.finish(false).unwrap();
}

#[test]
fn context_expiry_and_source_loss_cleanup_without_fabricating_a_new_match() {
    for lost in [false, true] {
        let definition = definition(
            r#"
if ticket.cause=='cancel' then assert(calls==1);return nil end
calls=calls+1;assert(calls==1);return nil
"#,
            false,
            false,
        );
        let mut replay = Configured::new(definition.validate().unwrap(), Some(definition)).unwrap();
        start(&mut replay, 1);
        replay.push(frame("seat0", 1, 1, 10, 1)).unwrap();
        if lost {
            replay
                .push(Record::Remove {
                    source: Source {
                        device: 1,
                        generation: u64::MAX,
                        seat: "seat0".into(),
                    },
                    at: ms(20),
                })
                .unwrap();
        } else {
            replay.push(Record::Tick { at: ms(300) }).unwrap();
        }
        assert!(matched(&replay).is_empty());
        assert!(
            replay
                .events()
                .iter()
                .any(|e| matches!(e, Event::Cancelled { .. }))
        );
        replay.finish(false).unwrap();
    }
}

#[test]
fn lock_and_session_replacement_do_not_turn_held_input_into_new_custom_births() {
    let definition = definition(
        r#"
if ticket.cause=='cancel' then return nil end
calls=calls+1;return {phase='discrete',data={calls=calls}}
"#,
        false,
        false,
    );
    let mut replay = Configured::new(definition.validate().unwrap(), Some(definition)).unwrap();
    start(&mut replay, 1);
    replay.push(frame("seat0", 1, 1, 10, 1)).unwrap();
    assert_eq!(matched(&replay).len(), 1);
    replay
        .push(context("seat0", 2, 1, 20, State::Locked))
        .unwrap();
    replay.push(frame("seat0", 1, 2, 21, 2)).unwrap();
    assert_eq!(matched(&replay).len(), 1);
    replay
        .push(context("seat0", 3, 1, 30, State::Desktop))
        .unwrap();
    replay.push(frame("seat0", 1, 3, 31, 2)).unwrap();
    assert_eq!(matched(&replay).len(), 1);
    replay.push(frame("seat0", 1, 4, 32, 0)).unwrap();
    replay.push(frame("seat0", 1, 5, 33, 1)).unwrap();
    assert!(
        matches!(matched(&replay).last(),Some(Event::Matched {session_generation:3,evidence:Evidence::Custom {data,..},..})if data["calls"]==1)
    );
    replay.finish(false).unwrap();
}

#[test]
fn per_seat_callbacks_and_generations_are_isolated() {
    let definition = definition(
        r#"
if ticket.cause=='cancel' then return nil end
calls=calls+1;return {phase='discrete',data={calls=calls,seat=p.seats.current().binding.seat}}
"#,
        false,
        false,
    );
    let mut config = definition.validate().unwrap();
    config.policy.allowed_seats.push("seat1".into());
    let mut replay = Configured::new(config, Some(definition)).unwrap();
    start(&mut replay, 1);
    replay
        .push(context("seat1", 7, 1, 0, State::Desktop))
        .unwrap();
    replay.push(device("seat1", 2)).unwrap();
    for (seat, id, generation) in [("seat0", 1, 1), ("seat1", 2, 7)] {
        replay.push(frame(seat, id, 1, 10, 1)).unwrap();
        assert!(
            matches!(matched(&replay).last(),Some(Event::Matched {session_generation,evidence:Evidence::Custom {data,..},..})if *session_generation==generation&&data["calls"]==1&&data["seat"]==seat)
        );
    }
    replay.finish(false).unwrap();
}

#[test]
fn invalid_record_or_callback_failure_retires_the_session_and_releases_its_vm() {
    for body in [
        "return {phase='update'}",
        "error('offline failure')",
        "while true do end",
    ] {
        let definition = definition(body, false, false);
        let mut replay = Configured::new(definition.validate().unwrap(), Some(definition)).unwrap();
        start(&mut replay, 1);
        assert!(replay.push(frame("seat0", 1, 1, 10, 1)).is_err());
        assert!(replay.baseline.is_none());
        assert_eq!(replay.budget.0.get(), 0);
        assert!(replay.push(frame("seat0", 1, 2, 11, 1)).is_err());
    }
}

#[test]
fn eof_cleanup_and_shared_vm_quotas_are_real_and_reusable_after_drop() {
    let definition = definition(
        r#"
if ticket.cause=='cancel' then assert(calls==1 and ticket.reason=='cancelled');return nil end
calls=calls+1;return nil
"#,
        false,
        false,
    );
    let config = definition.validate().unwrap();
    let budget = Budget::default();
    let mut sessions = vec![];
    for _ in 0..4 {
        let mut replay = Configured::with_budget(
            config.clone(),
            Some(Arc::new(definition.clone())),
            budget.clone(),
        )
        .unwrap();
        start(&mut replay, 1);
        replay.push(frame("seat0", 1, 1, 10, 1)).unwrap();
        sessions.push(replay);
    }
    assert_eq!(budget.0.get(), 4);
    let mut denied = Configured::with_budget(
        config.clone(),
        Some(Arc::new(definition.clone())),
        budget.clone(),
    )
    .unwrap();
    start(&mut denied, 1);
    assert!(
        denied
            .push(frame("seat0", 1, 1, 10, 1))
            .unwrap_err()
            .to_string()
            .contains("VM limit")
    );
    assert_eq!(budget.0.get(), 4);
    sessions[0].finish(false).unwrap();
    assert_eq!(budget.0.get(), 3);
    drop(sessions);
    assert_eq!(budget.0.get(), 0);
}

#[test]
fn recorded_authority_never_enters_generic_or_live_engines_and_context_is_strict() {
    let record = context("seat0", 1, 1, 0, State::Desktop);
    let mut generic = crate::replay::Replay::default();
    generic.push(Record::Header { version: 1 }).unwrap();
    assert!(generic.push(record.clone()).is_err());
    let mut native = Engine::new(Config::preset("minimal").unwrap()).unwrap();
    native.push(Record::Header { version: 1 }).unwrap();
    assert!(native.push(record).is_err());
    for invalid in [
        context("seat0", 1, 1, 10, State::Desktop),
        context("seat0", 0, 2, 10, State::Desktop),
    ] {
        let mut replay = Configured::new(Config::preset("minimal").unwrap(), None).unwrap();
        start(&mut replay, 1);
        assert!(replay.push(invalid).is_err());
        assert!(replay.baseline.is_none());
    }
}

#[test]
fn hostile_session_and_timestamp_counters_fail_without_overflowing_native_clocks() {
    for record in [
        context("seat0", u64::MAX, 1, 0, State::Desktop),
        Record::Tick { at: Duration::MAX },
    ] {
        let mut replay = Configured::new(Config::preset("minimal").unwrap(), None).unwrap();
        replay.push(Record::Header { version: 1 }).unwrap();
        assert!(replay.push(record).is_err());
        assert!(replay.baseline.is_none());
    }
}

#[test]
fn custom_replay_requires_exact_source_declarations_and_nonempty_checked_input() {
    let definition = definition("return nil", false, false);
    let config = definition.validate().unwrap();
    assert!(Configured::new(config.clone(), None).is_err());
    let mut bad = config.clone();
    bad.recognizers.clear();
    let Trigger::Custom { recognizer, .. } = &mut bad.rules[0].trigger else {
        unreachable!()
    };
    recognizer.reply_within = Millis(99);
    assert!(
        Configured::new(bad, Some(definition))
            .err()
            .unwrap()
            .to_string()
            .contains("approved source declaration")
    );
    let mut empty = Configured::new(Config::preset("minimal").unwrap(), None).unwrap();
    empty.push(Record::Header { version: 1 }).unwrap();
    assert!(
        empty
            .push(Record::ExpectedRules { events: vec![] })
            .is_err()
    );
    let mut checked = Configured::new(
        config.clone(),
        Some(self::definition("return nil", false, false)),
    )
    .unwrap();
    start(&mut checked, 1);
    assert!(checked.finish(true).is_err());
    assert!(checked.finish(false).is_err());
}

#[test]
fn composed_custom_replay_uses_actual_bound_closures_with_a_fenced_coordinator() {
    let definition = definition(
        r#"
if ticket.cause=='cancel' then return nil end
calls=calls+1;return {phase='discrete',data={calls=calls}}
"#,
        false,
        false,
    );
    let mut config = definition.validate().unwrap();
    let trigger = config.rules[0].trigger.clone();
    config.rules[0].trigger = Trigger::Sequence {
        steps: vec![trigger.clone(), trigger],
        timeout: Millis(1000),
    };
    for delivery in [Delivery::Observe, Delivery::Buffer, Delivery::Reserve] {
        config.rules[0].delivery = delivery;
        config.validate().unwrap();
        let mut replay = Configured::new(config.clone(), Some(definition.clone())).unwrap();
        start(&mut replay, 1);
        for (sequence, value) in [(1, 1), (2, 0), (3, 1)] {
            replay
                .push(frame("seat0", 1, sequence, sequence * 10, value))
                .unwrap();
        }
        assert!(
            matches!(&matched(&replay)[..],[Event::Matched {evidence:Evidence::Sequence {steps},..}]if steps.len()==2)
        );
        replay.finish(false).unwrap();
    }
}
