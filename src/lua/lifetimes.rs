use super::*;

fn collected(runtime: &mut Runtime) -> usize {
    runtime.lua.gc_collect();
    runtime.lua.gc_collect();
    runtime.lua.total_memory()
}

fn released(runtime: &mut Runtime, before: usize) {
    let after = collected(runtime);
    assert!(
        before > after + 512 * 1024,
        "captured closure retained: before={before}, after={after}"
    );
}

#[test]
fn disposed_subscription_releases_captures_while_its_handle_is_still_retained() {
    let mut runtime = Runtime::load(
        r#"
local p=require('prek')
local function subscribe()
 local captured=string.rep('s',1024*1024)
 return p.events.on('ready',function()p.log.info('live',{bytes=#captured})end)
end
local handle=subscribe()
p.events.on('matched',function()handle:dispose();handle:dispose()end)
return {version=1}
"#,
        "disposed.lua",
    )
    .unwrap();
    let before = collected(&mut runtime);
    assert_eq!(
        runtime
            .dispatch(EventKind::Ready, &serde_json::Value::Null)
            .unwrap()[0]
            .fields["bytes"],
        1024 * 1024
    );
    runtime
        .dispatch(EventKind::Matched, &serde_json::Value::Null)
        .unwrap();
    assert!(!runtime.registrations().unwrap()[0].active);
    assert!(
        runtime
            .dispatch(EventKind::Ready, &serde_json::Value::Null)
            .unwrap()
            .is_empty()
    );
    released(&mut runtime, before);
}

#[test]
fn self_disposal_releases_captures_only_after_the_current_callback_finishes() {
    let mut runtime = Runtime::load(
        r#"
local p=require('prek')
local function subscribe()
 local captured=string.rep('d',1024*1024)
 local handle
 handle=p.events.on('ready',function()
  handle:dispose()
  p.log.info('finished',{bytes=#captured})
 end)
 return handle
end
local handle=subscribe()
p.events.on('matched',function()handle:dispose()end)
return {version=1}
"#,
        "self-disposal.lua",
    )
    .unwrap();
    let before = collected(&mut runtime);
    let logs = runtime
        .dispatch(EventKind::Ready, &serde_json::Value::Null)
        .unwrap();
    assert_eq!(logs[0].message, "finished");
    assert_eq!(logs[0].fields["bytes"], 1024 * 1024);
    released(&mut runtime, before);
}

#[test]
fn cancelled_timers_release_captures_without_waiting_for_a_timer_wake() {
    for periodic in [false, true] {
        let constructor = if periodic { "every" } else { "after" };
        let mut runtime = Runtime::load(&format!(r#"
local p=require('prek')
local function schedule()
 local captured=string.rep('c',1024*1024)
 return p.scheduler.{constructor}('10s',function()p.log.info('cancelled callback ran',{{bytes=#captured}})end)
end
local handle=schedule()
p.events.on('ready',function()handle:cancel();handle:cancel()end)
return {{version=1}}
"#), "cancelled-timer.lua").unwrap();
        runtime.set_clock(Duration::ZERO).unwrap();
        let before = collected(&mut runtime);
        runtime
            .dispatch(EventKind::Ready, &serde_json::Value::Null)
            .unwrap();
        assert!(runtime.scheduler_deadline().unwrap().is_none());
        released(&mut runtime, before);
    }
}

#[test]
fn completed_one_shots_release_captures_while_their_handles_remain_live() {
    let mut runtime = Runtime::load(
        r#"
local p=require('prek')
local function schedule()
 local captured=string.rep('o',1024*1024)
 return p.scheduler.after('1ms',function()p.log.info('completed',{bytes=#captured})end)
end
local handle=schedule()
p.events.on('ready',function()handle:cancel()end)
return {version=1}
"#,
        "completed-timer.lua",
    )
    .unwrap();
    runtime.set_clock(Duration::ZERO).unwrap();
    let before = collected(&mut runtime);
    let logs = runtime
        .tick(Duration::from_millis(1), &serde_json::Value::Null)
        .unwrap();
    assert_eq!(logs[0].message, "completed");
    assert_eq!(logs[0].fields["bytes"], 1024 * 1024);
    assert!(runtime.scheduler_deadline().unwrap().is_none());
    released(&mut runtime, before);
}

#[test]
fn self_cancelled_periodic_callbacks_finish_then_release_their_captures() {
    let mut runtime = Runtime::load(
        r#"
local p=require('prek')
local function schedule()
 local captured=string.rep('p',1024*1024)
 local handle
 handle=p.scheduler.every('1ms',function()
  handle:cancel()
  p.log.info('finished',{bytes=#captured})
 end)
 return handle
end
local handle=schedule()
p.events.on('ready',function()handle:cancel()end)
return {version=1}
"#,
        "self-cancelled-timer.lua",
    )
    .unwrap();
    runtime.set_clock(Duration::ZERO).unwrap();
    let before = collected(&mut runtime);
    let logs = runtime
        .tick(Duration::from_millis(1), &serde_json::Value::Null)
        .unwrap();
    assert_eq!(logs[0].message, "finished");
    assert_eq!(logs[0].fields["bytes"], 1024 * 1024);
    assert!(runtime.scheduler_deadline().unwrap().is_none());
    released(&mut runtime, before);
}

#[test]
fn configuration_disposal_preserves_the_approved_snapshot_without_retaining_callbacks() {
    let definition = Definition::compile(
        r#"
local p=require('prek')
p.events.on('ready',function()error('disposed')end):dispose()
p.scheduler.after('1ms',function()error('cancelled')end):cancel()
return {version=1}
"#,
        "inactive-source.lua",
    )
    .unwrap();
    assert!(!definition.registrations[0].active);
    assert!(!definition.timers[0].active);
    let encoded = serde_json::to_vec(&definition).unwrap();
    let decoded: Definition = serde_json::from_slice(&encoded).unwrap();
    let mut runtime = decoded.instantiate().unwrap();
    runtime.lua.enter(|ctx| {
        for registry in [&runtime.registry, &runtime.timers] {
            let entry: Table = ctx.fetch(registry).get(ctx, 1).unwrap();
            assert!(entry.get::<_, Value>(ctx, "callback").unwrap().is_nil());
        }
    });
    assert!(
        runtime
            .dispatch(EventKind::Ready, &serde_json::Value::Null)
            .unwrap()
            .is_empty()
    );
    runtime.set_clock(Duration::ZERO).unwrap();
    assert!(
        runtime
            .tick(Duration::from_millis(1), &serde_json::Value::Null)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn disposal_does_not_recycle_source_generation_identities_or_lifetime_quotas() {
    let mut runtime = Runtime::load(
        r#"
local p=require('prek')
for index=1,256 do
 p.events.on('ready',function()end):dispose()
 p.scheduler.after('1ms',function()end):cancel()
end
assert(not pcall(p.events.on,'ready',function()end))
assert(not pcall(p.scheduler.after,'1ms',function()end))
return {version=1}
"#,
        "lifetime-quota.lua",
    )
    .unwrap();
    let subscriptions = runtime.registrations().unwrap();
    let timers = runtime.timer_registrations().unwrap();
    assert_eq!(subscriptions.len(), 256);
    assert_eq!(timers.len(), 256);
    assert!(
        subscriptions
            .iter()
            .enumerate()
            .all(|(index, entry)| usize::from(entry.id) == index + 1 && !entry.active)
    );
    assert!(
        timers
            .iter()
            .enumerate()
            .all(|(index, entry)| usize::from(entry.id) == index + 1 && !entry.active)
    );
}
