//! Bounded generation-owned coroutine tasks. Only host-approved sleeps yield.
use super::*;
use luna::{Context, Function};
use serde::{Deserialize, Serialize};
use std::{cell::RefCell, rc::Rc};

pub const MAX_TASKS: usize = 64;
pub const TASKS_PER_WAKE: usize = 8;
pub(super) const MAX_TASK_ID: u64 = 9_007_199_254_740_991;
pub(super) struct TaskExecution {
    executor: StashedExecutor,
    entry: luna::StashedTable,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TaskRegistration {
    pub id: u64,
    pub active: bool,
}
pub(super) fn defer<'gc>(ctx: &Context<'gc>, host: Rc<RefCell<runtime::Host>>) -> Callback<'gc> {
    Callback::from_fn(ctx, move |ctx, _, mut stack| {
        let due = host
            .borrow()
            .now
            .map(|now| nanos(now + Duration::from_millis(1)))
            .transpose()?;
        stack.replace(ctx, due);
        Ok(CallbackReturn::Return)
    })
}
pub(super) fn sleep<'gc>(ctx: &Context<'gc>, host: Rc<RefCell<runtime::Host>>) -> Callback<'gc> {
    Callback::from_fn(ctx, move |ctx, _, mut stack| {
        if host.borrow().task.is_none() {
            return Err(
                io::Error::other("scheduler.sleep requires an active scheduler task").into(),
            );
        }
        let text: luna::String = stack.consume(ctx)?;
        let text = text.to_str().map_err(io::Error::other)?;
        if text.len() > 64 {
            return Err(io::Error::other("task sleep duration exceeds byte limit").into());
        }
        let delay = crate::config::Millis::parse(text)
            .map_err(io::Error::other)?
            .duration();
        let mut state = host.borrow_mut();
        let (now, started) = state
            .task_clock
            .ok_or_else(|| io::Error::other("task sleep clock is unavailable"))?;
        let deadline = now + started.elapsed() + delay;
        if state.sleep.replace(deadline).is_some() {
            return Err(io::Error::other("multiple task sleeps in one resume").into());
        }
        stack.replace(ctx, ());
        Ok(CallbackReturn::Yield {
            to_thread: None,
            then: None,
        })
    })
}
fn nanos(time: Duration) -> io::Result<i64> {
    i64::try_from(time.as_nanos()).map_err(|_| io::Error::other("Lua task clock exceeds range"))
}
impl Runtime {
    pub fn task_registrations(&mut self) -> Result<Vec<TaskRegistration>, ConfigError> {
        self.lua
            .try_enter(|ctx| {
                let tasks = ctx.fetch(&self.tasks);
                let mut registrations = Vec::new();
                for id in 1..=tasks.length(&ctx) {
                    let entry: Table = tasks.get(ctx, id)?;
                    registrations.push(TaskRegistration {
                        id: entry.get(ctx, "id")?,
                        active: entry.get(ctx, "active")?,
                    });
                }
                registrations.sort_by_key(|task| task.id);
                Ok(registrations)
            })
            .map_err(error)
    }
    pub(super) fn clock_tasks(&mut self, now: Duration) -> Result<(), ConfigError> {
        let due = nanos(now + Duration::from_millis(1)).map_err(error)?;
        self.lua
            .try_enter(|ctx| {
                let tasks = ctx.fetch(&self.tasks);
                for id in 1..=tasks.length(&ctx) {
                    let entry: Table = tasks.get(ctx, id)?;
                    if entry.get::<_, Value>(ctx, "due")?.is_nil() {
                        entry.set(ctx, "due", due)?;
                    }
                }
                Ok(())
            })
            .map_err(error)?;
        self.sweep_tasks()
    }
    fn sweep_tasks(&mut self) -> Result<(), ConfigError> {
        let cancelled = self
            .lua
            .try_enter(|ctx| {
                let mut cancelled = Vec::new();
                for (id, task) in &self.task_executors {
                    let entry = ctx.fetch(&task.entry);
                    if !entry.get::<_, bool>(ctx, "active")? {
                        cancelled.push(*id);
                    }
                }
                Ok(cancelled)
            })
            .map_err(error)?;
        for id in cancelled {
            if let Some(executor) = self.task_executors.remove(&id) {
                self.lua
                    .enter(|ctx| ctx.fetch(&executor.executor).stop(&ctx));
            }
        }
        Ok(())
    }
    pub fn scheduler_deadline(&mut self) -> Result<Option<Duration>, ConfigError> {
        let timer = self.timer_deadline()?;
        self.sweep_tasks()?;
        let task = self
            .lua
            .try_enter(|ctx| {
                let tasks = ctx.fetch(&self.tasks);
                let mut minimum = None;
                for id in 1..=tasks.length(&ctx) {
                    let entry: Table = tasks.get(ctx, id)?;
                    let active: bool = entry.get(ctx, "active")?;
                    let due: Option<i64> = entry.get(ctx, "due")?;
                    if active && let Some(due) = due {
                        let due = Duration::from_nanos(due as u64);
                        minimum = Some(minimum.map_or(due, |old: Duration| old.min(due)));
                    }
                }
                Ok(minimum)
            })
            .map_err(error)?;
        Ok(match (timer, task) {
            (Some(timer), Some(task)) => Some(timer.min(task)),
            (timer, task) => timer.or(task),
        })
    }
    pub(super) fn tick_tasks(
        &mut self,
        now: Duration,
        context: &serde_json::Value,
    ) -> Result<(), ConfigError> {
        let timestamp = nanos(now).map_err(error)?;
        let mut ready = self
            .lua
            .try_enter(|ctx| {
                let tasks = ctx.fetch(&self.tasks);
                let mut ready = Vec::new();
                for id in 1..=tasks.length(&ctx) {
                    let entry: Table = tasks.get(ctx, id)?;
                    if entry.get::<_, bool>(ctx, "active")? {
                        let due: i64 = entry.get(ctx, "due")?;
                        if due <= timestamp {
                            ready.push((due, entry.get::<_, u64>(ctx, "id")?, ctx.stash(entry)));
                        }
                    }
                }
                Ok(ready)
            })
            .map_err(error)?;
        ready.sort_unstable_by_key(|(due, id, _)| (*due, *id));
        for (_, id, entry) in ready.into_iter().take(TASKS_PER_WAKE) {
            let active = self
                .lua
                .try_enter(|ctx| {
                    let entry = ctx.fetch(&entry);
                    Ok(entry.get::<_, bool>(ctx, "active")?)
                })
                .map_err(error)?;
            if !active {
                continue;
            }
            let payload = serde_json::json!({"context":context,"task":{"id":id},"at":now});
            let executor = if let Some(executor) = self.task_executors.remove(&id) {
                self.lua
                    .try_enter(|ctx| {
                        let value = values::payload(ctx, &payload)?;
                        ctx.fetch(&executor.executor)
                            .resume(ctx, (value,))
                            .map_err(io::Error::other)?;
                        Ok(())
                    })
                    .map_err(error)?;
                executor.executor
            } else {
                self.lua
                    .try_enter(|ctx| {
                        let entry = ctx.fetch(&entry);
                        let callback: Function = entry.get(ctx, "callback")?;
                        let value = values::payload(ctx, &payload)?;
                        Ok(ctx.stash(Executor::start(ctx, callback, (value,))))
                    })
                    .map_err(error)?
            };
            let (logs_before, bytes_before, proposal_before) = {
                let mut host = self.host.borrow_mut();
                host.task = Some(id);
                host.sleep = None;
                host.task_clock = Some((now, Instant::now()));
                (host.logs.len(), host.bytes, host.rules.pending_id())
            };
            let outcome = self.drive_task(&executor, &entry);
            let mut host = self.host.borrow_mut();
            host.task = None;
            host.sleep = None;
            host.task_clock = None;
            let retained = outcome?;
            let cancelled = self
                .lua
                .try_enter(|ctx| {
                    let entry = ctx.fetch(&entry);
                    let state: luna::String = entry.get(ctx, "state")?;
                    Ok(state.to_str()? == "cancelled")
                })
                .map_err(error)?;
            if retained {
                self.task_executors
                    .insert(id, TaskExecution { executor, entry });
            }
            if cancelled {
                host.logs.truncate(logs_before);
                host.bytes = bytes_before;
                if host.rules.pending_id() != proposal_before {
                    host.rules.discard("Lua task cancelled");
                }
            }
        }
        self.sweep_tasks()
    }
    fn drive_task(
        &mut self,
        executor: &StashedExecutor,
        entry: &luna::StashedTable,
    ) -> Result<bool, ConfigError> {
        self.lua
            .try_enter(|ctx| {
                let entry = ctx.fetch(entry);
                entry.set(ctx, "state", "running")?;
                Ok(())
            })
            .map_err(error)?;
        drive_budget(
            &mut self.lua,
            executor,
            Instant::now(),
            runtime::CALLBACK_TIME,
            runtime::CALLBACK_INSTRUCTIONS,
        )?;
        let sleep = self.host.borrow().sleep;
        self.lua
            .try_enter(|ctx| {
                let executor = ctx.fetch(executor);
                let values: luna::Variadic<Vec<Value>> =
                    executor.take_result(ctx).map_err(io::Error::other)??;
                let entry = ctx.fetch(entry);
                if !entry.get::<_, bool>(ctx, "active")? {
                    executor.stop(&ctx);
                    return Ok(false);
                }
                if !values.0.is_empty() {
                    return Err(io::Error::other(
                        "Lua tasks must return no values and cannot yield data",
                    )
                    .into());
                }
                match (executor.mode(), sleep) {
                    (luna::ExecutorMode::Stopped, None) => {
                        entry.set(ctx, "active", false)?;
                        entry.set(ctx, "state", "completed")?;
                        entry.set(ctx, "callback", Value::Nil)?;
                        Ok(false)
                    }
                    (luna::ExecutorMode::Suspended, Some(deadline)) => {
                        entry.set(ctx, "due", nanos(deadline)?)?;
                        entry.set(ctx, "state", "sleeping")?;
                        Ok(true)
                    }
                    _ => Err(
                        io::Error::other("Lua tasks may yield only through scheduler.sleep").into(),
                    ),
                }
            })
            .map_err(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn time(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }
    #[test]
    fn tasks_preserve_coroutines_sleep_context_and_completion_state() {
        let definition = Definition::compile(
            r#"
local p=require('prek')
local task
task=p.scheduler.spawn(function(wake)
 assert(task:status()=='running')
 p.log.info('first',{id=wake.task.id,value=wake.context.value})
 local next=p.scheduler.sleep('10ms')
 p.log.info('second',{id=next.task.id,value=next.context.value})
end)
p.events.on('matched',function()p.log.info(task:status())end)
return {version=1}
"#,
            "tasks.lua",
        )
        .unwrap();
        assert_eq!(
            definition.tasks,
            vec![TaskRegistration {
                id: 1,
                active: true
            }]
        );
        let mut runtime = definition.instantiate().unwrap();
        assert!(runtime.scheduler_deadline().unwrap().is_none());
        runtime.set_clock(time(0)).unwrap();
        assert_eq!(runtime.scheduler_deadline().unwrap(), Some(time(1)));
        assert!(
            runtime
                .tick(time(0), &serde_json::json!({"value":1}))
                .unwrap()
                .is_empty()
        );
        let first = runtime
            .tick(time(1), &serde_json::json!({"value":1}))
            .unwrap();
        assert_eq!(first[0].fields, serde_json::json!({"id":1,"value":1}));
        let due = runtime.scheduler_deadline().unwrap().unwrap();
        assert!(due >= time(11) && due < time(40));
        assert_eq!(
            runtime
                .dispatch(EventKind::Matched, &serde_json::Value::Null)
                .unwrap()[0]
                .message,
            "sleeping"
        );
        let next = runtime
            .tick(time(100), &serde_json::json!({"value":2}))
            .unwrap();
        assert_eq!(next[0].fields, serde_json::json!({"id":1,"value":2}));
        assert_eq!(
            runtime
                .dispatch(EventKind::Matched, &serde_json::Value::Null)
                .unwrap()[0]
                .message,
            "completed"
        );
        assert!(
            runtime.scheduler_deadline().unwrap().is_none() && runtime.task_executors.is_empty()
        );
    }
    #[test]
    fn cancellation_drops_sleeping_and_queued_tasks_and_preserves_other_work() {
        let mut runtime = Runtime::load(r#"
local p=require('prek')
local queued=p.scheduler.spawn(function()error('queued task ran')end)
queued:cancel(); queued:cancel()
local sleeping=p.scheduler.spawn(function()p.log.info('started');p.scheduler.sleep('1s');error('cancelled task resumed')end)
p.events.on('matched',function()sleeping:cancel(); sleeping:cancel(); p.log.info(sleeping:status())end)
p.scheduler.after('50ms',function()p.log.info('unrelated')end)
return {version=1}
"#, "cancel-tasks.lua").unwrap();
        runtime.set_clock(time(0)).unwrap();
        assert_eq!(
            runtime.tick(time(1), &serde_json::Value::Null).unwrap()[0].message,
            "started"
        );
        assert_eq!(runtime.task_executors.len(), 1);
        assert_eq!(
            runtime
                .dispatch(EventKind::Matched, &serde_json::Value::Null)
                .unwrap()[0]
                .message,
            "cancelled"
        );
        assert_eq!(runtime.scheduler_deadline().unwrap(), Some(time(50)));
        assert!(runtime.task_executors.is_empty());
        assert_eq!(
            runtime.tick(time(100), &serde_json::Value::Null).unwrap()[0].message,
            "unrelated"
        );
        assert!(
            runtime
                .tick(time(2000), &serde_json::Value::Null)
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn task_wakes_are_bounded_fair_and_callback_spawn_is_deferred() {
        let mut runtime = Runtime::load(
            r#"
local p=require('prek')
for i=1,16 do p.scheduler.spawn(function()p.log.info('task',{id=i})end)end
p.scheduler.spawn(function()p.scheduler.spawn(function()p.log.info('child')end)end)
return {version=1}
"#,
            "fair-tasks.lua",
        )
        .unwrap();
        runtime.set_clock(time(0)).unwrap();
        let first = runtime.tick(time(1), &serde_json::Value::Null).unwrap();
        assert_eq!(first.len(), TASKS_PER_WAKE);
        assert_eq!(first[0].fields, serde_json::json!({"id":1}));
        let second = runtime.tick(time(1), &serde_json::Value::Null).unwrap();
        assert_eq!(second.len(), TASKS_PER_WAKE);
        assert_eq!(second[0].fields, serde_json::json!({"id":9}));
        assert!(
            runtime
                .tick(time(1), &serde_json::Value::Null)
                .unwrap()
                .is_empty()
        );
        assert_eq!(runtime.scheduler_deadline().unwrap(), Some(time(2)));
        assert_eq!(
            runtime.tick(time(2), &serde_json::Value::Null).unwrap()[0].message,
            "child"
        );
    }
    #[test]
    fn task_failure_retires_the_vm_without_partial_wake_logs() {
        for body in [
            "while true do end",
            "coroutine.yield()",
            "return 1",
            "p.scheduler.sleep('0ms')",
        ] {
            let source = format!(
                "local p=require('prek'); p.scheduler.spawn(function()p.log.info('discard'); {body} end); return {{version=1}}"
            );
            let mut runtime = Runtime::load(&source, "task-fault.lua").unwrap();
            runtime.set_clock(time(0)).unwrap();
            assert!(runtime.tick(time(1), &serde_json::Value::Null).is_err());
            assert!(runtime.host.borrow().logs.is_empty());
            assert!(
                runtime
                    .tick(time(2), &serde_json::Value::Null)
                    .unwrap_err()
                    .to_string()
                    .contains("retired")
            );
        }
    }
    #[test]
    fn source_task_identity_quotas_and_sleep_boundaries_are_enforced() {
        for source in [
            "local p=require('prek'); p.scheduler.sleep('1ms'); return {version=1}",
            "local p=require('prek');assert=function()end;for i=1,65 do p.scheduler.spawn(function()end)end;return {version=1}",
        ] {
            assert!(Definition::compile(source, "invalid-task.lua").is_err());
        }
        let mut definition = Definition::compile(
            "local p=require('prek'); p.scheduler.spawn(function()end); return {version=1}",
            "identity.lua",
        )
        .unwrap();
        definition.tasks[0].active = false;
        assert!(definition.instantiate().is_err());
        let mut runtime = Runtime::load("local p=require('prek');p.events.on('ready',function()p.scheduler.sleep('1ms')end);return {version=1}", "outside-task.lua").unwrap();
        assert!(
            runtime
                .dispatch(EventKind::Ready, &serde_json::Value::Null)
                .is_err()
        );
    }
    #[test]
    fn self_cancellation_discards_its_logs_and_never_resumes() {
        let mut runtime = Runtime::load("local p=require('prek');local task;task=p.scheduler.spawn(function()p.log.info('discard');task:cancel();p.scheduler.sleep('1ms');error('resumed')end);return {version=1}", "self-cancel.lua").unwrap();
        runtime.set_clock(time(0)).unwrap();
        assert!(
            runtime
                .tick(time(1), &serde_json::Value::Null)
                .unwrap()
                .is_empty()
        );
        assert!(
            runtime.scheduler_deadline().unwrap().is_none() && runtime.task_executors.is_empty()
        );
    }

    #[test]
    fn completed_or_cancelled_task_capacity_is_reusable_without_handle_aliases() {
        let definition = Definition::compile("local p=require('prek');local old;for i=1,65 do old=p.scheduler.spawn(function()error('cancelled')end);old:cancel()end;p.scheduler.spawn(function()assert(old:status()=='cancelled');p.log.info('replacement')end);return {version=1}", "reused-tasks.lua").unwrap();
        let mut runtime = definition.instantiate().unwrap();
        runtime.set_clock(time(0)).unwrap();
        assert_eq!(
            runtime.tick(time(1), &serde_json::Value::Null).unwrap()[0].message,
            "replacement"
        );
    }

    #[test]
    fn reused_sleeping_slots_never_resume_or_cancel_the_old_identity() {
        let mut runtime = Runtime::load(r#"
local p=require('prek')
local old=p.scheduler.spawn(function()p.log.info('old');p.scheduler.sleep('1s');error('old resumed')end)
p.events.on('matched',function()
 old:cancel()
 p.scheduler.spawn(function()p.log.info('new');p.scheduler.sleep('10ms');assert(old:status()=='cancelled');p.log.info('done')end)
end)
return {version=1}
"#, "reuse-sleep.lua").unwrap();
        runtime.set_clock(time(0)).unwrap();
        assert_eq!(
            runtime.tick(time(1), &serde_json::Value::Null).unwrap()[0].message,
            "old"
        );
        runtime
            .dispatch(EventKind::Matched, &serde_json::Value::Null)
            .unwrap();
        assert_eq!(runtime.scheduler_deadline().unwrap(), Some(time(2)));
        assert!(runtime.task_executors.is_empty());
        assert_eq!(
            runtime.tick(time(2), &serde_json::Value::Null).unwrap()[0].message,
            "new"
        );
        assert_eq!(
            runtime.tick(time(100), &serde_json::Value::Null).unwrap()[0].message,
            "done"
        );
        assert!(
            runtime
                .tick(time(2000), &serde_json::Value::Null)
                .unwrap()
                .is_empty()
        );
    }
}
