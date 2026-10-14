# Lua API

Prek embeds [onix-os/luna](https://github.com/onix-os/luna), pinned to version
0.5.1. Files ending in `.lua` return one configuration table. Legacy TOML is
also accepted. Configuration evaluation declares actions and does not execute
them. Live production routing remains disabled.

This reference covers configuration, session event subscriptions, timers and named
action handlers producing command/event plans. Custom recognizers, coroutine tasks,
live adapter bindings and the remaining operational APIs remain required
work in `PLAN.md`. Discovery labels the runtime API partial.
Run `prek api --json` for the installed manifest or inspect `prek.api` in Lua.

```lua
local prek = require("prek")
local config = prek.config.preset("minimal")
config.policy.allowed_devices = { "keyboard" }
config.devices.keyboard = { keys = { "KEY_A" } }
config.rules = {
    prek.rules.define {
        id = "a-held", devices = { "keyboard" },
        trigger = prek.triggers.button_hold {
            key = "KEY_A", after = prek.units.duration("600ms"),
        },
        action = prek.actions.event { name = "a_held" },
    },
}
return config
```

Create a preset with `prek init --preset minimal --output config.lua`, validate
it with `prek check --config config.lua`, and use it with `replay`, `doctor`,
`run` or `broker`. The broker validates its protected system file and transfers
the compiled Rust model to the device worker; that worker evaluates no Lua
while reading or forwarding input. The broker compiles reloads asynchronously
in a disposable unprivileged process and retains its last approved model on
failure. `prek session --lua` opts the authenticated session agent into approved
Lua subscriptions and implies observation/context capabilities. The broker
transfers the exact approved source, model and registration snapshot; the agent
starts a separate unprivileged VM and verifies its model and registrations.
It never rereads the configuration filepath. Standalone `run` evaluates rules;
session callbacks require the authenticated agent.

## Session callbacks

Register subscriptions during configuration evaluation:

```lua
local p = require("prek")
local seen = 0
local subscription = p.events.on("matched", function(payload)
    seen = seen + 1
    p.log.info("matched rule", {rule = payload.event.rule, count = seen})
end)
return p.config.preset("minimal")
```

`events.on(kind, callback)` returns a `Subscription` table.
`subscription:dispose()` is idempotent and disables subsequent invocation,
including a later subscriber for the event currently being dispatched.
Subscriptions execute in registration order and retain closure state. A
subscription added by a callback starts with the next event. At most 256
subscriptions can be registered over one VM's lifetime, including disposed ones.

Supported kinds are `matched` (all matching phases), `begin`, `update`, `end`,
`cancel`, `skipped`, `ready`, `reload`, `session_changed`, `recovery` and `stop`.
Rule callbacks receive `{fence, event}`; `event` is the existing typed rule
outcome, including its source, monotonic time and applicable gesture identity,
ordinal, stage, action declaration and frozen target. No raw input is supplied.
The fence contains broker, configuration and session generations. `ready`
receives `{fence, seat}`; reload/stop receive `{fence}` and session/recovery
receive `{fence, state}`. Recovery here describes session inhibition, not all
future device/routing recovery hooks. Stop delivery has a 250 ms grace; stalled
or still-initializing VMs are terminated after it.

Successful configuration/session transitions discard the retired VM, queued
work and logs, then instantiate the approved source for the new fence. Closure
state and subscriptions restart. Failed configuration reloads retain the current
source generation. Input events arriving during source loading are queued with
byte/count bounds. Progress may be coalesced; overflow retires the Lua component.

Callbacks are observational: action constructors still produce declarations and
these subscriptions cannot execute commands, claim input or call adapters.
`log.info(message, fields)` appends a structured record; `fields` defaults to
nil and must be JSON-compatible, at most 16 levels deep. Messages have a 1 KiB
limit; the whole event can produce at most 32 records/16 KiB. Logs appear as
`lua_log` JSON rows from `session --json --lua`, with their active fence.
Logging during configuration evaluation is rejected. Logs commit only if every
callback for that event succeeds.

Each callback has 100,000 fuel units and the complete event dispatch has a
20 ms elapsed budget. Callbacks cannot yield or return data. Syntax, runtime,
resource or transport failure retires the Lua component until a new generation;
the agent keeps its broker lease and ordinary observations. The parent also
bounds response time, queues and source assembly. The helper has no new
privileges, no inherited input descriptors, and Linux data/heap/core limits.
CPU limits are renewed per operation while preserving an inherited hard limit.

## Timers

`scheduler.after(duration, callback)` schedules one callback after the delay;
`scheduler.every(period, callback)` schedules periodic callbacks;
`scheduler.defer(callback)` schedules a callback for a later wake with a 1 ms
delay. Durations use the existing whole-number `ms`, `s` or `m` units and must
be between 1 ms and one hour. Each returns a `Timer`; `timer:cancel()` is
idempotent and prevents future invocation, including another timer due at the
same wake. Registration has no configuration-time effects.

```lua
local p = require("prek")
local timer
timer = p.scheduler.every("1s", function(event)
    p.log.info("timer", {generation = event.context.fence.configuration})
    timer:cancel()
end)
return p.config.preset("minimal")
```

Source-time timers become active with runtime readiness. Timers registered by
callbacks use that callback's monotonic runtime clock. Payloads contain
`{context = {fence}, timer = {id, periodic}, at}`, where `at` is the service's
monotonic duration (`secs`/`nanos`). Handles and closure state belong to one VM
generation; replacement, logout/inhibition and shutdown retire the old timers
with that VM. Current timer callbacks have the same observational permissions
as event subscriptions.

Periodic timers preserve their initial phase and skip missed ticks. A late wake
invokes each due timer once and schedules the next future deadline; it never
bursts through all missed periods. Simultaneous deadlines run in timer-ID order,
and timers created by a callback start with a later wake. At most 256 timers can
be registered over a VM's lifetime, including canceled/completed timers. A wake
processes at most 32 due timers and shares the 20 ms dispatch and 32-record/16 KiB
log bounds; remaining due timers are handled on another wake. Error, yield,
nontermination or resource overflow retires the Lua component and discards its
uncommitted logs. The forwarding worker remains independent.

## Configuration and rule constructors

`require("prek")` returns the configuration module. Its `api_version` is 1;
`version` is the application version, and `api` contains the manifest.

- `config.new(table)` supplies `version = 1` if absent and returns the table.
- `config.preset(name)` returns a mutable `minimal`, `desktop` or `phone` preset.
- `rules.define(table)` returns a rule declaration.
- `units.duration(string)` and `units.distance(string)` require a string with
  explicit units; model validation checks its unit and range.

The returned configuration accepts `version`, `mode`, `timing`, `shape_capture`,
`policy`, `devices`, `layers`, `shapes` and `rules`. Rust schema validation remains
authoritative, including unknown fields, references, priorities, resource bounds,
activation keys, permitted seats and source selectors. A rule accepts `id`,
`enabled`, `priority`, `devices`, `contexts`, `app`, `scope`, `screen`,
`keyboard_visible`, `require_keys`, `keys_throughout`, `layer`, `delivery`,
`trigger` and `action`. Existing defaults and exact units remain in effect.

## Trigger constructors

All constructors take one table of named fields and return that table with the
correct `kind`. A conflicting `kind` is an error.

| Constructor | Fields |
|---|---|
| `triggers.key` | `key` |
| `triggers.button_taps` | `key`, `count` |
| `triggers.button_hold` | `key`, `after`, optional `repeat = {delay, interval}` |
| `triggers.chord` | `keys`, `ordered`, `timeout` |
| `triggers.wheel` | `horizontal`, `minimum` |
| `triggers.touch_taps` | `fingers`, `count` |
| `triggers.touch_hold` | `fingers`, `after` |
| `triggers.swipe` | `fingers`, `direction`, `travel`, `fallback_travel`, optional `continuous` |
| `triggers.pan` | Swipe fields; supplies `continuous = true` |
| `triggers.pinch` | `fingers`, `ratio` |
| `triggers.rotate` | `fingers`, `degrees` |
| `triggers.shape` | `name`, optional `fingers`, optional mouse `button` |
| `triggers.sequence` | `steps` containing trigger declarations, `timeout` |

Keys use Linux names/aliases or `code:N`; touch counts, source scope and timing
conditions retain their existing validated meaning. Pan is the continuous swipe
representation in the Rust model. Constructors never create a physical owner.

## Action constructors

`actions.define(name, {requires = capabilities}, callback) -> Action` registers
a named handler during configuration evaluation and returns a `lua` action
declaration for a rule. Names contain 1–128 ASCII letters/digits, `_`, `-`, `.`
or `:`. Names are unique; a generation may register at most 256 handlers. The
only option is `requires`, a nonempty capability array. Accepted names are
`commands`, `observations`, `synthetic_input`, `hyprland_actions`, `morf_actions`
and `power_actions`. Handler names, capabilities, exact source and model travel
in the broker-approved snapshot and must agree on runtime reconstruction.

Named handlers run in the supervised unprivileged Lua process after an offered
action receives a fresh broker grant. Use `prek session --lua --commands` to opt
in. The broker and agent require active desktop context, current fences, source
and gesture identity, configured authority and declared effect capabilities.
Production routing remains disabled while its separate handoff gates are pending.

The callback receives `{fence, event}`. `event` is the approved match: `id`,
`ordinal`, `rule`, `source`, `at`, `stage`, configuration/session generations,
evidence and nullable frozen `window`, `output`, `workspace`. It returns nil/no
values, one action constructor result, or a dense array of at most 32 actions.
Returning another named action, undeclared effects, malformed values, yielding,
errors or budget exhaustion discards the entire result/log batch and retires
this Lua component. Existing source/runtime fuel, memory and 20 ms callback
limits apply; the plan is bounded to 32 KiB. Registration during callbacks is
rejected. Source-time registration performs no effect.

The current session executor performs `commands` and `observations` effects.
Command arguments are exact argv entries without implicit shell expansion.
Commands in a plan spawn in order and run concurrently, under the existing
eight-child/session bound. The broker receives one completion for the whole
plan after its children finish. Failure or grant expiry after an earlier effect
is uncertain and is not retried. Event actions produce `lua_event` receipts carrying their fence and
gesture/ordinal. Other effect families require their own available adapters.

Pending results and later effects are canceled on generation change. Already
spawned commands continue to be reaped; their retired-generation completions
are discarded. A result arriving after grant expiry is discarded and completes
as canceled, while the session remains live. Nil means the handler completed without additional effects.
Logs remain available inside named callbacks. Event/timer subscriptions cannot
execute actions merely by returning an action table.

```lua
local p = require("prek")
local report = p.actions.define("report", {requires={"commands"}}, function(event)
    p.log.info("invoked", {rule=event.event.rule, ordinal=event.event.ordinal})
    return p.actions.command {argv={"printf", "%s\\n", event.event.rule}}
end)
return p.config.new {
    rules = {p.rules.define {
        id="report-a", contexts={"desktop"},
        trigger=p.triggers.key {key="KEY_A"}, action=report,
    }},
}
```

| Constructor | Fields |
|---|---|
| `actions.event` | `name` |
| `actions.keys` | `keys` |
| `actions.remap` | `key` |
| `actions.command` | `argv` containing exact arguments |
| `actions.morf` | `action` containing a supported Morf action name |
| `actions.hyprland` | `action` containing a supported window action name |
| `actions.power` | `action` containing a supported power action name |

Declaring an action does not establish availability, authorize a target or run
it. Observation and replay execute no live effect. Actions require the applicable
session capability, context and ownership checks in routing mode. Functions in
the configuration result are rejected. Named callbacks use their private
approved registry; callable pointers are never serialized into configuration.

## Limits, libraries and errors

The configuration VM includes Luna's core libraries and the `prek` module.
Ambient `io`, `os`, `package` and `debug` are absent. `print`, `warn`, `dofile`,
`loadfile`, `load` and `collectgarbage` are disabled. Other modules are unavailable
until a controlled module loader is implemented. The initial random seed is
deterministic. A configuration cannot yield or return multiple values.

Source is limited to 1 MiB, execution to 1,000,000 instruction-fuel units and
250 ms for user source evaluation, arena memory to a 32 MiB check between VM slices, and returned values to
64 nesting levels. Memory checks are slice-granular and do not promise a hard
allocation ceiling. Each CLI compilation runs in a separate process with a
256 MiB Linux data/heap limit, two-second CPU limit and two-second parent
deadline. Root brokers clear supplementary groups and use UID/GID 65534 for
that process; it has no new privileges and is killed if its parent dies.
Trusted VM/API setup is separate from the user-source budget; its bootstrap has
a separate bounded execution window. The approved serialized model is capped
at 1 MiB; the source/model/registration snapshot is capped at 4 MiB, and compiler
diagnostics at 4 KiB. Failed or abandoned children are
killed and reaped. These bounds do not establish a complete system-call sandbox.
Syntax/runtime errors include the named Lua chunk; schema errors identify invalid
data. A rejected model does not become the worker's active configuration.
