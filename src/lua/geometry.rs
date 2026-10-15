//! Owned compositor geometry views and conversions fenced to one snapshot.
use super::*;
use crate::{
    context::{OutputState, Region, Snapshot},
    gestures::Point,
    protocol::Fence,
};
use std::{cell::RefCell, rc::Rc};

fn name(value: Value<'_>) -> Result<String, io::Error> {
    let Value::String(value) = value else {
        return Err(io::Error::other("geometry names require strings"));
    };
    let text = value.to_str().map_err(io::Error::other)?;
    if text.is_empty() || text.len() > 256 {
        return Err(io::Error::other("geometry names require 1–256 bytes"));
    }
    Ok(text.into())
}
fn point<'gc>(ctx: luna::Context<'gc>, table: Table<'gc>) -> Result<Point, luna::Error<'gc>> {
    let number = |value| match value {
        Value::Integer(value) => Ok(value as f64),
        Value::Number(value) => Ok(value),
        _ => Err(io::Error::other("coordinates require numeric values")),
    };
    let point = Point {
        x: number(table.get(ctx, "x")?)?,
        y: number(table.get(ctx, "y")?)?,
    };
    if !point.x.is_finite() || !point.y.is_finite() {
        return Err(io::Error::other("coordinates must be finite numbers").into());
    }
    Ok(point)
}
#[derive(Clone, Copy)]
enum Space {
    Normalized,
    Local,
    Global,
}
fn space(value: luna::String<'_>) -> Result<Space, io::Error> {
    match value.to_str().map_err(io::Error::other)? {
        "normalized" => Ok(Space::Normalized),
        "local" => Ok(Space::Local),
        "global" => Ok(Space::Global),
        _ => Err(io::Error::other(
            "coordinate space must be normalized, local or global",
        )),
    }
}
fn output_view<'gc>(
    ctx: luna::Context<'gc>,
    fence: &Fence,
    revision: u64,
    output: &OutputState,
) -> Result<Value<'gc>, luna::Error<'gc>> {
    let table = Table::new(&ctx);
    let reference = Table::new(&ctx);
    reference.set(ctx, "fence", values::context_fence(ctx, fence)?)?;
    reference.set(ctx, "revision", revision.to_string())?;
    reference.set(ctx, "name", output.geometry.name.as_str())?;
    reference.set(
        ctx,
        "geometry_generation",
        output.geometry_generation.to_string(),
    )?;
    let Value::Table(state) = luna_util::serde::to_value(ctx, output).map_err(io::Error::other)?
    else {
        unreachable!()
    };
    state.set(
        ctx,
        "geometry_generation",
        output.geometry_generation.to_string(),
    )?;
    table.set(ctx, "reference", reference)?;
    table.set(ctx, "state", state)?;
    Ok(Value::Table(table))
}
fn region_view<'gc>(
    ctx: luna::Context<'gc>,
    fence: &Fence,
    revision: u64,
    region: &Region,
) -> Result<Value<'gc>, luna::Error<'gc>> {
    let table = Table::new(&ctx);
    table.set(ctx, "fence", values::context_fence(ctx, fence)?)?;
    table.set(ctx, "revision", revision.to_string())?;
    table.set(
        ctx,
        "region",
        luna_util::serde::to_value(ctx, region).map_err(io::Error::other)?,
    )?;
    Ok(Value::Table(table))
}
struct Reference {
    fence: Fence,
    revision: u64,
    generation: u64,
    name: String,
}
fn reference<'gc>(
    ctx: luna::Context<'gc>,
    table: Table<'gc>,
) -> Result<Reference, luna::Error<'gc>> {
    let fence: Table = table.get(ctx, "fence")?;
    let broker: Table = fence.get(ctx, "broker")?;
    Ok(Reference {
        fence: Fence {
            broker: crate::protocol::Epoch([
                values::canonical_identity(broker.get(ctx, 1)?)?,
                values::canonical_identity(broker.get(ctx, 2)?)?,
            ]),
            configuration: values::canonical_identity(fence.get(ctx, "configuration")?)?,
            session: values::canonical_identity(fence.get(ctx, "session")?)?,
        },
        revision: values::canonical_identity(table.get(ctx, "revision")?)?,
        generation: values::canonical_identity(table.get(ctx, "geometry_generation")?)?,
        name: name(table.get(ctx, "name")?)?,
    })
}
fn convert(point: Point, output: &OutputState, from: Space, to: Space) -> Result<Point, io::Error> {
    let origin = output.geometry.origin;
    let size = output.geometry.logical_size();
    let local = match from {
        Space::Normalized => Point {
            x: point.x * size.x,
            y: point.y * size.y,
        },
        Space::Local => point,
        Space::Global => Point {
            x: point.x - origin.x,
            y: point.y - origin.y,
        },
    };
    let result = match to {
        Space::Normalized => Point {
            x: local.x / size.x,
            y: local.y / size.y,
        },
        Space::Local => local,
        Space::Global => Point {
            x: local.x + origin.x,
            y: local.y + origin.y,
        },
    };
    if !local.x.is_finite()
        || !local.y.is_finite()
        || !result.x.is_finite()
        || !result.y.is_finite()
    {
        return Err(io::Error::other("coordinate conversion overflowed"));
    }
    Ok(result)
}
pub(super) fn register<'gc>(
    ctx: &luna::Context<'gc>,
    module: Table<'gc>,
    host: Rc<RefCell<runtime::Host>>,
) -> Result<(), luna::Error<'gc>> {
    for (namespace, fields) in [
        ("outputs", &["list", "get"][..]),
        ("regions", &["list", "get", "at"][..]),
        ("coordinates", &["convert"][..]),
    ] {
        let table: Table = module.get(*ctx, namespace)?;
        for &field in fields {
            let host = host.clone();
            table.set(
                *ctx,
                field,
                Callback::from_fn(ctx, move |ctx, _, mut stack| {
                    let host = host.borrow();
                    if host.configuring {
                        return Err(io::Error::other(
                            "geometry queries require runtime evaluation",
                        )
                        .into());
                    }
                    let requested = if field == "get" {
                        Some(name(stack.from_front(ctx)?)?)
                    } else {
                        None
                    };
                    let location = if field == "at" {
                        Some(point(ctx, stack.from_front(ctx)?)?)
                    } else {
                        None
                    };
                    let conversion = if field == "convert" {
                        Some((
                            reference(ctx, stack.from_front(ctx)?)?,
                            point(ctx, stack.from_front(ctx)?)?,
                            space(stack.from_front(ctx)?)?,
                            space(stack.from_front(ctx)?)?,
                        ))
                    } else {
                        None
                    };
                    let Some(view) = context::desktop(&host) else {
                        stack.replace(ctx, Value::Nil);
                        return Ok(CallbackReturn::Return);
                    };
                    let snapshot: &Snapshot = &view.context.as_ref().unwrap().snapshot;
                    let mut value = match (namespace, field) {
                        ("outputs", "get") => snapshot
                            .outputs
                            .iter()
                            .find(|output| Some(&output.geometry.name) == requested.as_ref())
                            .map(|output| output_view(ctx, &view.fence, snapshot.revision, output))
                            .transpose()?
                            .unwrap_or(Value::Nil),
                        ("outputs", "list") => {
                            let list = Table::new(&ctx);
                            for (index, output) in snapshot.outputs.iter().enumerate() {
                                list.set(
                                    ctx,
                                    index + 1,
                                    output_view(ctx, &view.fence, snapshot.revision, output)?,
                                )?;
                            }
                            Value::Table(list)
                        }
                        ("regions", "get") => snapshot
                            .regions
                            .iter()
                            .find(|region| Some(&region.id) == requested.as_ref())
                            .map(|region| region_view(ctx, &view.fence, snapshot.revision, region))
                            .transpose()?
                            .unwrap_or(Value::Nil),
                        ("regions", "list" | "at") => {
                            let list = Table::new(&ctx);
                            for (index, region) in snapshot
                                .regions
                                .iter()
                                .filter(|region| location.is_none_or(|p| region.rect.contains(p)))
                                .enumerate()
                            {
                                list.set(
                                    ctx,
                                    index + 1,
                                    region_view(ctx, &view.fence, snapshot.revision, region)?,
                                )?;
                            }
                            Value::Table(list)
                        }
                        ("coordinates", "convert") => {
                            let (reference, point, from, to) = conversion.unwrap();
                            snapshot
                                .outputs
                                .iter()
                                .find(|output| {
                                    reference.fence == view.fence
                                        && reference.revision == snapshot.revision
                                        && reference.name == output.geometry.name
                                        && reference.generation == output.geometry_generation
                                })
                                .map(|output| {
                                    convert(point, output, from, to).and_then(|point| {
                                        luna_util::serde::to_value(ctx, &point)
                                            .map_err(io::Error::other)
                                    })
                                })
                                .transpose()?
                                .unwrap_or(Value::Nil)
                        }
                        _ => unreachable!(),
                    };
                    if context::desktop(&host).is_none() {
                        value = Value::Nil;
                    }
                    stack.replace(ctx, value);
                    Ok(CallbackReturn::Return)
                }),
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{context_transfer::tests::view, gestures::Transform};

    fn runtime(body: &str) -> Runtime {
        Definition::compile(&format!("local p=require('prek'); local saved; p.events.on('matched', function(event) {body} end); return {{version=1}}"), "geometry.lua")
            .unwrap().instantiate().unwrap()
    }
    fn dispatch(runtime: &mut Runtime, payload: serde_json::Value) {
        runtime.dispatch(EventKind::Matched, &payload).unwrap();
    }
    #[test]
    fn runtime_only_and_unavailable_geometry_is_distinct_from_empty() {
        for call in [
            "outputs.list()",
            "outputs.get('x')",
            "regions.list()",
            "regions.get('x')",
            "regions.at({x=0,y=0})",
            "coordinates.convert({}, {x=0,y=0}, 'local', 'global')",
        ] {
            assert!(
                Definition::compile(
                    &format!("local p=require('prek'); p.{call}; return {{version=1}}"),
                    "source.lua"
                )
                .unwrap_err()
                .0
                .contains("runtime evaluation")
            );
        }
        let mut runtime = runtime(
            r#"
if event.empty then
  assert(#p.outputs.list()==0 and #p.regions.list()==0 and #p.regions.at({x=0,y=0})==0)
else
  assert(p.outputs.list()==nil and p.outputs.get('missing')==nil)
  assert(p.regions.list()==nil and p.regions.get('missing')==nil and p.regions.at({x=0,y=0})==nil)
end
"#,
        );
        dispatch(&mut runtime, serde_json::json!({}));
        let mut view = view(Duration::ZERO);
        let snapshot = &mut view.context.as_mut().unwrap().snapshot;
        snapshot.outputs.clear();
        snapshot.windows.clear();
        snapshot.regions.clear();
        runtime.set_context(view, Duration::ZERO).unwrap();
        dispatch(&mut runtime, serde_json::json!({"empty":true}));
        runtime
            .set_clock(crate::context_transfer::FRESHNESS)
            .unwrap();
        dispatch(&mut runtime, serde_json::json!({}));
    }
    #[test]
    fn copies_use_exact_identities_and_conversion_uses_host_rotation_scale_and_origin() {
        let mut runtime = runtime(
            r#"
local output=p.outputs.get('fixture-output'); assert(output and #p.outputs.list()==1)
assert(p.outputs.get('absent')==nil and p.regions.get('absent')==nil)
assert(output.reference.revision=='18446744073709551615')
assert(output.reference.geometry_generation=='9223372036854775808')
assert(output.reference.fence.broker[1]=='18446744073709551615')
output.state.geometry.origin.x=999; output.state.geometry.scale=999
assert(p.outputs.list()[1].state.geometry.origin.x==-100)
local global=p.coordinates.convert(output.reference,{x=0.25,y=0.5},'normalized','global')
assert(global.x==0 and global.y==300)
local local_point=p.coordinates.convert(output.reference,global,'global','local')
assert(local_point.x==100 and local_point.y==250)
local normalized=p.coordinates.convert(output.reference,local_point,'local','normalized')
assert(normalized.x==0.25 and normalized.y==0.5)
local outside=p.coordinates.convert(output.reference,{x=-1,y=2},'normalized','local')
assert(outside.x==-400 and outside.y==1000)
local region=p.regions.get('widget-1'); region.region.id='forged'
assert(p.regions.list()[1].region.id=='widget-1')
assert(region.revision==output.reference.revision and region.fence.session==output.reference.fence.session)
"#,
        );
        let mut view = view(Duration::ZERO);
        view.fence.broker.0[0] = u64::MAX;
        let snapshot = &mut view.context.as_mut().unwrap().snapshot;
        snapshot.revision = u64::MAX;
        let output = &mut snapshot.outputs[0];
        output.geometry_generation = 1 << 63;
        output.geometry.origin = Point { x: -100.0, y: 50.0 };
        output.geometry.scale = 2.0;
        output.geometry.transform = Transform::Rotate90;
        // Keep provider-reported regions/keyboard inside the transformed output.
        output.keyboard = None;
        snapshot.regions[0].rect.origin.y = 50.0;
        runtime.set_context(view, Duration::ZERO).unwrap();
        dispatch(&mut runtime, serde_json::json!({}));
    }
    #[test]
    fn region_hits_are_half_open_preserve_overlaps_and_never_infer_geometry() {
        let mut runtime = runtime(
            r#"
local hits=p.regions.at({x=1,y=20}); assert(#hits==2)
assert(hits[1].region.id=='widget-1' and hits[2].region.id=='overlap')
assert(hits[1].region.interactive and not hits[2].region.interactive)
assert(#p.regions.at({x=0.1,y=10})==2)
assert(#p.regions.at({x=100.1,y=20})==0)
assert(#p.regions.at({x=1,y=130})==0)
assert(#p.regions.at({x=-1,y=20})==0)
"#,
        );
        let mut view = view(Duration::ZERO);
        let snapshot = &mut view.context.as_mut().unwrap().snapshot;
        let mut overlap = snapshot.regions[0].clone();
        overlap.id = "overlap".into();
        overlap.interactive = false;
        snapshot.regions.push(overlap);
        runtime.set_context(view, Duration::ZERO).unwrap();
        dispatch(&mut runtime, serde_json::json!({}));
    }
    #[test]
    fn references_retire_on_revision_fence_generation_and_output_removal() {
        for change in ["revision", "fence", "generation", "removed"] {
            let mut runtime = runtime(
                r#"
if event.save then saved=p.outputs.list()[1].reference
else assert(p.coordinates.convert(saved,{x=1,y=1},'local','global')==nil) end
"#,
            );
            let mut view = view(Duration::ZERO);
            runtime.set_context(view.clone(), Duration::ZERO).unwrap();
            dispatch(&mut runtime, serde_json::json!({"save":true}));
            match change {
                "revision" => view.context.as_mut().unwrap().snapshot.revision += 1,
                "fence" => view.fence.configuration += 1,
                "generation" => {
                    view.context.as_mut().unwrap().snapshot.outputs[0].geometry_generation += 1
                }
                "removed" => {
                    let snapshot = &mut view.context.as_mut().unwrap().snapshot;
                    snapshot.outputs.clear();
                    snapshot.windows.clear();
                    snapshot.regions.clear();
                }
                _ => unreachable!(),
            }
            runtime.set_context(view, Duration::ZERO).unwrap();
            dispatch(&mut runtime, serde_json::json!({}));
        }
    }
    #[test]
    fn inhibited_and_expired_context_cannot_supply_geometry_or_conversions() {
        for change in ["locked", "secure", "binding", "expired", "missing"] {
            let mut runtime = runtime(
                r#"
if event.save then saved=p.outputs.list()[1].reference
else
  assert(p.outputs.list()==nil and p.regions.list()==nil)
  assert(p.coordinates.convert(saved,{x=1,y=1},'local','global')==nil)
end
"#,
            );
            let mut view = view(Duration::ZERO);
            runtime.set_context(view.clone(), Duration::ZERO).unwrap();
            dispatch(&mut runtime, serde_json::json!({"save":true}));
            match change {
                "locked" => {
                    view.state = crate::context::State::Locked;
                    view.context.as_mut().unwrap().snapshot.state = view.state;
                }
                "secure" => view.context.as_mut().unwrap().snapshot.secure_overlay = true,
                "binding" => {
                    view.context.as_mut().unwrap().binding.kind =
                        crate::context::SessionKind::Greeter
                }
                "expired" => {}
                "missing" => view.context = None,
                _ => unreachable!(),
            }
            runtime.set_context(view, Duration::ZERO).unwrap();
            if change == "expired" {
                runtime
                    .set_clock(crate::context_transfer::FRESHNESS)
                    .unwrap();
            }
            dispatch(&mut runtime, serde_json::json!({}));
        }
    }
    #[test]
    fn malformed_arguments_and_arithmetic_overflow_are_errors_without_corrupting_the_view() {
        let mut runtime = runtime(
            r#"
local output=p.outputs.list()[1]; local ref=output.reference
assert(not pcall(p.outputs.get,'')); assert(not pcall(p.regions.get,12))
assert(not pcall(p.regions.at,{x=0/0,y=0}))
assert(not pcall(p.regions.at,{x='1',y=0}))
assert(not pcall(p.coordinates.convert,ref,{x=false,y=0},'local','global'))
assert(not pcall(p.coordinates.convert,ref,{x=math.huge,y=0},'local','global'))
assert(not pcall(p.coordinates.convert,ref,{x=1e308,y=0},'normalized','local'))
assert(not pcall(p.coordinates.convert,ref,{x=0,y=0},'pixels','global'))
for _,bad in ipairs({'02','-1','18446744073709551616',2}) do
  local old=ref.geometry_generation; ref.geometry_generation=bad
  assert(not pcall(p.coordinates.convert,ref,{x=0,y=0},'local','global'))
  ref.geometry_generation=old
end
ref.fence.broker[1]='0'
assert(p.coordinates.convert(ref,{x=1,y=1},'local','global')==nil)
assert(p.coordinates.convert(p.outputs.list()[1].reference,{x=1,y=1},'local','global').x==1)
"#,
        );
        runtime
            .set_context(view(Duration::ZERO), Duration::ZERO)
            .unwrap();
        dispatch(&mut runtime, serde_json::json!({}));
    }
}
