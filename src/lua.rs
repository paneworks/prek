//! Luna-backed, side-effect-free configuration evaluation. Runtime callbacks
//! and session adapters are separate APIs; this compiler never owns input fds.
use crate::config::{Config, ConfigError, MAX_CONFIG_BYTES};
use luna::{Callback, CallbackReturn, Closure, Executor, Fuel, Lua, StashedExecutor, Table, Value};
use std::{
    io,
    time::{Duration, Instant},
};

pub const API_VERSION: u32 = 1;
/// Implemented configuration exports. Runtime families remain explicit work
/// in PLAN.md; discovery never advertises an unimplemented binding.
pub fn api() -> serde_json::Value {
    let mut symbols = vec![
        serde_json::json!({"name":"config.new", "signature":"config.new(table) -> Config", "description":"Apply the schema version default; validate the returned model."}),
        serde_json::json!({"name":"config.preset", "signature":"config.preset(name) -> Config", "description":"Return a mutable minimal, desktop or phone preset."}),
        serde_json::json!({"name":"rules.define", "signature":"rules.define(table) -> Rule", "description":"Declare a rule; strict validation occurs at compilation."}),
        serde_json::json!({"name":"units.duration", "signature":"units.duration(string) -> string", "description":"Declare an explicitly unit-qualified duration."}),
        serde_json::json!({"name":"units.distance", "signature":"units.distance(string) -> string", "description":"Declare an explicitly unit-qualified distance."}),
    ];
    for kind in [
        "key",
        "button_taps",
        "button_hold",
        "chord",
        "wheel",
        "touch_taps",
        "touch_hold",
        "swipe",
        "pan",
        "pinch",
        "rotate",
        "shape",
        "sequence",
    ] {
        symbols.push(serde_json::json!({"name":format!("triggers.{kind}"),
            "signature":format!("triggers.{kind}(named_fields) -> Trigger"),
            "description":"Construct a typed trigger with the documented schema fields."}));
    }
    for kind in [
        "event", "keys", "remap", "command", "morf", "hyprland", "power",
    ] {
        symbols.push(serde_json::json!({"name":format!("actions.{kind}"),
            "signature":format!("actions.{kind}(named_fields) -> Action"),
            "description":"Declare an action; configuration evaluation does not execute it."}));
    }
    serde_json::json!({"api_version":API_VERSION,"schema_version":crate::config::SCHEMA_VERSION,
        "engine":"onix-os/luna", "engine_version":"0.5.1", "phase":"configuration",
        "symbols":symbols, "limits":{"source_bytes":MAX_CONFIG_BYTES,"memory_bytes":MEMORY,
        "instructions":INSTRUCTIONS,"time_ms":TIME.as_millis(),"value_depth":64},
        "runtime_api":"pending"})
}
const MEMORY: usize = 32 * 1024 * 1024;
const INSTRUCTIONS: i32 = 1_000_000;
const SLICE: i32 = 2048;
const TIME: Duration = Duration::from_millis(250);
const BOOTSTRAP: &str = r#"
local api = __prek
__prek = nil
function api.config.new(value)
    assert(type(value) == "table", "config.new expects a table")
    if value.version == nil then value.version = 1 end
    return value
end
function api.rules.define(value)
    assert(type(value) == "table", "rules.define expects a table")
    return value
end
local function constructor(kind)
    return function(value)
        assert(type(value) == "table", kind .. " expects named fields")
        assert(value.kind == nil or value.kind == kind, "conflicting kind")
        value.kind = kind
        return value
    end
end
for _, kind in ipairs({"key", "button_taps", "button_hold", "chord", "wheel",
    "touch_taps", "touch_hold", "swipe", "pinch", "rotate", "shape", "sequence"}) do
    api.triggers[kind] = constructor(kind)
end
function api.triggers.pan(value)
    assert(type(value) == "table", "pan expects named fields")
    assert(value.continuous == nil or value.continuous == true, "pan is continuous")
    value.continuous = true
    return api.triggers.swipe(value)
end
for _, kind in ipairs({"event", "keys", "remap", "command", "morf", "hyprland", "power"}) do
    api.actions[kind] = constructor(kind)
end
function api.units.duration(value)
    assert(type(value) == "string", "duration requires explicit units")
    return value
end
function api.units.distance(value)
    assert(type(value) == "string", "distance requires explicit units")
    return value
end
function require(name)
    assert(name == "prek", "module is unavailable in configuration evaluation: " .. tostring(name))
    return api
end
math.randomseed(0)
print, warn, dofile, loadfile, load, collectgarbage = nil, nil, nil, nil, nil, nil
"#;

fn error(value: impl std::fmt::Display) -> ConfigError {
    ConfigError(value.to_string())
}

fn drive(
    lua: &mut Lua,
    executor: &StashedExecutor,
    started: Instant,
    time_limit: Duration,
) -> Result<(), ConfigError> {
    let mut remaining = INSTRUCTIONS;
    loop {
        if started.elapsed() >= time_limit {
            return Err(error("Lua configuration time limit exceeded"));
        }
        if lua.total_memory() > MEMORY {
            return Err(error("Lua configuration memory limit exceeded"));
        }
        if remaining <= 0 {
            return Err(error("Lua configuration instruction limit exceeded"));
        }
        let budget = remaining.min(SLICE);
        let mut fuel = Fuel::with(budget);
        let done = lua
            .enter(|ctx| ctx.fetch(executor).step(ctx, &mut fuel))
            .map_err(error)?;
        remaining -= budget.saturating_sub(fuel.remaining()).max(1);
        if done {
            if lua.total_memory() > MEMORY {
                return Err(error("Lua configuration memory limit exceeded"));
            }
            if started.elapsed() >= time_limit {
                return Err(error("Lua configuration time limit exceeded"));
            }
            return Ok(());
        }
    }
}

fn start(lua: &mut Lua, name: &str, source: &str) -> Result<StashedExecutor, ConfigError> {
    lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, Some(name), source.as_bytes())?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })
    .map_err(error)
}

/// Evaluate a Lua source into the same strict model used by existing TOML.
/// Callers loading untrusted sources must isolate compilation from their input
/// loop; the returned model owns no arena values, callbacks or descriptors.
pub fn compile(source: &str, name: &str) -> Result<Config, ConfigError> {
    if source.len() > MAX_CONFIG_BYTES {
        return Err(error("configuration exceeds 1 MiB"));
    }
    let mut lua = Lua::core();
    lua.set_memory_limit(Some(MEMORY));
    lua.try_enter(|ctx| {
        ctx.set_max_call_depth(256);
        let module = Table::new(&ctx);
        module.set(ctx, "api_version", API_VERSION)?;
        module.set(ctx, "version", env!("CARGO_PKG_VERSION"))?;
        module.set(
            ctx,
            "api",
            luna_util::serde::to_value(ctx, &api()).map_err(io::Error::other)?,
        )?;
        for namespace in ["config", "rules", "triggers", "actions", "units"] {
            module.set(ctx, namespace, Table::new(&ctx))?;
        }
        let config: Table = module.get(ctx, "config")?;
        config.set(
            ctx,
            "preset",
            Callback::from_fn(&ctx, |ctx, _, mut stack| {
                let name: luna::String = stack.consume(ctx)?;
                let preset = Config::preset(
                    name.to_str()
                        .map_err(|err| io::Error::other(err.to_string()))?,
                )
                .map_err(io::Error::other)?;
                let value = luna_util::serde::to_value(ctx, &preset).map_err(io::Error::other)?;
                stack.replace(ctx, value);
                Ok(CallbackReturn::Return)
            }),
        )?;
        ctx.globals().set(ctx, "__prek", module)?;
        Ok(())
    })
    .map_err(error)?;
    let bootstrap = start(&mut lua, "prek configuration API", BOOTSTRAP)?;
    // The pinned library/bootstrap contains no user code. Its initialization
    // is separate from the untrusted source's compilation/execution budget.
    drive(&mut lua, &bootstrap, Instant::now(), Duration::from_secs(1))?;
    lua.try_enter(|ctx| {
        ctx.fetch(&bootstrap)
            .take_result::<()>(ctx)
            .map_err(io::Error::other)??;
        Ok(())
    })
    .map_err(error)?;
    let started = Instant::now();
    let executor = start(&mut lua, name, source)?;
    drive(&mut lua, &executor, started, TIME)?;
    let config = lua
        .try_enter(|ctx| {
            let active = ctx.fetch(&executor);
            let values: luna::Variadic<Vec<Value>> =
                active.take_result(ctx).map_err(io::Error::other)??;
            if active.mode() != luna::ExecutorMode::Stopped {
                return Err(io::Error::other("Lua configuration cannot yield").into());
            }
            if values.0.len() != 1 {
                return Err(io::Error::other("Lua configuration must return one table").into());
            }
            let config: Config = luna_util::serde::from_value_with(
                ctx,
                values.0[0],
                luna_util::serde::DeOptions::default().max_depth(64),
            )
            .map_err(io::Error::other)?;
            config.validate().map_err(io::Error::other)?;
            Ok(config)
        })
        .map_err(error)?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn luna_config_helpers_compile_to_the_existing_strict_rule_model() {
        let config = compile(r#"
            local p = require("prek")
            return p.config.new {
                policy = { allowed_devices = { "keyboard" } },
                devices = { keyboard = { keys = { "KEY_A" } } },
                rules = { p.rules.define {
                    id = "lua-a", devices = { "keyboard" },
                    trigger = p.triggers.button_hold { key = "KEY_A", after = p.units.duration("600ms") },
                    action = p.actions.event { name = "lua_hold" },
                } },
            }
        "#, "fixture.lua").unwrap();
        assert_eq!(config.rules[0].id, "lua-a");
        assert_eq!(config.rules.len(), 1);
        assert_eq!(
            config,
            Config::parse(&toml::to_string(&config).unwrap()).unwrap()
        );
    }
    #[test]
    fn lua_errors_limits_and_unsupported_values_do_not_escape_configuration_evaluation() {
        for source in [
            "return { version = 1, unknown = true }",
            "while true do end",
            "return { version = 1, rules = function() end }",
            "error('fixture-error')",
            "return 1, 2",
            "return require('io')",
            "coroutine.yield({version=1})",
            "return { version = 1, mode = os.execute('false') }",
        ] {
            assert!(compile(source, "bad.lua").is_err(), "{source}");
        }
        let config = compile(
            "local p=require('prek'); return p.config.preset('minimal')",
            "preset.lua",
        )
        .unwrap();
        assert_eq!(config, Config::preset("minimal").unwrap());
    }
    #[test]
    fn manifest_exports_resolve_to_real_configuration_functions() {
        let mut source = "local p=require('prek');\n".to_owned();
        for symbol in api()["symbols"].as_array().unwrap() {
            source.push_str(&format!(
                "assert(type(p.{}) == 'function');\n",
                symbol["name"].as_str().unwrap()
            ));
        }
        source.push_str("return {version=1}");
        compile(&source, "api-parity.lua").unwrap();
    }
}
