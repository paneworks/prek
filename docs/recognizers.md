# Custom recognizer proposal gate

`recognizers.define` declares a Lua callback and returns its native trigger.
`recognition::Gate` is the Rust ticket/response component. Source bindings connect
approved callbacks through the supervised Lua helper; opt-in native rule/routing coordinators create and
arbitrate their tickets. The private observation worker now transports tickets,
readiness acknowledgements, native validation receipts and cancellation identities.
The broker/session observation path now carries these messages after protected
source approval and verified session admission. Live privileged admission and
physical routing acceptance remain unverified; source, native coordinator and
real helper tests use explicit admission/context models.

The sole routing worker also reconstructs complete native composed plans across
private subscriptions, rule edits and configuration reloads. Lua/TOML structural
validation accepts nested custom declarations; enabled callback execution still
requires a fenced coordinator and exact original source bindings. Deterministic custom replay runs actual isolated
callbacks with recorded context through the CLI and Lua API; see
[offline replay](lua-api.md#offline-replay) and
[compiled native plans](routing-chains.md#compiled-plans-in-the-sole-worker).

## Lua constructor

```lua
local p = require('prek')
local trigger = p.recognizers.define('my_recognizer', {
    selectors = {'keyboard'},
    inputs = {'key'},
    reply_within = '250ms',
    timeout = '1s',
    wake = '20ms',
}, function(event)
    if event.cause == 'cancel' then
        -- Release private state for event.candidate, then return nil.
        return nil
    end
    -- Return nil while pending, or {phase='discrete', data={...}}.
    return nil
end)
```

Use `trigger` in `rules.define {trigger=trigger, ...}`; one declaration can serve
multiple rules. Names are unique within the source snapshot. The declaration
has no effects during configuration evaluation and cannot register at runtime.
The [complete hold example](../examples/lua/custom_recognizer.lua) uses native
activation conditions, stationary wakes, a semantic match and cleanup.

| Option | Contract |
| --- | --- |
| `selectors` | Required nonempty list of at most 64 device selector IDs, all configured and permitted by protected policy. |
| `inputs` | Required nonempty list from `key`, `relative`, `wheel`, `absolute`, `switch`, `touch`. |
| `reply_within` | Required explicitly qualified duration, 1–250 ms. Queueing never renews it. |
| `continuous` | Optional boolean, default false. True requires begin before updates/end. |
| `timeout` | Optional native candidate lifetime, default 1s, allowed 1–10,000 ms. |
| `wake` | Optional stationary wake interval, 5–250 ms. Missed wakes are skipped. |

Selector/input lists represent sets and are copied into canonical native order.
Lists must have consecutive integer keys and string values; unknown options,
holes, named list fields and mistyped values fail. Protected policy flags also
require actual booleans; strings such as `"false"` cannot enable a permission.
The constructor returns a plain native trigger with separate private binding
metadata. Editing input options later does not change either copy. Changing the
returned declaration so it differs from its private binding fails compilation;
rule-local timeout/wake changes still undergo normal native validation.

`cause=frame` and `cause=timer` receive a `RecognitionTicket`: `fence`, `id`,
`candidate`, `revision`, `rule`, `recognizer`, `requested_at`, `deadline` and
`input`. The input is a normalized frame containing only declared kinds and
selectors. IDs, generations, revisions and fence components are exact decimal
strings. Timer input retains the previous physical timestamp/sequence, clears
transitions and marks retained contacts non-fresh. Return nil for pending or a
strict `{phase=..., data=...}` table. Phases are pending/discrete/cancel for a
discrete declaration, or pending/begin/update/end/cancel for a continuous one.
Data is plain JSON-compatible data, limited to eight levels and 4 KiB encoded.

`cause=cancel` receives `RecognitionCancellation`: the same fence, candidate,
revision, rule and recognizer, plus `source`, `through` and `reason`. It has no
input or new ticket. Reasons include completion, source/context loss, rule edits,
expiry and cancellation; exact wire values are listed below. Clear private
candidate state and return nil. Cleanup cannot produce a proposal or effect.

Callbacks cannot yield and have a 20 ms / 100,000-instruction budget. Failures
retire the VM without retrying. Native rules own activation conditions, frozen
targets, arbitration and effects; a Lua result is a proposal. The callback is
private to ticket dispatch and receives no ordinary `events.on('input')` events.
Every custom rule must have its exact source callback binding, including disabled
templates. Modules and complete source/snapshot metadata are checked again on
VM reconstruction. `config.export` rejects callback-bearing custom rules;
retain the original Lua source.

Every constructor declaration is retained in the canonical native
`Config.recognizers` list, even when no rule uses it yet. The list is bounded to
64 unique names and contains metadata, not closures. Editing or adding entries
does not register a callback; all entries must match private source bindings.
`config.current().config.recognizers` returns an owned copy of this registry.
Runtime rule transactions may install a rule using a retained trigger without a
disabled template. They cannot declare another recognizer, change its selectors,
inputs, continuous flag or reply budget, or mutate the registry. Removing all
rules that use a declaration does not retire its source approval. Reload replaces
the approved registry together with the source fence.

The [runtime rule example](../examples/lua/custom_runtime.lua) uses
`session --lua --recognizers --rule-mutations` and protected
`allow_runtime_rules=true`. Lua preparation, broker preparation and native
activation each check original declaration approval; only an accepted native
receipt advances the rule revision. Route brokers negotiate it too; their worker
routes only devices the bound Wayland compositor held, taken over at a neutral idle cut, which is
not yet accepted on real hardware.

An owning observation session uses `prek session --lua --recognizers`; protected
system policy must enable `allow_input_events` and the selected devices. This
permission defaults to false. Ordinary `--input` is a separate opt-in. Custom-only
sessions receive device metadata scoped to the union of approved native custom
declarations' selectors, including unused declarations and disabled templates. Initial metadata precedes
tickets; attach, reconciliation, mapping, replacement and removal update the same
`p.devices` catalog. This channel carries no held keys, contacts or transitions
and invokes no ordinary input handlers. When ordinary input is also enabled,
its existing reset notifications populate the catalog instead.

Metadata requires current owning desktop context, negotiated recognizer
capabilities and helper readiness. Delivery and helper execution recheck fence,
seat, declaration scope and the original 250 ms freshness limit. Packets and
queues are bounded; excessive metadata retires the affected observation stream,
not the input reader. Catalog queries become unavailable when the custom
capability view expires, even if desktop context remains fresh. There is no host
inventory scan or atomic full-device inventory guarantee. Source-declared custom
sequences retain the same scoped metadata contract. Live privileged sole-worker
routing acceptance remains unfinished. Production routing stays gated.

## Private sole-worker bridge

The descriptor-owning routing worker implements the same typed recognizer and
rule commands as the observation worker. Each seat uses its exact subscription
fence, native routing coordinator and current protected capability/context view.
Readiness loss cancels custom work; regrant requires neutral input before another
gesture. Device metadata is scoped to approved declarations and refreshed after
native readiness. Ordinary normalized input retains its observe-mode restriction:
negotiating `InputEvents` does not enable it in routing mode.

Commands wait behind a drained native input watermark rather than executing in
the private socket reader. Wall time independently checks ticket, context,
capability and command expiry. Output polling and complete-write acknowledgments
also use an independent monotonic clock, retaining bounded delivery-lateness
checks without advancing physical recognizers through unread frames. The shared
command/outcome queue is bounded; commands expire after 250 ms. Native validation
receipts precede resulting outcomes. Successful rule edits retire old tickets and
virtual transports; original descriptor-close acknowledgments remain separate.
Transport recovery cancels its custom candidates and invalidates parent effects.

Private-channel tests exercise this bridge with explicit context/readiness and a
test-owned metadata directory. `oslo make test-routing-helper` builds the matching
executable and runs the sole-worker protocol through its actual separately
supervised Luna helper: begin/update/end, exact source identities, native
consumption, ordered validation receipts and acknowledged closure cleanup.
The same check ships as `checks.<system>.native-routing-helper`; local `verify`
and native x86-64/ARM64 CI include it. The executable-provisioned case is ignored
by ordinary unit invocations; both explicit gates reject a zero-test selection.
Three ordinary helper integration cases also verify native continuous phase order,
late results after transport loss and exact-once unmatched raw fallback.

These tests open neither evdev nor uinput. This is not
privileged integration acceptance: CLI routing remains disabled, and the native
pre/post-grab backlog and authenticated ownership gates remain unresolved.

## Native proposal and delivery contract

The trusted input coordinator constructs `Candidate` from its verified rule,
source generation, eligibility revision, expiry and current router ownership.
It must recheck policy, active session, context, frozen targets and conditions;
input selection alone does not establish eligibility. `Candidate` deliberately
has no deserializer. A session client cannot submit its own eligibility state.

A `Definition` names explicit selector IDs and normalized input kinds, declares
continuous/discrete behavior and bounds the response window to 1–250 ms. Names
are at most 128 bytes; selector/rule/seat IDs at most 256, without controls.
Definitions are unique and immutable within a gate's fence. A frame projects to
only declared selectors and input kinds; undeclared contact data is removed.
Neither a declaration nor a frame confers input or action authority.

`Gate::offer` creates one monotonically numbered ticket from an eligible frame.
The response deadline is bounded by the frame's original timestamp and the
candidate's expiry. Delivery/queue time cannot renew it. Repeated or regressing
frame sequences and candidate/recognizer changes fail. There are at most 64
declarations, 64 active candidates/undelivered retirements combined, 64 pending tickets, 64 KiB per encoded ticket
and 1 MiB of pending encoded tickets. A full gate rejects new work without
evicting live tickets.

`offer_timer` wakes an existing, still eligible candidate when physical input is
stationary. The coordinator must check the current held state and conditions.
The ticket has `cause=timer`, a new Rust `requested_at` and response deadline,
and retains the last physical frame's timestamp and sequence. Changes, contact
births/releases and ended positions are cleared; retained contacts are marked
non-fresh. This never extends a previous ticket. Repeated wake times, changed
candidates and retired sources fail. The private observation worker schedules
approved wakes and retires work on source changes, rule edits and readiness loss.

`Gate::validate` consumes a known ticket and rechecks the current authoritative
candidate, source/eligibility revision, deadline and ownership. A forwarded or
draining source cannot validate a consuming response. A claimed source requires
that exact candidate to be the router's winner. Observation candidates cannot
be promoted into consuming candidates by changing their ownership category.
The response contains only the original fence/ticket, phase and bounded data;
it cannot supply a source, target, action, candidate or new deadline.

Discrete recognizers accept discrete or cancel decisions; continuous ones
require begin before updates/end. Pending responses produce no recognition.
Validated phases receive Rust-owned ordinals. Completed/cancelled candidates
cannot reopen, and accepting a newer reply excludes older replies. Returned
data is limited to eight nesting levels, bounded node traversal and 4 KiB after
JSON escaping. `retire`, `retire_source` and expiry remove pending work.

Retirement also queues exactly one `Cancellation` with the original fence,
candidate/revision/rule/recognizer/source, last issued ticket (`through`) and a
typed reason. Reasons are cancelled, completed, source_lost, context_lost,
rule_changed and expired. Notifications are at most 2 KiB each and reserve their
slot before candidate admission. They cannot be evicted under input pressure.
Drain `take_cancellations` even after an offer fails, because offers also expire
old candidates. If delivery cannot be maintained, retire that helper VM.

The result is a **validated proposal**, not a claim or execution grant. The
coordinator must still perform ordinary Rust arbitration, enforce its one-owner
rule and action permissions, and preserve fallback/draining behavior. It must
retire completed candidates, changed eligibility and lost sources. Existing
production routing remains disabled; observation negotiation requires the
protected source binding and session opt-in described above.

Source constructors create bindings automatically. The older Rust embedding
API, `lua::Definition::bind_recognizer`, remains available for host-approved input
subscriptions in protocol embeddings; native custom rules require canonical
source constructor metadata. At most 64 names/subscription IDs can be bound.
Bindings travel with the complete approved source snapshot; re-evaluation must
reproduce configuration and subscription metadata exactly.

After instantiation, the bound subscription receives only ticketed callbacks;
ordinary input dispatch skips it. `Runtime::recognize` requires fresh owning
desktop context, matching fence/seat, the separate `custom_recognizers` capability
alongside Lua events, observations and context, and the original unexpired ticket.
It checks these before execution and after
the callback. Unknown, expired or foreign context produces no proposal without
entering Lua. Repeated/reordered delivery is rejected before changing closure
state. Up to 64 delivered candidate identities are retained for cleanup.
Ticket/candidate/revision/fence/source/contact identities are copied as
exact decimal strings. Editing the copy cannot retarget the native proposal.

The callback returns nil for pending, or one `{phase=..., data=...}` table. Other
fields and yielding fail. Lua receives the existing 20 ms / 100,000-instruction
callback budget; expanded table traversal is bounded before deserialization,
including shared/cyclic tables. Data still obeys the Rust eight-level / 4 KiB
limit. Callback failure or expiry during execution retires the VM, discards its
logs/proposals and is never retried. Structured logging also preflights expanded
tables before native allocation.

`Runtime::cancel_recognition` delivers `cause=cancel` to the same bound callback
with only the cancellation identity/reason; there is no input, new ticket,
proposal or action grant. Identity fields including `through` remain decimal
strings. A previously delivered matching candidate may receive cleanup after
context/capabilities expire. Unknown or already cleaned candidates never enter
Lua. Foreign, retargeted and older-than-delivered cancellations fail. Cleanup
must return nil or no values, cannot yield and has the same callback limits.
Failure retires the VM and discards cleanup logs. Disposed subscriptions are
removed from the native ledger without invoking their callback. User closures
remain responsible for releasing their own state; the acknowledgement does not
prove erasure of arbitrary Lua data or physical input release.

`session::Session::request_recognition` and `take_recognitions` implement the
private supervised request/response channel, with fence/sequence/ticket checks,
bounded pending/results queues and normal stop retirement. Missing proposals
mean no recognition. This private transport does not create authenticated public
broker or worker admission. The caller must pass any returned proposal back to
the owning Rust gate; even a validated result still requires real routing
arbitration.

The private worker starts custom coordinators inactive. A protected subscription
must carry `custom_recognizers` and fresh owning desktop context before the worker
acknowledges readiness. Broad `input_events` permission alone never activates it.
Enabling reconciles held keys/contacts; revocation cancels native candidates and
purges unsent tickets. Only declared selectors/input kinds enter each ticket.
Queues and unread receipts remain bounded; congestion ends the affected stream.
Replies wait until the reader has drained input to its timestamp watermark.
Deadline/context checks use the current clock without advancing input timers past
unread frames. A receipt reports native proposal validation, not an input claim.
The session agent has an explicit `--lua --recognizers` opt-in. The broker grants
it only in observation mode, with protected input permission, Lua/context/observer
capabilities and exact approved source bindings covering the configured custom
declarations. It checks the live owning registration, exact agent executable and
desktop lock/context again before sending input or submitting proposals.
The sole routing worker carries the same typed callback bridge; actual privileged
custom-recognizer admission remains unverified.

The broker records each issued ticket, its actual delivery, the one submitted
proposal and its native worker receipt. Client proposals cannot select a source,
candidate, target, action or new deadline. A native cancellation purges queued
tickets and proposals. Only an exact cancellation acknowledgement releases its
reserved candidate slot; never-delivered candidates need no VM cleanup. Known
cancelled worker replies and in-flight client proposals retain bounded identity
reservations, so racing messages can be discarded once without reactivating work
or retiring unrelated callbacks. The agent also purges its unsent proposals by
candidate. Missing receipts or cleanup
expire the generation. Tickets and proposals share a 64-ticket / 1 MiB budget.
The helper must actually initialize and the ordinary worker subscription must
be acknowledged before the agent requests recognition readiness. The broker then
waits for the worker acknowledgement before delivering tickets. Helper failure
revokes readiness, discards unsent proposals and reaps the VM; cleanup identities
remain outside that VM until acknowledged. Retirement cannot reactivate under
the same fence. Whole configuration/session replacement clears the old ledgers.

The embedding revokes the Rust gate before calling `Session::cancel_recognition`.
The parent removes matching unsent work and retained results and discards any
in-flight proposal/logs. Identity-only cleanup has reserved capacity and runs
before ordinary queued callbacks, after pending context/capability delivery.
`take_recognition_cancellations` returns exact helper acknowledgements for the
embedding's ledger. Active candidates, queued/in-flight cleanup and unread cleanup
receipts share 64 reserved slots; consuming acknowledgements releases capacity.
These messages establish no execution, routing ownership or descriptor-release
proof. Whole-session stop discards queues and disables the old generation; the
supervisor owns process reaping and release of its VM state.

Actual privileged broker/kernel/physical acceptance remains required. Existing root-routing and
native handoff scripts do not establish live custom-recognizer admission.

## Native rule and routing integration

The internal declarative `custom` trigger embeds a recognizer declaration, a
1–10,000 ms recognition timeout and an optional 5–250 ms wake interval. Named
declarations must agree across rules and remain within the protected device/input
policy. Custom declarations may appear in bounded nested sequence steps.
Continuous consuming candidates must
start on a physical key/contact group; other declared changes may update that
group. Continuous consumption without such a group also remains unsupported.

Declaration discovery and source approval walk every nested trigger, preserving
canonical registration and exact callback matching across composition boundaries.
Baseline preflight and context requirements use the same walk. Parsing a custom
graph does not execute callbacks. Only the fenced root coordinator lends its
gate to the currently active step; there are no independent child ticket spaces
or future-step callback execution.

The private composition coordinator now lends the root's single native gate to
active custom steps, sharing candidate IDs, request/cancellation budgets and
readiness. Native built-in sequences retain their existing external identities.
Validated step results are collected without replaying physical records. An
input watermark prevents a delayed reply from borrowing already-drained input,
including input with the same timestamp. Parent timeouts also bound step tickets.
Private native fixtures cover these lifetimes, source replacement, sibling
isolation, timer wakes and restoration after child failure.
Native completion within an input record can project that record once into a
newly active custom step. The coordinator retains one local normalized snapshot,
not a per-step backlog; it does not replay the physical record. Source/target
checks and nested timeout anchoring precede admission. An asynchronous reply has
no current physical snapshot and cannot reuse the preceding record. Every step
also rechecks its parent source/permit; losing any admitted prefix transport
revokes the current step even when it uses another source.
An eligible native touch hold or completed custom touch step can instead hand
its unchanged held contacts to a new custom touch candidate. This native-only
continuation starts with a timer ticket: original physical sequence/time,
current held positions, no transitions and no fresh contacts. Its request time
is the actual activation/admission time; it copies the prior frozen permit and
rechecks source, contact membership, readiness, limits and parent deadlines.
The asynchronous input-watermark exception belongs only to that newly created
continuation identity, never an eager future-step candidate. Input loss,
reconciliation and mapping changes invalidate retained physical metadata.
A custom touch step can hand the same held contacts to a native swipe, pinch,
rotation or touch-shape suffix. The suffix gets a new private candidate and
motion origin at the actual admission time, preserving the original permit.
Shape capture starts with one sample of the current centroid; old eager motion,
paths and progress cannot become the suffix. A physical record timestamped before
that activation cancels under the native backwards-time checks, rather than
counting earlier motion. Admission does not advance the drained input clock.
A custom step completed after the group is fully neutral may instead wait for
a genuinely fresh group. Final custom proposals retain their original expiry
when translated to a parent outcome.
Public Lua/TOML parsing, portable source snapshots, broker source preparation,
runtime transactions and private worker reconstruction share one structural
validator. There is no serialized validation-mode selector. The original source
catalog is frozen for edits, including remove/restore; a source reload replaces
it only after complete bounded validation. TOML alone cannot recreate closures.
The [composed example](../examples/lua/composed_recognizers.lua) runs two approved
custom steps with one outer outcome. Direct sequences have 2–16 children, at most
64 leaves and eight nesting levels. Nested groups retain their own anchored
timeouts; evidence lists leaves in step order. Ordinary unfenced engines still
reject enabled callbacks before input access. These are source/native/private
worker and offline checks, not privileged, device or rendered acceptance.
The [native raw-group chain API](routing-chains.md) provides bounded multi-group
buffer/reserve, FIFO fallback and opaque ownership proofs. Built-in native
sequences use these through `routing::Routing`, with actual per-source step
admission and immutable dispatch receipts. Private custom ticket issue and reply
now validate complete native parent scopes before translating ownership to the
exact leaf. Leaf bindings cannot be created by replies or retargeted to another
parent/source token; their aggregate source metadata is bounded. Each ticket
also preserves the original pending buffer decision without shortening an
already-claimed continuous candidate's lifetime. The full composed worker/source
path and public admission still need integration.
`oslo make test-composition-helper` exercises a private native hold-to-custom-to-
custom-to-native-motion plan with source-bound callbacks in the actual supervised executable;
it is not public source admission or consuming-route acceptance.
`oslo make test-chain-helper` separately runs an actual supervised Lua child
through private consuming hold/custom/custom Routing, with exact parent/leaf
proofs and correlated cleanup acknowledgements. This is still model source,
readiness and context authority, not the complete sole-worker or physical path.

Built-in native sequences join a later step's eligible sources before its match,
so an explicitly reserved second-device press cannot escape while a hold matures.
The step must start after activation or satisfy the existing same-contact
continuation rule. Earlier future-step input remains ordinary and cannot be
claimed retroactively. A different frozen target cancels the chain before that
source is reserved; losing an admitted source cancels the parent. These native
routing model checks are included in `oslo make test-composition`, not claimed as
physical multi-device or custom-callback sequence acceptance.

`rules::Engine::with_recognition` and `routing::Routing::with_recognition` are
trusted embedding constructors. Ordinary constructors reject custom triggers
instead of silently running them without a coordinator. No Lua runs on the
input thread. The native coordinator requires a fresh owning desktop, validates
the normal rule activation/permit and frozen targets, filters input before
queueing, suppresses reconciled held input until neutral and bounds queued offers
to 64 entries / 1 MiB. Timers skip missed wakes and pause while a reply is pending.
A queued wake retains its original request time; transport delay cannot renew it.

`recognition_requests` exposes tickets; `recognition_response` rechecks the
current native candidate, rule revision, source, context/conditions and permit.
Successful replies emit normal typed rule evidence with Rust ordinals and the
rule's frozen action/targets. Timeouts, source/context loss, queue exhaustion,
invalid proposals and completion retire native candidates and generate cleanup.
Reload and acknowledged rule edits preserve ticket monotonicity, revoke old work
and reconcile held state. Runtime rule edits may reuse source-approved
declarations, including restoring a removed rule, but cannot add declarations.
Lua bindings must match any corresponding native rule declaration exactly.

Routing preflight retains the existing coordinator fence during valid edits and
reloads. Rule edits keep the configuration fence; a source reload advances it
and can replace the approved declarations. Successful changes retire every
attached routing transport and queued dispatch. The embedding must consume the
recoveries and establish fresh neutral readiness with a newer source incarnation
before reattaching. Rejected edits preserve the old routes, tickets and model.
An internal generation override cannot disagree with a custom ticket fence.

The routing coordinator supplies actual router ownership and winner identity.
Each consuming candidate is additionally bound to the original source/router
ticket/authorization; a later physical group cannot inherit old proposals. A
completed claimed group can retain its completion identity while awaiting the
terminal callback. Terminal completion, cancellation, expiry or fresh unrelated
input releases that identity. Normal input can start a new group immediately.

Lower-priority results wait in Rust while a higher-priority candidate remains
eligible. Cached continuous progress preserves begin, latest update and end.
The cache is bounded to 192 outcomes / 1 MiB and preserves each original proposal
deadline through arbitration and dispatch. Expired or superseded groups cannot
claim later input. Actual claims, buffering, fallback cadence, draining, dispatch
availability and recovery use the existing router. These core tests model
readiness and desktop authority; they establish no kernel admission or physical
consumer retirement.

The real supervised-helper test also connects the actual private observer worker
socket to Luna: physical-frame model → pending → stationary timer → typed native match
→ correlated cleanup acknowledgement. It explicitly models provider authority
and performs no device I/O. Broker ledger and protocol tests model admission;
they do not verify a live privileged broker/logind registration. Full source
graphs now run through the sole worker helper across configuration generations,
and offline replay exercises real held-touch and cross-source composition.
End-to-end production reload/recovery and physical acceptance remain open.
Production routing remains gated.
