//! Lossless Lua representations of host identities. Lua integers are signed;
//! opaque u64 identities always use decimal strings, even for small values.
use super::*;
use luna::Context;
use serde_json::Value as Json;

fn identity<'gc>(ctx: Context<'gc>, value: u64) -> Value<'gc> {
    Value::String(ctx.intern(value.to_string().as_bytes()))
}

pub(super) fn canonical_identity(value: Value<'_>) -> Result<u64, io::Error> {
    let Value::String(value) = value else {
        return Err(io::Error::other("identities require decimal strings"));
    };
    let text = value.to_str().map_err(io::Error::other)?;
    let number = text.parse::<u64>().map_err(io::Error::other)?;
    if number.to_string() != text {
        return Err(io::Error::other(
            "identities require canonical decimal strings",
        ));
    }
    Ok(number)
}

fn fields<'gc>(
    ctx: Context<'gc>,
    table: Table<'gc>,
    json: &Json,
    names: &[&str],
) -> Result<(), luna::Error<'gc>> {
    for name in names {
        if let Some(value) = json.get(name).and_then(Json::as_u64) {
            table.set(ctx, *name, identity(ctx, value))?;
        }
    }
    Ok(())
}

fn fence<'gc>(ctx: Context<'gc>, table: Table<'gc>, json: &Json) -> Result<(), luna::Error<'gc>> {
    fields(ctx, table, json, &["configuration", "session"])?;
    if let Some(words) = json.get("broker").and_then(Json::as_array) {
        let broker: Table = table.get(ctx, "broker")?;
        for (index, word) in words.iter().enumerate() {
            if let Some(word) = word.as_u64() {
                broker.set(ctx, index + 1, identity(ctx, word))?;
            }
        }
    }
    Ok(())
}

/// Project the documented host envelope only. Action bodies, evidence and
/// arbitrary embedding payloads retain their original data types.
pub(super) fn payload<'gc>(ctx: Context<'gc>, json: &Json) -> Result<Value<'gc>, luna::Error<'gc>> {
    let value = luna_util::serde::to_value(ctx, json).map_err(io::Error::other)?;
    if let Value::Table(table) = value {
        if let Some(json) = json.get("fence").filter(|json| json.is_object()) {
            fence(ctx, table.get(ctx, "fence")?, json)?;
        }
        if let Some(context) = json.get("context")
            && let Some(json) = context.get("fence").filter(|json| json.is_object())
        {
            let context: Table = table.get(ctx, "context")?;
            fence(ctx, context.get(ctx, "fence")?, json)?;
        }
        if let Some(event) = json.get("event")
            && matches!(
                event.get("outcome").and_then(Json::as_str),
                Some("matched" | "cancelled" | "skipped")
            )
        {
            let event_table: Table = table.get(ctx, "event")?;
            fields(
                ctx,
                event_table,
                event,
                &["id", "ordinal", "config_generation", "session_generation"],
            )?;
            if let Some(source) = event.get("source").filter(|json| json.is_object()) {
                fields(
                    ctx,
                    event_table.get(ctx, "source")?,
                    source,
                    &["device", "generation"],
                )?;
            }
        }
        if let Some(input) = json.get("input").filter(|input| {
            matches!(
                input.get("kind").and_then(Json::as_str),
                Some("frame" | "reset")
            )
        }) {
            let input_table: Table = table.get(ctx, "input")?;
            fields(ctx, input_table, input, &["sequence"])?;
            if let Some(source) = input.get("source").filter(|source| source.is_object()) {
                fields(
                    ctx,
                    input_table.get(ctx, "source")?,
                    source,
                    &["device", "generation"],
                )?;
            }
            for name in [
                "contacts",
                "ended_contacts",
                "contacts_started",
                "contacts_ended",
            ] {
                if let Some(contacts) = input.get(name).and_then(Json::as_array) {
                    let list: Table = input_table.get(ctx, name)?;
                    for (index, contact) in contacts.iter().enumerate() {
                        let entry: Table = list.get(ctx, index + 1)?;
                        if let Some(id) = contact.get("id") {
                            fields(ctx, entry.get(ctx, "id")?, id, &["generation"])?;
                        } else {
                            fields(ctx, entry, contact, &["generation"])?;
                        }
                    }
                }
            }
            if let Some(metadata) = input.get("metadata").filter(|value| value.is_object()) {
                let info: crate::input_events::DeviceInfo =
                    serde_json::from_value(metadata.clone()).map_err(io::Error::other)?;
                input_table.set(ctx, "metadata", device_info(ctx, &info)?)?;
            }
        }
    }
    Ok(value)
}

pub(super) fn device_info<'gc>(
    ctx: Context<'gc>,
    info: &crate::input_events::DeviceInfo,
) -> Result<Value<'gc>, luna::Error<'gc>> {
    let value = luna_util::serde::to_value(ctx, info).map_err(io::Error::other)?;
    let Value::Table(table) = value else {
        unreachable!()
    };
    let device: Table = table.get(ctx, "device")?;
    let source: Table = device.get(ctx, "source")?;
    source.set(ctx, "device", identity(ctx, info.device.source.device))?;
    source.set(
        ctx,
        "generation",
        identity(ctx, info.device.source.generation),
    )?;
    if let Some(mapping) = &info.mapping {
        let table: Table = table.get(ctx, "mapping")?;
        table.set(ctx, "generation", identity(ctx, mapping.generation))?;
    }
    Ok(value)
}

pub(super) fn event_payload<'gc>(
    ctx: Context<'gc>,
    kind: EventKind,
    json: &Json,
) -> Result<Value<'gc>, luna::Error<'gc>> {
    let value = payload(ctx, json)?;
    if kind == EventKind::ContextChanged
        && let Value::Table(table) = value
    {
        fields(ctx, table, json, &["revision"])?;
    }
    Ok(value)
}

#[cfg(target_os = "linux")]
pub(super) fn snapshot<'gc>(
    ctx: Context<'gc>,
    snapshot: &crate::context::Snapshot,
) -> Result<Value<'gc>, luna::Error<'gc>> {
    let value = luna_util::serde::to_value(ctx, snapshot).map_err(io::Error::other)?;
    let Value::Table(table) = value else {
        unreachable!("snapshot serializes as a table")
    };
    table.set(ctx, "generation", identity(ctx, snapshot.generation))?;
    table.set(ctx, "revision", identity(ctx, snapshot.revision))?;
    let outputs: Table = table.get(ctx, "outputs")?;
    for (index, output) in snapshot.outputs.iter().enumerate() {
        let table: Table = outputs.get(ctx, index + 1)?;
        table.set(
            ctx,
            "geometry_generation",
            identity(ctx, output.geometry_generation),
        )?;
    }
    Ok(value)
}

#[cfg(target_os = "linux")]
pub(super) fn binding<'gc>(
    ctx: Context<'gc>,
    binding: &crate::context::Binding,
) -> Result<Value<'gc>, luna::Error<'gc>> {
    let value = luna_util::serde::to_value(ctx, binding).map_err(io::Error::other)?;
    let Value::Table(table) = value else {
        unreachable!("binding serializes as a table")
    };
    table.set(
        ctx,
        "compositor_start",
        identity(ctx, binding.compositor_start),
    )?;
    Ok(value)
}

#[cfg(target_os = "linux")]
pub(super) fn context_fence<'gc>(
    ctx: Context<'gc>,
    value: &crate::protocol::Fence,
) -> Result<Value<'gc>, luna::Error<'gc>> {
    let table = Table::new(&ctx);
    table.set(ctx, "configuration", identity(ctx, value.configuration))?;
    table.set(ctx, "session", identity(ctx, value.session))?;
    let broker = Table::new(&ctx);
    for (index, word) in value.broker.0.iter().enumerate() {
        broker.set(ctx, index + 1, identity(ctx, *word))?;
    }
    table.set(ctx, "broker", broker)?;
    Ok(Value::Table(table))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callbacks_handlers_timers_and_task_resumes_keep_exact_identities() {
        let source = r#"
local p=require('prek')
local function check(fence)
  assert(fence.broker[1]=='18446744073709551615' and fence.broker[2]=='9223372036854775808')
  assert(fence.configuration=='1' and fence.session=='18446744073709551614')
end
local function event(payload)
  check(payload.fence)
  local e=payload.event
  assert(e.id=='18446744073709551615' and e.ordinal=='18446744073709551614')
  assert(e.source.device=='9223372036854775808' and e.source.generation=='0')
  assert(e.config_generation=='1' and e.session_generation=='18446744073709551614')
  assert(e.evidence.count==3 and e.action.fields.id==17 and e.workspace==-3)
  p.log.info('exact-event',{id=e.id, broker=payload.fence.broker[1]})
end
p.events.on('matched',event)
local action=p.actions.define('exact',{requires={'observations'}},event)
p.scheduler.after('1ms',function(wake)check(wake.context.fence);assert(type(wake.timer.id)=='number');p.log.info('exact-timer')end)
p.scheduler.spawn(function(wake)
  check(wake.context.fence);assert(type(wake.task.id)=='number');p.log.info('exact-task')
  local wake=p.scheduler.sleep('1ms');check(wake.context.fence);p.log.info('exact-resume')
end)
return p.config.new{rules={p.rules.define{id='exact',trigger=p.triggers.key{key='KEY_A'},action=action}}}
"#;
        let definition = Definition::compile(source, "exact-values.lua").unwrap();
        let action = definition.validate().unwrap().rules[0].action.clone();
        let mut runtime = definition.instantiate().unwrap();
        runtime.set_clock(Duration::ZERO).unwrap();
        let fence = serde_json::json!({"broker":[u64::MAX, 1u64 << 63], "configuration":1, "session":u64::MAX-1});
        let payload = serde_json::json!({"fence":fence,"event":{
            "outcome":"matched","id":u64::MAX,"ordinal":u64::MAX-1,
            "source":{"device":1u64 << 63,"generation":0},"config_generation":1,
            "session_generation":u64::MAX-1,"evidence":{"count":3},
            "action":{"fields":{"id":17}},"workspace":-3}});
        let logs = runtime.dispatch(EventKind::Matched, &payload).unwrap();
        assert_eq!(
            logs[0].fields,
            serde_json::json!({"id":u64::MAX.to_string(),"broker":u64::MAX.to_string()})
        );
        let result = runtime.resolve_action(&action, &payload).unwrap();
        assert_eq!(result.logs[0].fields, logs[0].fields);
        let context = serde_json::json!({"fence":fence});
        let first = runtime.tick(Duration::from_millis(1), &context).unwrap();
        assert_eq!(
            first
                .iter()
                .map(|log| log.message.as_str())
                .collect::<Vec<_>>(),
            vec!["exact-timer", "exact-task"]
        );
        let resume_at = runtime.scheduler_deadline().unwrap().unwrap();
        assert_eq!(
            runtime.tick(resume_at, &context).unwrap()[0].message,
            "exact-resume"
        );
    }
}
