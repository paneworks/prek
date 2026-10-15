//! Read-only host context. Lua receives owned copies and no authority setter.
use super::*;
use std::{cell::RefCell, rc::Rc};

pub(super) fn getter<'gc>(
    ctx: &luna::Context<'gc>,
    host: Rc<RefCell<runtime::Host>>,
    field: &'static str,
) -> Callback<'gc> {
    Callback::from_fn(ctx, move |ctx, _, mut stack| {
        let host = host.borrow();
        if host.configuring {
            return Err(io::Error::other("context queries require runtime evaluation").into());
        }
        let clock = || {
            host.now.map(|now| {
                now.checked_add(
                    host.context_clock
                        .map_or(Duration::ZERO, |clock| clock.elapsed()),
                )
                .unwrap_or(Duration::MAX)
            })
        };
        let fresh = |at| {
            clock().is_some_and(|now| now >= at && now - at < crate::context_transfer::FRESHNESS)
        };
        let current = host.context.as_ref().filter(|view| {
            clock().is_some_and(|now| {
                now >= view.sampled_at && now - view.sampled_at < crate::context_transfer::FRESHNESS
            })
        });
        let provided = current
            .and_then(|view| view.context.as_ref())
            .filter(|context| {
                clock().is_some_and(|now| {
                    now >= context.snapshot.captured_at
                        && now - context.snapshot.captured_at < crate::context_transfer::FRESHNESS
                })
            });
        let mut value = match field {
            "snapshot" => provided
                .map(|context| values::snapshot(ctx, &context.snapshot))
                .transpose()?
                .unwrap_or(Value::Nil),
            "binding" => provided
                .map(|context| values::binding(ctx, &context.binding))
                .transpose()?
                .unwrap_or(Value::Nil),
            "state" => luna_util::serde::to_value(
                ctx,
                &current.map_or(crate::context::State::Unknown, |view| view.state),
            )
            .map_err(io::Error::other)?,
            "fence" => current
                .map(|view| values::context_fence(ctx, &view.fence))
                .transpose()?
                .unwrap_or(Value::Nil),
            "error" => current
                .and_then(|view| view.error.as_ref())
                .map(|error| Value::String(ctx.intern(error.as_bytes())))
                .unwrap_or(Value::Nil),
            _ => return Err(io::Error::other("unknown context query").into()),
        };
        // Serialization may take time; never return a copy that expired while
        // constructing it, or freeze freshness at callback entry.
        if current.is_none_or(|view| !fresh(view.sampled_at)) {
            value = if field == "state" {
                Value::String(ctx.intern(b"unknown"))
            } else {
                Value::Nil
            };
        } else if matches!(field, "snapshot" | "binding")
            && provided.is_none_or(|context| !fresh(context.snapshot.captured_at))
        {
            value = Value::Nil;
        }
        stack.replace(ctx, value);
        Ok(CallbackReturn::Return)
    })
}
impl Runtime {
    pub fn set_context(
        &mut self,
        view: crate::context_transfer::View,
        now: Duration,
    ) -> Result<(), ConfigError> {
        view.validate().map_err(error)?;
        if view.sampled_at > now
            || view
                .context
                .as_ref()
                .is_some_and(|context| context.snapshot.captured_at > now)
        {
            return Err(error("context clock is from the future"));
        }
        self.set_clock(now)?;
        let mut host = self.host.borrow_mut();
        host.devices.context(&view);
        host.context = Some(view);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        context_transfer::tests::view,
        lua::{Definition, EventKind},
    };

    #[test]
    fn context_queries_are_runtime_only() {
        for field in ["snapshot", "binding", "state", "fence", "error"] {
            let source =
                format!("local p=require('prek'); p.context.{field}(); return {{version=1}}");
            assert!(
                Definition::compile(&source, "context.lua")
                    .unwrap_err()
                    .0
                    .contains("runtime evaluation")
            );
        }
    }

    #[test]
    fn context_identities_preserve_all_unsigned_bits() {
        let definition = Definition::compile(
            r#"
local p=require('prek')
p.events.on('context_changed',function(event)
  local fence=p.context.fence(); local snapshot=p.context.snapshot(); local binding=p.context.binding()
  assert(fence.broker[1]=='18446744073709551615' and fence.broker[2]=='9223372036854775808')
  assert(fence.configuration=='18446744073709551614' and fence.session=='18446744073709551615')
  assert(snapshot.generation==fence.session and snapshot.revision=='18446744073709551614')
  assert(event.revision==snapshot.revision)
  assert(snapshot.outputs[1].geometry_generation=='18446744073709551615')
  assert(binding.compositor_start=='18446744073709551615' and type(binding.uid)=='number')
  assert(snapshot.windows[1].rect.origin.x==0.1 and snapshot.outputs[1].active_workspace==3)
  fence.broker[1]='forged'; snapshot.revision='forged'; binding.compositor_start='forged'
  assert(p.context.fence().broker[1]=='18446744073709551615')
  assert(p.context.snapshot().revision=='18446744073709551614')
  assert(p.context.binding().compositor_start=='18446744073709551615')
  p.log.info('exact-identities')
end)
return {version=1}
"#,
            "exact-context.lua",
        ).unwrap();
        let mut view = view(Duration::ZERO);
        view.fence.broker = crate::protocol::Epoch([u64::MAX, 1 << 63]);
        view.fence.configuration = u64::MAX - 1;
        view.fence.session = u64::MAX;
        let context = view.context.as_mut().unwrap();
        context.snapshot.generation = u64::MAX;
        context.snapshot.revision = u64::MAX - 1;
        context.snapshot.outputs[0].geometry_generation = u64::MAX;
        context.binding.compositor_start = u64::MAX;
        let mut runtime = definition.instantiate().unwrap();
        runtime.set_context(view, Duration::ZERO).unwrap();
        assert_eq!(
            runtime
                .dispatch(
                    EventKind::ContextChanged,
                    &serde_json::json!({"revision":u64::MAX-1})
                )
                .unwrap()[0]
                .message,
            "exact-identities"
        );
    }

    #[test]
    fn context_queries_return_full_owned_copies_and_expire_at_the_original_timestamp() {
        let definition = Definition::compile(
            r#"
local p=require('prek')
p.events.on('matched',function(event)
  if event.expired then
    assert(p.context.snapshot()==nil and p.context.binding()==nil)
    assert(p.context.state()=='unknown' and p.context.fence()==nil and p.context.error()==nil)
    p.log.info('expired')
  else
    local snapshot=p.context.snapshot()
    assert(snapshot.revision=='7' and #snapshot.windows==1 and #snapshot.outputs==1)
    assert(snapshot.outputs[1].active_workspace==3)
    assert(snapshot.outputs[1].keyboard.layout=='us' and snapshot.regions[1].id=='widget-1')
    assert(snapshot.windows[1].rect.origin.x==0.1)
    snapshot.windows[1].id='forged'; snapshot.revision=999
    local binding=p.context.binding(); binding.uid=0
    local fence=p.context.fence(); fence.configuration=999
    local again=p.context.snapshot()
    assert(again.windows[1].id=='window-1' and again.revision=='7')
    assert(p.context.binding().uid==1000 and p.context.fence().configuration=='1')
    assert(p.context.state()=='desktop' and p.context.error()==nil)
    p.log.info('fresh')
  end
end)
return {version=1}
"#,
            "context.lua",
        )
        .unwrap();
        let mut runtime = definition.instantiate().unwrap();
        runtime
            .set_context(view(Duration::ZERO), Duration::from_millis(200))
            .unwrap();
        assert_eq!(
            runtime
                .dispatch(EventKind::Matched, &serde_json::json!({"expired":false}))
                .unwrap()[0]
                .message,
            "fresh"
        );
        runtime.set_clock(Duration::from_millis(250)).unwrap();
        assert_eq!(
            runtime
                .dispatch(EventKind::Matched, &serde_json::json!({"expired":true}))
                .unwrap()[0]
                .message,
            "expired"
        );
        // Receiving the same old snapshot again cannot extend its freshness.
        runtime
            .set_context(view(Duration::ZERO), Duration::from_millis(251))
            .unwrap();
        assert_eq!(
            runtime
                .dispatch(EventKind::Matched, &serde_json::json!({"expired":true}))
                .unwrap()[0]
                .message,
            "expired"
        );
        assert!(
            runtime
                .set_context(view(Duration::from_secs(1)), Duration::from_millis(300))
                .is_err()
        );
        runtime
            .set_context(view(Duration::from_millis(300)), Duration::from_millis(300))
            .unwrap();
        runtime.host.borrow_mut().context_clock = Some(Instant::now() - Duration::from_millis(251));
        assert_eq!(
            runtime
                .dispatch(EventKind::Matched, &serde_json::json!({"expired":true}))
                .unwrap()[0]
                .message,
            "expired"
        );
    }
}
