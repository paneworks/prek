//! Broker-verified native connection metadata; all values are owned copies.
use super::*;
use std::{cell::RefCell, rc::Rc};

pub(super) fn getter<'gc>(
    ctx: &luna::Context<'gc>,
    host: Rc<RefCell<runtime::Host>>,
    field: &'static str,
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
            if field == "regions" {
                let now = host.now.unwrap()
                    + host
                        .context_clock
                        .map_or(Duration::ZERO, |clock| clock.elapsed());
                let reports = Table::new(&ctx);
                for report in view
                    .morf_rendered
                    .iter()
                    .filter(|report| report.context.fresh(now))
                {
                    let json = serde_json::to_value(report).map_err(io::Error::other)?;
                    let Value::Table(table) =
                        luna_util::serde::to_value(ctx, &json).map_err(io::Error::other)?
                    else {
                        unreachable!()
                    };
                    table.set(ctx, "fence", values::context_fence(ctx, &view.fence)?)?;
                    reports.set(ctx, reports.length(&ctx) + 1, table)?;
                }
                stack.replace(ctx, Value::Table(reports));
                return Ok(CallbackReturn::Return);
            }
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
    fn native_frame_queries_are_copies_and_expire_independently_of_desktop_refresh() {
        let definition = Definition::compile(
            r#"
local p=require('prek')
p.events.on('ready',function()
  local reports=p.morf.regions()
  if reports==nil then p.log.info('unavailable');return end
  if #reports==0 then p.log.info('empty');return end
  local c=reports[1].context
  assert(reports[1].output=='fixture-output' and c.ui_generation=='2' and c.revision=='7')
  assert(c.coverage=='primary_surface_named_bounds' and c.frame.width==1000)
  assert(c.frame.regions[1].id=='drawer-keyboard' and c.frame.regions[1].bounds.x==10.25)
  assert(c.frame.regions[1].node=='18446744073709551615')
  assert(c.frame.regions[1].semantic.role=='keyboard' and c.frame.regions[1].semantic.layout=='dev')
  assert(c.frame.surface.namespace=='shell' and c.frame.surface.local_scale==1)
  assert(c.frame.keyboard.scope=='registered_morf_primary' and c.frame.keyboard.state=='visible')
  assert(c.frame.keyboard.node=='18446744073709551615')
  assert(reports[1].layout.basis=='stock_layer_layout' and reports[1].layout.producer_start=='18446744073709551615')
  assert(reports[1].layout.regions[1].bounds.origin.x==10.25)
  c.frame.regions[1].id='forged'
  c.frame.regions[1].semantic.layout='forged'
  c.frame.keyboard.layout='forged'
  reports[1].layout.regions[1].bounds.origin.x=999
  assert(p.morf.regions()[1].context.frame.regions[1].id=='drawer-keyboard')
  assert(p.morf.regions()[1].context.frame.regions[1].semantic.layout=='dev')
  assert(p.morf.regions()[1].context.frame.keyboard.layout=='dev')
  assert(p.morf.regions()[1].layout.regions[1].bounds.origin.x==10.25)
  p.log.info('frame')
end)
return {version=1}
"#,
            "native-frame.lua",
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
        let binding = &current.context.as_ref().unwrap().binding;
        current.morf.push(ShellState {
            version: 1,
            role: "shell".into(),
            epoch: "a".repeat(32),
            revision: 2,
            phase: ShellPhase::Configured,
            compositor_pid: binding.compositor_pid,
            compositor_start: binding.compositor_start,
            compositor_uid: binding.uid,
            output: "fixture-output".into(),
        });
        current
            .morf_rendered
            .push(crate::morf::rendered::OutputContext {
                layout: None,
                output: "fixture-output".into(),
                context: crate::morf::rendered::tests::sample(at),
            });
        current.morf_rendered[0]
            .context
            .frame
            .as_mut()
            .unwrap()
            .regions[0]
            .semantic = Some(crate::morf::rendered::Semantic {
            role: crate::morf::rendered::SemanticRole::Keyboard,
            layout: Some("dev".into()),
        });
        let report = &mut current.morf_rendered[0];
        let frame = report.context.frame.as_mut().unwrap();
        frame.surface = Some(crate::morf::rendered::SurfaceSpace {
            namespace: "shell".into(),
            width: 1000,
            height: 800,
            local_scale: 1.0,
        });
        frame.keyboard = Some(crate::morf::rendered::KeyboardRegistry {
            scope: crate::morf::rendered::RegistryScope::RegisteredMorfPrimary,
            registered: 1,
            state: crate::morf::rendered::KeyboardState::Visible,
            node: Some(u64::MAX.to_string()),
            layout: Some("dev".into()),
        });
        let output = &current.context.as_ref().unwrap().snapshot.outputs[0];
        let layer = crate::hyprland::layers::Layer {
            output: output.geometry.name.clone(),
            address: "0x123".into(),
            namespace: "shell".into(),
            pid: Some(4321),
            level: 2,
            rect: output.rect(),
        };
        report.layout = crate::morf::rendered::LayoutProjection::from_samples(
            &report.context,
            output,
            std::slice::from_ref(&layer),
            std::slice::from_ref(&layer),
            (4321, u64::MAX),
            at,
        )
        .unwrap();
        runtime.set_context(current.clone(), at).unwrap();
        assert_eq!(
            runtime
                .dispatch(EventKind::Ready, &serde_json::json!({}))
                .unwrap()[0]
                .message,
            "frame"
        );
        let later = at + Duration::from_millis(240);
        current.sampled_at = later;
        current.context.as_mut().unwrap().snapshot.captured_at = later;
        runtime.set_context(current, later).unwrap();
        runtime.set_clock(at + Duration::from_millis(250)).unwrap();
        assert_eq!(
            runtime
                .dispatch(EventKind::Ready, &serde_json::json!({}))
                .unwrap()[0]
                .message,
            "empty"
        );
        assert!(
            Definition::compile(
                "local p=require('prek');p.morf.regions();return {version=1}",
                "frame.lua"
            )
            .is_err()
        );
    }
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
