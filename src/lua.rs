//! Luna-backed, side-effect-free configuration evaluation. Runtime callbacks
//! and session adapters are separate APIs; this compiler never owns input fds.
use crate::config::{Config, ConfigError, MAX_CONFIG_BYTES};
use luna::{Callback, CallbackReturn, Closure, Executor, Fuel, Lua, StashedExecutor, Table, Value};
use std::{
    io,
    time::{Duration, Instant},
};

#[cfg(target_os = "linux")]
mod context;
#[cfg(target_os = "linux")]
mod devices;
mod editor;
mod export;
#[cfg(target_os = "linux")]
mod geometry;
mod handlers;
mod modules;
mod rule_api;
mod runtime;
mod scheduler;
#[cfg(target_os = "linux")]
pub mod session;
pub mod source_transfer;
mod tasks;
mod values;
pub use editor::editor_definitions;
pub use export::export_config;
pub use handlers::{ActionResult, HandlerRegistration};
pub use modules::{Module, Sources};
pub use rule_api::{RuleProposal, RuleReceipt};
pub use runtime::{Definition, EventKind, LogRecord, MAX_SNAPSHOT_BYTES, Registration, Runtime};
pub use scheduler::TimerRegistration;
pub use tasks::TaskRegistration;

pub const API_VERSION: u32 = 1;
/// Implemented configuration exports. Runtime families remain explicit work
/// in PLAN.md; discovery never advertises an unimplemented binding.
pub fn api() -> serde_json::Value {
    let mut symbols = vec![
        serde_json::json!({"name":"config.new", "signature":"config.new(table) -> Config", "description":"Apply the schema version default; validate the returned model."}),
        serde_json::json!({"name":"config.preset", "signature":"config.preset(name) -> Config", "description":"Return a mutable minimal, desktop or phone preset."}),
        serde_json::json!({"name":"config.export", "signature":"config.export(config) -> string", "phase":"configuration", "description":"Validate and export a complete declarative model as bounded Lua source; performs no I/O and requires original source for named handlers."}),
        serde_json::json!({"name":"rules.define", "signature":"rules.define(table) -> Rule", "description":"Declare a rule; strict validation occurs at compilation."}),
        serde_json::json!({"name":"layers.define", "signature":"layers.define(table) -> Layer", "description":"Declare a physical-key layer with physical, active or inactive activation; strict validation occurs at compilation."}),
        serde_json::json!({"name":"units.duration", "signature":"units.duration(string) -> string", "description":"Declare an explicitly unit-qualified duration."}),
        serde_json::json!({"name":"units.distance", "signature":"units.distance(string) -> string", "description":"Declare an explicitly unit-qualified distance."}),
        serde_json::json!({"name":"events.on", "signature":"events.on(kind, callback) -> Subscription", "description":"Subscribe to approved semantic or lifecycle events in this session generation; registration has no evaluation effects."}),
        serde_json::json!({"name":"Subscription.dispose", "receiver":"Subscription", "signature":"subscription:dispose() -> nil", "description":"Idempotently disable this subscription before its next invocation."}),
        serde_json::json!({"name":"log.info", "signature":"log.info(message, fields = nil) -> nil", "description":"Append a bounded structured log in an event callback; unavailable during configuration evaluation."}),
        serde_json::json!({"name":"scheduler.after", "signature":"scheduler.after(duration, callback) -> Timer", "description":"Schedule one callback after an explicitly unit-qualified monotonic delay in this VM generation."}),
        serde_json::json!({"name":"scheduler.every", "signature":"scheduler.every(period, callback) -> Timer", "description":"Schedule periodic callbacks; preserve phase and skip missed ticks instead of catching up in a burst."}),
        serde_json::json!({"name":"scheduler.defer", "signature":"scheduler.defer(callback) -> Timer", "description":"Schedule a callback on a later runtime wake using a one-millisecond delay."}),
        serde_json::json!({"name":"Timer.cancel", "receiver":"Timer", "signature":"timer:cancel() -> nil", "description":"Idempotently cancel this generation-scoped timer before its next invocation."}),
        serde_json::json!({"name":"scheduler.spawn", "signature":"scheduler.spawn(callback) -> Task", "description":"Start a bounded generation-scoped coroutine on a later scheduler wake; never execute it during configuration evaluation."}),
        serde_json::json!({"name":"scheduler.sleep", "signature":"scheduler.sleep(duration) -> TaskWake", "description":"Suspend the current scheduler task for a unit-qualified monotonic delay and return its next wake context."}),
        serde_json::json!({"name":"Task.cancel", "receiver":"Task", "signature":"task:cancel() -> nil", "description":"Cancel future resumes and release retained coroutine state at the next scheduler boundary."}),
        serde_json::json!({"name":"Task.status", "receiver":"Task", "signature":"task:status() -> TaskState", "description":"Inspect pending, running, sleeping, completed or cancelled state in the owning generation."}),
        serde_json::json!({"name":"actions.define", "signature":"actions.define(name, {requires = capabilities}, callback) -> Action", "description":"Register an approved generation-scoped named handler; resolve a bounded typed effect plan in the unprivileged session after a broker execution grant."}),
    ];
    #[cfg(target_os = "linux")]
    for (name, signature, description) in [
        (
            "outputs.list",
            "outputs.list() -> OutputView[] | nil",
            "Copy all outputs in the fresh owning desktop snapshot; each reference is fenced to that exact snapshot revision.",
        ),
        (
            "outputs.get",
            "outputs.get(name) -> OutputView | nil",
            "Query one output by exact name; no discovery outside the verified snapshot.",
        ),
        (
            "regions.list",
            "regions.list() -> RegionView[] | nil",
            "Copy provider-reported rendered regions with their snapshot fence/revision; no guessed geometry.",
        ),
        (
            "regions.get",
            "regions.get(id) -> RegionView | nil",
            "Query one rendered region by exact stable ID in the fresh snapshot.",
        ),
        (
            "regions.at",
            "regions.at(point) -> RegionView[] | nil",
            "Return every rendered region containing a global logical point, with half-open bounds; does not arbitrate overlapping regions.",
        ),
        (
            "coordinates.convert",
            "coordinates.convert(reference, point, from, to) -> Point | nil",
            "Convert normalized/output-local/global logical coordinates using host geometry; exact fence/revision/output generation required, stale references return nil, invalid arguments raise errors.",
        ),
    ] {
        symbols.push(serde_json::json!({"name":name,"signature":signature,"phase":"runtime_callback","description":description}));
    }
    for (name, signature, description) in [
        (
            "list",
            "rules.list() -> Rule[]",
            "Return owned copies of the current acknowledged rules.",
        ),
        (
            "get",
            "rules.get(id) -> Rule | nil",
            "Query one current acknowledged rule by stable ID; returned edits do not change active rules.",
        ),
        (
            "revision",
            "rules.revision() -> string",
            "Return the exact decimal runtime revision without Lua numeric precision loss.",
        ),
        (
            "transaction",
            "rules.transaction() -> RuleTransaction",
            "Create a private revision-scoped builder; requires an observation broker, system allow_runtime_rules permission and the negotiated runtime_rules capability.",
        ),
    ] {
        symbols.push(serde_json::json!({"name":format!("rules.{name}"),"signature":signature,"phase":"runtime_callback","description":description}));
    }
    for (name, signature, description) in [
        (
            "enable",
            "transaction:enable(id) -> nil",
            "Stage enabling an existing rule.",
        ),
        (
            "disable",
            "transaction:disable(id) -> nil",
            "Stage disabling an existing rule.",
        ),
        (
            "replace",
            "transaction:replace(rule) -> nil",
            "Stage replacing or adding a fully typed rule; named handlers must match the approved source registrations.",
        ),
        (
            "remove",
            "transaction:remove(id) -> nil",
            "Stage removing an existing rule.",
        ),
        (
            "activate_layer",
            "transaction:activate_layer(id) -> nil",
            "Stage activating a declared layer; fresh input is required after acknowledgement.",
        ),
        (
            "deactivate_layer",
            "transaction:deactivate_layer(id) -> nil",
            "Stage deactivating a declared layer and retiring its candidates.",
        ),
        (
            "reset_layer",
            "transaction:reset_layer(id) -> nil",
            "Stage returning a declared layer to physical-key control.",
        ),
        (
            "commit",
            "transaction:commit() -> nil",
            "Validate the complete batch and submit one proposal; active queries change only after an accepted host receipt.",
        ),
        (
            "cancel",
            "transaction:cancel() -> nil",
            "Idempotently abandon an open builder; submitted proposals await the host receipt.",
        ),
        (
            "status",
            "transaction:status() -> RuleTransactionStatus",
            "Return an owned state/revision/error copy; revisions are exact decimal strings.",
        ),
    ] {
        symbols.push(serde_json::json!({"name":format!("RuleTransaction.{name}"),"receiver":"RuleTransaction","signature":signature,"phase":"runtime_callback","description":description}));
    }
    for (name, signature) in [
        ("list", "layers.list() -> Layer[]"),
        ("get", "layers.get(id) -> Layer | nil"),
        ("revision", "layers.revision() -> string"),
        ("transaction", "layers.transaction() -> RuleTransaction"),
    ] {
        symbols.push(serde_json::json!({"name":format!("layers.{name}"),"signature":signature,"phase":"runtime_callback","description":"Query acknowledged layer definitions/activation overrides or open the shared rule/layer transaction; physical key state is not exposed."}));
    }
    #[cfg(target_os = "linux")]
    for (name, result, description) in [
        (
            "snapshot",
            "Snapshot | nil",
            "Return an owned copy of the complete fresh broker snapshot, including outputs, windows, regions and keyboard geometry; generations/revisions are decimal strings; nil when unavailable or expired.",
        ),
        (
            "binding",
            "Binding | nil",
            "Return an owned copy of the verified session/compositor binding for a fresh snapshot; compositor_start is a decimal string; conveys no authority.",
        ),
        (
            "state",
            "State",
            "Return the broker's fresh session state, or unknown after the view expires.",
        ),
        (
            "fence",
            "Fence | nil",
            "Return the current broker/configuration/session fence with lossless decimal-string identities while the context view is fresh.",
        ),
        (
            "error",
            "string | nil",
            "Return the bounded broker context diagnostic while the context view is fresh.",
        ),
    ] {
        symbols.push(serde_json::json!({"name":format!("context.{name}"),
            "signature":format!("context.{name}() -> {result}"),"phase":"runtime","description":description}));
    }
    #[cfg(target_os = "linux")]
    for (name, signature, description) in [
        (
            "devices.list",
            "devices.list() -> DeviceInfo[] | nil",
            "Return owned permitted metadata received in this VM while desktop context is fresh; nil before the catalog is available.",
        ),
        (
            "devices.get",
            "devices.get(source) -> DeviceInfo | nil",
            "Look up the exact source identity/generation/seat; stale, unknown or foreign-seat identities return nil.",
        ),
        (
            "devices.revision",
            "devices.revision() -> string | nil",
            "Return the lossless received-catalog revision while its context is usable.",
        ),
        (
            "devices.position",
            "devices.position(source, x, y) -> Position | nil",
            "Map integer device counts through the permitted source's validated mapping; invalid coordinates raise an error, unavailable mappings return nil.",
        ),
        (
            "seats.current",
            "seats.current() -> SeatInfo | nil",
            "Return an owned binding/fence/state copy for this VM's fresh desktop session; cannot enumerate other seats.",
        ),
    ] {
        symbols.push(serde_json::json!({"name":name,"signature":signature,"phase":"runtime","description":description}));
    }
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
        "engine":"onix-os/luna", "engine_version":"0.5.1", "phase":"configuration_and_session_actions",
        "symbols":symbols, "limits":{"source_bytes":MAX_CONFIG_BYTES,"memory_bytes":MEMORY,
        "instructions":INSTRUCTIONS,"time_ms":TIME.as_millis(),"value_depth":64},
        "runtime_limits":{"subscriptions":256,"timers":256,"tasks":tasks::MAX_TASKS,"tasks_per_wake":tasks::TASKS_PER_WAKE,"handlers":256,"action_effects":32,"action_bytes":32768,"timers_per_wake":32,"callback_instructions":100000,"callback_time_ms":20,"event_bytes":65536,"log_bytes":16384,"logs_per_event":32},
        "modules":{"root":"entry_parent/modules","path":"<identifier>/.../<identifier>.lua","count":modules::MAX_MODULES,"import_depth":modules::MAX_IMPORT_DEPTH,"source_set_bytes":MAX_CONFIG_BYTES,"owner":"entry_file_uid","group_other_writes":false,"symlinks":false,"runtime":"approved sources only; no filesystem reads"},
        "event_kinds":["input","matched","begin","update","end","cancel","skipped","ready","reload","stop","recovery","session_changed","context_changed","rules_changed"],
        "runtime_identities":{"type":"decimal_string","range":"0..18446744073709551615","fields":["fence.broker[]","fence.configuration","fence.session","event.id","event.ordinal","event.source.device","event.source.generation","event.config_generation","event.session_generation","Snapshot.generation","Snapshot.revision","OutputState.geometry_generation","Binding.compositor_start","context_changed.revision","rules.revision","input.source.device","input.source.generation","input.sequence","input.contacts[].id.generation","input.contacts_started[].generation","input.contacts_ended[].generation","input.ended_contacts[].id.generation"],"comparison":"exact string equality; no numeric conversion","bounded_handles":"subscription/timer/task IDs remain Lua integers","protocol":"Rust transport retains u64 values"},
        "rule_transactions":{"builders":64,"operations":64,"transaction_bytes":32768,"pending_proposals":1,"receipt_time_ms":250,"revision_type":"decimal_string","transport":"explicit authorized host opt-in","production_broker":"observation_mode","system_permission":"policy.allow_runtime_rules","agent_flags":["lua","rule-mutations"],"capability":"runtime_rules","context":"fresh owning desktop","routing":"unavailable"},
        "runtime_layers":{"declared_layers":64,"activation_values":["physical","active","inactive"],"default":"physical","queries":"acknowledged definitions and activation overrides; no physical-key state","transaction":"shared RuleTransaction revision/receipt/permissions","definitions":"immutable at runtime","activation":"fresh input only; existing candidates retired","physical_toggles":"preserved across runtime edits; reconciled on source reload","seat":"owning broker registration"},
        "input_events":{"event":"input","configuration_mode":"observe","agent_flags":["lua","input"],"capability":"input_events","system_permission":"policy.allow_input_events","production_broker":"observation_mode","sources":"explicit policy.allowed_devices selectors on the owning seat; virtual devices excluded","context":"fresh desktop without secure overlay","freshness_ms":250,"payload":"InputPayload","raw_frames":false,"input_claims":false,"delivery":"ordered, no coalescing; overflow retires the affected stream or Lua component","resets":["attached","reconciled","mapping_changed","removed","lost"],"coordinates":"device axis counts","maximum_changes":4096,"maximum_contacts":256,"identity_fields":["source.device","source.generation","sequence","contacts[].id.generation","contacts_started[].generation","contacts_ended[].generation","ended_contacts[].id.generation"],"identity_type":"decimal_string"},
        "context_limits":{"snapshot_bytes":4194304,"chunk_bytes":32768,"transfer_time_ms":1000,"freshness_ms":250,"refresh_ms":100,"queued_snapshots":2,"read_only":true},
        "geometry_queries":{"context":"fresh owning desktop without secure overlay","lookup":"exact output name or rendered region ID","references":"canonical decimal-string fence, snapshot revision and output geometry generation; copies confer no authority","spaces":["normalized","local","global"],"normalized":"post-transform output axes, not raw device axes","local":"post-transform logical units; rotation and scale applied through output logical size","region_bounds":"global logical, half-open","overlaps":"all matches in provider order, no target selection","unavailable":"nil for absent/stale/inhibited context, missing name or retired reference; known empty arrays remain empty","conversion":"finite numeric points, no clamping; arithmetic overflow raises an error","effects":false},
        "runtime_requires":{"uid":"unprivileged active session","capabilities":["context","observations","lua_events"],"thread":"owning Luna arena","generation":"current broker/configuration/session fence"},
        "action_requires":{"agent_flags":["lua","commands"],"capabilities":["context","observations","lua_events","lua_actions","commands"],"implemented_effects":["commands","observations"],"grant_time_ms":250},
        "editor":{"format":"LuaCATS","module":"prek","cli":["prek","api","--lua"],"path":"share/prek/lua/prek.lua","scope":"implemented_configuration_and_runtime"},
        "device_queries":{"catalog":"permitted metadata received through input reset notifications; no host inventory scan","maximum_sources":256,"maximum_bytes":4194304,"lookup":"exact Source device/generation/seat with canonical decimal strings","nil":"no received metadata, unknown source, missing mapping or unavailable/stale desktop context","empty_list":"all known sources removed while context remains usable","revision":"decimal string; metadata changes/removals only","map_keys":"numeric evdev type/axis keys in Lua","position":"validated device counts to normalized/mm/output-local/global coordinates; no action authority","seat":"current fresh desktop binding/fence only; no other-seat enumeration"},
        "runtime_api":"partial"})
}
const MEMORY: usize = 32 * 1024 * 1024;
const INSTRUCTIONS: i32 = 1_000_000;
const SLICE: i32 = 2048;
const TIME: Duration = Duration::from_millis(250);
const BOOTSTRAP: &str = r#"
local assert, type, pairs, ipairs = assert, type, pairs, ipairs
local api = __prek
__prek = nil
local registrations = __prek_registry
__prek_registry = nil
local timers = __prek_timers
__prek_timers = nil
local tasks = __prek_tasks
__prek_tasks = nil
local handlers = __prek_handlers
__prek_handlers = nil
local modules = __prek_modules
__prek_modules = nil
local handler_guard = api.actions.__guard
api.actions.__guard = nil
function api.actions.define(name, options, callback)
    handler_guard()
    assert(type(name)=="string" and #name>0 and #name<=128, "invalid Lua action name")
    assert(type(options)=="table" and type(options.requires)=="table", "Lua actions require explicit capabilities")
    for key in pairs(options) do assert(key=="requires", "unknown Lua action option") end
    assert(type(callback)=="function", "Lua action expects a callback")
    assert(#handlers<256, "Lua action handler limit exceeded")
    for _, handler in ipairs(handlers) do assert(handler.name~=name, "duplicate Lua action name") end
    local requires = {}
    for index, value in ipairs(options.requires) do requires[index] = value end
    local entry = {id=#handlers+1, name=name, requires=requires, callback=callback}
    handlers[entry.id]=entry
    return {kind="lua",handler=name,requires=options.requires}
end
local timer_duration = api.scheduler.__duration
api.scheduler.__duration = nil
local function schedule(delay, callback, periodic)
    assert(type(callback)=="function", "scheduler expects a callback")
    assert(#timers<256, "Lua timer limit exceeded")
    local milliseconds, deadline = timer_duration(delay)
    local entry = {id=#timers+1, delay_ms=milliseconds, due=deadline,
        periodic=periodic, active=true, callback=callback}
    timers[entry.id]=entry
    return {cancel=function(self)entry.active=false end}
end
function api.scheduler.after(delay, callback) return schedule(delay,callback,false) end
function api.scheduler.every(period, callback) return schedule(period,callback,true) end
function api.scheduler.defer(callback) return schedule("1ms",callback,false) end
local task_defer = api.scheduler.__task_defer
api.scheduler.__task_defer = nil
local task_assert, task_type = assert, type
local next_task_id = 0
function api.scheduler.spawn(callback)
    task_assert(task_type(callback)=="function", "scheduler.spawn expects a callback")
    local slot
    for index=1,64 do
        if tasks[index]==nil or not tasks[index].active then slot=index; break end
    end
    task_assert(slot~=nil, "Lua active task limit exceeded")
    task_assert(next_task_id<9007199254740991, "Lua task identity exhausted")
    next_task_id=next_task_id+1
    local entry={id=next_task_id, callback=callback, due=task_defer(), active=true, state="pending"}
    tasks[slot]=entry
    return {
        cancel=function(self)
            if entry.active then entry.active=false; entry.state="cancelled"; entry.callback=nil end
        end,
        status=function(self)return entry.state end,
    }
end
local allowed_events = {
    input=true,
    matched=true, begin=true, update=true, ["end"]=true, cancel=true,
    skipped=true, ready=true, reload=true, stop=true, recovery=true,
    session_changed=true, context_changed=true, rules_changed=true,
}
function api.events.on(kind, callback)
    assert(allowed_events[kind], "unsupported event subscription")
    assert(type(callback) == "function", "events.on expects a callback")
    assert(#registrations < 256, "Lua subscription limit exceeded")
    local entry = {id=#registrations+1, kind=kind, callback=callback, active=true}
    registrations[entry.id] = entry
    return {dispose=function(self) entry.active=false end}
end
function api.config.new(value)
    assert(type(value) == "table", "config.new expects a table")
    if value.version == nil then value.version = 1 end
    return value
end
function api.rules.define(value)
    assert(type(value) == "table", "rules.define expects a table")
    return value
end
function api.layers.define(value)
    assert(type(value) == "table", "layers.define expects a table")
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
local loaded, values, loading, imports = {}, {}, {}, {}
local module_assert, module_type, module_pcall, module_error, module_concat = assert, type, pcall, error, table.concat
function require(name)
    module_assert(module_type(name)=="string" and #name<=128, "require expects a bounded module name")
    if name=="prek" then return api end
    if loaded[name] then return values[name] end
    local loader=modules[name]
    module_assert(loader~=nil, "module is not in the approved source snapshot: " .. name)
    module_assert(not loading[name], "Lua module cycle: " .. module_concat(imports," -> ") .. " -> " .. name)
    module_assert(#imports<16, "Lua module import depth exceeded")
    loading[name]=true
    imports[#imports+1]=name
    local ok, value=module_pcall(loader)
    imports[#imports]=nil
    loading[name]=nil
    if not ok then module_error(value,0) end
    if value==nil then value=true end
    loaded[name], values[name]=true, value
    return value
end
math.randomseed(0)
print, warn, dofile, loadfile, load, collectgarbage = nil, nil, nil, nil, nil, nil
"#;

fn error(value: impl std::fmt::Display) -> ConfigError {
    ConfigError(value.to_string())
}

fn drive_budget(
    lua: &mut Lua,
    executor: &StashedExecutor,
    started: Instant,
    time_limit: Duration,
    instructions: i32,
) -> Result<(), ConfigError> {
    drive_progress(
        lua,
        executor,
        started,
        time_limit,
        instructions,
        &mut || Ok(()),
    )
}

fn drive_progress(
    lua: &mut Lua,
    executor: &StashedExecutor,
    started: Instant,
    time_limit: Duration,
    instructions: i32,
    progress: &mut dyn FnMut() -> io::Result<()>,
) -> Result<(), ConfigError> {
    let mut remaining = instructions;
    loop {
        progress().map_err(error)?;
        if started.elapsed() >= time_limit {
            return Err(error("Lua execution time limit exceeded"));
        }
        if lua.total_memory() > MEMORY {
            return Err(error("Lua execution memory limit exceeded"));
        }
        if remaining <= 0 {
            return Err(error("Lua execution instruction limit exceeded"));
        }
        let budget = remaining.min(SLICE);
        let mut fuel = Fuel::with(budget);
        let done = lua
            .enter(|ctx| ctx.fetch(executor).step(ctx, &mut fuel))
            .map_err(error)?;
        remaining -= budget.saturating_sub(fuel.remaining()).max(1);
        if done {
            if lua.total_memory() > MEMORY {
                return Err(error("Lua execution memory limit exceeded"));
            }
            if started.elapsed() >= time_limit {
                return Err(error("Lua execution time limit exceeded"));
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
    let mut runtime = Runtime::load(source, name)?;
    if !runtime.registrations()?.is_empty()
        || !runtime.timer_registrations()?.is_empty()
        || !runtime.handler_registrations()?.is_empty()
        || !runtime.task_registrations()?.is_empty()
    {
        return Err(error(
            "Lua subscriptions require the runtime snapshot loader",
        ));
    }
    Ok(runtime.config)
}

fn instantiate(
    source: &str,
    name: &str,
    modules: &std::collections::BTreeMap<String, Module>,
    progress: &mut dyn FnMut() -> io::Result<()>,
) -> Result<Runtime, ConfigError> {
    modules::validate(name, source, modules)?;
    let mut lua = Lua::core();
    lua.set_memory_limit(Some(MEMORY));
    let host = std::rc::Rc::new(std::cell::RefCell::new(runtime::Host {
        configuring: true,
        ..Default::default()
    }));
    let (registry, timers, handlers, module_loaders, tasks) = lua
        .try_enter(|ctx| {
            ctx.set_max_call_depth(256);
            let module = Table::new(&ctx);
            module.set(ctx, "api_version", API_VERSION)?;
            module.set(ctx, "version", env!("CARGO_PKG_VERSION"))?;
            module.set(
                ctx,
                "api",
                luna_util::serde::to_value(ctx, &api()).map_err(io::Error::other)?,
            )?;
            for namespace in [
                "config",
                "rules",
                "layers",
                "triggers",
                "actions",
                "units",
                "events",
                "log",
                "scheduler",
                "context",
                "devices",
                "seats",
                "outputs",
                "regions",
                "coordinates",
            ] {
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
                    let value =
                        luna_util::serde::to_value(ctx, &preset).map_err(io::Error::other)?;
                    stack.replace(ctx, value);
                    Ok(CallbackReturn::Return)
                }),
            )?;
            let export_host = host.clone();
            config.set(
                ctx,
                "export",
                Callback::from_fn(&ctx, move |ctx, _, mut stack| {
                    if !export_host.borrow().configuring {
                        return Err(io::Error::other(
                            "configuration export requires configuration evaluation",
                        )
                        .into());
                    }
                    let value: Value = stack.consume(ctx)?;
                    let config: Config = luna_util::serde::from_value_with(
                        ctx,
                        value,
                        luna_util::serde::DeOptions::default().max_depth(64),
                    )
                    .map_err(io::Error::other)?;
                    let source = export_config(&config).map_err(io::Error::other)?;
                    stack.replace(ctx, ctx.intern(source.as_bytes()));
                    Ok(CallbackReturn::Return)
                }),
            )?;
            ctx.globals().set(ctx, "__prek", module)?;
            let registry = Table::new(&ctx);
            ctx.globals().set(ctx, "__prek_registry", registry)?;
            let timers = Table::new(&ctx);
            ctx.globals().set(ctx, "__prek_timers", timers)?;
            let tasks = Table::new(&ctx);
            ctx.globals().set(ctx, "__prek_tasks", tasks)?;
            let handlers = Table::new(&ctx);
            ctx.globals().set(ctx, "__prek_handlers", handlers)?;
            let module_loaders = Table::new(&ctx);
            ctx.globals().set(ctx, "__prek_modules", module_loaders)?;
            let actions: Table = module.get(ctx, "actions")?;
            rule_api::register(&ctx, module.get(ctx, "rules")?, host.clone())?;
            rule_api::register_layers(
                &ctx,
                module.get(ctx, "layers")?,
                module.get(ctx, "rules")?,
                host.clone(),
            )?;
            actions.set(ctx, "__guard", handlers::guard(&ctx, host.clone()))?;
            let scheduler: Table = module.get(ctx, "scheduler")?;
            scheduler.set(ctx, "__duration", scheduler::duration(&ctx, host.clone()))?;
            scheduler.set(ctx, "__task_defer", tasks::defer(&ctx, host.clone()))?;
            scheduler.set(ctx, "sleep", tasks::sleep(&ctx, host.clone()))?;
            let log: Table = module.get(ctx, "log")?;
            log.set(ctx, "info", runtime::logger(&ctx, host.clone()))?;
            #[cfg(target_os = "linux")]
            {
                geometry::register(&ctx, module, host.clone())?;
                let context: Table = module.get(ctx, "context")?;
                for field in ["snapshot", "binding", "state", "fence", "error"] {
                    context.set(ctx, field, self::context::getter(&ctx, host.clone(), field))?;
                }
                let devices: Table = module.get(ctx, "devices")?;
                for field in ["list", "get", "revision", "position"] {
                    devices.set(ctx, field, self::devices::getter(&ctx, host.clone(), field))?;
                }
                let seats: Table = module.get(ctx, "seats")?;
                seats.set(
                    ctx,
                    "current",
                    self::devices::getter(&ctx, host.clone(), "seat"),
                )?;
            }
            Ok((
                ctx.stash(registry),
                ctx.stash(timers),
                ctx.stash(handlers),
                ctx.stash(module_loaders),
                ctx.stash(tasks),
            ))
        })
        .map_err(error)?;
    let bootstrap = start(&mut lua, "prek configuration API", BOOTSTRAP)?;
    // The pinned library/bootstrap contains no user code. Its initialization
    // is separate from the untrusted source's compilation/execution budget.
    drive_progress(
        &mut lua,
        &bootstrap,
        Instant::now(),
        Duration::from_secs(1),
        INSTRUCTIONS,
        progress,
    )?;
    lua.try_enter(|ctx| {
        ctx.fetch(&bootstrap)
            .take_result::<()>(ctx)
            .map_err(io::Error::other)??;
        Ok(())
    })
    .map_err(error)?;
    let started = Instant::now();
    for (module, approved) in modules {
        if started.elapsed() >= TIME {
            return Err(error("Lua module compilation time limit exceeded"));
        }
        progress().map_err(error)?;
        lua.try_enter(|ctx| {
            let name = format!("{name}:modules/{}.lua", module.replace('.', "/"));
            let closure = Closure::load(ctx, Some(&name), approved.source.as_bytes())?;
            ctx.fetch(&module_loaders)
                .set(ctx, ctx.intern(module.as_bytes()), closure)?;
            Ok(())
        })
        .map_err(error)?;
    }
    let executor = start(&mut lua, name, source)?;
    drive_progress(&mut lua, &executor, started, TIME, INSTRUCTIONS, progress)?;
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
    host.borrow_mut().configuring = false;
    let mut runtime = Runtime {
        lua,
        registry,
        timers,
        handlers,
        tasks,
        task_executors: Default::default(),
        host,
        config,
        failed: false,
    };
    let handlers = runtime.handler_registrations()?;
    runtime
        .host
        .borrow_mut()
        .rules
        .initialize(runtime.config.clone(), handlers);
    Ok(runtime)
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
            if symbol["receiver"] == "Subscription" {
                source.push_str("local handle=p.events.on('ready',function()end); assert(type(handle.dispose)=='function'); handle:dispose();\n");
                continue;
            }
            if symbol["receiver"] == "Timer" {
                source.push_str("local timer=p.scheduler.defer(function()end); assert(type(timer.cancel)=='function'); timer:cancel();\n");
                continue;
            }
            if symbol["receiver"] == "Task" {
                source.push_str("local task=p.scheduler.spawn(function()end); assert(type(task.cancel)=='function' and type(task.status)=='function'); task:cancel();\n");
                continue;
            }
            if symbol["receiver"] == "RuleTransaction" {
                let method = symbol["name"].as_str().unwrap().split('.').nth(1).unwrap();
                source.push_str(&format!("p.events.on('ready',function()local tx=p.rules.transaction();assert(type(tx.{method})=='function');tx:cancel()end);\n"));
                continue;
            }
            source.push_str(&format!(
                "assert(type(p.{}) == 'function');\n",
                symbol["name"].as_str().unwrap()
            ));
        }
        source.push_str("return {version=1}");
        let definition = Definition::compile(&source, "api-parity.lua").unwrap();
        let mut runtime = definition.instantiate().unwrap();
        runtime.enable_rule_transactions();
        runtime
            .dispatch(EventKind::Ready, &serde_json::json!({}))
            .unwrap();
    }
}
