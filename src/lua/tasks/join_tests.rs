use super::*;

fn ms(value: u64) -> Duration {
    Duration::from_millis(value)
}
fn load(source: &str) -> Runtime {
    Runtime::load(source, "task-join.lua").unwrap()
}
fn clock(runtime: &mut Runtime) {
    runtime.set_clock(Duration::ZERO).unwrap();
}

#[test]
fn join_waits_without_polling_and_resumes_on_a_later_wake_with_fresh_context() {
    let mut runtime = load(
        r#"
local p=require('prek')
local target=p.scheduler.spawn(function()p.scheduler.sleep('100ms');p.log.info('target')end)
local waiter=p.scheduler.spawn(function()
 local state,wake=target:join()
 assert(state=='completed' and wake.task.id==2 and wake.context.value=='fresh')
 p.log.info('joined',{state=state})
end)
p.events.on('ready',function()assert(waiter:status()=='waiting')end)
return {version=1}
"#,
    );
    clock(&mut runtime);
    assert!(
        runtime
            .tick(ms(1), &serde_json::json!({"value":"old"}))
            .unwrap()
            .is_empty()
    );
    runtime
        .dispatch(EventKind::Ready, &serde_json::Value::Null)
        .unwrap();
    let target = runtime.scheduler_deadline().unwrap().unwrap();
    assert!(target >= ms(101));
    assert!(
        runtime
            .tick(ms(50), &serde_json::Value::Null)
            .unwrap()
            .is_empty()
    );
    assert_eq!(runtime.scheduler_deadline().unwrap(), Some(target));
    assert_eq!(
        runtime.tick(target, &serde_json::Value::Null).unwrap()[0].message,
        "target"
    );
    let joined = runtime.scheduler_deadline().unwrap().unwrap();
    assert_eq!(joined, target + ms(1));
    assert_eq!(
        runtime
            .tick(joined, &serde_json::json!({"value":"fresh"}))
            .unwrap()[0]
            .message,
        "joined"
    );
    assert!(runtime.scheduler_deadline().unwrap().is_none());
    assert!(runtime.task_executors.is_empty());
}

#[test]
fn cancelling_a_target_wakes_its_joiner_without_waiting_for_the_sleep_deadline() {
    let mut runtime = load(
        r#"
local p=require('prek')
local target=p.scheduler.spawn(function()p.scheduler.sleep('60m');error('cancelled target ran')end)
p.scheduler.spawn(function()local state,wake=target:join(); assert(wake.context.current);p.log.info(state)end)
p.events.on('ready',function()target:cancel()end)
return {version=1}
"#,
    );
    clock(&mut runtime);
    runtime.tick(ms(1), &serde_json::Value::Null).unwrap();
    runtime
        .dispatch(EventKind::Ready, &serde_json::Value::Null)
        .unwrap();
    assert_eq!(runtime.scheduler_deadline().unwrap(), Some(ms(2)));
    assert_eq!(
        runtime
            .tick(ms(2), &serde_json::json!({"current":true}))
            .unwrap()[0]
            .message,
        "cancelled"
    );
    assert!(runtime.scheduler_deadline().unwrap().is_none());
}

#[test]
fn cancelling_a_waiter_releases_its_executor_without_cancelling_the_target() {
    let mut runtime = load(
        r#"
local p=require('prek')
local target=p.scheduler.spawn(function()p.scheduler.sleep('100ms');p.log.info('target')end)
local waiter=p.scheduler.spawn(function()target:join();error('cancelled waiter ran')end)
p.events.on('ready',function()waiter:cancel();assert(target:status()=='sleeping')end)
return {version=1}
"#,
    );
    clock(&mut runtime);
    runtime.tick(ms(1), &serde_json::Value::Null).unwrap();
    let target = runtime.scheduler_deadline().unwrap().unwrap();
    runtime
        .dispatch(EventKind::Ready, &serde_json::Value::Null)
        .unwrap();
    assert_eq!(runtime.scheduler_deadline().unwrap(), Some(target));
    assert_eq!(runtime.task_executors.len(), 1);
    assert_eq!(
        runtime.tick(target, &serde_json::Value::Null).unwrap()[0].message,
        "target"
    );
    assert!(runtime.scheduler_deadline().unwrap().is_none());
}

#[test]
fn terminal_handles_join_immediately_after_their_capacity_slots_are_reused() {
    for cancelled in [false, true] {
        let mut runtime = load(&format!(
            r#"
local p=require('prek')
local old=p.scheduler.spawn(function()end)
if {cancelled} then old:cancel() end
p.events.on('ready',function()
 p.scheduler.spawn(function()
  local state,wake=old:join()
  assert(state=='{state}' and wake==nil)
  p.log.info(state)
 end)
end)
return {{version=1}}
"#,
            state = if cancelled { "cancelled" } else { "completed" }
        ));
        clock(&mut runtime);
        runtime.tick(ms(1), &serde_json::Value::Null).unwrap();
        runtime
            .dispatch(EventKind::Ready, &serde_json::Value::Null)
            .unwrap();
        assert_eq!(
            runtime.tick(ms(2), &serde_json::Value::Null).unwrap()[0].message,
            if cancelled { "cancelled" } else { "completed" }
        );
        assert!(runtime.scheduler_deadline().unwrap().is_none());
    }
}

#[test]
fn self_joins_and_dependency_cycles_fail_instead_of_leaving_an_idle_deadlock() {
    for source in [
        "local a; a=p.scheduler.spawn(function()a:join()end)",
        "local a,b;a=p.scheduler.spawn(function()b:join()end);b=p.scheduler.spawn(function()a:join()end)",
        "local a,b,c;a=p.scheduler.spawn(function()b:join()end);b=p.scheduler.spawn(function()c:join()end);c=p.scheduler.spawn(function()a:join()end)",
    ] {
        let mut runtime = load(&format!(
            "local p=require('prek'); {source}; return {{version=1}}"
        ));
        clock(&mut runtime);
        let reason = runtime
            .tick(ms(1), &serde_json::Value::Null)
            .unwrap_err()
            .to_string();
        assert!(
            reason.contains("join itself") || reason.contains("cycle"),
            "{reason}"
        );
        assert!(runtime.failed);
        assert!(runtime.tick(ms(2), &serde_json::Value::Null).is_err());
    }
}

#[test]
fn joining_is_task_only_and_private_bindings_are_not_exposed() {
    let mut runtime = load(
        r#"
local p=require('prek')
local target=p.scheduler.spawn(function()end)
assert(type(target.join)=='function' and p.scheduler.__task_join==nil)
assert(not pcall(function()target:join()end))
p.events.on('ready',function()assert(not pcall(function()target:join()end))end)
return {version=1}
"#,
    );
    clock(&mut runtime);
    runtime.tick(ms(1), &serde_json::Value::Null).unwrap();
    runtime
        .dispatch(EventKind::Ready, &serde_json::Value::Null)
        .unwrap();
}

#[test]
fn joined_wakes_keep_the_existing_eight_resume_budget_and_stable_order() {
    let mut runtime = load(
        r#"
local p=require('prek')
local target=p.scheduler.spawn(function()p.scheduler.sleep('10ms')end)
for i=1,9 do p.scheduler.spawn(function()assert(target:join()=='completed');p.log.info('joined',{id=i})end) end
return {version=1}
"#,
    );
    clock(&mut runtime);
    assert!(
        runtime
            .tick(ms(1), &serde_json::Value::Null)
            .unwrap()
            .is_empty()
    );
    assert!(
        runtime
            .tick(ms(1), &serde_json::Value::Null)
            .unwrap()
            .is_empty()
    );
    let target = runtime.scheduler_deadline().unwrap().unwrap();
    assert!(
        runtime
            .tick(target, &serde_json::Value::Null)
            .unwrap()
            .is_empty()
    );
    let due = runtime.scheduler_deadline().unwrap().unwrap();
    let first = runtime.tick(due, &serde_json::Value::Null).unwrap();
    assert_eq!(first.len(), 8);
    for (index, log) in first.iter().enumerate() {
        assert_eq!(log.fields["id"], index + 1);
    }
    assert_eq!(runtime.scheduler_deadline().unwrap(), Some(due));
    assert_eq!(
        runtime.tick(due, &serde_json::Value::Null).unwrap()[0].fields["id"],
        9
    );
    assert!(runtime.scheduler_deadline().unwrap().is_none());
}

#[test]
fn cancelling_a_waiting_task_releases_captures_even_with_a_retained_handle() {
    let mut runtime = load(
        r#"
local p=require('prek')
local target=p.scheduler.spawn(function()p.scheduler.sleep('60m')end)
local function make()
 local captured=string.rep('j',1024*1024)
 return p.scheduler.spawn(function()target:join();p.log.info('unreachable',{bytes=#captured})end)
end
local waiter=make()
p.events.on('ready',function()waiter:cancel();assert(waiter:status()=='cancelled')end)
return {version=1}
"#,
    );
    clock(&mut runtime);
    runtime.tick(ms(1), &serde_json::Value::Null).unwrap();
    runtime.lua.gc_collect();
    runtime.lua.gc_collect();
    let before = runtime.lua.total_memory();
    runtime
        .dispatch(EventKind::Ready, &serde_json::Value::Null)
        .unwrap();
    runtime.scheduler_deadline().unwrap();
    runtime.lua.gc_collect();
    runtime.lua.gc_collect();
    let after = runtime.lua.total_memory();
    assert!(
        before > after + 512 * 1024,
        "waiting coroutine capture retained: {before} -> {after}"
    );
    assert_eq!(runtime.task_executors.len(), 1);
    assert!(runtime.host.borrow().task_joins.is_empty());
}

#[test]
fn a_waiting_joiner_keeps_the_cancelled_target_when_its_capacity_slot_is_reused() {
    let mut runtime = load(
        r#"
local p=require('prek')
local target=p.scheduler.spawn(function()p.scheduler.sleep('60m')end)
p.scheduler.spawn(function()p.log.info(target:join())end)
p.events.on('ready',function()
 target:cancel()
 p.scheduler.spawn(function()p.scheduler.sleep('100ms');p.log.info('replacement')end)
end)
return {version=1}
"#,
    );
    clock(&mut runtime);
    runtime.tick(ms(1), &serde_json::Value::Null).unwrap();
    runtime
        .dispatch(EventKind::Ready, &serde_json::Value::Null)
        .unwrap();
    assert_eq!(runtime.scheduler_deadline().unwrap(), Some(ms(2)));
    let logs = runtime.tick(ms(2), &serde_json::Value::Null).unwrap();
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].message, "cancelled");
    let due = runtime.scheduler_deadline().unwrap().unwrap();
    assert!(due >= ms(102));
    assert_eq!(
        runtime.tick(due, &serde_json::Value::Null).unwrap()[0].message,
        "replacement"
    );
    assert!(runtime.scheduler_deadline().unwrap().is_none());
}

#[test]
fn a_caught_cycle_error_preserves_the_existing_waiter_until_its_target_completes() {
    let mut runtime = load(
        r#"
local p=require('prek')
local a,b
a=p.scheduler.spawn(function()assert(b:join()=='completed');p.log.info('a')end)
b=p.scheduler.spawn(function()
 local ok,reason=pcall(function()a:join()end)
 assert(not ok and tostring(reason):find('cycle'))
 p.log.info('b')
end)
return {version=1}
"#,
    );
    clock(&mut runtime);
    assert_eq!(
        runtime.tick(ms(1), &serde_json::Value::Null).unwrap()[0].message,
        "b"
    );
    assert_eq!(runtime.scheduler_deadline().unwrap(), Some(ms(2)));
    assert_eq!(
        runtime.tick(ms(2), &serde_json::Value::Null).unwrap()[0].message,
        "a"
    );
    assert!(!runtime.failed);
    assert!(runtime.host.borrow().task_joins.is_empty());
    assert!(runtime.scheduler_deadline().unwrap().is_none());
}

#[test]
fn a_self_cancelled_task_cannot_retain_a_new_join_or_resume_after_it() {
    let mut runtime = load(
        r#"
local p=require('prek')
local target=p.scheduler.spawn(function()p.scheduler.sleep('100ms');p.log.info('target')end)
local waiter
waiter=p.scheduler.spawn(function()
 waiter:cancel()
 p.log.info('discarded')
 target:join()
 error('self-cancelled waiter resumed')
end)
return {version=1}
"#,
    );
    clock(&mut runtime);
    assert!(
        runtime
            .tick(ms(1), &serde_json::Value::Null)
            .unwrap()
            .is_empty()
    );
    assert_eq!(runtime.task_executors.len(), 1);
    assert!(runtime.host.borrow().task_joins.is_empty());
    let due = runtime.scheduler_deadline().unwrap().unwrap();
    assert!(due >= ms(101));
    assert_eq!(
        runtime.tick(due, &serde_json::Value::Null).unwrap()[0].message,
        "target"
    );
    assert!(runtime.scheduler_deadline().unwrap().is_none());
}
