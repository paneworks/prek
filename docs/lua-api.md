# Lua API

Prek embeds [onix-os/luna](https://github.com/onix-os/luna), pinned to version
0.5.1. Files ending in `.lua` return one configuration table. Legacy TOML is
also accepted. Configuration evaluation declares actions and does not execute
them. Live production routing remains disabled.

This reference covers configuration, rule queries and host transaction bindings, read-only session context/native Morf connections, event subscriptions, timers, tasks and named
action handlers producing command/event plans. Custom recognizers,
live adapter bindings and the remaining operational APIs remain required
work in `PLAN.md`. Discovery labels the runtime API partial.
Run `prek api --json` for the installed manifest or inspect `prek.api` in Lua.
`oslo make test-lua-api` requires every function reachable from `require('prek')`
and every method on a returned replay, timer, task, subscription or rule
transaction to appear in the manifest, every manifest symbol to resolve to a real
function and be cited here, the editor stubs to match the manifest, and every
editor class field to be described in README or `docs/`. It also
runs `prek check --json` on every shipped `examples/lua` entry point (top-level
files and each directory's `config.lua`) and dispatches each one's `ready`
event with a session-shaped `{fence, seat}` payload; other Lua files must live
under a `modules/` tree. This exercises callbacks without live adapter effects.

## Native Morf connections

`morf.providers() -> MorfProviderBinding[] | nil` copies the native shell
connection bindings currently verified by the broker for this desktop. Call it
in a runtime callback. Configuration-time calls raise an error. It returns
`nil` for missing, stale or inhibited desktop context; an empty array means no
native shell provider is currently verified. This uses the existing context
capability and the protected broker `morfExecutable` policy. User Lua cannot
register providers or populate this view.

Each binding contains `version=1`, `role="shell"`, `output`, a 32-character
native `epoch`, `revision`, `phase`, `compositor_pid`, `compositor_uid`,
`compositor_start` and the owning `fence`. Process birth is an exact unsigned
decimal string. Fence identities are also decimal strings. Compare them as
strings. Returned tables are independent copies and confer no authority.

`pending`/revision 1 means the native shell connection exists for that output.
`configured`/revision 2 means the native host witnessed its primary surface
configure. The epoch changes when the backend connection is replaced. A Lua
configuration reload on the same connection does not by itself change this
connection epoch. These fields prove neither compositor presentation nor
rendered widget regions, keyboard state, authentication, recognizer ownership
or an action grant. The other Morf runtime operations remain pending.

The broker verifies the private endpoint, exact Morf executable and process
lifetime, active logind membership and the actual bound compositor. It samples
existing connections with a shared 200 ms IPC deadline and discovers providers
at most once per second. The context expires after 250 ms and is limited to 64
unique output bindings. Loss, malformed state, retargeting or ambiguity clears
the view; it cannot establish unlock. Missing Morf does not inhibit unrelated
generic rules. Authentication input and inactive/unrelated sessions cannot use
this query. See [morf_providers.lua](../examples/lua/morf_providers.lua) for a
runnable observation recipe.

`morf.regions() -> MorfRenderedContext[] | nil` copies fresh native named bounds
from the shell's primary GPU frame. Each item contains `output`, `fence` and
`context={version,epoch,ui_generation,revision,sampled_ns,coverage,frame}`.
Generation, revision and monotonic nanoseconds are exact decimal strings.
`coverage` is `primary_surface_named_bounds`; `frame` contains logical surface
`width`, `height`, `scale_120` and up to 256 `regions`. Each region supplies its
scene name `id`, an opaque decimal-string `node` identity, a surface-local
`bounds={x,y,width,height}` and `interactive`. Names can repeat, such as a label
in each launcher row; the native node identities keep every result distinct.
Node identities are meaningful only within the endpoint epoch and UI generation.
An optional `semantic={role,layout?}` annotates the actual node: `keyboard`,
`secure_overlay`, `clock_background`, `content_bottom`, `physical_bottom`,
`top_left`, `top_right`, `side_left`, `side_right` or `local_control`. A keyboard
role requires its existing layout name; other roles cannot set a layout.
Annotations accept no coordinates or authentication/ownership fields.
Bounds include ancestor transforms and clipping and exclude hidden, transparent,
masked and exiting subtrees. They are bounding boxes, including for curved or
rotated items. `interactive` describes an enabled native input target itself;
it does not describe the exact hit shape or the compositor input mask.

With native `layer_space` support, `frame.surface={namespace,width,height,local_scale,mapped_ns?}`
identifies the immutable Wayland layer namespace and native logical viewport;
multiply Morf local coordinates by `local_scale` to obtain compositor logical
units. `mapped_ns`, once present, is the decimal CLOCK_MONOTONIC time at which
Morf received the surface's first frame callback, an upper bound of its
compositor map time. An optional report-level `layout` contains `basis`,
`producer_pid`, exact decimal-string `producer_start`, `geometry_generation`,
`started_ns` and `sampled_ns`, optional `map_in_ns` and `rendered_from_ns`,
owning output geometry, matched `surface` and projected `regions`. Each
projected row preserves its native identity and metadata;
`bounds={origin={x,y},size={x,y}}` is global logical layout geometry, or `nil`
when the native viewport clips it completely. This mapping requires one stable
compositor layer with the same native producer, namespace, output and viewport.

`basis="stock_layer_layout"` describes the layout goal, which an animating
layer has not reached. `basis="settled_stock_layer"` means the layer's map-in
animation was provably over before `started_ns`, the time before the first of
the two bracketing layer samples. `map_in_ns` is the stock `layersIn` bound and
is present only when both samples agree. It is zero while `animations:enabled`
is off or the node is disabled, because the stock tick then warps every
animated value. Otherwise it is `speed` tenths of a second for a listed cubic
bezier; springs and unknown curves give no bound.
`rendered_from_ns = mapped_ns + map_in_ns + 100ms` must not exceed
`started_ns`. Stock layers move without animation once mapped, so a settled box
is the layer's current position. It is still not presentation, stacking or the
surface's input mask, and a spring defined with a listed bezier's name cannot
be distinguished through stock IPC.

With `keyboard_registry` support, `frame.keyboard` contains
`{scope="registered_morf_primary",registered,state,node?,layout?}`. `state` is
`unknown`, `hidden` or `visible`. Known states identify exactly one declared
keyboard on the primary scene. A hidden registered keyboard says nothing about
other programs' keyboards. In the report itself these fields are inspection
data; see below for how a settled report updates the routing snapshot.

With `input_region` support, `frame.input` lists the rectangles of the input
region committed with that frame, `{x,y,width,height}` in Morf layout units; it
is absent when Morf did not describe it (a configured mask or more than 512
rectangles), which means unknown, not empty. `layout.input` holds the same
rectangles as global logical boxes, scaled by `local_scale`, rounded outward
and clipped to the layer. `layout.exclusive` is true when neither bracketing
stock sample showed another process's bottom, top or overlay layer on that
output.

The native host records a successful GPU submission, then refreshes unchanged
geometry only while the current scene, surface size/scale and Lua generation
still match it. A query cannot refresh itself. Busy, skipped, layout-only, stale
or replaced frames supply no regions. Invalid or excessive named regions make
the report unavailable rather than truncating it. Duplicate native node identities
are rejected. The broker verifies the
native binding around each read within the shared 200 ms deadline. Lua gets
independent tables, `nil` without fresh desktop context and an empty array when
no fresh report is available. Unavailable reports are never proof of absence.

The broker refines its desktop snapshot from a fresh report only when its
`layout` is `settled_stock_layer` and describes `input`, and the stock snapshot
holds that exact layer as `layer:<output>:<address>`. That coarse interactive
region is then replaced by one interactive region per input rectangle
(`morf:<epoch>:input:<i>`) and one region per visible semantic node
(`morf:<epoch>:node:<node>`): `local_control` is an interactive unscoped region,
and `clock_background`, `content_bottom`, `physical_bottom`, `top_left`,
`top_right`, `side_left` and `side_right` become regions of that scope. On an
exclusive output whose keyboard is still unknown, a `hidden` registry sets
`keyboard_known=true` without geometry, and a `visible` one also sets
`keyboard={rect,layout}` from the registered node. Any visible `secure_overlay`
node sets `secure_overlay=true`, settled or not. An inconsistent view changes
nothing else. The result must still pass snapshot validation.

This does not prove compositor presentation, authentication, surface stacking
or the contents of auxiliary layers, and other programs' xdg windows are not
keyboards for this purpose. Production ownership/cutover remains required
before Morf action negotiation.

Output snapshots explicitly distinguish keyboard knowledge. `keyboard_known=false`
means unknown, even when `keyboard=nil`. Keyboard visibility conditions and
spatial eligibility on an awake output then fail with `KeyboardUnavailable`.
A known state with no keyboard geometry establishes hidden; a known state with
geometry establishes visible. Knowledge loss invalidates frozen output targets.
Explicit global rules can remain independent of keyboard context, and an explicit
screen-off rule can use its supplied region without inferring a visible keyboard.
Stock compositor metadata reports unknown. Only the settled, exclusive Morf
refinement above supplies knowledge; missing keyboard annotations cannot
establish hidden.

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
`:activate_layer(id)`, `:deactivate_layer(id)`, `:reset_layer(id)`,
`:select_mode(id)`, `:reset_mode()`, `:commit()`,
`:cancel()` and `:status()`. Edit methods return nil. Each batch
contains 1–64 operations and at most 32 KiB of encoded changes. Commit validates
the complete resulting configuration and approved named-handler references before
submitting. It cannot change system policy, selectors, layer/mode definitions or Observe/Route authority. At most
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

### Named operating modes

`modes.define {id, active=false}` declares a mode in the configuration's `modes`
array. There are at most 64 declarations, with unique nonempty IDs of at most
256 bytes without control characters. At most one mode is active; the default
is no selection. `Rule.when_mode` names a declared mode and makes that rule
eligible only while it is selected. Rules without `when_mode` remain eligible
in every mode and with no selection. Key/layer/context/device conditions still
apply. These named modes do not change the configuration's `mode="observe"`
or `mode="route"`, protected policy or negotiated capabilities.

| Operation | Result |
| --- | --- |
| `modes.list()` | Owned copies of acknowledged `{id, active}` declarations. |
| `modes.get(id)` | Owned declaration, or nil for an unknown ID. |
| `modes.current()` | Owned selected declaration, or nil if none is selected. |
| `modes.revision()` | Exact decimal shared rule/layer/mode revision. |
| `modes.transaction()` | The same authorized `RuleTransaction` as `rules.transaction()`. |
| `transaction:select_mode(id)` | Stage selection of one declared mode, deactivating the previous selection. |
| `transaction:reset_mode()` | Stage clearing the selection. |

Queries require a runtime callback/task and return historical copies. Changes
become visible only after the correlated accepted worker receipt and emit
`rules_changed`; rejected or stale proposals do not invent an active selection.
Mixed rule/layer/mode batches share one revision, pending slot, deadline and
builder/resource limits. Only the owning seat registration changes. Runtime
requests cannot add or redefine modes, grant another seat, or activate action
capabilities the session has not negotiated. The broker currently authorizes
these changes in observation mode with protected `allow_runtime_rules` and
`session --lua --rule-mutations`.

Selection validates the complete resulting configuration, including exclusive
rule conflicts. An unconditional rule can conflict with a newly selected mode;
disable it in the same transaction when appropriate. A successful transaction
retires pending and active candidates, including sequences; it never reuses a
previously held key/contact as a fresh activation. Reload restores the approved
initial selection with ordinary held-state reconciliation. Routing embeddings
must complete neutral transport recovery before accepting new input. Production
routing remains unavailable.

`examples/lua/mode_transactions.lua` uses F9 to switch between normal/navigation
and shares A between the two declared modes. The local `cage-test -- --modes`
fixture switches modes while the original rule and its layer stay enabled,
then checks actual application delivery through the owned compositor.

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

Disposal clears the stored callback reference, allowing captured state to be
collected even when the subscription handle remains reachable. The subscription's
bounded identity and inactive metadata remain until the generation ends. An
invocation already running keeps its callback alive and finishes normally;
disposal prevents later invocations and does not replenish the lifetime quota.

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

## Custom Lua recognizers

`recognizers.define(name, options, callback)` is a configuration-time constructor
that returns a reusable native trigger for `rules.define`. Options require
explicit selector IDs, input kinds and a 1–250 ms reply budget. Optional
`continuous`, `timeout` and `wake` control phase ordering, native lifetime and
stationary callbacks. The source registry keeps a separate canonical declaration;
altering the returned declaration cannot widen its binding. Custom rules without
their exact source callback fail compilation. Callback-bearing configurations
require their original source; declarative export fails.

Declared input families also bound candidate state. `key` tracks physical button
keys, excluding contact/tool flags; `touch` tracks contacts. A recognizer without
`touch` neither inherits contact identities/counts nor joins new contacts into its
frozen scope. A `key`+`touch` candidate remains active until both declared held
families are neutral. Relative, wheel, absolute and switch values are per-frame
input, not additional held keys or contacts. Non-observing candidates reject
`begin`/`update` after their declared held domains finish. Helper re-entry blocks
already-held declared families, not unrelated input from the same device. These
candidate boundaries do not replace the routing layer's separate ownership proof.

An observation session opts in with `--lua --recognizers`; protected
`policy.allow_input_events=true`, allowed devices and fresh owning desktop
context are also required. Ordinary `--input` is independent. The callback gets
only the declared normalized input under a native ticket. It returns nil while
pending or a strict phase/data proposal; cancellation carries identity/reason
only and returns nil. Native rules retain activation, frozen target, arbitration
and effect authority. No Lua runs on the input thread. Each callback has a
20 ms / 100,000-instruction budget; data is limited to eight levels and 4 KiB.
All IDs/generations/revisions are exact decimal strings.

See [the full constructor, ticket and cleanup contract](recognizers.md) and
[hold example](../examples/lua/custom_recognizer.lua). The real helper and
native-worker tests use explicit context/admission models. Source-declared nested
custom sequences compile into one native coordinator and run through worker
reconstruction and deterministic offline replay. Actual privileged, raw downstream
and physical acceptance remain required; production routing stays disabled.

Constructors also retain canonical declarations in `Config.recognizers`,
independent of the current rules. Runtime transactions can use a retained trigger
without a disabled template; they cannot add or alter recognizer declarations.
Removal/readdition preserves approval, while a source reload replaces it.
See [the runtime installation example](../examples/lua/custom_runtime.lua).

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

The owned [Cage touch test](../tests/cage.md#lua-touch-routing) exercises
[`touch_observations.lua`](../examples/lua/touch_observations.lua) in a real
observation helper, independently of
[`touch_routing.lua`](../examples/lua/touch_routing.lua)'s phase/rule helper.
This separation preserves the mode and capability boundary; the rendered test
uses modeled source/readiness and does not enable production routing.

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
actually received in permitted input reset notifications or through the
declaration-scoped custom recognizer metadata channel. They perform no host
device scan and open no device nodes. Both require protected policy and device
allowlists. `session --lua --input` delivers ordinary reset metadata;
`session --lua --recognizers` independently delivers metadata only for approved
native custom declarations' selectors, with no held state or ordinary input
callbacks. If both flags are present, ordinary resets supply the catalog.

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
queries require runtime evaluation and fresh desktop context. Custom-only
catalogs additionally require current recognizer capabilities. Copies that expire
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
| `context.layouts()` | Matching native layout graph on a fresh owning desktop, or `nil`; inspection only. |
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

### Native layout inspection

`context.layouts()` is runtime-only and requires the owning registration's
`context` capability, not permission to execute window actions. It copies
broker-collected Hyprland 0.56.2 Lua metadata matched against that same complete
desktop snapshot. Unsupported backends, absent/inconsistent metadata, greeters,
secure overlays, stale context and inhibition return `nil`. Ordinary geometry
remains available when only optional layout collection fails. An observed lock
invalidates the whole desktop context and remains latched until the verified
unlock transition.

The returned `NativeLayouts` contains:

- `fence`, `generation`, `revision`, `captured_at`: the owning broker fence and
  exact desktop sample identity. Generation/revision/fence identities are
  canonical decimal strings; `captured_at` is `{secs, nanos}` and is conservatively
  the original geometry capture time, not completion of later metadata reads.
- `workspaces`: at most 64 records with `id`, optional `output`/`monitor`, native
  `active`/`special` flags, tiled algorithm `layout`, `windows` and `columns`.
- `windows`: at most 512 globally. Each has exact hexadecimal `id`
  (`address:stable`), goal `rect = {x, y, width, height}`, `hidden`, `floating`,
  `grouped`, native `fullscreen` mode, and optional `column = {index, row}`.
- `columns`: ordered scrolling records with native `index`, normalized primary
  `width` fraction and ordered `members` containing exact window lifetime IDs.
  Native column indices/member rows start at **zero**; Lua arrays start at one.
- `profile`: present only when an active, non-special workspace uses the
  scrolling layout and one more locked batch succeeded within the same budget.
  It copies the stock settings of the
  [validated phone profile](hyprland-scrolling.md#validated-phone-profile):
  `direction`, `gaps_in`/`gaps_out` as `{top, right, bottom, left}`, `border`,
  `focus_fit` (`scrolling:focus_fit_method`, 1 = fit),
  the `workspace_rules` count and each output's `reserved = {output, area}` with
  `area = {left, top, right, bottom}`. It is read after the graph, so it does not
  share its capture time.

Copies can be changed locally; subsequent queries still return the original
host data. Receiving, copying or retaining a view never renews its 250 ms lease.
Native collection gets at most 50 ms inside the existing combined 200 ms context
budget. Mapped windows, goals, outputs, workspaces, graph membership and sample
identities must match; incomplete or mismatched graphs are not published.
Large views use the existing bounded chunked context transfer without truncating
window/member lists. This inspection-only graph is excluded from private
recognition packets, preserving their existing complete core context and size
budget; it is not yet worker preclaim evidence.

These are **animation goals, not rendered hit tests**. No effective camera
offset/work area, arbitrary workspace-rule resolution, empty-strip scope, input
ownership or execution grant is implied. G11 uses the attached `profile`
through its own preclaim, proof and executor, not through this view. See
[continuous viewport motion](hyprland-scrolling.md).

| Type | Fields |
| --- | --- |
| `Snapshot` | `generation`, `revision`, `captured_at`, `state`, `outputs`, `windows`, `regions`, `secure_overlay`. `generation` matches the session fence. |
| `Binding` | `seat`, `session`, `uid`, `compositor_pid`, `compositor_start`, `kind` (`user` or `greeter`). Identity comes from broker process/logind verification. |
| `OutputState` | `geometry`, `geometry_generation`, `active_workspace` (integer or `nil`), `screen` (`on` or `off`), `keyboard_known` (boolean), `keyboard` (`Keyboard` or `nil`). `keyboard_known=false` means unknown; only a known state with `keyboard=nil` establishes hidden. |
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

## Negotiated capabilities and executor readiness

These read-only queries run in callbacks, named handlers, timers and tasks:

| Function | Result |
| --- | --- |
| `capabilities.current()` | An owned `CapabilityView`, or `nil` without fresh metadata. |
| `capabilities.has(name)` | Whether the fresh registration negotiated that flag. |
| `capabilities.ready(name)` | Whether the parent executor currently reports capacity for that effect family. |

`CapabilityView` contains `fence`, `negotiated`, `ready` and `sampled_at`.
The two capability lists are ordinary one-based arrays. Fence identities are
exact decimal strings; sampling time uses `{secs, nanos}` on the system
monotonic clock. Editing a returned table does not change the host view.

Names are `context`, `observations`, `lua_events`, `input_events`, `custom_recognizers`, `lua_actions`,
`runtime_rules`, `hyprland_actions`, `morf_actions`, `power_actions`, `commands`
and `synthetic_input`. Unknown names return `false` for forward feature
detection. Nonstring, empty or greater-than-128-byte names raise an error.
Configuration-time queries raise an error.

The session supplies the actual broker-negotiated flags after checking the
registration reply. Lua declarations and system policy alone do not populate
this view. Negotiated flags remain fixed within the session fence; readiness
can change. `commands`, `lua_actions`, `power_actions`, `hyprland_actions` and
the Morf submission transport's `morf_actions`
can be ready. Window effects require protected `policy.allow_hyprland_actions`
and `session --hyprland` on a routing broker; declarations alone grant nothing.
Observation sessions report an empty readiness list. Source loading, executor
capacity and adapter/context expiry can withdraw readiness.

Morf readiness describes the submission transport's fresh metadata and capacity,
not native ownership or rendered geometry. `session --morf` is an explicit
opt-in; the current broker rejects public negotiation until native rendered
context and physical ownership gates pass. A Lua declaration cannot enable it.
Once negotiated, one Morf effect per offered ordinal may join a named plan's
other permitted effects. Its completion means controller acceptance, not a
finished animation. Cancellation, missing outcomes and partial plans cannot
manufacture successful completion. See [Morf integration](morf.md).

These queries grant no effects. A negotiated flag is registration metadata;
current policy, fresh owning context, exact targets and a separate broker grant
still govern each operation. Readiness uses the same executor calculation as
broker availability, sampled before transport to Lua. It is not an atomic
promise that a later operation can execute.

The parent refreshes unchanged metadata at 100 ms intervals and replaces an
unsent snapshot when readiness changes. One queued snapshot and one correlated
in-flight acknowledgement bound transport. The original sampling timestamp is
preserved. At 250 ms, or with a mismatched context fence, `current()` returns
`nil` and both predicates return `false`. Stop clears the view before callbacks.
Use [the capability example](../examples/lua/capabilities.lua) for inspection.

## Callback limits

Native deserialization preflights expanded Lua table keys and values, counting
every repeated reference separately. The configuration compiler, declarative
export and `config.validate` allow at most 1,048,576 expanded values, 64 nesting
levels and 16 MiB of conservative expansion charges. Each nonstring value costs
16 bytes; strings cost their byte length. These charges bound traversal and
prevent small aliased/cyclic tables from bypassing native decoding limits; they
are not measurements of total Rust heap or RSS. Source, model, VM arena, fuel,
wall-clock and supervised-process limits remain separate and unchanged.

Before decoding, configuration collections retain the native schema limits:
1,024 rules, 256 device selectors, 64 layers, 128 shapes, 64 modes and 64 custom
recognizers. Every table entry counts, including non-array keys. The compiler,
export, validation and both offline replay constructors apply these checks.

Named action results are preflighted at 32,768 values, depth 8 and 512 KiB before
native decoding. Rule replacement uses the same value/byte bounds at depth 64.
Their final action-plan/transaction wire limit remains 32 KiB. Hook/effect
metadata uses 4,096 values/64 KiB at depth 4; recognizer metadata uses
65,536 values/1 MiB at depth 4. Offline replay retains its smaller existing
configuration budget. Preset names are bounded to 64 bytes and transaction IDs
to 256 bytes before native copying. `native_value_limits` in the API manifest
publishes these bounds.

An excessive action result follows ordinary failure handling: no returned
effects or buffered logs escape, and the callback VM retires. A caught invalid
replacement leaves its transaction open and unchanged, so a later valid edit
can still be proposed. These checks create no routing or execution authority.

Event subscription callbacks are observational: action constructors produce declarations and
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

Cancelling a timer clears its stored callback reference, including when its
handle remains reachable. A one-shot clears that reference when it starts; its
running executor keeps the callback alive until completion. Self-cancellation
does not interrupt the current invocation. These releases allow captured state
to be collected without recycling timer identities or replenishing the
generation's lifetime quota.

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

`task:status() -> TaskState` returns `pending`, `running`, `sleeping`, `waiting`,
`completed` or `cancelled`. `task:cancel() -> nil` is idempotent. Cancellation stops later
resumes and releases the retained coroutine at the next scheduler boundary.
A task cancelling itself finishes its current bounded slice; that slice's logs
are discarded. Cancellation cannot roll back Lua closure changes. A completed
task remains completed when cancelled. Errors/resource failures retire the VM
and discard the current scheduler wake's logs; other service components remain
supervised independently. Successful source/generation replacement retires all
old task state, while rejected reloads preserve the active generation.

`task:join() -> TaskTerminal, TaskWake?` is available only inside another scheduler
task. It waits for that exact task to complete or cancel without blocking a thread
or polling its status. While suspended the caller reports `waiting`; it remains
within the 64-active-task bound. Completion or target cancellation schedules the
caller on a later wake, sharing the existing eight-resume budget and stable order.
The result is `completed` or `cancelled`, followed by that resume's fresh wake
payload. An already-terminal target returns its state immediately with a nil
wake. Returning from a join does not confer authority from either task's context.

Cancelling a waiter releases its coroutine and dependency at the next scheduler
boundary without cancelling the target. Reusing a completed/cancelled task's
capacity slot cannot retarget its retained handles or joiners. Each active task
has at most one dependency; self-joins and dependency cycles raise errors under
the existing fail-closed VM policy. Generation replacement retires all waits.
Use an ordinary cancellation timer when a target needs a timeout. Joins carry no
device descriptors, synthetic ownership or action grants.

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
- `config.validate(config)` preflights expanded values, applies the Rust schema and
  model validation, and returns a normalized independent copy with defaults.
  It is configuration-only and performs no I/O or effects. It does not reload an
  active configuration, register callbacks, approve new source bindings or grant
  capabilities. The complete source compilation still validates every named
  handler, rule hook and custom recognizer registration. Invalid models and
  excessive/cyclic input raise an error without modifying the original table.
  See the executable [validation example](../examples/lua/config_validation.lua).
- `rules.define(table)` returns a rule declaration.
- `units.duration(string)` and `units.distance(string)` require a string with
  explicit units; model validation checks its unit and range.

The returned configuration accepts `version`, `mode`, `timing`, `shape_capture`,
`policy`, `devices`, `layers`, `modes`, `shapes` and `rules`. Rust schema validation remains
authoritative, including unknown fields, references, priorities, resource bounds,
activation keys, permitted seats and source selectors. A rule accepts `id`,
`enabled`, `priority`, `devices`, `seats`, `contexts`, `scope`, `output`, `screen`,
`keyboard_visible`, `require_keys`, `keys_throughout`, `layer`, `delivery`,
`trigger` and `action`. Existing defaults and exact units remain in effect.

### Timing, policy, selector and shape fields

Every `timing` field is optional. Durations need `ms`, `s` or `m`; distances
need `mm`, `px` or `norm`.

| Field | Default | Meaning |
|---|---|---|
| `tap_max` | `250ms` | Longest press, or contact group, that still counts as a tap. |
| `tap_gap` | `300ms` | Wait after a release before a tap count is final. |
| `hold_after` | `600ms` | Press duration that reports a hold; must exceed `tap_max`. |
| `join_window` | `120ms` | Time for further contacts to join a touch group before a touch claim. |
| `decision_window` | `300ms` | Longest buffered input waits for a decision before ordinary delivery. |
| `swipe_timeout` | `1500ms` | Longest a contact group may last and still complete a swipe. |
| `tap_motion` | `3mm` | Contact movement that disqualifies a tap. |
| `fallback_motion` | `0.01norm` | Normalized movement used without physical size; must use `norm`. |
| `pan_slop` | `3mm` | Coherent movement before a pan begins. |

Validation requires `0 < join_window <= decision_window <= 5000ms`; buffered
routing additionally requires `join_window < decision_window`.

`policy` is protected system policy. `allowed_devices` names the selector IDs a
broker may open and `allowed_seats` the seats it may serve (default
`{"seat0"}`). `allow_runtime_rules`, `allow_input_events`,
`allow_power_actions` and `allow_hyprland_actions` default to false and must be
booleans.

A `devices` selector matches a non-virtual device whose `identity`, `path`,
`name`, `physical`, `unique` and `seat` equal every given value, which reports
every listed `keys` code and every listed `absolute_axes` code (numeric
`EV_ABS` codes, for example `53`/`54` for multi-touch X/Y), and which has every
listed input `properties` code.

A `shapes` template has a `name`, 2–2048 `points`, a `threshold`
(minimum similarity in `(0, 1]`) and a `margin` (minimum lead over the runner-up
in `(0, 1]`; a smaller lead is ambiguous and produces no match). Optional
`rotation_invariant` aligns the input to the template's best rotation and
`reflection_invariant` also tries horizontally and vertically mirrored input.
A match reports `ShapeMatch {name, confidence, margin}` with that lead. Wheel
evidence reports `amount`, the signed steps accumulated since the previous
match; its magnitude reaches the rule's `minimum`.

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

Sequences accept registered custom triggers and nested sequences: 2–16 direct
children, at most 64 leaf recognizers and eight nesting levels. Each nested
timeout stays anchored to its own admission. One native coordinator issues only
the active step's tickets and returns flattened leaf evidence under the outer
result identity. Source approval, context/frozen targets, ownership and cleanup
remain native checks. Runtime edits may compose already approved declarations,
not register or change callbacks. See
[composed_recognizers.lua](../examples/lua/composed_recognizers.lua).

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

## Experimental targeted Hyprland window adapter

`prek session --hyprland` opts into protected-policy `close`,
`move_left_and_follow` and `move_right_and_follow` actions on a routing broker.
The native 0.56.2 Lua provider additionally supports experimental `reorder_up`
and `reorder_down` with a complete frozen vertical scrolling-layout graph;
see [the selection and execution contract](hyprland-reorder.md). A rule whose
action is `scroll_stack` on a `side_left`/`side_right` strip moves the
validated phone workspace continuously; see
[continuous viewport motion](hyprland-scrolling.md). Handlers resolving to
`scroll_stack` stay unavailable because they carry no workspace proof.
Use `--lua --hyprland` for approved named handlers returning
`actions.hyprland {action=...}`. `--commands` and `--power` are independent
permissions and are required only for their effect families. The protected
system configuration must set `policy.allow_hyprland_actions=true` (default
false) and declare an enabled supported window action or approved handler
requiring `hyprland_actions`. User overlays cannot grant this permission;
observe brokers and inspector registrations refuse it. **Production takeover
remains disabled; the flag does not bypass the routing gate.**

The session opens only the compositor matching its broker-provided
PID/start/executable/UID/seat binding and refreshes the adapter from that context.
The Rust host equivalents remain `HyprlandAdapter::new(client, view)`,
`Executor::set_hyprland_adapter(adapter)`,
`Executor::update_hyprland_context(view)` and
`Executor::sync_hyprland_context(runtime, instance, view)`. These are not Lua
authority setters. [The example](../examples/lua/window_actions.lua) declares
protected touchscreen actions; offline compilation executes no effect.

The adapter supports **Hyprland 0.54.3** and exact **0.56.2**. For 0.56.2,
`j/status.configProvider` must be `hyprlang` or `lua`. The adapter freezes the
selected dialect at installation and rechecks it immediately before mutation
within the existing IPC deadline. Changed, missing, malformed and unknown
providers refuse without mutation; there is no syntax fallback or mutation retry.
This does not change Prek's own Luna API. The Lua dialect uses closed, typed
dispatcher factories with an explicit window or monitor. Its window factory
checks the frozen `stable_id`, mapped state, workspace, output name, window goal
box, and output position/pixel size/scale/transform/DPMS/active workspace before
constructing an effect. An active special workspace refuses window effects.
These checks run inside the same native Lua factory, closing the gap after
the final context sample for those fields; a focus change does not select a
different target. Output names are bounded byte-escaped Lua literals, not code.
The expected native scale is reconstructed as binary32, matching Hyprland's
stored float and its Lua getter; this is exact identity, not an epsilon test.
Its power factory checks the monitor exists and
has the frozen name before constructing an effect, never passing a nil monitor
that could select every output. No user-supplied code or raw dispatch API is added.
The provider-sensitive
dispatch behavior is defined by the pinned
[Hyprland source](https://github.com/hyprwm/Hyprland/blob/v0.56.2/src/debug/HyprCtl.cpp).
The factories follow the pinned
[Lua dispatcher bindings](https://github.com/hyprwm/Hyprland/blob/v0.56.2/src/config/lua/bindings/LuaBindingsDispatchers.cpp).
The exact
window address **and stableId**, output/geometry generation, positive numeric
workspace and geometry are frozen at offer time. Missing or malformed targets,
another binding/seat/fence, secure overlays, lock inhibition, expired context,
retired geometry and changed window lifetimes refuse execution. A focus change
never retargets an offer. Move/follow computes the adjacent absolute workspace
from the starting window, rather than from later focus. Moves currently require
a single-output desktop; cross-output workspace-rule topology remains pending.
Numeric destinations must fit stock Hyprland's positive 32-bit workspace parser
(`1..2147483647`); moving left from 1 or right from 2147483647 is unavailable.
Continuous scrolling-layout actions remain unavailable. Legacy Hyprlang address
dispatchers cannot enforce the same atomic window guards: they retain a
read/dispatch race despite the pre-dispatch checks. Goal boxes do not establish
rendered visibility, and these guards do not prove atomic lock/overlay state or
monitor lifetime across removal/replacement. Production routing remains gated.

Routing checks protected policy, current negotiated availability and a valid
frozen target before claiming input. Missing/unsafe targets, a different active
workspace, screen-off/locked/secure-overlay state, unsupported direct operations,
invalid adjacent destinations and multiple outputs for direct moves preserve
ordinary input. Broker dispatch repeats target/policy checks. A handler's
declared family must have an eligible frozen target; its returned plan is
validated after the grant and may still be refused.

`scope="everywhere"` keyboard rules can set an explicit output and freeze its
mapped focused window at the initial key event. They do not perform spatial
hit testing or infer rendered geometry; a later focus change does not retarget
them. App-content and other spatial scopes still require provider-proven
rendered visibility and correct starting geometry. Stock IPC animation goals
alone do not authorize app-content touch consumption. The stock sampler marks a
window `visible` only when the same locked context batch shows that no window or
workspace movement can animate. That means `animations:enabled` is off, or each
of `windows`, `windowsIn`, `windowsOut`, `windowsMove`, `workspaces`,
`workspacesIn`, `workspacesOut`, `specialWorkspace`, `specialWorkspaceIn` and
`specialWorkspaceOut` appears once and is disabled. In that case the stock tick
warps every value to its goal at the next frame. The window must also be
unhidden, report its fullscreen state, sit on its monitor's active workspace
with no special workspace shown, and not be covered by another fullscreen
window there. With movement animations enabled, stock windows stay not visible.
See [the keyboard recipe](../examples/lua/window_keys.lua) for exact-output
global key rules; the touchscreen example uses app-content targeting.

Each effect requires a separate current broker grant. Direct declarations act
only on discrete/begin; named handlers choose their phases explicitly. The
executor preflights the complete plan and allows at most one Hyprland or power
operation per plan. Exact argv commands and observation effects can accompany
it, retaining partial-effect uncertainty. Source/candidate cancellation and
generation retirement use the existing action lifetime rules.

The remaining grant, capped at 200 ms, covers independent stock context/lock
preflight, one exact address-targeted mutation and state verification. Close
uses `closewindow address:...`; move/follow uses
`movetoworkspace N,address:...`. No implicit active-window selector or shell is
used. Applied completion means the compositor accepted the request and the
original window disappeared, or that same window moved to the exact workspace,
remained on the output and became focused there. It does not prove rendered
animation completion. Missing confirmation or any error after request bytes
were written reports uncertainty and is never retried.

**Stock IPC race:** Hyprland dispatch selectors accept an address, not an
atomic address-plus-stableId guard. Preflight rejects already replaced
windows, but a replacement between the final query and dispatch can still be
affected. The adapter cannot guarantee lifetime identity across that interval;
postchecks do not remove the race. Lock/topology can also change after
preflight. This limitation is separate from broker authority and must be
resolved or explicitly accepted before production window routing is enabled.

Run the owned local fixture with `nix run .#cage-test -- --layers --input
--power --windows` (add `--visible` for a visible nested window). It uses actual
Lua/helper/stock IPC and test receiver close receipts with explicit model
seat/grant metadata; it does not establish physical root/logind takeover.

## Experimental output-scoped DPMS adapter

`prek session --power` opts into protected-policy DPMS actions on a routing
broker. Use `--lua --power` for named Lua handlers; `--commands` is independent
and is required only for command effects. The protected system configuration
must set `policy.allow_power_actions=true` (default false) and declare an enabled
blank/wake rule or an approved handler requiring `power_actions`. A user overlay
cannot grant this policy. Observe brokers and inspector registrations refuse
power negotiation. **Production routing remains disabled pending takeover
acceptance; this flag does not bypass that gate.**

A rule can set `output="eDP-1"` to freeze an exact verified output for keyboard
or other non-spatial input. When a device mapping exists it must name the same
output; a missing/mismatched output is unavailable. No name is inferred from
focus or from a single-monitor layout. The complete output state and generation
are validated throughout the gesture. Normal input passes through when policy,
target or current power availability is missing. Unsupported lock-and-blank
also remains unavailable before claiming input.

The Rust session executor additionally supports explicit host installation:
`actions::PowerAdapter::new(client, view)`,
`Executor::set_power_adapter(adapter)` and `Executor::update_power_context(view)`.
The host supplies a broker-approved binding/view and an already verified
Hyprland client; these are not Lua authority setters. The public session agent
uses `Executor::sync_power_context` to open the exact compositor from its fresh
broker binding, verify process birth/executable/UID and private endpoint, and
install the adapter. It refreshes this view without depending on a running Lua
VM. Fence changes retire the old adapter. This supplies no raw fd or authority
setter to user Lua.

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
change host-display DPMS. Authenticated broker/logind and production-routing
acceptance, lock-provider integration and physical phone/power trials
remain required. The installed `power.lua` declaration shows exact
output selection, separate screen-on/off rules and a named handler that performs
an effect only in the begin phase. Offline checking performs no effect.

## Service inspection

These runtime callback/task queries return independent tables. They do not
require desktop context, and editing a returned table changes no service state.

`config.current() -> ConfigurationView | nil` returns `{revision,config}` using
the owning VM's acknowledged Rust model. `revision` is an exact decimal string.
The configuration contains the documented `Config` fields, including protected
policy, selectors, timing, shapes, layers, modes and rules. Source closures and
approved module code remain in their original source snapshot. A pending rule
transaction leaves this view unchanged; a successful worker receipt updates it.
A rejected receipt reporting an unknown revision retires the view and yields
`nil` rather than presenting an unacknowledged model.

`policy.current() -> PolicyView | nil` returns `{revision,policy}` from that same
acknowledged model. Its allowlists and permission booleans constrain user code;
they are distinct from current negotiated capabilities, executor readiness and
one-use execution grants. Query `capabilities` for negotiated membership and
readiness. No setter or permission expansion is exposed here.

`status.current() -> StatusView | nil` copies `{fence,mode,state,revision,pending,
last_reload_error?,context_error?,sampled_at}` from the authenticated broker
connection. `mode` comes from its owning registration; `state` is the broker's
reported context state, including `unknown`, without establishing native lock
completion or authentication. `revision` is its context snapshot revision as a
decimal string, or `"0"` when unavailable. `pending` counts outgoing replies
queued for this requesting agent connection; it is not a global gesture count
or input-ownership report. Error strings are bounded and optional.

The parent requests status every 100 ms while loading/running Lua, with one
outstanding request. It preserves the original broker packet time in
`sampled_at={secs,nanos}` and relays the latest view through the existing bounded
metadata channel. Parent/helper delivery and capability refresh cannot extend
status freshness. Status likewise cannot refresh capability readiness. The
getter rechecks the original 250 ms lifetime and current fence after copying.
It returns `nil` before the first report, after expiry, on a mismatched fence
or at stop; it can still report a fresh `unknown` state with a provider error.
None of these views confers input, action or authentication authority.

See [inspection.lua](../examples/lua/inspection.lua) for a runtime recipe.

## Offline replay

`replay.new(config = nil) -> Replay` creates a generation-local replay using
the same Rust engines as `prek replay`. Omit the configuration for generic
normalized/button/touch observations, or pass a complete validated configuration
for configured rule outcomes. The configuration is copied; subsequent Lua edits
do not change the replay. Both configuration evaluation and runtime callbacks or
tasks can use this API without live context or effect capabilities.

Custom callbacks require runtime evaluation and the owning approved source
snapshot. Each recorded seat gets fresh isolated source closures; replay never
reuses or changes live callback state. Configuration-time custom replay and
custom replay inside an offline callback are rejected. Native proposals, phase
ordering, source incarnations, frozen targets, timeouts and cancellations use
the ordinary Rust coordinator. Source-declared nested steps use the same
validator, exact source catalog and native coordinator as worker reconstruction.
Parsing a graph grants no input ownership, context or executable callback fence.

- `replay:push(record_json)` accepts one native recording JSON object, beginning
  with `{"record":"header","version":1}`. Use the existing recording format for
  device registration, frames, mapping, reconciliation, removal, ticks and final
  expectations. Configured replay additionally accepts
  `{"record":"context","at":duration,"binding":Binding,"snapshot":Snapshot}`.
  It must follow the header. Context generation/revision, session kind, bounds,
  capture age and monotonic timestamps are validated. Session generation must
  be below `u64::MAX`; timestamps reserve one hour below the signed 64-bit
  nanosecond ceiling for native deadlines. Context is **simulation data**,
  not authenticated provider or lock evidence. Generic replay and live engines
  reject context records. JSON preserves full unsigned identities on input. Push has no
  file access, live input injection, context setter or action dispatch.
- `replay:finish(check = false) -> ReplayResult` returns `{mode,records,bytes,
  checked,outcomes,json}` and releases the engine. `mode` is `observations` or
  `rules` (`routing` for the downstream driver below). `outcomes` copies the native observation/event schema, using canonical
  decimal strings for opaque identities and unsigned integers above Lua's signed
  range. `json` preserves the native CLI-compatible JSON, including exact unsigned
  identity numbers. Edit returned tables freely; they have no live effect.
- `finish(true)` requires a matching final `expected` or `expected_rules` record
  for its engine. Missing or mismatched expectations raise an error. An unchecked
  finish sets `checked=false` and never claims that an expectation test passed.
- `replay:status()` copies `{mode,state,records,bytes,outcomes}`. States are
  `open`, `finished`, `failed` and `closed`. `replay:close()` is idempotent and
  discards the engine immediately. Push/finish after a terminal state fail.

Each VM permits four open replays, each with at most 4,096 records, 32 KiB per
record, 1 MiB total input, 16 distinct source device identities, 4,096 outcomes
and 1 MiB encoded outcomes. Outcome accounting is conservative and includes
per-push JSON delimiters. A configuration copy is preflighted to 65,536 expanded
values, 64 nesting levels and 1 MiB including conservative value overhead, so
cyclic/shared tables cannot cause unbounded native allocations. Every push and finish is still subject to the owning
VM's fuel, wall-time and memory limits; split longer offline work across task
wakes. The CLI remains the external file reader for larger recordings. A record,
resource or expectation error retires that replay; an uncaught callback error
also follows normal VM retirement. Finish, close, errors and VM replacement
release owned replay state. No commands, Morf controls or power actions contained
in replay outcomes are executed.

Up to four custom callback VMs are shared across all replay handles in one
owning VM; at most 16 recorded context seats are retained. VMs are created only
when a ticket reaches a callback and released on finish/close/failure. Each push
and finish has a 250 ms aggregate host-work budget in addition to normal source
and callback instruction/memory/wall limits. Long replay work still needs task
wakes; this API does not make callback work asynchronous.

### Downstream routing traces

`replay.routing(config) -> Replay` uses the actual native raw router. It requires
an explicit route-mode configuration and returns `mode = 'routing'`. Existing
`replay.new(config)` remains recognition-only. The new handle accepts a separate
strict `routing`-tagged JSON trace with explicit recorded context, exact neutral
descriptor/consumer admission, action availability, independent input/wall
clocks, poll/write acknowledgements and retirement. Source-bound custom callbacks
use the same runtime restrictions, isolated VMs and shared quotas as configured
replay. There are no new live capabilities or effects.

`finish(true)` compares final complete downstream expectations: offered raw
frames, exact write receipts, fenced dispatch proposals, diagnostics and recovery
requests. Every admitted transport must be retired or recovered explicitly.
Recorded readiness and recovery requests are simulation assertions, not proof of
kernel release or real consumer acceptance. See [routing replay](routing-replay.md)
for every record/outcome field, admission rule, deadline and failure contract.

Context, normalized input and wakes advance only on recording timestamps, not
elapsed host time. Refresh context before its 250 ms lifetime; after revocation,
a later context needs a new session generation. Session replacement reconciles
held input before recognition resumes. Cleanup runs for delivered callbacks on
source/context loss, completion and checked/unchecked finish. EOF cleanup does
not add a recorded recognition outcome. Ordinary input subscribers, source timers,
tasks and action handlers are not dispatched; callback logs are discarded.

`prek replay PATH --config config.lua --check --json` uses this same driver and
the compiled original source. Root callback execution is refused. A custom TOML
model cannot recreate its closures; provide the original Lua source. CLI limits
remain the recording format's larger file bounds. This is pure configured
recognition, not proof of raw routing ownership, delivered input, rendered UI
or physical behavior.

See [offline_replay.lua](../examples/lua/offline_replay.lua) for both modes.
The [custom replay example](../examples/lua/custom_replay.lua) runs a checked
recorded-context fixture with a fresh source-bound callback.

## Limits, libraries and errors

The configuration VM includes Luna's core libraries and the `prek` module.
Ambient `io`, `os`, `package` and `debug` are absent. `print`, `warn`, `dofile`,
`loadfile`, `load` and `collectgarbage` are disabled. Additional imports use the
captured module loader described above; ambient search paths are unavailable.
The initial random seed is deterministic. A configuration cannot yield or return
multiple values.

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
killed and reaped. A single operation can outgrow the slice-granular VM check
(for example repeatedly doubling a string); the child then aborts at its data
limit and the error reads "Lua configuration exceeded the isolated compiler
memory limit" instead of runtime abort text.

`oslo make soak-lua` runs one virtual hour (`--seconds` overrides it) of a
5 ms periodic timer, a sleeping task loop and context callbacks, replacing the
runtime every virtual minute. It fails if resident memory grows by 4 MiB or more
after warm-up or if the 99th-percentile tick exceeds 5 ms, and writes its
measurements to `target/tmp/lua-soak/receipt.json`. These bounds do not establish a complete system-call sandbox.
Syntax/runtime errors include the named Lua chunk; schema errors identify invalid
data. A rule that fails on its own is reported as
`chunk:line: rules[index] ("id"): reason`; the line is that of the statement
which called `rules.define`, because the pinned Luna attributes lines per
statement, and it is omitted for plain-table rules. A rejected model does not
become the worker's active configuration.
