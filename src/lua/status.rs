//! Broker status carried by the trusted session parent; Lua can only copy it.
use super::*;
use crate::{
    context::State,
    protocol::{Fence, Mode},
};
use serde::{Deserialize, Serialize};
use std::{cell::RefCell, rc::Rc};

pub const FRESHNESS: Duration = Duration::from_millis(250);
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StatusView {
    pub fence: Fence,
    pub mode: Mode,
    pub state: State,
    pub revision: u64,
    pub pending: usize,
    pub last_reload_error: Option<String>,
    pub context_error: Option<String>,
    pub sampled_at: Duration,
}
impl StatusView {
    pub(crate) fn validate(&self) -> io::Result<()> {
        self.fence.validate()?;
        if self.pending > 4096
            || [&self.last_reload_error, &self.context_error]
                .into_iter()
                .flatten()
                .any(|error| error.len() > 4096 || error.chars().any(char::is_control))
        {
            return Err(io::Error::other("invalid broker status metadata"));
        }
        Ok(())
    }
}
fn current(host: &runtime::Host) -> Option<&StatusView> {
    let now = host.now?.checked_add(
        host.context_clock
            .map_or(Duration::ZERO, |clock| clock.elapsed()),
    )?;
    host.status.as_ref().filter(|view| {
        now >= view.sampled_at
            && now - view.sampled_at < FRESHNESS
            && host
                .context
                .as_ref()
                .is_none_or(|context| context.fence == view.fence)
            && host
                .capabilities
                .as_ref()
                .is_none_or(|capabilities| capabilities.fence == view.fence)
    })
}
pub(super) fn getter<'gc>(
    ctx: &luna::Context<'gc>,
    host: Rc<RefCell<runtime::Host>>,
) -> Callback<'gc> {
    Callback::from_fn(ctx, move |ctx, _, mut stack| {
        let host = host.borrow();
        if host.configuring {
            return Err(io::Error::other("status queries require runtime evaluation").into());
        }
        let mut result = if let Some(view) = current(&host) {
            let Value::Table(table) =
                luna_util::serde::to_value(ctx, view).map_err(io::Error::other)?
            else {
                unreachable!()
            };
            table.set(ctx, "fence", values::context_fence(ctx, &view.fence)?)?;
            table.set(
                ctx,
                "revision",
                ctx.intern(view.revision.to_string().as_bytes()),
            )?;
            Value::Table(table)
        } else {
            Value::Nil
        };
        if current(&host).is_none() {
            result = Value::Nil;
        }
        stack.replace(ctx, result);
        Ok(CallbackReturn::Return)
    })
}
impl Runtime {
    pub fn set_status(&mut self, view: StatusView, now: Duration) -> Result<(), ConfigError> {
        view.validate().map_err(error)?;
        {
            let host = self.host.borrow();
            if view.sampled_at > now
                || host
                    .context
                    .as_ref()
                    .is_some_and(|context| context.fence != view.fence)
                || host
                    .capabilities
                    .as_ref()
                    .is_some_and(|capabilities| capabilities.fence != view.fence)
                || host
                    .status
                    .as_ref()
                    .is_some_and(|old| old.fence != view.fence || old.sampled_at > view.sampled_at)
            {
                return Err(error(
                    "broker status differs from the owning fence or clock",
                ));
            }
        }
        self.set_clock(now)?;
        self.host.borrow_mut().status = Some(view);
        Ok(())
    }
    pub(crate) fn clear_status(&mut self) {
        self.host.borrow_mut().status = None;
    }
}

#[cfg(test)]
mod tests;
