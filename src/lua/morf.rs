//! Broker-verified native connection metadata; all values are owned copies.
use super::*;
use std::{cell::RefCell, rc::Rc};

pub(super) fn getter<'gc>(
    ctx: &luna::Context<'gc>,
    host: Rc<RefCell<runtime::Host>>,
) -> Callback<'gc> {
    Callback::from_fn(ctx, move |ctx, _, mut stack| {
        let host = host.borrow();
        if host.configuring || !host.running {
            return Err(
                io::Error::other("Morf provider queries require a runtime callback").into(),
            );
        }
        let mut value = Value::Nil;
        if let Some(view) = context::desktop(&host) {
            let providers = Table::new(&ctx);
            for (index, provider) in view.morf.iter().enumerate() {
                let mut json = serde_json::to_value(provider).map_err(io::Error::other)?;
                json["compositor_start"] =
                    serde_json::Value::String(provider.compositor_start.to_string());
                let Value::Table(table) =
                    luna_util::serde::to_value(ctx, &json).map_err(io::Error::other)?
                else {
                    unreachable!()
                };
                table.set(ctx, "fence", values::context_fence(ctx, &view.fence)?)?;
                providers.set(ctx, index as i64 + 1, table)?;
            }
            if context::desktop(&host).is_some() {
                value = Value::Table(providers);
            }
        }
        stack.replace(ctx, value);
        Ok(CallbackReturn::Return)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        context_transfer::tests::view,
        lua::{Definition, EventKind},
        morf::{ShellPhase, ShellState},
    };
    #[test]
    fn native_morf_views_are_fresh_fenced_lossless_and_cannot_modify_host_state() {
        let definition = Definition::compile(
            r#"
local p=require('prek')
p.events.on('ready',function()
  local list=p.morf.providers()
  if list==nil then p.log.info('unavailable');return end
  if #list==0 then p.log.info('empty');return end
  assert(list[1].compositor_start=='18446744073709551615')
  assert(list[1].phase=='configured' and list[1].output=='fixture-output')
  assert(type(list[1].fence.session)=='string')
  list[1].output='forged'
  assert(p.morf.providers()[1].output=='fixture-output')
  p.log.info('verified')
end)
return {version=1}
"#,
            "morf.lua",
        )
        .unwrap();
        let mut runtime = definition.instantiate().unwrap();
        assert_eq!(
            runtime
                .dispatch(EventKind::Ready, &serde_json::json!({}))
                .unwrap()[0]
                .message,
            "unavailable"
        );
        let at = Duration::from_secs(1);
        let mut current = view(at);
        current.context.as_mut().unwrap().binding.compositor_start = u64::MAX;
        runtime.set_context(current.clone(), at).unwrap();
        assert_eq!(
            runtime
                .dispatch(EventKind::Ready, &serde_json::json!({}))
                .unwrap()[0]
                .message,
            "empty"
        );
        current.morf.push(ShellState {
            version: 1,
            role: "shell".into(),
            epoch: "a".repeat(32),
            revision: 2,
            phase: ShellPhase::Configured,
            compositor_pid: 123,
            compositor_start: u64::MAX,
            compositor_uid: 1000,
            output: "fixture-output".into(),
        });
        runtime.set_context(current, at).unwrap();
        assert_eq!(
            runtime
                .dispatch(EventKind::Ready, &serde_json::json!({}))
                .unwrap()[0]
                .message,
            "verified"
        );
        runtime.set_clock(at + Duration::from_millis(250)).unwrap();
        assert_eq!(
            runtime
                .dispatch(EventKind::Ready, &serde_json::json!({}))
                .unwrap()[0]
                .message,
            "unavailable"
        );
        assert!(
            Definition::compile(
                "local p=require('prek');p.morf.providers();return {version=1}",
                "morf.lua"
            )
            .is_err()
        );
    }
}
