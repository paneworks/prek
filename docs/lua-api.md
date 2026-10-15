# Lua API

Prek embeds [onix-os/luna](https://github.com/onix-os/luna), pinned to version
0.5.1. Files ending in `.lua` return one configuration table. Legacy TOML is
also accepted. Configuration evaluation declares actions and does not execute
them. Live production routing remains disabled.

This reference covers configuration, rule queries and host transaction bindings, read-only session context, event subscriptions, timers, tasks and named
action handlers producing command/event plans. Custom recognizers,
live adapter bindings and the remaining operational APIs remain required
work in `PLAN.md`. Discovery labels the runtime API partial.
Run `prek api --json` for the installed manifest or inspect `prek.api` in Lua.

## Editor definitions

`prek api --lua` prints the same `prek.lua` installed at `share/prek/lua/prek.lua`.
The checkout copy is [docs/lua/prek.lua](lua/prek.lua). It uses
[Lua Language Server annotations](https://luals.github.io/wiki/annotations/)
for constructor fields and defaults, trigger/action variants, lossless host
identities, typed callbacks, context geometry, timers/tasks and transaction
handles. Overloads narrow callback payloads according to the event kind.
Definitions cover implemented bindings; pending adapter execution and operational
APIs remain pending. Rust schema validation and broker permissions are authoritative.

Point your project's `.luarc.json` at the installed directory:

```json
{
  "workspace.library": ["/path/to/prek/share/prek/lua"],
  "workspace.checkThirdParty": false
}
```

Without an installation, save `prek api --lua` as `.editor/prek.lua` and use
`"workspace.library": [".editor"]`. Keep definitions outside the approved
`modules/` tree: these files describe the embedded host for tooling. Their
functions raise errors if executed. They provide no standalone implementation,
ambient I/O or extra broker permissions. Unit tests compare exports and model
fields with Rust, and parse the complete file using pinned Luna. Run
`nix build .#checks.x86_64-linux.lua-editor` (or `aarch64-linux` on native ARM)
to check installed artifact parity, all shipped examples and positive/negative
type diagnostics with the pinned Lua Language Server package.

## Runtime rules

### Rule phase callbacks

`rules.on(id, phase, callback) -> Subscription` registers an observational
callback during configuration evaluation. The rule ID must exist in the fully
validated initial configuration; registration may precede its declaration.
Supported phases are `"discrete"`, `"begin"`, `"update"`, `"end"` and `"cancel"`.
Unknown IDs/phases, invalid callbacks and runtime registration raise errors.
The returned subscription supports idempotent `:dispose()`. Registration shares
the 256-subscription lifetime quota with `events.on`, including disposed entries.

`rules.define` additionally accepts `on_discrete`, `on_begin`, `on_update`,
`on_end` and `on_cancel` functions. It registers them in that phase order and
removes the function fields from the returned declarative rule. Use this
constructor for inline hooks; raw configuration tables cannot contain functions.
Inline hooks live for the source generation; use `rules.on` when a disposal
handle is needed. Registration performs no effect during evaluation.

Callbacks receive the same `{fence, event}` owned payloads as semantic event
subscriptions. Non-cancel phases receive a matched event; cancel receives either
a matched `stage="cancel"` event or a cancellation with a reason. Cancellation
events do not necessarily contain an ordinal or stage. IDs, ordinals and source
generations retain the normal exact decimal-string representation. Hooks filter
by exact rule ID and phase inside the approved VM, run in subscription order and
receive independent copies. Global matched/phase subscriptions may coexist;
each scoped hook runs once for its canonical notification, rather than once for
both global matched and phase notifications. Updates may coalesce; begin/end/
cancel keep their existing queue ordering.

Hooks follow a stable rule ID across acknowledged enable/disable/replace
transactions. Removing a rule prevents new matches for it; existing cancellation
notifications may still arrive. Adding another rule ID does not transfer its
hooks. The broker/worker's existing generation and pending-event retirement
rules apply; source reload creates new closures and retires the old VM.
Hook IDs/phases, registration order and active flags are part of the approved
source snapshot and must match when the helper reconstructs it.

Hooks share the event's 20 ms deadline, memory limits and 32-record/16 KiB log
batch, with 100,000 instruction fuel units per callback. They must return no values and cannot yield.
Returning an action table grants no effect: it fails the callback, discards the
whole dispatch's logs/proposals and retires this Lua component. Infinite loops
and errors have the same failure behavior. Authorized runtime transactions and
observational queries retain their existing permissions inside callbacks.
Hooks grant no input claim, synthetic ownership or adapter authority.

```lua
local p = require("prek")
local rule = p.rules.define {
    id="watch-a", delivery="observe", contexts={"desktop"},
    trigger=p.triggers.key {key="KEY_A"},
    action=p.actions.event {name="a-observed"},
    on_begin=function(payload) p.log.info("begin", {id=payload.event.id}) end,
    on_end=function(payload) p.log.info("end", {id=payload.event.id}) end,
}
return p.config.new {rules={rule}}
```

The installed `rule_callbacks.lua` example also handles cancellation. Preserve
the original source to preserve hooks: the Rust model contains no functions,
and `config.export` refuses a declarative export once rule hooks are registered.
Runtime transaction replacement accepts declarative rules only; it cannot add
new callback code or change approved registrations.

### Acknowledged rule queries and transactions

`rules.list()`, `rules.get(id)` and `rules.revision()` are available inside runtime
callbacks. Queries return owned copies of the current acknowledged rule view;
editing those tables has no effect. A missing ID returns nil. Revisions are
decimal strings, preserving all 64 bits without Lua numeric rounding.

`rules.transaction()` requires an explicitly enabled, authorized hosting transport.
The observation broker transport requires `policy.allow_runtime_rules = true`
in its protected system configuration and `prek session --lua --rule-mutations`.
The permission defaults to false. The broker negotiates `runtime_rules` only
with the owning agent, verifies current logind/compositor identity and unlocked
desktop context, and rejects work during configuration installation. A mutable
user rule cannot change policy, selectors, physical layer definitions or mode. Every replacement
or enabled action must fit negotiated capabilities; named Lua handler identity
and declared effects must match the approved source exactly.

The session starts its rule-enabled helper after the actual worker subscription
acknowledgement. The broker retires old queued matches and action offers before
activation, correlates the private worker receipt, and verifies identity, lock,
freshness and receipt time again before publishing acceptance. Source/session
changes discard unsent requests and reset registration-scoped rule state.
Routing-mode mutation remains unavailable pending production transport/handoff
integration. Broker policy/protocol checks and helper/worker model IPC are tested;
real login, kernel routing and phone acceptance remain required.

`examples/lua/rule_transactions.lua` demonstrates an acknowledged disable
transaction and reports unavailable permission explicitly. Validate it with
`prek check --config examples/lua/rule_transactions.lua --json`. Install it as
the protected system source before requesting mutation from an owning desktop
session; a user-owned example file grants no broker authority.

An enabled host provides a generation-scoped `RuleTransaction` with
`:enable(id)`, `:disable(id)`, `:replace(rule)` (also adds a new ID), `:remove(id)`,
`:activate_layer(id)`, `:deactivate_layer(id)`, `:reset_layer(id)`, `:commit()`,
`:cancel()` and `:status()`. Edit methods return nil. Each batch
contains 1–64 operations and at most 32 KiB of encoded changes. Commit validates
the complete resulting configuration and approved named-handler references before
submitting. It cannot change system policy, selectors, physical layer definitions or mode. At most
64 open builders and one pending proposal are permitted per helper generation.
An old builder retains its original revision and fails after another batch is accepted.

Status returns an owned `{state, revision, error}` table. States are `open`,
`cancelled`, `pending`, `accepted` and `rejected`; absent revision/error fields
read as nil. Commit does not update queries. Acceptance updates them and invokes
`rules_changed` with `{accepted, revision, error}`; rejection preserves the view
when the reported revision still matches. A mismatched revision makes that view
unavailable instead of inventing its contents. Cancel abandons an open builder
idempotently; a submitted proposal awaits its receipt. Callback failure or task
self-cancellation discards its new unsent proposal. Stopping retires pending work.
The supervised host transport requires receipts within 250 ms and checks the exact
broker/configuration/session fence, private proposal ID and accepted revision.

```lua
local p = require("prek")
p.events.on("ready", function()
    local rule = p.rules.get("my-rule")
    if rule then
        p.log.info("rule-state", {enabled = rule.enabled, revision = p.rules.revision()})
    end
end)
```

### Runtime layers

`layers.define {id, key, toggle=false, activation="physical"}` declares a layer
inside the configuration's `layers` array. `key` is required even for an
override, so returning to physical control has a defined binding. There are at
most 64 declared layers; IDs are nonempty, at most 256 bytes and contain no
control characters. `activation` is `physical` (derive activation from the
momentary/toggled key), `active` (eligible without holding that key), or
`inactive` (ineligible). Overrides do not synthesize key events.

| Function/method | Meaning |
| --- | --- |
| `layers.list()` | Owned copies of acknowledged `Layer` definitions. |
| `layers.get(id)` | Owned acknowledged `Layer`, or `nil` for an unknown ID. |
| `layers.revision()` | The shared rule/layer revision as a decimal string. |
| `layers.transaction()` | Open the same `RuleTransaction` as `rules.transaction()`. |
| `transaction:activate_layer(id)` | Stage the declared layer's `active` override. |
| `transaction:deactivate_layer(id)` | Stage its `inactive` override. |
| `transaction:reset_layer(id)` | Stage restoring `physical` control. |

`Layer` contains `{id, key, toggle, activation}`. Queries/transactions require a
runtime callback or task. Queries describe definitions and overrides; they do
not expose whether a physical key is held. Mutating a query result changes no
host state. Runtime operations cannot add/remove layers or redefine their keys.
Unknown IDs or an invalid operation reject the complete mixed rule/layer batch.
Different layer names do not establish disjoint exclusive scopes: layers can
activate together. Equally prioritized overlapping exclusive rules are rejected
unless their layer is forced `inactive`. Deactivate one layer and activate the
other in the same transaction to switch an otherwise conflicting pair atomically.

These operations use the protected `policy.allow_runtime_rules` permission,
`--lua --rule-mutations` negotiation, owning desktop/session checks and bounded
receipt transport described above. Activating or restoring a layer also checks
the action capabilities and approved named handlers of its enabled rules.
Changes become visible only after the worker acknowledges activation; the
`rules_changed` callback covers both rule and layer edits. They share one pending
proposal, one revision, and all cancellation/retirement limits.

An accepted transaction retires existing recognition candidates. Activating a
layer cannot claim a key/contact that was already held; fresh input is required.
Physical press/toggle state survives runtime edits, so reset restores that
state without inventing presses. A source reload follows ordinary held-state
reconciliation. Routing embeddings must also complete transport recovery and
establish fresh neutral readiness; the production broker currently permits
mutation in observation mode only. Actual routing-mode transitions remain pending.

`examples/lua/layer_transactions.lua` activates a layer after permission is
granted, then restores physical control after its configured key rule ends.

Lua modules live in `modules/` beside the entry configuration. For example,
`require("phone.bindings")` loads `modules/phone/bindings.lua`. Names use ASCII
identifiers separated by dots; the `prek` namespace is reserved. Search paths,
absolute paths and traversal are unavailable. There are at most 64 modules,
16 nested imports and 1 MiB of combined entry/module source. All captured
modules are syntax checked, including unused modules. Import cycles fail with
the import chain; an import evaluates once and caches its returned value. A
module returning nil is cached as true; false is also cached.

Directories and Lua files must belong to the entry file's owner and permit no
group/other writes. Symlinks and special files are rejected. The CLI/broker
captures sources through directory descriptors and rejects changes during
capture. Disposable compilation and session reconstruction use exactly those
bytes. A successful reload replaces the complete approved snapshot; failed
module imports, permissions, syntax or bounds preserve the active generation.
Callbacks can require captured modules under their usual budgets, with no
filesystem reads. Cycles first reached during a callback retire that VM.

```lua
-- modules/bindings.lua
local prek = require("prek")
return function()
    return prek.config.preset("desktop")
end

-- config.lua
return require("bindings")()
```

The runnable example is `examples/lua/modular/config.lua`, installed at
`share/prek/examples/lua/modular/config.lua`. Validate it with
`prek check --config examples/lua/modular/config.lua --json`.

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

Create a minimal preset with `prek init` (defaults to `config.lua`), validate
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

`config.export(config) -> string` validates a complete declarative configuration
and returns deterministic Lua source containing every effective model value.
It performs no file I/O and is available during configuration evaluation only.
Both its serialized model and generated source are bounded to 1 MiB. Unknown
fields, invalid configurations, oversized output, runtime use and named-handler
references without their original source are rejected. Closures, subscriptions
and timers are not included in a declarative export; retain their source separately.

```lua
local p = require("prek")
local config = p.config.preset("desktop")
local source = p.config.export(config)
assert(type(source) == "string")
return config
```

`prek migrate --config old.toml --output config.lua` exports a validated legacy
configuration and checks model equality through the isolated compiler before
writing it. The input is preserved, including when accessed through an alias;
existing output requires `--force`. Comments and unit spelling are normalized;
values, priorities, units, rules and supported phone bindings retain their
effective semantics. Lua shape-learning output uses this same verified export.
Legacy TOML input/output remains supported without a scheduled removal date.

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
`cancel`, `skipped`, `ready`, `reload`, `session_changed`, `context_changed`,
`recovery` and `stop`.
Rule callbacks receive `{fence, event}`; `event` is the existing typed rule
outcome, including its source, monotonic time and applicable gesture identity,
ordinal, stage, action declaration and frozen target. No raw input is supplied.
Host identities are lossless decimal strings, including both words in
`fence.broker`, `fence.configuration`, `fence.session`, rule `event.id` and
`event.ordinal`, `event.source.device`/`generation`, and rule
`config_generation`/`session_generation`. This representation is consistent for
small and large values, up to `"18446744073709551615"`. Compare identities with
string equality; `tonumber` can lose precision. Timer/task/subscription handle
IDs remain bounded Lua integers. Coordinates, counts, UID/PID, workspaces and
timestamps retain their documented numeric types. The Rust control protocol
continues to carry exact unsigned integers.
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

## Permitted normalized input

`events.on("input", callback)` observes normalized input from the owning seat.
It requires the protected system source to set `policy.allow_input_events=true`,
explicit selector names in `policy.allowed_devices`, and the owning agent to use
`prek session --lua --input`. The broker negotiates `input_events` together with
`context`, `observations` and `lua_events`. Source registration alone grants no
permission. This is implemented for observation brokers; routing brokers reject
the capability while production takeover remains pending. The protected source
must also use `mode="observe"` (the configuration default), so negotiation and
worker delivery enforce the same mode requirement.

The worker and broker require fresh desktop context from the verified session,
with no secure overlay. Greeter, console, locked, locking, unknown, stale/future
context, another seat, virtual devices and sources outside the protected
allowlist do not produce input callbacks. Pending frames from before subscription
or late opt-in are reconciled internally without exposing historical typing as
reset snapshots. Lock/context loss retires the session fence and its old work.
The broker rechecks lock state and registration and checks the input permit again
at actual send time. A queued observation older than 250 ms invalidates the
stream instead of being delivered under a refreshed context.

Callbacks receive `{fence, input}`. `input.kind` is `frame` or `reset`. Every
payload includes `source`, monotonic `at={secs,nanos}` and `selectors`, the matched
protected allowlist names. Frames contain `sequence`, `changes`, `contacts`,
`contacts_started`, `contacts_ended` and `ended_contacts`; no raw frame/event list
or descriptor is exposed. Source device/generation, sequence and contact
generation identities are exact decimal strings. Slot/tracking IDs, codes,
coordinates, pressure and timestamps remain numeric.

`changes` is an ordered array of these tagged values:

| Kind | Fields |
| --- | --- |
| `key` | Numeric evdev `code`, `state="press"|"release"|"repeat"`. No text/layout translation. |
| `relative` | `code`, signed `value` in device counts, without a second acceleration curve. |
| `absolute` | `code`, signed `value` in device axis counts. |
| `switch` | `code`, boolean `on`. |
| `wheel` | Boolean `horizontal`, numeric `steps`, boolean `high_resolution`. Normalization avoids double-counting legacy companions. |

A contact is `{id={slot,tracking,generation},x,y,pressure,fresh}`; absent pressure
reads as nil. Positions use device axis counts. `ended_contacts` preserves the
last position, including motion in the release frame. Reset notifications carry
`reason="attached"|"reconciled"|"mapping_changed"|"removed"|"lost"`, a `keys`
array of held numeric codes and `contacts`. Reset local recognizer state first;
held state is not a new press/contact. Reset contacts have `fresh=false`.
Removal/loss clears held state. Device connection generation distinguishes a
replacement from the retired source, even when physical identity is unchanged.

Frames are not coalesced: ordering and contact transitions are preserved within
the existing packet and queue budgets. Each input value allows at most 4096
changes and 256 contacts. Oversized packets or worker overflow end affected
observation registrations; helper/source-loading overflow retires the Lua
component. The physical input loop does not wait for callbacks. `Subscription`
disposal and VM retirement use the ordinary event semantics and limits.
The session agent does not automatically print normalized input to stdout.

[input_events.lua](../examples/lua/input_events.lua) demonstrates counting a
configured key without recording general typing. The editor definitions include
`InputPayload`, the frame/reset union and each change/contact variant. This
subscription supplies observations only: returning a value cannot claim,
consume, remap or dispatch an action. Custom recognizer proposals under Rust
ownership/deadline arbitration remain required work. Model permission tests and
actual worker/helper IPC are separate from root/logind and physical acceptance.
`nix run .#cage-test -- --layers --input` checks ordered model source frames and
resets through worker/helper IPC alongside actual compositor context and app
delivery. The fixture supplies synthetic device/seat authority. Add `--visible`
for the owned visible nested-Hyprland path; it opens no host evdev devices.

## Output, region and coordinate queries

`outputs`, `regions` and `coordinates` are runtime-only, read-only APIs over the
verified owning desktop snapshot. They require fresh context (250 ms), a user
session and no secure overlay. They perform no compositor calls or host scans
inside a Lua callback. Queries check freshness again after constructing results.
Missing, expired or inhibited context returns nil; an available snapshot with
no reported outputs/regions returns an empty list. Output and region names are
exact strings of 1–256 UTF-8 bytes. Invalid argument types or values raise errors.

| Function | Result |
| --- | --- |
| `outputs.list()` | `OutputView[]`, or nil while context is unavailable. |
| `outputs.get(name)` | `OutputView`, or nil for an absent output/unavailable context. |
| `regions.list()` | `RegionView[]`, or nil while context is unavailable. |
| `regions.get(id)` | `RegionView`, or nil for an absent region/unavailable context. |
| `regions.at(point)` | Every region containing a global logical point, or nil while unavailable. |
| `coordinates.convert(reference, point, from, to)` | Converted `Point`, or nil for an unavailable/retired output reference. |

`OutputView = {reference, state}` contains an owned `OutputState` and
`OutputReference = {fence, revision, name, geometry_generation}`. All identities,
including the snapshot revision and geometry generation, are canonical unsigned
64-bit decimal strings. Coordinate conversion requires every identity to match
the current host snapshot. **Any snapshot revision change retires a reference**,
even if output geometry is unchanged; obtain another view explicitly. Fence
changes, geometry-generation changes and output removal also return nil. These
references are comparisons against host state, not authority tokens. Editing
`state.geometry` never changes conversion geometry or an external target.

`RegionView = {fence, revision, region}` contains a rendered provider-reported
`Region`, with its ID, optional scope, output, global logical rectangle and
interactive flag. Missing rendered regions remain absent; keyboard/output
rectangles are not invented as shell regions. `regions.at` uses half-open bounds
and returns all matches in provider order, including overlapping interactive
and noninteractive regions. It does not select a target, consume input or grant
permission to a rule.

Coordinate spaces are `"normalized"`, `"local"` and `"global"`. Normalized axes
follow the transformed output, with (0,0) at its local origin and (1,1) at its far
corner. Local/global positions use logical units; output scale and rotated axis
dimensions come from Rust's `Output.logical_size`. Global points include the
output origin. These are output coordinates, not untransformed device axes:
use `devices.position` for calibrated raw device counts and millimetres.
Points require finite numbers. Conversion does not clamp outside points; finite
arithmetic overflow raises an error. Metadata/query copies grant no live effect.

```lua
local p = require("prek")
p.events.on("context_changed", function()
    local output = p.outputs.get("eDP-1")
    if output then
        local center = p.coordinates.convert(output.reference,
            {x=0.5, y=0.5}, "normalized", "global")
        if center then p.log.info("output-center", {point=center}) end
    end
end)
return p.config.new {}
```

The installed [geometry example](../examples/lua/geometry.lua) also inspects
rendered regions. Unit tests cover reference retirement, full-width identities,
copy isolation, rotation/scale/origin, region overlap and argument failures.
Helper-process and local compositor tests exercise these queries through the
same context transport used by callbacks. They do not prove physical touch
routing, shell controller effects or authentication ownership.

## Permitted device and seat queries

These runtime queries return owned copies; changing a returned table changes
neither the worker nor future query results. Device queries describe metadata
actually received in permitted input reset notifications. They perform no host
device scan and open no device nodes. The same protected policy, allowlists and
`session --lua --input` capability gates apply to that metadata delivery.

| Function | Result |
| --- | --- |
| `devices.list()` | `DeviceInfo[]`, or nil while the received catalog is unavailable. |
| `devices.get(source)` | `DeviceInfo`, or nil for an unknown, retired, foreign-seat or unavailable source. |
| `devices.revision()` | Exact decimal-string catalog revision, or nil while unavailable. |
| `devices.position(source,x,y)` | Projected `Position`, or nil when the source/mapping is unavailable. Invalid integer coordinates raise an error. |
| `seats.current()` | `{binding,fence,state="desktop"}`, or nil while fresh desktop context is unavailable. |

`source` is the complete `{device,generation,seat}` identity from an input
payload. Both numeric identities must remain canonical decimal strings; numeric
values, leading zeros and overflow are rejected. Lookup never redirects an old
connection generation to its replacement. The catalog holds at most 256 entries
and 4 MiB of encoded metadata. Metadata changes, replacement and removal advance
its revision; ordinary input frames do not. A malformed, regressing or excessive
metadata update retires the Lua component. Context inhibition or a new fence
retires the cached view.

Before the first metadata arrives, `devices.list()` returns nil. After all known
sources are removed, it returns an empty array while context remains usable.
This is the received permitted catalog, not a claim that every host device has
been enumerated or that startup delivery is an atomic inventory snapshot. All
queries require runtime evaluation and fresh desktop context. Copies that expire
during serialization are discarded. `seats.current()` uses the verified context
binding without requiring device access; it cannot enumerate other seats.

`DeviceInfo` contains `{device,selectors,mapping}`. `device` contains `source`,
stable `identity`, `name`, metadata `path`, `physical`, `unique`, numeric
`vendor`/`product`, `properties`, `capabilities`, `axes` and `virtual_device`.
Paths grant no permission to open them. Capabilities and axes use **numeric Lua
keys**: `info.device.capabilities[1]` is the key-code array and
`info.device.axes[53]` is the touch-X axis. This representation is identical in
reset payload metadata and direct queries. Each axis has `minimum`, `maximum`,
`resolution`, `fuzz` and `flat`. Present-source reset notifications include this
same optional `metadata`; removal notifications omit it.

An optional mapping contains exact string `generation`, X/Y axis metadata, a
2×3 calibration matrix and an optional output geometry. `devices.position`
reuses the Rust mapping implementation. Its result has calibrated `normalized`
coordinates, `millimetres` when both resolutions are positive, and optional
output-local/global logical coordinates. Lua accesses the reserved field name
as `position["local"]`. Without an output, local/global coordinates are nil;
without a mapping, the function returns nil. Mapping and coordinate queries
confer no target, consumption, synthetic-input or action authority.

[device_queries.lua](../examples/lua/device_queries.lua) demonstrates metadata and
touch projection. This read API does not implement device creation/removal,
operating-mode changes, custom recognizer claims or physical takeover.

## Runtime context

The `context` namespace reads the broker-verified session snapshot. Queries
are available in runtime callbacks and tasks; configuration evaluation rejects
them. Each result is a new owned Lua value. Editing or retaining a result does
not change broker state, a later query, or an action grant.

| Function | Result |
| --- | --- |
| `context.snapshot()` | Complete fresh `Snapshot`, or `nil` when absent/expired. |
| `context.binding()` | Verified `Binding` for that snapshot, or `nil`. |
| `context.state()` | Fresh broker state, or `"unknown"` when the view expires. |
| `context.fence()` | Fresh `{broker, configuration, session}` fence, or `nil`. |
| `context.error()` | Fresh broker diagnostic of at most 512 bytes, or `nil`. |

`State` is one of `console`, `desktop`, `locking`, `locked`, `greeter`,
`inactive`, or `unknown`. A fresh state can be available without geometry.
The agent requests a new view 100 ms after completing the previous transfer.
Both the view's original sample time and the snapshot's capture time must be
less than 250 ms old when queried. Receiving an old snapshot does not extend
its lifetime. All times use the system monotonic clock and appear as
`{secs, nanos}`. Lua-held copies are historical data after this freshness
window; use a new query before making a decision.

| Type | Fields |
| --- | --- |
| `Snapshot` | `generation`, `revision`, `captured_at`, `state`, `outputs`, `windows`, `regions`, `secure_overlay`. `generation` matches the session fence. |
| `Binding` | `seat`, `session`, `uid`, `compositor_pid`, `compositor_start`, `kind` (`user` or `greeter`). Identity comes from broker process/logind verification. |
| `OutputState` | `geometry`, `geometry_generation`, `active_workspace` (integer or `nil`), `screen` (`on` or `off`), `keyboard` (`Keyboard` or `nil`). |
| `Output` | `name`, `pixel_width`, `pixel_height`, `scale`, `origin`, `transform`. Pixel dimensions and scale define logical dimensions; origins use logical desktop coordinates. |
| `Window` | `id`, `output`, `workspace`, `rect`, `content`, `mapped`, `visible`, `focused`, `z_order` (integer or `nil`). Mapped and visible are distinct; absent stacking order conveys no order. |
| `Region` | `id`, `scope` (or `nil` for a local widget/overlay), `output`, `rect`, `interactive`. |
| `Keyboard` | `rect`, `layout`. |
| `Rect` | `origin={x,y}`, `size={x,y}` in logical desktop coordinates. |

`Snapshot.generation`, `Snapshot.revision`, `OutputState.geometry_generation`
and `Binding.compositor_start` use the same decimal-string identity type.
`context_changed.revision` matches the snapshot revision, or is `"0"` when
no snapshot is available. Fences have identical field types in lifecycle/rule
callbacks, named action handlers, timer/task contexts and `context.fence()`.

Arrays have ordinary one-based Lua indices. Optional fields are `nil`; getters
do not invent workspaces, regions, keyboard bounds, visibility or stacking order
when a provider cannot supply them. Snapshots are transferred whole, with up to
256 outputs, 4096 windows and 4096 regions. The serialized view is limited to
4 MiB; oversized or invalid transfers fail rather than truncate geometry.
Queries and notifications share the callback memory, fuel, elapsed-time and
logging budgets. Copying a large snapshot can exhaust those budgets.

`events.on("context_changed", callback)` receives
`{fence, state, available, revision}` after the owning helper installs a view.
`available` describes fresh snapshot availability and `revision` is zero when
no snapshot exists. Notifications can repeat for refreshed captures of the same
revision. Initial loading and generation replacement are asynchronous; a
`ready` callback can observe `nil` until the first context notification.

```lua
local p = require("prek")
p.events.on("context_changed", function(event)
    local snapshot = p.context.snapshot()
    if snapshot then
        p.log.info("desktop geometry", {
            revision = snapshot.revision,
            outputs = #snapshot.outputs,
            windows = #snapshot.windows,
        })
    end
end)
return p.config.preset("minimal")
```

Each transfer uses 32 KiB chunks with a one-second deadline and the current
broker/configuration/session fence. Agent-to-helper updates retain at most two
bounded serialized views: the active upload and the latest pending update.
Partial uploads finish before replacement; unsent intermediate updates coalesce.
The helper acknowledges each completed view within the existing 250 ms response
deadline. Stop cancels unsent uploads. Reload/session transitions clear cached
views and retire the old helper. The broker rechecks authority and lock state
before delivering geometry. Context queries expose no setter, input stream,
unlock operation or effect capability; effects still require broker grants.

## Callback limits

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

## Coroutine tasks

`scheduler.spawn(callback) -> Task` registers a task during configuration or a
runtime callback. It starts on a later scheduler wake; source evaluation never
runs its body. At most 64 tasks are active. Completed/cancelled entries free
capacity, and identities are never reused within the VM generation. A retained
old handle keeps its terminal state when its capacity slot is reused.

`scheduler.sleep(duration) -> TaskWake` is available only inside a scheduler
task. It accepts the timer duration syntax and range (1 ms through 1 hour),
yields without blocking, and resumes after a monotonic deadline measured from
the sleep call. Both the initial callback argument and the sleep result contain
`task.id`, `at` and `context`; the supervised session supplies its current fence
as `context.fence`. No context survives a retired generation as execution
authority. Plain coroutine yields, yielded data and task return values are
rejected. Each resume has 100,000 instructions and a 20 ms wall limit; at most
eight tasks resume per wake, ordered by deadline and identity. New tasks start
on a subsequent wake. Tasks share the VM's memory, log and callback limits.

`task:status() -> TaskState` returns `pending`, `running`, `sleeping`, `completed`
or `cancelled`. `task:cancel() -> nil` is idempotent. Cancellation stops later
resumes and releases the retained coroutine at the next scheduler boundary.
A task cancelling itself finishes its current bounded slice; that slice's logs
are discarded. Cancellation cannot roll back Lua closure changes. A completed
task remains completed when cancelled. Errors/resource failures retire the VM
and discard the current scheduler wake's logs; other service components remain
supervised independently. Successful source/generation replacement retires all
old task state, while rejected reloads preserve the active generation.

The runnable task recipe is `examples/lua/tasks/config.lua`, also installed at
`share/prek/examples/lua/tasks/config.lua`.

Tasks are observational. They may emit logs and register permitted callbacks,
timers or tasks; effects require explicit broker grants through named handlers.

```lua
local p = require("prek")
local task = p.scheduler.spawn(function(wake)
    p.log.info("task-start", {generation=wake.context.fence.configuration})
    local next = p.scheduler.sleep("100ms")
    p.log.info("task-resumed", {generation=next.context.fence.configuration})
end)
p.events.on("stop", function() task:cancel() end)
return p.config.preset("minimal")
```

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
`enabled`, `priority`, `devices`, `seats`, `contexts`, `scope`, `screen`,
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
Candidate cancellation, source removal/reconciliation and mapping changes also
invalidate pending offers for the affected gesture/source. Unrelated work keeps
its generation and VM. A queued callback is removed before invocation; an
already-running callback remains bounded and its effects/logs are discarded.
Cancellation cannot undo commands that already started. Grants expire by both
the 250 ms execution limit and the verified context snapshot's remaining age.
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

## Experimental output-scoped DPMS adapter

The Rust session executor has an explicit host opt-in for typed power plans:
`actions::PowerAdapter::new(client, view)`,
`Executor::set_power_adapter(adapter)` and `Executor::update_power_context(view)`.
The host supplies a broker-approved binding/view and an already verified
Hyprland client; these are not Lua authority setters. **Public `prek session`
power negotiation remains disabled.** Ordinary sessions still execute only the
command/event effects described above. This adapter foundation does not enable
production routing or supply a user CLI permission shortcut.

When a host authorizes it, existing `actions.power {action="blank"}` and
`actions.power {action="wake"}` plans execute against the offered match's
explicit frozen output. A named handler must declare `power_actions`; the
executor also requires a route-mode offer, negotiated capability, its exact
seat/fence/gesture/ordinal and a fresh execution grant. It captures output state
at offer time, rejects missing/unsafe names, retired geometry generations and
unusable context, and rechecks actual compositor identity/lock/geometry before
the effect. No focused-output fallback or global DPMS command is used.

The deadline is the grant's remaining time, capped at 200 ms for all adapter
IPC. The unprivileged parent executor performs this bounded work outside the
Lua VM and physical forwarding loop. A plan may contain at most one power
operation. Direct power declarations run only in discrete/begin phases;
named handlers can explicitly return an operation in their authorized phase.
Lock-and-blank, toggles, suspend and hardware-key policy changes are unavailable.
A witnessed lock latches adapter inhibition; a new approved generation needs
its own adapter. Expired/unusable adapters withdraw power availability.

An applied completion requires dispatch acceptance followed by a snapshot
reporting the requested DPMS state for the same output geometry. This proves
the compositor's DPMS state, not physical panel power or suspend wake. A failure
before mutation is unavailable/cancelled; any failure after bytes of the
mutation may have been written is uncertain. Effects are never retried after
uncertainty. Existing duplicate suppression, cancellation and generation
retirement apply. Partial composed plans retain uncertain completion semantics.

`nix run .#cage-test -- --layers --input --power` executes named Lua plans through
the actual helper and executor, with explicit model broker grants, and checks
DPMS off/on through stock IPC on the owned nested output. Add `--visible` for
the owned visible path. These fixtures open no host evdev devices and do not
change host-display DPMS. Broker/logind authorization, production target
preflight/negotiation, lock-provider integration and physical phone/power
acceptance remain required.

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
