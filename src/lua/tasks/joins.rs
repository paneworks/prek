use super::*;
use std::collections::BTreeSet;

pub(in crate::lua) fn wait<'gc>(
    ctx: &Context<'gc>,
    host: Rc<RefCell<runtime::Host>>,
) -> Callback<'gc> {
    Callback::from_fn(ctx, move |ctx, _, mut stack| {
        let target: Table = stack.consume(ctx)?;
        let id: u64 = target.get(ctx, "id")?;
        let active: bool = target.get(ctx, "active")?;
        let mut state = host.borrow_mut();
        let current = state
            .task
            .ok_or_else(|| io::Error::other("Task.join requires an active scheduler task"))?;
        if id == current {
            return Err(io::Error::other("a task cannot join itself").into());
        }
        if !active {
            stack.replace(ctx, ());
            return Ok(CallbackReturn::Return);
        }
        if state.sleep.is_some() || state.waiting.is_some() {
            return Err(io::Error::other("multiple task waits in one resume").into());
        }
        let mut cursor = id;
        let mut seen = BTreeSet::new();
        while let Some(next) = state.task_joins.get(&cursor) {
            if cursor == current || !seen.insert(cursor) || seen.len() >= MAX_TASKS {
                return Err(io::Error::other("task join dependency cycle or limit").into());
            }
            cursor = *next;
        }
        if cursor == current {
            return Err(io::Error::other("task join dependency cycle").into());
        }
        state.task_joins.insert(current, id);
        state.waiting = Some(ctx.stash(target));
        stack.replace(ctx, ());
        Ok(CallbackReturn::Yield {
            to_thread: None,
            then: None,
        })
    })
}

impl Runtime {
    pub(super) fn wake_joiners(&mut self) -> Result<(), ConfigError> {
        let Some(now) = self.host.borrow().now else {
            return Ok(());
        };
        let due = nanos(now + Duration::from_millis(1)).map_err(error)?;
        let mut ready = Vec::new();
        let executors = &mut self.task_executors;
        self.lua
            .try_enter(|ctx| {
                for (id, task) in executors {
                    if let Some(target) = &task.waiting
                        && !ctx.fetch(target).get::<_, bool>(ctx, "active")?
                    {
                        ctx.fetch(&task.entry).set(ctx, "due", due)?;
                        task.waiting = None;
                        ready.push(*id);
                    }
                }
                Ok(())
            })
            .map_err(error)?;
        let mut host = self.host.borrow_mut();
        for id in ready {
            host.task_joins.remove(&id);
        }
        Ok(())
    }
}
