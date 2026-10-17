use super::*;
use crate::{
    input::{Device, EV_KEY, EV_SYN, Frame, RawEvent, Source},
    lua::{Definition, EventKind},
};
use std::collections::BTreeMap;

#[cfg(target_os = "linux")]
#[test]
fn routed_replay_checks_exact_downstream_frames_and_keeps_handles_isolated() {
    let (config, records) = crate::replay::routed::tests::checked_trace();
    let mut source = "local p=require('prek');assert(not pcall(function()p.replay.routing()end));local c=p.config.preset('minimal');c.mode='route';c.policy.allowed_devices={'keys'};c.devices={keys={keys={'KEY_A'}}};local r=p.replay.routing(c);assert(r:status().mode=='routing')\n".to_string();
    for record in &records {
        source.push_str(&format!(
            "r:push([==[{}]==])\n",
            serde_json::to_string(record).unwrap()
        ));
    }
    let crate::replay::routed::Record::Expected { outcomes } = records.last().unwrap() else {
        unreachable!()
    };
    source.push_str(&format!("local result=r:finish(true);assert(result.mode=='routing' and result.checked);assert(result.json==[==[{}]==]);assert(#result.outcomes==5);assert(result.outcomes[1].scheduled.ticket=='1');assert(result.outcomes[1].scheduled.frame.source.generation=='18446744073709551615');assert(r:status().state=='finished');r:close();return {{version=1}}", serde_json::to_string(outcomes).unwrap()));
    Definition::compile(&source, "routed-replay.lua").unwrap();
    let mut native = crate::replay::routed::Routed::new(config, None).unwrap();
    for record in records {
        native.push(record).unwrap();
    }
    native.finish(true).unwrap();
}

fn records() -> Vec<Record> {
    let source = Source {
        device: u64::MAX,
        generation: u64::MAX,
        seat: "seat0".into(),
    };
    let device = Device {
        source: source.clone(),
        identity: "fixture".into(),
        name: "fixture".into(),
        path: "fixture".into(),
        physical: "fixture".into(),
        unique: String::new(),
        vendor: 1,
        product: 1,
        properties: vec![],
        capabilities: BTreeMap::from([(EV_KEY, vec![30])]),
        axes: BTreeMap::new(),
        virtual_device: false,
    };
    let mut records = vec![
        Record::Header { version: 1 },
        Record::Device {
            device,
            keys: Default::default(),
            contacts: vec![],
        },
    ];
    for (sequence, millis, value) in [(1, 0, 1), (2, 50, 0)] {
        records.push(Record::Frame {
            frame: Frame {
                source: source.clone(),
                at: Duration::from_millis(millis),
                sequence,
                events: vec![
                    RawEvent {
                        kind: EV_KEY,
                        code: 30,
                        value,
                    },
                    RawEvent {
                        kind: EV_SYN,
                        code: 0,
                        value: 0,
                    },
                ],
            },
        });
    }
    records.push(Record::Tick {
        at: Duration::from_millis(350),
    });
    records
}

fn script(records: &[Record], configured: bool, check: bool) -> String {
    let mut script = String::from("local p=require('prek')\nlocal function run()\n");
    if configured {
        script.push_str("local c=p.config.preset('desktop');c.shapes={};c.rules[1].enabled=true;c.rules[1].trigger=p.triggers.button_taps{key='KEY_A',count=1};local r=p.replay.new(c)\n");
    } else {
        script.push_str("local r=p.replay.new()\n");
    }
    for record in records {
        script.push_str(&format!(
            "r:push([==[{}]==])\n",
            serde_json::to_string(record).unwrap()
        ));
    }
    script.push_str(&format!("local result=r:finish({check})\n"));
    script.push_str("assert(r:status().state=='finished');assert(#result.outcomes>0);assert(type(result.json)=='string');assert(not pcall(function()r:push('{}')end));r:close();r:close();return result\nend\n");
    script
}

#[test]
fn lua_generic_replay_matches_cli_engine_and_keeps_unsigned_identities() {
    let records = records();
    let mut engine = crate::replay::Replay::default();
    for record in &records {
        engine.push(record.clone()).unwrap();
    }
    let expected = serde_json::to_string(engine.observations()).unwrap();
    let mut script = script(&records, false, false);
    script.push_str(&format!("local result=run();assert(result.mode=='observations');assert(result.json==[==[{expected}]==]);assert(result.outcomes[1].frame.raw.source.device=='18446744073709551615');assert(result.outcomes[1].frame.raw.source.generation=='18446744073709551615');return {{version=1}}"));
    Definition::compile(&script, "replay.lua").unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn custom_replay_uses_fresh_approved_closures_instead_of_the_live_callback_state() {
    use crate::{
        context::State,
        replay::configured::tests::{context, device, frame},
    };
    let records = [
        Record::Header { version: 1 },
        context("seat0", 1, 1, 0, State::Desktop),
        device("seat0", u64::MAX),
        frame("seat0", u64::MAX, 1, 10, 1),
    ];
    let mut pushes = String::new();
    for record in records {
        pushes.push_str(&format!(
            "[==[{}]==],\n",
            serde_json::to_string(&record).unwrap()
        ));
    }
    let source = format!(
        r#"
local p=require('prek');local calls=0;local config
local trigger=p.recognizers.define('offline',{{selectors={{'keys'}},inputs={{'key'}},reply_within='100ms'}},function(ticket)
 if ticket.cause=='cancel' then assert(ticket.reason=='completed');return nil end
 calls=calls+1
 assert(not pcall(p.replay.new,config))
 return {{phase='discrete',data={{calls=calls}}}}
end)
config=p.config.new{{policy={{allow_input_events=true,allowed_devices={{'keys'}}}},devices={{keys={{keys={{'KEY_A'}}}}}},
 rules={{{{id='offline',devices={{'keys'}},contexts={{'desktop'}},scope='everywhere',delivery='observe',trigger=trigger,action=p.actions.event{{name='offline'}}}}}}}}
assert(not pcall(p.replay.new,config))
local records={{{pushes}}};local r;local index
p.events.on('ready',function()
 if not r then r=p.replay.new(config);index=1;return end
 if records[index] then r:push(records[index]);index=index+1;return end
 local result=r:finish();r=nil
 assert(result.mode=='rules' and #result.outcomes==1)
 assert(result.outcomes[1].evidence.data.calls==1)
 assert(result.outcomes[1].source.device=='18446744073709551615')
 assert(calls==0)
 p.log.info('isolated-replay-ok')
end)
return config
"#
    );
    let definition = Definition::compile(&source, "isolated-replay.lua").unwrap();
    for loaded in [false, true] {
        let mut runtime = if loaded {
            Runtime::load(&source, "isolated-replay.lua").unwrap()
        } else {
            definition.instantiate().unwrap()
        };
        for _ in 0..2 {
            for wake in 0..6 {
                let logs = runtime
                    .dispatch(EventKind::Ready, &serde_json::json!({}))
                    .unwrap();
                if wake == 5 {
                    assert!(matches!(&logs[..],[log]if log.message=="isolated-replay-ok"));
                } else {
                    assert!(logs.is_empty());
                }
            }
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn shipped_custom_replay_example_executes_its_checked_fixture() {
    let definition = Definition::compile(
        include_str!("../../../examples/lua/custom_replay.lua"),
        "custom_replay.lua",
    )
    .unwrap();
    let mut runtime = definition.instantiate().unwrap();
    runtime.set_clock(Duration::ZERO).unwrap();
    assert!(
        runtime
            .dispatch(EventKind::Ready, &serde_json::json!({}))
            .unwrap()
            .is_empty()
    );
    let mut logs = vec![];
    for wake in 1..=6 {
        logs.extend(
            runtime
                .tick(Duration::from_millis(wake), &serde_json::json!({}))
                .unwrap(),
        );
    }
    assert!(runtime.timer_deadline().unwrap().is_none());
    assert!(
        matches!(&logs[..],[log]if log.message=="custom-replay-checked"&&log.fields["outcomes"]==1)
    );
}

#[test]
fn generic_expectations_are_checked_and_mismatches_cannot_be_reused() {
    let records = records();
    let mut reference = crate::replay::Replay::default();
    for record in &records {
        reference.push(record.clone()).unwrap();
    }
    for matches in [true, false] {
        let mut session = Session::new(None).unwrap();
        for record in &records {
            session
                .push(&serde_json::to_string(record).unwrap())
                .unwrap();
        }
        let expected = Record::Expected {
            observations: if matches {
                reference.observations().to_vec()
            } else {
                vec![]
            },
        };
        session
            .push(&serde_json::to_string(&expected).unwrap())
            .unwrap();
        assert_eq!(session.finish(true).is_ok(), matches);
        assert_eq!(session.state, if matches { "finished" } else { "failed" });
        assert!(session.engine.is_none());
        assert!(session.finish(false).is_err());
    }
}

#[test]
fn replay_bounds_total_recording_and_source_history_and_rejects_stale_frames() {
    let mut session = Session::new(None).unwrap();
    session
        .push("{\"record\":\"header\",\"version\":1}")
        .unwrap();
    let tick = "{\"record\":\"tick\",\"at\":{\"secs\":0,\"nanos\":0}}";
    let padded = format!("{tick}{}", " ".repeat(MAX_RECORD_BYTES - tick.len()));
    while session.push(&padded).is_ok() {}
    assert_eq!(session.state, "failed");
    assert!(session.bytes <= MAX_BYTES);

    let mut session = Session::new(None).unwrap();
    session
        .push("{\"record\":\"header\",\"version\":1}")
        .unwrap();
    let Record::Device { device, .. } = &records()[1] else {
        unreachable!()
    };
    for id in 1..=MAX_SOURCES as u64 {
        let mut device = device.clone();
        device.identity = format!("source-{id}");
        device.source.device = id;
        session
            .push(
                &serde_json::to_string(&Record::Device {
                    device,
                    keys: Default::default(),
                    contacts: vec![],
                })
                .unwrap(),
            )
            .unwrap();
    }
    let mut extra = device.clone();
    extra.source.device = MAX_SOURCES as u64 + 1;
    assert!(
        session
            .push(
                &serde_json::to_string(&Record::Device {
                    device: extra,
                    keys: Default::default(),
                    contacts: vec![]
                })
                .unwrap()
            )
            .is_err()
    );
    assert!(session.engine.is_none());

    let mut session = Session::new(None).unwrap();
    let records = records();
    for record in &records[..3] {
        session
            .push(&serde_json::to_string(record).unwrap())
            .unwrap();
    }
    let mut stale = records[2].clone();
    let Record::Frame { frame } = &mut stale else {
        unreachable!()
    };
    frame.source.generation -= 1;
    assert!(
        session
            .push(&serde_json::to_string(&stale).unwrap())
            .is_err()
    );
    assert!(session.engine.is_none());
}

#[test]
fn aliased_and_cyclic_configuration_cannot_expand_unbounded_native_state() {
    Definition::compile(
        r#"
local p=require('prek')
local c=p.config.preset('minimal')
local aliases={}
for i=1,17 do aliases={aliases,aliases}end
c.rules=aliases
local ok,err=pcall(p.replay.new,c)
assert(not ok and tostring(err):find('value limit'))
c.rules=c
assert(not pcall(p.replay.new,c))
for i=1,4 do local replay=p.replay.new();replay:close()end
return {version=1}
"#,
        "bounded-replay-config.lua",
    )
    .unwrap();
}

#[test]
fn lua_configured_replay_only_returns_outcomes_and_uses_exact_native_expectations() {
    let records = records();
    let mut config = Config::preset("desktop").unwrap();
    config.shapes.clear();
    config.rules[0].enabled = true;
    config.rules[0].trigger = crate::config::Trigger::ButtonTaps {
        key: crate::config::Key(30),
        count: 1,
    };
    let mut engine = crate::rules::Engine::new(config).unwrap();
    for record in &records {
        engine.push(record.clone()).unwrap();
    }
    let events = engine.take_events();
    assert_eq!(events.len(), 1);
    let mut records = records;
    records.push(Record::ExpectedRules {
        events: events.clone(),
    });
    let mut script = script(&records, true, true);
    script.push_str(&format!("local result=run();assert(result.mode=='rules' and result.checked);assert(result.json==[==[{}]==]);assert(result.outcomes[1].id=='1');assert(result.outcomes[1].source.device=='18446744073709551615');return {{version=1}}", serde_json::to_string(&events).unwrap()));
    Definition::compile(&script, "configured-replay.lua").unwrap();
}

#[test]
fn replay_can_run_inside_runtime_callbacks_and_releases_closed_capacity() {
    let mut script = script(&records(), false, false);
    script.push_str("p.events.on('ready',function()for i=1,6 do assert(run().mode=='observations')end;p.log.info('replayed')end);return {version=1}");
    let mut runtime = Definition::compile(&script, "runtime-replay.lua")
        .unwrap()
        .instantiate()
        .unwrap();
    assert_eq!(
        runtime
            .dispatch(EventKind::Ready, &serde_json::json!({}))
            .unwrap()[0]
            .message,
        "replayed"
    );
}

#[test]
fn replay_errors_close_the_session_without_becoming_successful_checks() {
    for records in [vec![], vec![Record::Header { version: 1 }], records()] {
        let mut session = Session::new(None).unwrap();
        for record in records {
            session
                .push(&serde_json::to_string(&record).unwrap())
                .unwrap();
        }
        assert!(session.finish(true).is_err());
        assert_eq!(session.state, "failed");
        assert!(session.engine.is_none());
    }
    for invalid in [
        "{}",
        "{\"record\":\"header\",\"version\":2}",
        "{\"record\":\"header\",\"version\":1}\n{}",
    ] {
        let mut session = Session::new(None).unwrap();
        assert!(session.push(invalid).is_err());
        assert_eq!(session.state, "failed");
        assert!(session.engine.is_none());
    }
}

#[test]
fn replay_limits_and_open_handle_capacity_are_enforced() {
    let source = "local p=require('prek');local handles={};for i=1,4 do handles[i]=p.replay.new()end;assert(not pcall(p.replay.new));handles[1]:close();assert(p.replay.new());return {version=1}";
    Definition::compile(source, "capacity.lua").unwrap();
    let mut session = Session::new(None).unwrap();
    assert!(session.push(&" ".repeat(MAX_RECORD_BYTES + 1)).is_err());
    assert!(session.engine.is_none());
    let mut session = Session::new(None).unwrap();
    session.records = MAX_RECORDS;
    assert!(
        session
            .push("{\"record\":\"header\",\"version\":1}")
            .is_err()
    );
    assert!(session.engine.is_none());
}
