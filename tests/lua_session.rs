#![cfg(target_os = "linux")]
use prek::{
    ipc::monotonic,
    lua::{Definition, EventKind, LogRecord, session::Session},
    protocol::{Epoch, Fence},
};
use std::{path::Path, time::Duration};
fn fence(generation: u64) -> Fence {
    Fence {
        broker: Epoch([1, 2]),
        configuration: generation,
        session: 1,
    }
}
fn drain(session: &mut Session) -> Result<Vec<LogRecord>, std::io::Error> {
    let end = monotonic()? + Duration::from_secs(5);
    let mut logs = Vec::new();
    loop {
        logs.extend(session.step(monotonic()?)?);
        if session.finished() {
            return Ok(logs);
        }
        assert!(monotonic()? < end, "Lua session never settled");
        std::thread::sleep(Duration::from_millis(1));
    }
}
#[test]
fn real_supervised_vm_preserves_closures_and_runs_stop_callbacks() {
    let definition = Definition::compile(
        r#"
local p=require('prek')
local count=0
p.events.on('matched',function(event)
 count=count+1; p.log.info('match',{count=count,rule=event.event.rule})
end)
p.events.on('stop',function()p.log.info('stopped')end)
return {version=1}
"#,
        "session.lua",
    )
    .unwrap();
    let mut session = Session::spawn(
        Path::new(env!("CARGO_BIN_EXE_prek")),
        fence(1),
        "seat0",
        definition,
    )
    .unwrap();
    assert!(drain(&mut session).unwrap().is_empty());
    for count in 1..=2 {
        session
            .notify(
                EventKind::Matched,
                serde_json::json!({"fence":fence(1),"event":{"rule":"fixture"}}),
            )
            .unwrap();
        let logs = drain(&mut session).unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(
            logs[0].fields,
            serde_json::json!({"count":count,"rule":"fixture"})
        );
    }
    session.stop().unwrap();
    assert_eq!(drain(&mut session).unwrap()[0].message, "stopped");
}
#[test]
fn callback_failure_does_not_poison_a_replacement_generation() {
    let binary = Path::new(env!("CARGO_BIN_EXE_prek"));
    let bad=Definition::compile("local p=require('prek');p.events.on('matched',function()while true do end end);return {version=1}","failed.lua").unwrap();
    let mut failed = Session::spawn(binary, fence(1), "seat0", bad).unwrap();
    drain(&mut failed).unwrap();
    failed
        .notify(
            EventKind::Matched,
            serde_json::json!({"fence":fence(1),"event":{}}),
        )
        .unwrap();
    assert!(drain(&mut failed).is_err());
    drop(failed);
    let good=Definition::compile("local p=require('prek');p.events.on('ready',function()p.log.info('new generation')end);return {version=1}","new.lua").unwrap();
    let mut replacement = Session::spawn(binary, fence(2), "seat0", good).unwrap();
    assert_eq!(
        drain(&mut replacement).unwrap()[0].message,
        "new generation"
    );
}

#[test]
fn supervised_timers_run_during_idle_and_cancel_before_the_next_tick() {
    let definition = Definition::compile(
        r#"
local p=require('prek')
local periodic
periodic=p.scheduler.every('10ms',function(event)
 p.log.info('periodic',{generation=event.context.fence.configuration})
 periodic:cancel()
end)
p.scheduler.after('20ms',function()p.log.info('after')end)
return {version=1}
"#,
        "timer-process.lua",
    )
    .unwrap();
    let mut session = Session::spawn(
        Path::new(env!("CARGO_BIN_EXE_prek")),
        fence(1),
        "seat0",
        definition,
    )
    .unwrap();
    let end = monotonic().unwrap() + Duration::from_secs(5);
    let mut logs = Vec::new();
    while logs.len() < 2 {
        logs.extend(session.step(monotonic().unwrap()).unwrap());
        assert!(monotonic().unwrap() < end);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        logs.iter()
            .map(|log| log.message.as_str())
            .collect::<Vec<_>>(),
        vec!["periodic", "after"]
    );
    assert_eq!(logs[0].fields["generation"], 1);
    for _ in 0..40 {
        assert!(session.step(monotonic().unwrap()).unwrap().is_empty());
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn replacing_a_vm_retires_its_pending_timer_generation() {
    let binary = Path::new(env!("CARGO_BIN_EXE_prek"));
    let old=Definition::compile("local p=require('prek');p.scheduler.after('100ms',function()p.log.info('old generation')end);return {version=1}","old-timer.lua").unwrap();
    let mut retired = Session::spawn(binary, fence(1), "seat0", old).unwrap();
    drain(&mut retired).unwrap();
    drop(retired);
    let new=Definition::compile("local p=require('prek');p.scheduler.after('100ms',function(event)p.log.info('new generation',{generation=event.context.fence.configuration})end);return {version=1}","new-timer.lua").unwrap();
    let mut replacement = Session::spawn(binary, fence(2), "seat0", new).unwrap();
    let end = monotonic().unwrap() + Duration::from_secs(5);
    loop {
        let logs = replacement.step(monotonic().unwrap()).unwrap();
        if !logs.is_empty() {
            assert_eq!(logs.len(), 1);
            assert_eq!(logs[0].message, "new generation");
            assert_eq!(logs[0].fields["generation"], 2);
            break;
        }
        assert!(monotonic().unwrap() < end);
        std::thread::sleep(Duration::from_millis(1));
    }
}
