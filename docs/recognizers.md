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

An owning observation session uses `prek session --lua --recognizers`; protected
system policy must enable `allow_input_events` and the selected devices. This
permission defaults to false. Ordinary `--input` is a separate opt-in. Device
catalog queries currently need separately received ordinary input-reset metadata;
custom-only tickets do not populate that catalog. Custom sequence steps,
additional consuming group lifetimes and the sole routing-worker bridge remain
unfinished. Production routing stays gated.

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
The sole routing worker still needs its live custom callback bridge.

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

The sole routing-worker integration and actual privileged
broker/kernel/physical acceptance remain required. Existing root-routing and
native handoff scripts do not establish live custom-recognizer admission.

## Native rule and routing integration

The internal declarative `custom` trigger embeds a recognizer declaration, a
1–10,000 ms recognition timeout and an optional 5–250 ms wake interval. Named
declarations must agree across rules and remain within the protected device/input
policy. Custom sequence steps currently fail validation: coordinated step and
multi-group lifetimes remain required work. Continuous consuming candidates must
start on a physical key/contact group; other declared changes may update that
group. Continuous consumption without such a group also remains unsupported.

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
they do not verify a live privileged broker/logind registration. Routing-worker
callbacks, full sequence/group composition, offline
custom replay and end-to-end reload/recovery still need implementation or live
acceptance. Production routing and Morf action admission remain gated.
