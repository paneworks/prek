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
fn named_handler_executes_exact_argv_only_after_a_current_model_grant() {
    use prek::{
        actions::Executor,
        config::Action,
        input::Source,
        ipc::Channel,
        protocol::{Capability, Completion, Mode, Request},
        rules::{Event, Evidence, Stage},
    };
    use std::{collections::BTreeSet, fs};
    let path = std::env::temp_dir().join(format!(
        "prek-lua-action-{}-$(literal);'quoted'",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    let definition=Definition::compile(r#"
local p=require('prek')
local action=p.actions.define('touch',{requires={'commands'}},function(event)
 p.log.info('named-action',{rule=event.event.rule})
 return p.actions.command{argv={'touch','--',event.event.output}}
end)
return p.config.new{rules={p.rules.define{id='touch-rule',trigger=p.triggers.key{key='KEY_A'},action=action}}}
"#,"named-process.lua").unwrap();
    let action = definition.validate().unwrap().rules[0].action.clone();
    assert!(matches!(action, Action::Lua { .. }));
    let mut session = Session::spawn(
        Path::new(env!("CARGO_BIN_EXE_prek")),
        fence(1),
        "seat0",
        definition,
    )
    .unwrap();
    drain(&mut session).unwrap();
    assert!(session.action_ready());
    let mut executor = Executor::new(
        fence(1),
        "seat0".into(),
        Mode::Route,
        BTreeSet::from([Capability::Commands, Capability::LuaActions]),
    )
    .unwrap();
    executor.lua_ready(true);
    let (mut agent_channel, mut model_broker) = Channel::pair().unwrap();
    let event = Box::new(Event::Matched {
        version: 1,
        rule: "touch-rule".into(),
        id: 1,
        ordinal: 1,
        source: Source {
            device: 1,
            generation: 1,
            seat: "seat0".into(),
        },
        at: monotonic().unwrap(),
        config_generation: 1,
        session_generation: 1,
        stage: Stage::Discrete,
        action,
        evidence: Evidence::Key { down: true },
        window: None,
        output: Some(path.to_str().unwrap().into()),
        workspace: None,
    });
    executor
        .offer(fence(1), event.clone(), monotonic().unwrap())
        .unwrap();
    executor
        .execute_ready_with_lua(monotonic().unwrap(), Some(&mut session))
        .unwrap();
    assert!(
        !path.exists(),
        "an offered action is not an execution grant"
    );
    executor
        .grant(
            fence(1),
            1,
            1,
            monotonic().unwrap() + Duration::from_millis(250),
            monotonic().unwrap(),
        )
        .unwrap();
    assert!(
        executor
            .offer(fence(1), event.clone(), monotonic().unwrap())
            .is_err()
    );
    executor
        .execute_ready_with_lua(monotonic().unwrap(), Some(&mut session))
        .unwrap();
    // Progress coalescing must retain the byte charge for the queued action.
    // Otherwise sending it can underflow the shared queue counter.
    for _ in 0..2 {
        session
            .notify(
                EventKind::Update,
                serde_json::json!({
                    "fence": fence(1), "event": {"id": 1, "stage": "update", "source": "fixture"}
                }),
            )
            .unwrap();
    }
    // Cancel before the queued callback starts. Its command and log must stay
    // absent while the same supervised VM accepts a later gesture.
    executor.cancel_action(fence(1), 1, 1).unwrap();
    session.cancel_action(1, 1).unwrap();
    drain(&mut session).unwrap();
    for (id, ordinal, result) in session.take_actions() {
        assert!(
            executor
                .resolve_action(id, ordinal, result, monotonic().unwrap())
                .unwrap()
                .is_empty()
        );
    }
    assert!(!path.exists() && session.action_ready());
    let mut later = event;
    let Event::Matched { id, at, .. } = later.as_mut() else {
        unreachable!()
    };
    *id = 2;
    *at = monotonic().unwrap();
    executor
        .offer(fence(1), later, monotonic().unwrap())
        .unwrap();
    let now = monotonic().unwrap();
    executor
        .grant(fence(1), 2, 1, now + Duration::from_millis(250), now)
        .unwrap();
    executor
        .execute_ready_with_lua(now, Some(&mut session))
        .unwrap();
    let end = monotonic().unwrap() + Duration::from_secs(3);
    let mut logs = Vec::new();
    let mut completions = Vec::new();
    while completions.len() < 2 {
        session.step(monotonic().unwrap()).unwrap();
        for (id, ordinal, result) in session.take_actions() {
            logs.extend(
                executor
                    .resolve_action(id, ordinal, result, monotonic().unwrap())
                    .unwrap(),
            );
        }
        executor
            .execute_ready_with_lua(monotonic().unwrap(), Some(&mut session))
            .unwrap();
        executor.reap().unwrap();
        executor.flush(&mut agent_channel).unwrap();
        while let Some(packet) = model_broker.receive::<Request>().unwrap() {
            if let Request::Complete {
                fence,
                id,
                ordinal,
                result,
            } = packet.message
            {
                completions.push((fence, id, ordinal, result));
            }
        }
        assert!(monotonic().unwrap() < end, "named command never executed");
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].message, "named-action");
    assert!(path.exists());
    assert_eq!(
        completions,
        vec![
            (fence(1), 1, 1, Completion::Cancelled),
            (fence(1), 2, 1, Completion::Applied)
        ]
    );
    assert!(drain(&mut session).unwrap().is_empty());
    fs::remove_file(&path).unwrap();
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
