//! Luna-backed, side-effect-free configuration evaluation. Runtime callbacks
//! and session adapters are separate APIs; this compiler never owns input fds.
use crate::config::{Config, ConfigError, MAX_CONFIG_BYTES};
use luna::{Callback, CallbackReturn, Closure, Executor, Fuel, Lua, StashedExecutor, Table, Value};
use std::{
    io,
    time::{Duration, Instant},
};

#[cfg(target_os = "linux")]
mod capabilities;
mod config_api;
#[cfg(target_os = "linux")]
mod context;
#[cfg(target_os = "linux")]
mod devices;
mod editor;
mod export;
#[cfg(target_os = "linux")]
mod geometry;
mod handlers;
mod hooks;
#[cfg(test)]
mod lifetimes;
mod modules;
#[cfg(target_os = "linux")]
mod morf;
#[cfg(test)]
mod native_values;
mod recognizers;
mod replay;
mod rule_api;
mod runtime;
mod scheduler;
#[cfg(target_os = "linux")]
pub mod session;
pub mod source_transfer;
#[cfg(target_os = "linux")]
mod status;
mod tasks;
mod values;
#[cfg(target_os = "linux")]
pub use capabilities::CapabilityView;
pub use editor::editor_definitions;
pub use export::export_config;
pub use handlers::{ActionResult, HandlerRegistration};
pub use hooks::RuleHook;
pub use modules::{Module, Sources};
pub use recognizers::{Binding as RecognizerBinding, RecognitionResult};
pub use rule_api::{RuleProposal, RuleReceipt};
pub(crate) use runtime::bounded_json;
pub use runtime::{Definition, EventKind, LogRecord, MAX_SNAPSHOT_BYTES, Registration, Runtime};
pub use scheduler::TimerRegistration;
#[cfg(target_os = "linux")]
pub use status::StatusView;
pub use tasks::TaskRegistration;

pub const API_VERSION: u32 = 1;
/// Implemented configuration exports. Runtime families remain explicit work
/// in PLAN.md; discovery never advertises an unimplemented binding.
pub fn api() -> serde_json::Value {
    let mut symbols = vec![
        serde_json::json!({"name":"config.new", "signature":"config.new(table) -> Config", "description":"Apply the schema version default; validate the returned model."}),
        serde_json::json!({"name":"config.preset", "signature":"config.preset(name) -> Config", "description":"Return a mutable minimal, desktop or phone preset."}),
        serde_json::json!({"name":"config.validate", "signature":"config.validate(config) -> Config", "phase":"configuration", "description":"Preflight expanded values, validate the complete Rust schema/model and return a normalized independent copy; no activation, reload, source binding approval or execution grant."}),
        serde_json::json!({"name":"config.current", "signature":"config.current() -> ConfigurationView | nil", "phase":"runtime", "description":"Copy the owning VM's acknowledged validated configuration and exact rule revision; no source closures or live effect authority."}),
        serde_json::json!({"name":"policy.current", "signature":"policy.current() -> PolicyView | nil", "phase":"runtime", "description":"Copy protected policy from the acknowledged configuration and its revision; negotiated execution grants remain separate."}),
        serde_json::json!({"name":"config.export", "signature":"config.export(config) -> string", "phase":"configuration", "description":"Validate and export a complete declarative model as bounded Lua source; performs no I/O and requires original source for named handlers."}),
        serde_json::json!({"name":"rules.define", "signature":"rules.define(table) -> Rule", "description":"Declare a rule; strict validation occurs at compilation."}),
        serde_json::json!({"name":"rules.on", "signature":"rules.on(id, phase, callback) -> Subscription", "phase":"configuration", "description":"Register a generation-scoped observational rule-phase callback for an existing configured rule; phases are discrete, begin, update, end and cancel; dispose with Subscription.dispose."}),
        serde_json::json!({"name":"layers.define", "signature":"layers.define(table) -> Layer", "description":"Declare a physical-key layer with physical, active or inactive activation; strict validation occurs at compilation."}),
        serde_json::json!({"name":"modes.define", "signature":"modes.define(table) -> NamedMode", "description":"Declare a named operating mode with optional initial active selection; at most one mode is selected per owning seat."}),
        serde_json::json!({"name":"units.duration", "signature":"units.duration(string) -> string", "description":"Declare an explicitly unit-qualified duration."}),
        serde_json::json!({"name":"units.distance", "signature":"units.distance(string) -> string", "description":"Declare an explicitly unit-qualified distance."}),
        serde_json::json!({"name":"events.on", "signature":"events.on(kind, callback) -> Subscription", "description":"Subscribe to approved semantic or lifecycle events in this session generation; registration has no evaluation effects."}),
        serde_json::json!({"name":"recognizers.define", "signature":"recognizers.define(name, options, callback) -> CustomTrigger", "phase":"configuration", "description":"Declare a source-bound recognizer with explicit permitted selectors, normalized input kinds and reply budget; callbacks return proposals to the native coordinator."}),
        serde_json::json!({"name":"Subscription.dispose", "receiver":"Subscription", "signature":"subscription:dispose() -> nil", "description":"Idempotently disable this subscription and release its callback reference; an invocation already running finishes normally."}),
        serde_json::json!({"name":"log.info", "signature":"log.info(message, fields = nil) -> nil", "description":"Append a bounded structured log in an event callback; unavailable during configuration evaluation."}),
        serde_json::json!({"name":"scheduler.after", "signature":"scheduler.after(duration, callback) -> Timer", "description":"Schedule one callback after an explicitly unit-qualified monotonic delay in this VM generation."}),
        serde_json::json!({"name":"scheduler.every", "signature":"scheduler.every(period, callback) -> Timer", "description":"Schedule periodic callbacks; preserve phase and skip missed ticks instead of catching up in a burst."}),
        serde_json::json!({"name":"scheduler.defer", "signature":"scheduler.defer(callback) -> Timer", "description":"Schedule a callback on a later runtime wake using a one-millisecond delay."}),
        serde_json::json!({"name":"Timer.cancel", "receiver":"Timer", "signature":"timer:cancel() -> nil", "description":"Idempotently cancel this generation-scoped timer and release its callback reference; an invocation already running finishes normally."}),
        serde_json::json!({"name":"scheduler.spawn", "signature":"scheduler.spawn(callback) -> Task", "description":"Start a bounded generation-scoped coroutine on a later scheduler wake; never execute it during configuration evaluation."}),
        serde_json::json!({"name":"scheduler.sleep", "signature":"scheduler.sleep(duration) -> TaskWake", "description":"Suspend the current scheduler task for a unit-qualified monotonic delay and return its next wake context."}),
        serde_json::json!({"name":"Task.cancel", "receiver":"Task", "signature":"task:cancel() -> nil", "description":"Cancel future resumes and release retained coroutine state at the next scheduler boundary."}),
        serde_json::json!({"name":"Task.status", "receiver":"Task", "signature":"task:status() -> TaskState", "description":"Inspect pending, running, sleeping, waiting, completed or cancelled state in the owning generation."}),
        serde_json::json!({"name":"Task.join", "receiver":"Task", "signature":"task:join() -> TaskTerminal, TaskWake?", "description":"Inside a scheduler task, wait without polling for this exact task to complete or cancel; resume on a later bounded wake with its terminal state and fresh wake payload. Already-terminal targets return immediately with nil wake. Self-joins and cycles raise errors."}),
        serde_json::json!({"name":"actions.define", "signature":"actions.define(name, {requires = capabilities}, callback) -> Action", "description":"Register an approved generation-scoped named handler; resolve a bounded typed effect plan in the unprivileged session after a broker execution grant."}),
        serde_json::json!({"name":"replay.new", "signature":"replay.new(config = nil) -> Replay", "phase":"configuration_and_runtime", "description":"Create an offline observation or configured replay; custom callbacks require the approved runtime source and fresh isolated closures, never live effects."}),
        serde_json::json!({"name":"replay.routing", "signature":"replay.routing(config) -> Replay", "phase":"configuration_and_runtime", "description":"Simulate downstream routing with explicit recorded readiness, dual clocks and exact write receipts; require route mode and explicit transport retirement, never kernel I/O or effects."}),
        serde_json::json!({"name":"Replay.push", "receiver":"Replay", "signature":"replay:push(record_json) -> nil", "description":"Apply one bounded recording JSON object; configured context is simulation data only, and malformed, stale or excessive input retires the replay."}),
        serde_json::json!({"name":"Replay.finish", "receiver":"Replay", "signature":"replay:finish(check = false) -> ReplayResult", "description":"Finish once, optionally require exact native expectations, and return copied outcomes plus lossless CLI-compatible JSON; release replay resources."}),
        serde_json::json!({"name":"Replay.status", "receiver":"Replay", "signature":"replay:status() -> ReplayStatus", "description":"Copy this generation-local replay's state, mode, record/byte counts and outcome count."}),
        serde_json::json!({"name":"Replay.close", "receiver":"Replay", "signature":"replay:close() -> nil", "description":"Idempotently discard this offline replay and release its open-handle capacity."}),
    ];
    #[cfg(target_os = "linux")]
    for (name, signature, description) in [
        (
            "status.current",
            "status.current() -> StatusView | nil",
            "Copy fresh broker-stamped mode, reported context state/revision, queued reply count and errors for this session fence; no desktop or effect authority is inferred.",
        ),
        (
            "morf.providers",
            "morf.providers() -> MorfProviderBinding[] | nil",
            "Copy fresh broker-verified native shell connection bindings for the owning desktop/output; no rendered regions, lock proof, recognizer ownership or action grant is inferred.",
        ),
        (
            "morf.regions",
            "morf.regions() -> MorfRenderedContext[] | nil",
            "Copy fresh native primary-frame named bounds in surface-local coordinates with output, endpoint/UI generation and fence; inspection only, without input-mask, keyboard-absence, authentication or compositor-presentation authority.",
        ),
        (
            "capabilities.current",
            "capabilities.current() -> CapabilityView | nil",
            "Copy fresh broker-negotiated flags and parent executor readiness for this session fence; neither grants effects.",
        ),
        (
            "capabilities.has",
            "capabilities.has(name) -> boolean",
            "Test fresh negotiated membership; unknown bounded names and unavailable snapshots return false.",
        ),
        (
            "capabilities.ready",
            "capabilities.ready(name) -> boolean",
            "Test current parent executor capacity for commands, lua_actions, power_actions or hyprland_actions; every effect still requires fresh context, target checks and a broker grant.",
        ),
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
    for (name, signature) in [
        ("list", "modes.list() -> NamedMode[]"),
        ("get", "modes.get(id) -> NamedMode | nil"),
        ("current", "modes.current() -> NamedMode | nil"),
        ("revision", "modes.revision() -> string"),
        ("transaction", "modes.transaction() -> RuleTransaction"),
    ] {
        symbols.push(serde_json::json!({"name":format!("modes.{name}"),"signature":signature,"phase":"runtime_callback","description":"Query copied acknowledged named modes or open the shared seat-local rule/layer/mode transaction; mode selection does not change Observe/Route authority."}));
    }
    for (name, signature, description) in [
        (
            "select_mode",
            "transaction:select_mode(id) -> nil",
            "Stage selecting one approved named mode and deactivating all others; fresh input is required after acknowledgement.",
        ),
        (
            "reset_mode",
            "transaction:reset_mode() -> nil",
            "Stage clearing the named mode selection; unconditional rules remain eligible.",
        ),
    ] {
        symbols.push(serde_json::json!({"name":format!("RuleTransaction.{name}"),"receiver":"RuleTransaction","signature":signature,"phase":"runtime_callback","description":description}));
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
            "layouts",
            "NativeLayouts | nil",
            "Copy native layout/column metadata matched to the fresh owning desktop snapshot and fence; nil when unavailable, inhibited or expired; no camera bounds, rendered scope or action authority.",
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
    let hyprland_adapter = serde_json::json!({
        "public_session_negotiation": true, "production_routing_available": false,
        "system_permission": "policy.allow_hyprland_actions", "agent_flags": ["hyprland"],
        "named_handler_flags": ["lua","hyprland"], "broker_mode": "route",
        "version": "0.54.3", "host_api": "actions::Executor::set_hyprland_adapter(HyprlandAdapter)",
        "additional_versions": ["0.56.2"], "config_provider": "hyprlang or lua; frozen at installation and rechecked via j/status before each 0.56.2 mutation",
        "lua_dispatch": "closed typed factories check frozen stable_id, workspace, window/output geometry or explicit power monitor name; no raw dispatch API, nil target or syntax fallback",
        "capability": "hyprland_actions", "actions": ["close","move_left_and_follow","move_right_and_follow","reorder_up","reorder_down"],
        "excluded": ["scroll_stack"],
        "reorder": {"version":"0.56.2","provider":"lua","layout":"scrolling with unambiguous vertical native column order","context":"complete same-generation/revision/capture native graph before claim and frozen source/neighbor/workspace at offer","neighbor":"adjacent vertical column; unique closest horizontally overlapping member","refusals":["stack boundary","ambiguous alignment","horizontal or overlapping columns","hidden/floating/grouped/fullscreen app","missing or changed native graph"],"dispatch":"one explicit-source/explicit-neighbor native swap; whole-graph checks in native factory","completion":"observed exact pair permutation with unchanged workspace/column topology; camera repositioning is not a failure"},
        "workspace_moves": "adjacent positive numeric workspace on a single-output desktop",
        "workspace_id_min": 1, "workspace_id_max": 2147483647, "desktop_operations_per_plan": 1,
        "direct_phases": ["discrete","begin"],
        "target": "explicit frozen window address plus stableId, output, geometry generation and workspace; focus never selects a target",
        "race": "legacy address selectors retain a read/dispatch race; Lua factories recheck native target fields; no atomic lock/overlay or rendered-visibility claim",
        "deadline": "remaining broker grant and 200ms total IPC budget",
        "completion": "accepted dispatch plus observed target disappearance or exact workspace/output/focus",
        "uncertain": "any failure after mutation bytes were written; never retry",
        "cage_flags": ["layers","input","power","windows"]
    });
    let action_requires = serde_json::json!({
        "agent_flags":["lua","commands"],"capabilities":["context","observations","lua_events","lua_actions","commands"],
        "implemented_effects":["commands","observations","power_actions","hyprland_actions"],
        "power_alternative":{"agent_flags":["lua","power"],"system_permission":"policy.allow_power_actions","broker_mode":"route","target":"explicit frozen output"},
        "hyprland_alternative":{"agent_flags":["lua","hyprland"],"system_permission":"policy.allow_hyprland_actions","broker_mode":"route","target":"frozen window/output/workspace"},
        "grant_time_ms":250
    });
    let morf_adapter = serde_json::json!({"agent_flags":["morf"],"named_handler_flags":["lua","morf"],"public_session_negotiation":false,"production_gate":"native rendered context and physical cutover","transport":"broker offer ID/ordinal and typed action only; no caller target, motion or deadline","completion":"native controller acceptance; no rendered animation/presentation proof","pending_agent":8,"pending_broker":64,"morf_effects_per_ordinal":1,"result_wait_after_grant_ms":2000,"native_receipt_poll_ms":5,"ownership_refresh_ms":100,"cancellation":"known owning gesture only; no uncertain retry"});
    let custom_recognizers = serde_json::json!({"constructor":"recognizers.define","agent_flags":["lua","recognizers"],"capability":"custom_recognizers","system_permission":"policy.allow_input_events","production_broker":"observation_mode","context":"fresh owning desktop without secure overlay","inputs":["key","relative","wheel","absolute","switch","touch"],"declaration_registry":"Config.recognizers; canonical source-approved definitions persist without rules; runtime additions need no template and cannot add or alter definitions","maximum_declarations":64,"maximum_candidates_and_cleanup":64,"maximum_pending_tickets":64,"ticket_bytes":65536,"pending_bytes":1048576,"reply_ms":[1,250],"timeout_ms":[1,10000],"wake_ms":[5,250],"callback_ms":20,"callback_instructions":100000,"data_bytes":4096,"data_depth":8,"identities":"canonical decimal strings","result":"nil for pending or a strict phase/data table; cancellation returns nil","source":"private canonical declaration, exact rule binding and complete approved snapshot parity","claims":false,"raw_frames":false,"routing":"unavailable","custom_sequences":"source-bound nested plans; native coordinator required","device_catalog":"independent declaration-scoped metadata without held state or ordinary input callbacks; ordinary resets when --input is also enabled","root_acceptance":"pending user-launched live tests"});
    let capability_queries = serde_json::json!({"source":"broker-negotiated registration flags and parent executor readiness; never inferred from Lua source","freshness_ms":250,"refresh_ms":100,"queued_snapshots":1,"in_flight_snapshots":1,"ready_families":["commands","lua_actions","power_actions","hyprland_actions","morf_actions"],"hyprland":"protected policy.allow_hyprland_actions and session --hyprland on a routing broker; production takeover remains unavailable","unavailable":"current returns nil; has and ready return false","unknown_names":"false; names must be strings of 1 to 128 bytes","grants":false,"lifetime":"negotiated flags immutable within the owning fence; stop clears metadata"});
    let native_value_limits = serde_json::json!({
        "accounting":"expanded keys and values; aliases counted for every expansion; conservative bytes are not measured RSS",
        "configuration":{"depth":64,"values":values::CONFIG_VALUES,"conservative_bytes":values::CONFIG_EXPANDED_BYTES},
        "collections":{"rules":crate::config::MAX_RULES,"devices":crate::config::MAX_SELECTORS,"layers":crate::config::MAX_LAYERS,"shapes":crate::config::MAX_SHAPES,"modes":crate::config::MAX_MODES,"recognizers":crate::recognition::MAX_RECOGNIZERS},
        "action_plan":{"depth":8,"values":values::EFFECT_VALUES,"conservative_bytes":values::EFFECT_EXPANDED_BYTES},
        "rule_replacement":{"depth":64,"values":crate::rule_transactions::MAX_BYTES,"conservative_bytes":16 * crate::rule_transactions::MAX_BYTES},
        "offline_replay_configuration":{"depth":64,"values":replay::MAX_CONFIG_VALUES,"conservative_bytes":replay::MAX_BYTES},
        "metadata":{"depth":4,"hook_or_effect_values":4096,"hook_or_effect_bytes":65536,"recognizer_values":65536,"recognizer_bytes":1048576},
        "preset_name_bytes":64,"rule_identity_bytes":256
    });
    let mut manifest = serde_json::json!({"api_version":API_VERSION,"schema_version":crate::config::SCHEMA_VERSION,
        "engine":"onix-os/luna", "engine_version":"0.5.1", "phase":"configuration_and_session_actions",
        "symbols":symbols, "limits":{"source_bytes":MAX_CONFIG_BYTES,"memory_bytes":MEMORY,
        "instructions":INSTRUCTIONS,"time_ms":TIME.as_millis(),"value_depth":64},
        "runtime_limits":{"subscriptions":256,"timers":256,"tasks":tasks::MAX_TASKS,"tasks_per_wake":tasks::TASKS_PER_WAKE,"handlers":256,"action_effects":32,"action_bytes":32768,"timers_per_wake":32,"callback_instructions":100000,"callback_time_ms":20,"event_bytes":65536,"log_bytes":16384,"logs_per_event":32},
        "modules":{"root":"entry_parent/modules","path":"<identifier>/.../<identifier>.lua","count":modules::MAX_MODULES,"import_depth":modules::MAX_IMPORT_DEPTH,"source_set_bytes":MAX_CONFIG_BYTES,"owner":"entry_file_uid","group_other_writes":false,"symlinks":false,"runtime":"approved sources only; no filesystem reads"},
        "event_kinds":["input","matched","begin","update","end","cancel","skipped","ready","reload","stop","recovery","session_changed","context_changed","rules_changed"],
        "runtime_identities":{"type":"decimal_string","range":"0..18446744073709551615","fields":["fence.broker[]","fence.configuration","fence.session","event.id","event.ordinal","event.source.device","event.source.generation","event.config_generation","event.session_generation","Snapshot.generation","Snapshot.revision","OutputState.geometry_generation","Binding.compositor_start","context_changed.revision","rules.revision","input.source.device","input.source.generation","input.sequence","input.contacts[].id.generation","input.contacts_started[].generation","input.contacts_ended[].generation","input.ended_contacts[].id.generation"],"comparison":"exact string equality; no numeric conversion","bounded_handles":"subscription/timer/task IDs remain Lua integers","protocol":"Rust transport retains u64 values"},
        "rule_transactions":{"builders":64,"operations":64,"transaction_bytes":32768,"pending_proposals":1,"receipt_time_ms":250,"revision_type":"decimal_string","transport":"explicit authorized host opt-in","production_broker":"observation_mode","system_permission":"policy.allow_runtime_rules","agent_flags":["lua","rule-mutations"],"capability":"runtime_rules","context":"fresh owning desktop","routing":"unavailable"},
        "runtime_layers":{"declared_layers":64,"activation_values":["physical","active","inactive"],"default":"physical","queries":"acknowledged definitions and activation overrides; no physical-key state","transaction":"shared RuleTransaction revision/receipt/permissions","definitions":"immutable at runtime","activation":"fresh input only; existing candidates retired","physical_toggles":"preserved across runtime edits; reconciled on source reload","seat":"owning broker registration"},
        "runtime_modes":{"declared_modes":64,"selected_modes":1,"default":"no selection","rule_condition":"when_mode","queries":"owned acknowledged definitions and active selection","transaction":"shared RuleTransaction revision/receipt/permissions","definitions":"immutable at runtime","activation":"fresh input only; pending and active candidates retired","authority":"does not alter Observe/Route, policy or capability grants","lifetime":"owning seat registration; source reload restores declared initial selection"},
        "rule_hooks":{"registration":"configuration only; rule ID must exist in the validated initial configuration","phases":["discrete","begin","update","end","cancel"],"inline_fields":["on_discrete","on_begin","on_update","on_end","on_cancel"],"budget":"shared event subscription, fuel, time and log limits","delivery":"rule ID and phase filtered in the owning VM; registration order; updates may coalesce","effects":"observational; no returned effects, input claims or synthetic ownership","lifetime":"approved source generation; follows stable rule ID across acknowledged edits; removed rules emit no fresh matches; reload disposes the old VM","export":"callbacks require original Lua source"},
        "input_events":{"event":"input","configuration_mode":"observe","agent_flags":["lua","input"],"capability":"input_events","system_permission":"policy.allow_input_events","production_broker":"observation_mode","sources":"explicit policy.allowed_devices selectors on the owning seat; virtual devices excluded","context":"fresh desktop without secure overlay","freshness_ms":250,"payload":"InputPayload","raw_frames":false,"input_claims":false,"delivery":"ordered, no coalescing; overflow retires the affected stream or Lua component","resets":["attached","reconciled","mapping_changed","removed","lost"],"coordinates":"device axis counts","maximum_changes":4096,"maximum_contacts":256,"identity_fields":["source.device","source.generation","sequence","contacts[].id.generation","contacts_started[].generation","contacts_ended[].generation","ended_contacts[].id.generation"],"identity_type":"decimal_string"},
        "context_limits":{"snapshot_bytes":4194304,"chunk_bytes":32768,"transfer_time_ms":1000,"freshness_ms":250,"refresh_ms":100,"queued_snapshots":2,"read_only":true},
        "morf_provider_queries":{"role":"shell","capability":"context","freshness_ms":250,"maximum_outputs":64,"discovery_ms":1000,"ipc_budget_ms":200,"phases":{"pending":1,"configured":2},"identity":"native Wayland socket credentials, exact executable/process, logind registration and bound compositor; connection epoch is independent of Lua configuration reload","integers":"compositor_start and fence identities are canonical decimal strings","unavailable":"nil without a fresh owning desktop; empty array when no native provider is currently verified","authority":"read-only connection/configure metadata; no presentation, rendered regions, authentication, recognizer ownership or action grant"},
        "capability_queries":capability_queries,
        "geometry_queries":{"context":"fresh owning desktop without secure overlay","lookup":"exact output name or rendered region ID","references":"canonical decimal-string fence, snapshot revision and output geometry generation; copies confer no authority","spaces":["normalized","local","global"],"normalized":"post-transform output axes, not raw device axes","local":"post-transform logical units; rotation and scale applied through output logical size","region_bounds":"global logical, half-open","overlaps":"all matches in provider order, no target selection","unavailable":"nil for absent/stale/inhibited context, missing name or retired reference; known empty arrays remain empty","conversion":"finite numeric points, no clamping; arithmetic overflow raises an error","effects":false},
        "runtime_requires":{"uid":"unprivileged active session","capabilities":["context","observations","lua_events"],"thread":"owning Luna arena","generation":"current broker/configuration/session fence"},
        "action_requires":action_requires,
        "experimental_adapters":{"power":{"public_session_negotiation":true,"broker_mode":"route","production_routing_available":false,"system_permission":"policy.allow_power_actions","agent_flags":["power"],"named_handler_flags":["lua","power"],"rule_output":"explicit name or validated input mapping; no focus fallback","host_api":"actions::Executor::set_power_adapter(PowerAdapter)","actions":["blank","wake"],"excluded":["lock_and_blank","toggle","suspend"],"target":"explicit frozen owning output, exact geometry generation and binding","deadline":"remaining broker grant and 200ms total IPC budget","completion":"accepted dispatch and observed compositor-reported DPMS state; no hardware power claim","uncertain":"any failure after a mutating request may have been written; never retry","cage_flags":["layers","input","power"]}},
        "hyprland_adapter":hyprland_adapter,
        "editor":{"format":"LuaCATS","module":"prek","cli":["prek","api","--lua"],"path":"share/prek/lua/prek.lua","scope":"implemented_configuration_and_runtime"},
        "device_queries":{"catalog":"permitted metadata received through input resets or declaration-scoped custom recognizer metadata; no host inventory scan","delivery":"ordinary --input resets, or independent --recognizers metadata without held state or ordinary input callbacks; current owning context and capabilities required","maximum_sources":256,"maximum_bytes":4194304,"lookup":"exact Source device/generation/seat with canonical decimal strings","nil":"no received metadata, unknown source, missing mapping or unavailable/stale desktop context","empty_list":"all known sources removed while context remains usable","revision":"decimal string; metadata changes/removals only","map_keys":"numeric evdev type/axis keys in Lua","position":"validated device counts to normalized/mm/output-local/global coordinates; no action authority","seat":"current fresh desktop binding/fence only; no other-seat enumeration"},
        "runtime_api":"partial"});
    manifest["native_value_limits"] = native_value_limits;
    manifest["custom_recognizers"] = custom_recognizers;
    manifest["custom_recognizers"]["candidate_state"] = serde_json::json!({
        "key_activity":"physical button keys only when key is declared; contact/tool flags are excluded",
        "touch_activity":"contacts only when touch is declared; only these declarations retain contact identities, joining and peak counts",
        "mixed_activity":"key and touch declarations remain active until both held families are neutral",
        "other_families":"relative, wheel, absolute and switch values do not create held key/contact state",
        "exclusive_progress":"begin/update after the declared held domains finish is rejected",
        "helper_reentry":"already-held declared domains must become neutral before a new candidate"
    });
    manifest["morf_adapter"] = morf_adapter;
    manifest["service_inspection"] = serde_json::json!({
        "configuration":"acknowledged Rust model and canonical decimal-string rule revision; closures require original source",
        "policy":"protected permissions in that model; not negotiated execution grants",
        "phase":"runtime callback or task; independent of desktop context",
        "configuration_unavailable":"nil when the acknowledged rule view has retired",
        "status":{"freshness_ms":250,"refresh_ms":100,"timestamp":"original broker packet time; never renewed by parent/helper delivery",
            "identity":"current broker/configuration/session fence; revision and fence words are canonical decimal strings",
            "pending":"queued outgoing replies on the requesting agent connection; not global gestures or input ownership",
            "state":"broker-reported state, not native lock completion or authentication",
            "transport":"latest status accompanies bounded coalesced capability metadata without renewing either clock",
            "unavailable":"nil before a broker report, after expiry, on a different fence or at stop"},
        "authority":false
    });
    manifest["offline_replay"] = serde_json::json!({
        "phases":["configuration","runtime"],"effects":false,"io":false,
        "modes":["observations","rules","routing"],"input":"one native versioned recording JSON object per push; routing uses the separate routing-tagged trace schema",
        "routing":{"version":1,"readiness":"recorded descriptor/consumer/feedback and neutral source; no live grant","delivery":"explicit poll/write tickets; exact raw scheduled frames; independent input/wall clocks","finish":"every admitted transport explicitly retired or recovered","effects":false},
        "configuration":"validated copy; no live rule or policy change",
        "custom":{"phase":"runtime","source":"fresh approved source VM per recorded seat; no live callback state","clock":"recorded time only; real execution budgets remain","maximum_shared_vms":4,"maximum_context_seats":16,"work_budget_ms":250,"nested_custom_replay":false,"live_authority":false},
        "recorded_context":"configured replay only: record=context with at, binding and snapshot; generic and live engines reject it",
        "expectations":"finish(true) requires matching final expected, expected_rules or routing=expected; unchecked replay never claims a passing test",
        "identities":"outcome tables use canonical decimal strings for opaque identities and unsigned values above Lua's signed range; result.json preserves exact native CLI JSON",
        "lifetime":"owning VM generation; close/finish/error releases engine; no external handles",
        "maximum_open":replay::MAX_HANDLES,"maximum_records":replay::MAX_RECORDS,
        "maximum_record_bytes":replay::MAX_RECORD_BYTES,"maximum_input_bytes":replay::MAX_BYTES,
        "maximum_outcome_bytes":replay::MAX_BYTES,"maximum_sources":16,"maximum_outcomes":4096,
        "maximum_configuration_values":replay::MAX_CONFIG_VALUES,
        "failure":"malformed records, engine errors and resource/expectation failures retire that replay; ordinary uncaught callback errors retire the VM"
    });
    manifest["morf_rendered_queries"] = serde_json::json!({
        "capability":"context","freshness_ms":250,"ipc_budget_ms":200,
        "maximum_regions":256,"maximum_reply_bytes":131072,
        "coverage":"primary_surface_named_bounds","coordinates":"surface-local logical bounding boxes",
        "identity":"verified native output/endpoint epoch, UI generation and broker fence",
        "integers":"node identity, UI generation, revision and sampled_ns are canonical decimal strings",
        "unavailable":"nil without a fresh owning desktop; empty array when no fresh native frame is available",
        "authority":"inspection only; no input-mask, keyboard-absence, authentication, stacking or presentation proof",
        "scopes":false,
        "layout_projection":{
            "basis":"stock_layer_layout",
            "coordinates":"global logical layout boxes; not animated rendered positions",
            "identity":"unique stable native producer PID, process birth, output, layer namespace/address/level/box and geometry generation",
            "missing_bounds":"viewport-clipped rows retain their native identities with null bounds",
            "optional":true
        },
        "keyboard_registry":{
            "capability":"keyboard_registry","scope":"registered_morf_primary",
            "states":["unknown","hidden","visible"],
            "known":"one declared keyboard under the primary root, with node and layout",
            "absence":"hidden applies only to the registered Morf node; does not set snapshot keyboard_known"
        },
        "surface_space":{
            "capability":"layer_space","fields":["namespace","width","height","local_scale"],
            "local_scale":"compositor logical units per native Morf layout unit"
        }
    });
    manifest["morf_rendered_queries"]["semantic"] = serde_json::json!({
        "capability":"semantic_regions","property":"input_context","fields":["role","layout"],
        "roles":["keyboard","secure_overlay","clock_background","content_bottom","physical_bottom","top_left","top_right","side_left","side_right","local_control"],
        "layout":"required for keyboard only; 1 to 128 bytes without control characters",
        "coordinates":"native frame bounds only; caller coordinates rejected",
        "absence":"does not establish hidden keyboard or complete coverage",
        "ownership":false
    });
    manifest["keyboard_context"] = serde_json::json!({
        "knowledge":"OutputState.keyboard_known; false means unknown even without geometry",
        "visible":"known with keyboard geometry","hidden":"known without keyboard geometry",
        "unknown":"KeyboardUnavailable for visibility conditions and awake spatial scopes",
        "independent":"explicit global rules and explicit screen-off scopes need not infer keyboard state",
        "loss":"invalidates the frozen output target","stock_compositor":"unknown",
        "native_annotations":"inspection only until complete verified mapping and coverage"
    });
    manifest["native_layout_queries"] = serde_json::json!({
        "capability":"context","backend":"Hyprland 0.56.2 Lua","freshness_ms":250,
        "combined_ipc_budget_ms":200,"optional_ipc_budget_ms":50,"maximum_workspaces":64,"maximum_mapped_windows":512,
        "identity":"same owning desktop sample, generation, revision and conservative timestamp; current broker fence",
        "integers":"generation/revision/fence identities are canonical decimal strings; window lifetime identities are address:stable hexadecimal strings",
        "columns":"zero-based native indices/rows; one-based Lua arrays; width is a native primary-axis fraction",
        "coordinates":"integer native animation-goal rectangles [x,y,width,height], not rendered hit tests",
        "unavailable":"nil for unsupported, unavailable, inconsistent, non-desktop, stale or inhibited context; a verified empty inventory is an empty workspace array",
        "authority":"inspection only; no camera/work-area/effective-rule bounds or input ownership/grants",
        "effects":false
    });
    manifest["task_dependencies"] = serde_json::json!({
        "operation":"Task.join","states":["pending","running","sleeping","waiting","completed","cancelled"],
        "terminal":["completed","cancelled"],"maximum_edges":tasks::MAX_TASKS,
        "scheduling":"one later wake; shared eight-resume budget and deadline/identity ordering",
        "polling":false,"cycles":"self and dependency cycles raise errors",
        "identity":"exact private task entry; capacity reuse never retargets a handle",
        "cancellation":"target wakes joiners; waiter cancellation does not cancel its target",
        "result":"terminal state and fresh TaskWake after suspension; nil wake for an already-terminal target",
        "authority":"observational owning-VM dependency only; no execution grant or native input ownership"
    });
    manifest
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
local rule_sites = setmetatable(__prek_rule_sites, {__mode="k"})
__prek_rule_sites = nil
local rule_site = api.rules.__site
api.rules.__site = nil
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
    return {cancel=function(self)entry.active=false; entry.callback=nil end}
end
function api.scheduler.after(delay, callback) return schedule(delay,callback,false) end
function api.scheduler.every(period, callback) return schedule(period,callback,true) end
function api.scheduler.defer(callback) return schedule("1ms",callback,false) end
local task_defer = api.scheduler.__task_defer
api.scheduler.__task_defer = nil
local task_join = api.scheduler.__task_join
api.scheduler.__task_join = nil
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
        join=function(self)
            local wake=task_join(entry)
            return entry.state,wake
        end,
    }
end
local allowed_events = {
    input=true,
    matched=true, begin=true, update=true, ["end"]=true, cancel=true,
    skipped=true, ready=true, reload=true, stop=true, recovery=true,
    session_changed=true, context_changed=true, rules_changed=true,
}
local function subscribe(kind, callback, rule)
    assert(allowed_events[kind], "unsupported event subscription")
    assert(type(callback) == "function", "events.on expects a callback")
    assert(#registrations < 256, "Lua subscription limit exceeded")
    local entry = {id=#registrations+1, kind=kind, callback=callback, active=true, rule=rule}
    registrations[entry.id] = entry
    return {dispose=function(self) entry.active=false; entry.callback=nil end}
end
function api.events.on(kind, callback) return subscribe(kind,callback,nil) end
local recognizer_guard=api.recognizers.__define
api.recognizers.__define=nil
local recognizer_names={}
local recognizer_count=0
function api.recognizers.define(name, options, callback)
    assert(type(name)=="string", "recognizer name must be a string")
    assert(type(callback)=="function", "recognizer expects a callback")
    assert(not recognizer_names[name], "duplicate recognizer name")
    assert(recognizer_count<64, "Lua recognizer limit exceeded")
    assert(#registrations<256, "Lua subscription limit exceeded")
    local trigger, declaration=recognizer_guard(name,options)
    local entry={id=#registrations+1,kind="input",callback=callback,active=true,recognizer=declaration}
    registrations[entry.id]=entry
    recognizer_names[name]=true
    recognizer_count=recognizer_count+1
    return trigger
end
local hook_guard=api.rules.__hook_guard
api.rules.__hook_guard=nil
local rule_phases={discrete=true,begin=true,update=true,["end"]=true,cancel=true}
function api.rules.on(id,phase,callback)
    hook_guard(false)
    assert(type(id)=="string" and #id>0 and #id<=256, "invalid rule hook ID")
    assert(rule_phases[phase], "unsupported rule hook phase")
    local handle=subscribe(phase=="cancel" and "cancel" or "matched",callback,{id=id,phase=phase})
    hook_guard(true)
    return handle
end
function api.config.new(value)
    assert(type(value) == "table", "config.new expects a table")
    if value.version == nil then value.version = 1 end
    return value
end
function api.rules.define(value)
    assert(type(value) == "table", "rules.define expects a table")
    rule_sites[value] = rule_site()
    for _,phase in ipairs({"discrete","begin","update","end","cancel"}) do
        local key="on_" .. phase
        if value[key]~=nil then
            api.rules.on(value.id,phase,value[key])
            value[key]=nil
        end
    end
    return value
end
function api.layers.define(value)
    assert(type(value) == "table", "layers.define expects a table")
    return value
end
function api.modes.define(value)
    assert(type(value) == "table", "modes.define expects a table")
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
    let (registry, timers, handlers, module_loaders, tasks, rule_sites) = lua
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
                "modes",
                "triggers",
                "recognizers",
                "actions",
                "units",
                "events",
                "log",
                "scheduler",
                "context",
                "capabilities",
                "devices",
                "seats",
                "outputs",
                "regions",
                "coordinates",
                "morf",
                "replay",
                "status",
                "policy",
            ] {
                module.set(ctx, namespace, Table::new(&ctx))?;
            }
            let config: Table = module.get(ctx, "config")?;
            config.set(
                ctx,
                "preset",
                Callback::from_fn(&ctx, |ctx, _, mut stack| {
                    let name: luna::String = stack.consume(ctx)?;
                    let name = name.to_str().map_err(io::Error::other)?;
                    if name.len() > 64 || name.chars().any(char::is_control) {
                        return Err(io::Error::other("invalid preset name").into());
                    }
                    let preset = Config::preset(name).map_err(io::Error::other)?;
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
                    if export_host.borrow().rule_hooks {
                        return Err(io::Error::other("rule callbacks require original Lua source; declarative export would discard them").into());
                    }
                    if export_host.borrow().recognizer_callbacks {
                        return Err(io::Error::other("recognizer callbacks require original Lua source; declarative export would discard them").into());
                    }
                    let value: Value = stack.consume(ctx)?;
                    let config = values::configuration(ctx, value)?;
                    let source = export_config(&config).map_err(io::Error::other)?;
                    stack.replace(ctx, ctx.intern(source.as_bytes()));
                    Ok(CallbackReturn::Return)
                }),
            )?;
            ctx.globals().set(ctx, "__prek", module)?;
            let registry = Table::new(&ctx);
            ctx.globals().set(ctx, "__prek_registry", registry)?;
            let recognizers: Table = module.get(ctx, "recognizers")?;
            recognizers.set(ctx, "__define", self::recognizers::constructor(&ctx, host.clone()))?;
            let timers = Table::new(&ctx);
            ctx.globals().set(ctx, "__prek_timers", timers)?;
            let tasks = Table::new(&ctx);
            ctx.globals().set(ctx, "__prek_tasks", tasks)?;
            let handlers = Table::new(&ctx);
            ctx.globals().set(ctx, "__prek_handlers", handlers)?;
            let module_loaders = Table::new(&ctx);
            ctx.globals().set(ctx, "__prek_modules", module_loaders)?;
            let actions: Table = module.get(ctx, "actions")?;
            config_api::register(&ctx, config, host.clone())?;
            replay::register(&ctx, module.get(ctx, "replay")?, host.clone())?;
            rule_api::register(&ctx, module.get(ctx, "rules")?, host.clone())?;
            rule_api::register_inspection(&ctx, module, host.clone())?;
            let rules: Table = module.get(ctx, "rules")?;
            let hook_host = host.clone();
            rules.set(
                ctx,
                "__hook_guard",
                Callback::from_fn(&ctx, move |ctx, _, mut stack| {
                    if !hook_host.borrow().configuring {
                        return Err(io::Error::other(
                            "rule hook registration requires configuration evaluation",
                        )
                        .into());
                    }
                    if stack.consume::<bool>(ctx)? {
                        hook_host.borrow_mut().rule_hooks = true;
                    }
                    Ok(CallbackReturn::Return)
                }),
            )?;
            rule_api::register_layers(
                &ctx,
                module.get(ctx, "layers")?,
                module.get(ctx, "rules")?,
                host.clone(),
            )?;
            rule_api::register_modes(
                &ctx,
                module.get(ctx, "modes")?,
                module.get(ctx, "rules")?,
                host.clone(),
            )?;
            actions.set(ctx, "__guard", handlers::guard(&ctx, host.clone()))?;
            let scheduler: Table = module.get(ctx, "scheduler")?;
            scheduler.set(ctx, "__duration", scheduler::duration(&ctx, host.clone()))?;
            scheduler.set(ctx, "__task_defer", tasks::defer(&ctx, host.clone()))?;
            scheduler.set(ctx, "__task_join", tasks::wait(&ctx, host.clone()))?;
            scheduler.set(ctx, "sleep", tasks::sleep(&ctx, host.clone()))?;
            let log: Table = module.get(ctx, "log")?;
            log.set(ctx, "info", runtime::logger(&ctx, host.clone()))?;
            #[cfg(target_os = "linux")]
            {
                let capabilities: Table = module.get(ctx, "capabilities")?;
                let status: Table = module.get(ctx, "status")?;
                status.set(ctx, "current", self::status::getter(&ctx, host.clone()))?;
                for field in ["current", "has", "ready"] {
                    capabilities.set(ctx, field, self::capabilities::getter(&ctx, host.clone(), field))?;
                }
                geometry::register(&ctx, module, host.clone())?;
                let morf: Table = module.get(ctx, "morf")?;
                for field in ["providers", "regions"] {
                    morf.set(ctx, field, self::morf::getter(&ctx, host.clone(), field))?;
                }
                let context: Table = module.get(ctx, "context")?;
                for field in ["snapshot", "binding", "state", "fence", "error", "layouts"] {
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
            let rule_sites = Table::new(&ctx);
            ctx.globals().set(ctx, "__prek_rule_sites", rule_sites)?;
            let rules: Table = module.get(ctx, "rules")?;
            rules.set(
                ctx,
                "__site",
                Callback::from_fn(&ctx, |ctx, exec, mut stack| {
                    match exec.frame_at(1) {
                        Some(frame) => {
                            let site = format!(
                                "{}:{}",
                                frame.chunk_name.display_lossy(),
                                frame.current_line
                            );
                            stack.replace(ctx, ctx.intern(site.as_bytes()));
                        }
                        None => stack.replace(ctx, Value::Nil),
                    }
                    Ok(CallbackReturn::Return)
                }),
            )?;
            Ok((
                ctx.stash(registry),
                ctx.stash(timers),
                ctx.stash(handlers),
                ctx.stash(module_loaders),
                ctx.stash(tasks),
                ctx.stash(rule_sites),
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
            let config =
                self::values::located_configuration(ctx, values.0[0], ctx.fetch(&rule_sites))?;
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
        recognizers: Vec::new(),
        recognition_last: None,
        recognition_candidates: Default::default(),
    };
    let handlers = runtime.handler_registrations()?;
    let registrations = runtime.registrations()?;
    hooks::validate(&registrations, &runtime.config)?;
    runtime.recognizers = recognizers::bindings(&registrations);
    recognizers::merge(&mut runtime.config, &runtime.recognizers)?;
    recognizers::validate(&runtime.recognizers, &registrations, &runtime.config)?;
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
            if symbol["receiver"] == "Replay" {
                let method = symbol["name"].as_str().unwrap().split('.').nth(1).unwrap();
                source.push_str(&format!("local replay=p.replay.new();assert(type(replay.{method})=='function');replay:close();\n"));
                continue;
            }
            if symbol["receiver"] == "Subscription" {
                source.push_str("local handle=p.events.on('ready',function()end); assert(type(handle.dispose)=='function'); handle:dispose();\n");
                continue;
            }
            if symbol["receiver"] == "Timer" {
                source.push_str("local timer=p.scheduler.defer(function()end); assert(type(timer.cancel)=='function'); timer:cancel();\n");
                continue;
            }
            if symbol["receiver"] == "Task" {
                let method = symbol["name"].as_str().unwrap().split('.').nth(1).unwrap();
                source.push_str(&format!("local task=p.scheduler.spawn(function()end);assert(type(task.{method})=='function');task:cancel();\n"));
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
    #[test]
    fn every_live_binding_and_returned_method_is_in_the_manifest() {
        let known = api()["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .map(|symbol| format!("[{:?}]=true", symbol["name"].as_str().unwrap()))
            .collect::<Vec<_>>()
            .join(",");
        let source = format!(
            r#"
local p=require('prek')
local known={{{known}}}
local function check(name) assert(known[name], 'undocumented binding '..name) end
local function walk(prefix,t,depth)
 for k,v in pairs(t) do
  local name=prefix..tostring(k)
  if type(v)=='function' then check(name)
  elseif type(v)=='table' and depth<4 then walk(name..'.',v,depth+1) end
 end
end
local function methods(receiver,object)
 for k,v in pairs(object) do if type(v)=='function' then check(receiver..'.'..k) end end
end
walk('',p,0)
local replay=p.replay.new();methods('Replay',replay);replay:close()
local timer=p.scheduler.defer(function()end);methods('Timer',timer);timer:cancel()
local task=p.scheduler.spawn(function()end);methods('Task',task);task:cancel()
local handle=p.events.on('ready',function()
 local tx=p.rules.transaction();methods('RuleTransaction',tx);tx:cancel()
 p.log.info('methods-documented')
end)
methods('Subscription',handle)
return {{version=1,policy={{allow_runtime_rules=true}}}}
"#
        );
        let definition = Definition::compile(&source, "live-bindings.lua").unwrap();
        let mut runtime = definition.instantiate().unwrap();
        runtime.enable_rule_transactions();
        let logs = runtime
            .dispatch(EventKind::Ready, &serde_json::json!({}))
            .unwrap();
        assert!(logs.iter().any(|log| log.message == "methods-documented"));
    }
    #[test]
    fn invalid_rules_report_their_index_id_and_defining_statement() {
        let error = compile(
            "local p=require('prek')\n\
             local second=p.rules.define{id='second',tirgger=p.triggers.key{key='KEY_B'},action=p.actions.event{name='b'}}\n\
             return p.config.new{rules={\n\
              p.rules.define{id='first',trigger=p.triggers.key{key='KEY_A'},action=p.actions.event{name='a'}},\n\
              second,\n\
             }}",
            "bad-rules.lua",
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("bad-rules.lua:2: rules[2] (\"second\"): unknown field `tirgger`"),
            "{error}"
        );
        let error = compile(
            "return {version=1,rules={{id='plain',trigger={kind='key',key='KEY_NOPE'},action={kind='event',name='c'}}}}",
            "plain-rules.lua",
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("rules[1] (\"plain\"): unknown key \"KEY_NOPE\""),
            "{error}"
        );
        let error = compile(
            "return {version=1,devices={keyboard={kees={'KEY_A'}}}}",
            "bad-device.lua",
        )
        .unwrap_err()
        .to_string();
        assert!(!error.contains("rules["), "{error}");
    }
    #[test]
    fn every_manifest_symbol_is_cited_by_the_reference() {
        let reference = include_str!("../docs/lua-api.md");
        for symbol in api()["symbols"].as_array().unwrap() {
            let name = symbol["name"].as_str().unwrap();
            let cited = match name.split_once('.') {
                Some((receiver, method)) if receiver.starts_with(char::is_uppercase) => {
                    reference.contains(&format!(":{method}("))
                }
                _ => [
                    format!("{name}("),
                    format!("{name} {{"),
                    format!("`{name}`"),
                ]
                .iter()
                .any(|form| reference.contains(form)),
            };
            assert!(cited, "reference does not cite {name}");
        }
    }
}
