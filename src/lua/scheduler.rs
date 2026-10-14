//! Timers share their VM's generation and callback budgets. Periodic timers
//! preserve their monotonic phase, skip missed ticks and never catch up in a burst.
use super::*;
use luna::{Context, Function};
use serde::{Deserialize, Serialize};
use std::{cell::RefCell, rc::Rc};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TimerRegistration {
    pub id: u16,
    pub delay_ms: u64,
    pub periodic: bool,
    pub active: bool,
}
pub(super) fn duration<'gc>(ctx: &Context<'gc>, host: Rc<RefCell<runtime::Host>>) -> Callback<'gc> {
    Callback::from_fn(ctx, move |ctx, _, mut stack| {
        let text: luna::String = stack.consume(ctx)?;
        let text = text.to_str().map_err(io::Error::other)?;
        if text.len() > 64 {
            return Err(io::Error::other("timer duration exceeds byte limit").into());
        }
        let delay = crate::config::Millis::parse(text).map_err(io::Error::other)?;
        let deadline = host
            .borrow()
            .now
            .map(|now| nanos(now + delay.duration()))
            .transpose()?;
        stack.replace(ctx, (delay.0 as i64, deadline));
        Ok(CallbackReturn::Return)
    })
}
fn nanos(time: Duration) -> io::Result<i64> {
    i64::try_from(time.as_nanos()).map_err(|_| io::Error::other("Lua scheduler time exceeds range"))
}
impl Runtime {
    pub fn timer_registrations(&mut self) -> Result<Vec<TimerRegistration>, ConfigError> {
        self.lua
            .try_enter(|ctx| {
                let timers = ctx.fetch(&self.timers);
                let mut registrations = Vec::new();
                for id in 1..=timers.length(&ctx) {
                    let entry: Table = timers.get(ctx, id)?;
                    let delay: i64 = entry.get(ctx, "delay_ms")?;
                    registrations.push(TimerRegistration {
                        id: id as u16,
                        delay_ms: delay as u64,
                        periodic: entry.get(ctx, "periodic")?,
                        active: entry.get(ctx, "active")?,
                    });
                }
                Ok(registrations)
            })
            .map_err(error)
    }
    /// Activate relative source-time timers, then update the callback clock.
    /// Clock updates never execute user code.
    pub fn set_clock(&mut self, now: Duration) -> Result<(), ConfigError> {
        let timestamp = nanos(now).map_err(error)?;
        if self.host.borrow().now.is_some_and(|old| now < old) {
            return Err(error("Lua scheduler clock moved backwards"));
        }
        self.host.borrow_mut().now = Some(now);
        #[cfg(target_os = "linux")]
        {
            self.host.borrow_mut().context_clock = Some(Instant::now());
        }
        self.lua
            .try_enter(|ctx| {
                let timers = ctx.fetch(&self.timers);
                for id in 1..=timers.length(&ctx) {
                    let entry: Table = timers.get(ctx, id)?;
                    let due: Value = entry.get(ctx, "due")?;
                    if due.is_nil() {
                        let delay: i64 = entry.get(ctx, "delay_ms")?;
                        let due = timestamp
                            .checked_add(delay * 1_000_000)
                            .ok_or_else(|| io::Error::other("Lua timer deadline exceeds range"))?;
                        entry.set(ctx, "due", due)?;
                    }
                }
                Ok(())
            })
            .map_err(error)?;
        self.clock_tasks(now)
    }
    pub fn timer_deadline(&mut self) -> Result<Option<Duration>, ConfigError> {
        self.lua
            .try_enter(|ctx| {
                let timers = ctx.fetch(&self.timers);
                let mut deadline = None;
                for id in 1..=timers.length(&ctx) {
                    let entry: Table = timers.get(ctx, id)?;
                    let active: bool = entry.get(ctx, "active")?;
                    let due: Option<i64> = entry.get(ctx, "due")?;
                    if active && let Some(due) = due {
                        let due = Duration::from_nanos(due as u64);
                        deadline = Some(deadline.map_or(due, |old: Duration| old.min(due)));
                    }
                }
                Ok(deadline)
            })
            .map_err(error)
    }
    pub fn tick(
        &mut self,
        now: Duration,
        context: &serde_json::Value,
    ) -> Result<Vec<LogRecord>, ConfigError> {
        if self.failed {
            return Err(error("Lua runtime was retired after callback failure"));
        }
        runtime::bounded_json(context, 60 * 1024).map_err(error)?;
        self.set_clock(now)?;
        let timestamp = nanos(now).map_err(error)?;
        let mut due = self
            .lua
            .try_enter(|ctx| {
                let timers = ctx.fetch(&self.timers);
                let mut due = Vec::new();
                for id in 1..=timers.length(&ctx) {
                    let entry: Table = timers.get(ctx, id)?;
                    let active: bool = entry.get(ctx, "active")?;
                    let deadline: i64 = entry.get(ctx, "due")?;
                    if active && deadline <= timestamp {
                        due.push((deadline, id));
                    }
                }
                Ok(due)
            })
            .map_err(error)?;
        due.sort_unstable();
        // Bound one wake even when all 256 timers share a deadline.
        due.truncate(32);
        self.host.borrow_mut().running = true;
        let started = Instant::now();
        let result = (|| {
            for (deadline, id) in due {
                let executor=self.lua.try_enter(|ctx| {
                    let entry:Table=ctx.fetch(&self.timers).get(ctx,id)?;
                    let active:bool=entry.get(ctx,"active")?;
                    if !active {return Ok(None);}
                    let delay:i64=entry.get(ctx,"delay_ms")?;
                    let periodic:bool=entry.get(ctx,"periodic")?;
                    let period=delay*1_000_000;
                    if periodic {
                        let skipped=(timestamp-deadline)/period;
                        let advance=(skipped+1).checked_mul(period)
                            .ok_or_else(||io::Error::other("Lua periodic deadline exceeds range"))?;
                        let next=deadline.checked_add(advance)
                            .ok_or_else(||io::Error::other("Lua periodic deadline exceeds range"))?;
                        entry.set(ctx,"due",next)?;
                    } else {entry.set(ctx,"active",false)?;}
                    let callback:Function=entry.get(ctx,"callback")?;
                    let payload=serde_json::json!({"context":context,"timer":{"id":id,"periodic":periodic},"at":now});
                    let value=luna_util::serde::to_value(ctx,&payload).map_err(io::Error::other)?;
                    Ok(Some(ctx.stash(Executor::start(ctx,callback,(value,)))))
                }).map_err(error)?;
                if let Some(executor) = executor {
                    self.invoke_executor(&executor, started)?;
                }
            }
            self.tick_tasks(now, context)?;
            Ok(())
        })();
        let mut host = self.host.borrow_mut();
        host.running = false;
        host.bytes = 0;
        match result {
            Ok(()) => Ok(std::mem::take(&mut host.logs)),
            Err(reason) => {
                self.failed = true;
                host.logs.clear();
                host.rules.discard("Lua scheduler callback failed");
                Err(reason)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn time(milliseconds: u64) -> Duration {
        Duration::from_millis(milliseconds)
    }
    #[test]
    fn delayed_and_periodic_timers_preserve_phase_and_skip_missed_ticks() {
        let mut runtime = Runtime::load(
            r#"
local p=require('prek')
p.scheduler.after('100ms',function()p.log.info('once')end)
p.scheduler.every('100ms',function()p.log.info('periodic')end)
local cancelled=p.scheduler.defer(function()error('cancelled timer ran')end)
cancelled:cancel()
return {version=1}
"#,
            "timers.lua",
        )
        .unwrap();
        assert!(runtime.timer_deadline().unwrap().is_none());
        runtime.set_clock(Duration::ZERO).unwrap();
        assert_eq!(runtime.timer_deadline().unwrap(), Some(time(100)));
        assert!(
            runtime
                .tick(time(99), &serde_json::Value::Null)
                .unwrap()
                .is_empty()
        );
        let logs = runtime.tick(time(100), &serde_json::Value::Null).unwrap();
        assert_eq!(
            logs.iter()
                .map(|log| log.message.as_str())
                .collect::<Vec<_>>(),
            vec!["once", "periodic"]
        );
        let logs = runtime.tick(time(350), &serde_json::Value::Null).unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].message, "periodic");
        assert_eq!(runtime.timer_deadline().unwrap(), Some(time(400)));
    }
    #[test]
    fn cancellation_and_new_timers_have_deterministic_callback_order() {
        let mut runtime = Runtime::load(
            r#"
local p=require('prek')
local second
p.scheduler.after('10ms',function()
 second:cancel()
 p.scheduler.after('5ms',function()p.log.info('new timer')end)
end)
second=p.scheduler.after('10ms',function()error('cancelled timer ran')end)
return {version=1}
"#,
            "order.lua",
        )
        .unwrap();
        runtime.set_clock(Duration::ZERO).unwrap();
        assert!(
            runtime
                .tick(time(10), &serde_json::Value::Null)
                .unwrap()
                .is_empty()
        );
        assert_eq!(runtime.timer_deadline().unwrap(), Some(time(15)));
        assert_eq!(
            runtime.tick(time(15), &serde_json::Value::Null).unwrap()[0].message,
            "new timer"
        );
        assert!(runtime.timer_deadline().unwrap().is_none());
    }
    #[test]
    fn timer_errors_retire_the_vm_and_discard_uncommitted_logs() {
        let mut runtime=Runtime::load("local p=require('prek');p.scheduler.defer(function()p.log.info('uncommitted');while true do end end);return {version=1}","failed.lua").unwrap();
        runtime.set_clock(Duration::ZERO).unwrap();
        assert!(runtime.tick(time(1), &serde_json::Value::Null).is_err());
        assert!(runtime.failed);
        assert!(runtime.host.borrow().logs.is_empty());
    }
    #[test]
    fn snapshot_timer_metadata_and_duration_resource_bounds_are_checked() {
        for duration in ["0ms", "-1ms", "1", "999999m"] {
            assert!(Runtime::load(&format!("require('prek').scheduler.after('{duration}',function()end);return {{version=1}}"),"invalid.lua").is_err());
        }
        assert!(Runtime::load("local p=require('prek');for i=1,257 do p.scheduler.defer(function()end)end;return {version=1}","limit.lua").is_err());
        let mut definition = Definition::compile(
            "require('prek').scheduler.after('10ms',function()end);return {version=1}",
            "snapshot.lua",
        )
        .unwrap();
        definition.instantiate().unwrap();
        definition.timers[0].delay_ms = 11;
        assert!(definition.instantiate().is_err());
    }
}
