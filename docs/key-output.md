# Generated key output

`keys::output::Output<S>` emits remaps and generated chords through one owned
per-seat keyboard sink. Ordinary input retains its per-device counterpart; do
not feed raw passthrough ownership into this driver's counts.

This is a library execution component, **not an authorization or readiness
gate**. Configured `keys`/`remap` actions have private broker/worker binding,
but live execution remains unaccepted. `SyntheticInput` remains ungrantable and
native takeover remains closed. The native `kernel::Virtual` sink adapter is compiled; device-free tests
exercise the driver and native byte-prefix accounting. Its kernel execution is
not verified. The real Cage tests use owned protocol keyboards, not kernel devices.

## Native preparation

`keys::native::Prepared::new` prepares a generated keyboard using the current
worker's supervisor `Heartbeat`. `Descriptor::generated_keyboard` has its own
origin, `prek/keys` marker namespace and protected `PREK_GENERATED_KEYBOARD=1`
metadata; it is not a fabricated physical counterpart. Its fixed keyboard-only
capabilities exclude the BTN ranges in the
[Linux input event definitions](https://github.com/torvalds/linux/blob/v6.18/include/uapi/linux/input-event-codes.h),
axes, switches, LEDs and kernel autorepeat. Physical publication entry points
reject generated descriptors, and generated readiness cannot authorize a
physical grab.

Native preparation also separates consumers: production publications accept
compositor/console consumers, not the test-only isolated receiver. Isolated
generated publications accept only their explicitly isolated receiver, not a
production compositor/console acknowledgement.

`Registry::prepare_generated_isolated_for_worker` retains the same authenticated
worker binding for an explicitly isolated generated publication. The nondefault
managed native test harness uses it with the actual Slot/Stream, owned kernel
receivers and eight correlated key/timestamp phases, including the private
software producer protocol, shared physical ownership and idle lease expiry.
Twenty-one bounded admission records distinguish acceptance from emission and
require stale/forged scope rejection. Public software authentication remains
untested. Its desktop context and
source normalization are modeled; it does not open public grants or production
consumer admission. Build/inventory evidence is not kernel execution; only the
user-launched [root runner](root-routing.md) can supply that receipt.

The preparation keeps its native device private and unwritten. The caller
obtains `descriptor()` observations for its authenticated consumer registration,
calls `observe(evidence)`, and retries `activate()` until it returns an
output driver. Activation requires processed protected metadata, exact evdev ABI,
owned-node key neutrality and current matching consumer evidence within the
five-second preparation deadline. Native preparation reads its own monotonic
clock and checks it again after the native metadata/state probes. `None` means
waiting; an error discards the
unwritten device/publication. Activation is one-shot. `deadline()` supplies the
worker's bounded readiness wakeup deadline, not a forwarding-loop implementation.

The sole worker now manages one pending/active generated slot per seat, with
separate bounded retiring output, heartbeat-bearing service and write polling.
Actual broker authentication, kernel/consumer execution and complete reentry
acceptance remain unproved; delegated input remains unfinished.
`./tests/run-root-routing.sh` includes a user-launched isolated generated-keyboard
test with persistent logs; see [root testing](root-routing.md). The agent does not
run it. `oslo make test-generated-keyboard` executes only device-free models.

## Claims

- `Claim.owner`: `Owner::Remap { key, generation, id }` for an independent
  configured remap instance, legacy `Owner::Physical(SourceKey)` for a single
  caller-managed physical owner, or
  `Owner::Synthetic { seat, generation, id }` for generated input.
- `Claim.origin`: the exact source for a physical remap; the originating source
  for a generated gesture; `None` for provider-only software-keyboard input.
- `Claim.configuration`: the nonzero configuration generation. A synthetic
  or instance-remap owner's `generation` must match it.
- `Claim.serial`: a nonzero, strictly increasing **output submission serial**
  shared by remaps and generated actions within each configuration. A synthetic
  or instance-remap owner's `id` must equal it. A new physical press gets a new
  serial, even on the same source/key/configuration. Delayed release/repeat/cancel
  operations cannot affect a later press.
- The caller allocates serials in output submission order, not from recognition
  gesture IDs or raw frame sequences. It retains each accepted claim for
  deduplication and lifecycle cleanup. Do not reorder pending normal operations
  or allocate a new serial to retry an uncertain accepted action. Completed or
  canceled serials cannot be replayed. Duplicate holds of the exact still-active
  claim and ordered key list are idempotent.

Claims do not carry broker/session authority. A production caller must validate
the complete current execution fence and source binding before submitting work.

## Route-scoped scheduler

`keys::worker::Seat<S>` schedules configured key work on one output incarnation.
It is a library actor used by the private native worker binding, **not an
accepted public native execution path**. Creating it does not grant `SyntheticInput`, prove native readiness,
or permit a physical grab. The caller supplies an idle output and its exact
coordinator `Fence`; rejected construction returns the original output.

`Routing::with_recognition` can retain opaque `routing::KeyWork` inside a
`ScopedDispatch`. `into_key_work()` consumes the original dispatch; editing its
public event or reorder scope prevents conversion. Work is neither cloneable nor
deserializable. It retains the actual route/chain, rule, source incarnations,
configuration, runtime rule revision, coordinator and synthetic-grant epoch
instead of reconstructing
authority from a public match event. A coordinator-free observer cannot mint it.

| Operation | Behavior |
| --- | --- |
| `enqueue(work, at)` | Queue one canonical action. A remap begin returns an opaque `Remap` handle. A normal end queues a release behind its pending press, preserving fast taps. |
| `admit(routing, work, input, at)` | Merge canonical work and original repeats in input order before admitting an end that closes a repeat binding. Used by the managed worker and protocol fixture. |
| `poll(routing, at)` | Revalidate live work before writing or flushing; cancel stale scopes, advance one pending frame, and process at most eight immediately completed queue operations. |
| `repeat(handles, frame, index, at)` | Accept the exact value-2 event supplied by the trusted input coordinator. Deduplicate its source/sequence/event position and emit once per distinct mapped target, with no new owner or value-1 press. |
| `repeat_positioned(routing, handles, frame, index, at)` | Require the exact current original frame and matching live coordinator; retain its scheduling position and observer raw-prefix requirement. |
| `cancel(fence, event, at)` | Cancel the exact recognized gesture in this coordinator. Discard unwritten tails and compensate only written prefixes; preserve sibling owners. |
| `retire_source(source, at)` | Cancel work whose retained scope depends on this source, then tombstone its incarnation. Preserve unrelated sources. |
| `pause(routing, at)` | Revoke this coordinator's synthetic-grant epoch and cancel accepted work. A different coordinator is rejected before mutation. |
| `resume(routing, at)` | Require drained local output and fresh matching authority. This does not replace native consumer/neutral checks by the caller. |
| `retire(at)` / `drain(at)` | Permanently reject new work and drain release-only cleanup without retaining a removed Routing. Cleanup has a 250 ms bound. |
| `blocks_raw(source, sequence)` | Legacy source/frame barriers; never supply an allocated fragment output sequence here. |
| `blocks_raw_span(source, span)` | Seat-wide original-position barriers for raw reports/fragments, including pending generated writes and cancellation cleanup. Not consumer-delivery acknowledgement. |
| `blocks_ordered_span(routing, source, span)` | Apply original-position barriers to the matching coordinator's key-ordering domain. Independent routes keep forwarding; neutral cleanup still fences the seat. A mismatched coordinator fails closed. |
| `deadline()` / `poll_fd()` | Supply queue/write/cleanup wakeups and an optional Linux sink write fd. Also poll Routing/context/consumer deadlines and lifecycle notifications. |

Normal completion and cancellation are deliberately separate: a down followed
quickly by an up must still emit the pair, whereas lost authority must not
finish an unwritten chord. Submission serials and the output clock survive an
idle output being wrapped in a new actor. Timely unchanged grant renewals keep
held work valid; revoked, expired or exhausted epochs never resurrect old work.
Retained route scopes share an irreversible cancellation token. Candidate or
sequence cancellation invalidates previously dequeued work even before the
caller forwards the diagnostic cancellation event. Normal completion leaves
that token valid for a queued down/up pair. Sequence roots and rebased held-touch
children have distinct tokens; a canceled ancestor cannot start a continuation.
Scheduling failures are terminal. Explicit retirement may still drain releases
through a healthy sink after a scheduling timeout, but the failed actor is not
reusable. A failed sink retains its exact uncertain frame and cannot prove
neutrality.

Plain `Key` triggers evaluate activation conditions at each normalized key edge,
in original raw-event order. A later activation press cannot authorize an earlier
trigger press in the same report. `KeyWork::position()` retains the original
event offset under the coordinator's increasing input-frame watermark, including
offsets occupied by metadata and ignored duplicate edges. Rule/configuration
replacement preserves the watermark. Interleaved key matches are dequeued by
position, not grouped by gesture ID. A sequence ending in a key inherits that
final edge's position. This does not change other recognizers' frame-level
condition evaluation or allow arbitrary sequence steps within one report.
Non-key reports and asynchronous completions use the current after-frame
boundary (`event == usize::MAX`); timers advanced before a new input report use
the previous frame's boundary. A due timer cannot be sorted behind the new
report's first key merely because both have the same timestamp.

`Routing::with_key_projection` retains original positions across attached
sources. Eligible pure-key sources with immediate consuming key actions arbitrate
one winner per physical press by priority/rule ID. Unrelated keys remain ordinary.
Other raw routes retain whole-report positions, including legacy buffered groups.
`KeyWork::scheduling_position()` retains the coordinator cut; legacy source modes
use whole-report boundaries rather than claiming exact cuts. The managed worker
uses this constructor and the same `admit` implementation as the protocol fixture.
The older constructors retain legacy source/frame behavior.

Consuming work waits for its same-seat key-domain raw prefix before its edge. Observing work
also waits for its own raw edge; observed repeats do likewise. Pending/in-flight
generated writes block later raw spans on every source in that domain, not just the
triggering keyboard. Local backpressure retains barriers through full report
acknowledgement. Closing generated slots fence raw forwarding until their owned
cleanup finishes. A blocked or in-flight raw report wakes at its existing
write-budget failure boundary, not continuously at an expired due timestamp.
These are local coordinator/write guarantees. They do not prove stock-compositor
cross-fd delivery, consumer-neutral acknowledgement or native takeover.

The key-ordering domain includes physical sources with any generated-keyboard
key capability, including media-only and hybrid devices, plus sources configured
to emit keys. Enabled rules in the active named mode connect possible same-seat source groups through
key conditions, layers, chords, sequences and custom recognizer selectors.
Connections are transitive. Alternative selectors on an ordinary noncompound
rule do not connect independent sources. Disabled rules and inactive named modes
do not create connections. Attachment and removal rebuild the cached components;
rule/mode/reload transactions retire transports before a fresh incarnation can
use the replacement graph. Forwarding does not rebuild a rule graph per poll.

Independent buffered touch/pointer routes neither hold generated work's raw
prefix nor wait behind a pending generated write. Their existing per-source
buffering and complete-report FIFO rules remain unchanged. A dependent legacy
keyboard or explicitly connected buffered group still participates in ordering
and retains its existing timing/write budget; no timeout is extended to hide
backpressure. Neutral cancellation/closing cleanup remains a seat-wide barrier.
Independent delivery lanes do not promise a single raw delivery order across
unrelated sources; recognition retains the common input timeline. Ordinary
model/private-file evidence does not establish combined native or graphical
key/touch latency, stock consumer interpretation, or full resource-policy acceptance.

Before polling, the owning loop must forward Routing's cancellation events to
`cancel` and its source-recovery transitions to `retire_source`. A retained
source record is not proof that a retired raw route remains usable. Runtime
rule edits invalidate old key work even when a later edit restores the same
rule text within the original approved configuration fence.

The actor bounds queued operations at 64, retained remap bindings at 4096,
serialized event/repeat charges at 512 KiB, and queue/write age at one second.
The byte counter is not a complete heap/RSS measurement of retained route and
permit structures. Broader resource-policy acceptance remains open. Callers must
retire/recover on acceptance errors instead of silently dropping required input
releases, and must never pass untrusted JSON frames as source-repeat evidence.

### Raw projection queue

`Router::push_key_frame` is the per-press alternative to the router's existing
whole-device ownership model. Its `KeyClaim` inputs must come from a trusted
arbitrator that already validated the original fenced route work. The router
checks its authorization counter, fresh-down position and increasing owner IDs;
this is not a new broker permission or authority reconstructed from match JSON.
`ConsumedKey` is an ownership receipt, not executable action work.

Owned repeat/up edges drain even if later frames supply no claims. Duplicate
owned releases remain suppressed until a genuinely fresh down chooses a new
owner or the ordinary path. Other pressed keys stay independent. Effect-bearing
consumption cuts precede omitted edges; observing cuts follow their original key.
Drained duplicate edges create no automatic cut; explicit observing cuts still apply.
Each nonempty fragment ends with `SYN_REPORT`. Unaffected reports keep their
events intact. Only a single immediately adjacent `MSC_SCAN`/key pair can be
transformed; the scan paired with an ordinary key stays with that key.

Queued fragments have separate increasing output sequences. `input_span(ticket)`
retains their original source sequence and coordinator event bounds until the
full report is acknowledged. Original-sequence prefix queries use those input
sequences, never the allocated fragment sequence. Position-prefix queries cover
this router only; neither query proves delivery to a compositor consumer.

All report/claim/cut validation and total fragment capacity checks happen before
physical/ownership/queue mutation. Queue exhaustion includes inserted report
boundaries. Output-sequence exhaustion cannot publish a prefix of a multi-report
plan. The existing scheduler/write deadlines and fatal recovery rules remain.
Legacy and selective modes cannot be mixed within one router incarnation.

Transformed reports containing nonkeyboard data or ambiguous scan metadata
currently fail with `UnsupportedProjection` and require recovery; untouched
hybrid reports remain intact. Timed/chord/sequence and mixed-source modes still
use legacy whole-report ownership; their selective per-key behavior is not
complete. Hybrid/ambiguous transformations, native consumer acknowledgement,
neutral reentry and the full original acceptance matrix remain required. The
public synthetic capability stays closed.

`oslo make test-key-positions` runs device-free original-position, edge-ordered
activation, retained-cancellation and held-touch continuation regressions.
`oslo make test-key-projection` runs per-press raw queue, scan association,
complete-report cut, original-position, capacity and recovery models.
`oslo make test-key-coordinator` checks nonempty selections for fenced arbitration,
global actor barriers, legacy buffered positions, merged repeats and actual
sole-worker ABI writes on owned private regular files (no devices).
`oslo make test-key-worker` runs device-free scope, ordering, ownership,
cancellation, timeout and bounds regressions. `oslo make test-key-worker-cage`
drives the actual scheduler through owned protocol keyboards and a rendered app
with both Hyprland configuration dialects. Its authority/readiness fixtures are
explicitly modeled. Nine phases include consumed per-key projection, ordinary
typing within one report and a separate source behind a pending modifier, using
three owned protocol keyboards. They do not grant native takeover or prove
physical/kernel selective consumption. Repeat interleaving has direct worker/file
evidence; these rendered phases do not simulate kernel repeat.

For fixture development, `test-key-worker-cage-dev --runner=/nix/store/.../bin/prek-cage-host-test`
requires an explicitly supplied, already built immutable compositor runner. It
builds and selects the current Cargo test executable, prints its and the current
helper's SHA-256, and runs both configuration dialects without rebuilding the release
application. This is development-binary evidence, not packaged-release evidence.

## Private native worker binding

The supervised input loop calls `InputSink::service_owned(health)` even with no
physical readers. The routing worker can prepare one generated keyboard for an
authorized seat only with its live supervisor heartbeat. Subscription permission
is distinct from execution availability: `SyntheticInput` is masked in Routing
until that seat's prepared output activates with fresh consumer evidence.
Availability changes never extend the original capability lease.

Private transport replies carry the exact publication marker, source and fence.
Numeric source values do not distinguish physical and generated roles. Generated
offers do not enroll physical original-release evidence, close its enrollment
window, or receive Morf recognizer admission. The broker still checks the actual
agent registration before and after obtaining its device evidence.

Consumer identity, marker, name, class, seat, settings and timestamp remain
checked after activation. A fresh sample cannot revive an expired active lease.
Consumer loss seals the output, cancels unwritten tails and requires worker
recovery, not an inventory-only resume. Cancellation and source-recovery reports
are applied before new work. New reader incarnations remain necessary after
raw-route replacement; a retained physical source generation is not reusable.

The slot supplies preparation/offer/consumer/write/drain deadlines and pending
write fds. Scope teardown retains the generated writer, dependent raw-output
cleanup and original-reader barrier after local release completion. Only then
does it issue a one-use private consumer-drain challenge bound to the fence,
source incarnation, publication, consumer and nonce. The trusted consumer must
consume complete final reports and verify live neutral state before replying;
inventory evidence cannot acknowledge drain. Local writes and consumer waiting
share the same 250 ms retirement deadline. Stale, foreign, early, future and
duplicate receipts cannot close another slot. Shutdown pumps private receipts
within that bound, destroys outputs on failure, and reports neutrality unproven
before releasing original readers. A retired slot never resumes in place.

This binding does not complete native acceptance. The stock consumer boundary
still refuses takeover, public `SyntheticInput` negotiation stays disabled, and
the complete native consumer prefix/drain/neutral boundary remains unproven.
Mixed press/release/repeat reports use canonical merged admission, which rejects
unrecorded or modified repeat frames before admitting new work.
Authenticated provider-owned repeat, native neutral reentry and the complete Lua input-effect APIs
also remain open.

`oslo make test-native-key-worker` runs slot models, actual sole-worker dispatch
through the native ABI writer on isolated private **regular files**, broker
ledger/publication tests and the owned service-hook test. Its fixture rejects
nonregular files and never creates a uinput device. It does not prove kernel
injection, authenticated broker registration or physical consumer behavior.

## Operations

| Operation | Behavior |
| --- | --- |
| `hold(claim, keys, at)` | Own 1–32 distinct supported keys; physical remaps and software presses own one target. Emit a down only for the first owner of each output code. |
| `tap(claim, keys, at)` | Synthetic-only chord. Press in supplied order, then release in reverse order after the press frame completes. |
| `release(claim, at)` | Release only the exact stored claim. A stale configuration/origin cannot release a rebound owner. Emit an up only for the last owner. |
| `repeat(claim, at)` | Emit value `2` for a held remap or software press without adding an owner. Ignore an unowned remap repeat; reject an unheld/retired software repeat. |
| `cancel(claim, at)` | Cancel only this exact accepted held/pending claim, discarding its unwritten press tail. Preserve other claims, including those from the same source. |
| `cancel_all(at)` | Cancel all currently accepted claims without permanently retiring their source/configuration. New work still requires the caller's current authority. |
| `retire_source(source, at)` | Permanently retire that device's source generations through the specified incarnation, including origin-bound generated input. Other sources remain independent. |
| `retire_generation(through, at)` | Retire all key claims through that configuration and reclaim its serial receipts. |
| `retire_producer(nonce, at)` | Permanently retire this software producer's presses, including partial writes; preserve other owners. |
| `retire(at)` | Seal the entire output and release its state. New holds/chords are denied. |
| `flush()` | Advance at most one enqueued frame; return `Pending` while work remains. |

Hold/repeat timestamps cannot move backwards. Release/cancellation/retirement
cleanup clamps to the last accepted timestamp instead of depending on a working
forward clock.
Repeat forwarding here does not establish physical/kernel repeat transparency.

## Writes and cancellation

The caller supplies a fresh owned sink and drives `flush` when it can make
progress. `enqueue` accepts one frame, `flush` acknowledges its complete write,
and `abort` discards its unwritten tail and returns the exact successful prefix.
Normal requests while a frame is pending return `WouldBlock` without accepting
another operation. Lifecycle cancellation/retirement remains admissible during
backpressure. An unknown cancellation does not reserve a serial or cancel future
work. A mismatched claim for an existing held/pending owner is rejected.

Local ownership commits only after a complete frame. Cancellation never finishes
an unwritten chord tail: it abandons the tail, releases only known written
presses, seals the partial report, then releases other retired owners. A pending
release continues its remaining ups. Unrelated pending claims are preserved.
After `cancel_all`, the caller can submit new, independently authorized claims
with fresh serials in the same configuration. This is not a session/context
authority check; it is the output cleanup mechanism for that transition.

Any enqueue, write, abort or prefix-validation failure is terminal. The sink is
dropped, `failed()` becomes true, and `uncertain_frame()` retains the possibly
written request. Do not replay it or infer neutral consumer state from it. Linux
uinput can return an error after injecting part of a buffer.

`down()` describes confirmed local ownership only. `settled()` means no local
frame is pending and the driver has not failed; neither is consumer-neutral
evidence. `into_sink()` succeeds only after all local ownership is released.
Descriptor removal does not prove downstream cleanup.

## Software-keyboard ownership

`Owner::Software` identifies a provider nonce and a generation-scoped press,
without a physical `SourceKey`. Its claim must have `origin=None`; borrowing a
physical identity is rejected. Each press holds one supported key. An explicit
`repeat` emits value 2 only while that exact press remains held; it does not
create another owner, simulate down/up or enable device autorepeat. Existing
remap repeat semantics are unchanged.

`retire_producer(nonce, at)` cancels only that provider's accepted or partially
written presses. Other software providers, remaps and physical modifiers retain
their reference counts. Producer retirement is permanent across configurations
within this output incarnation. At 4096 retained producer identities, a further
retirement seals and drains the entire output rather than recycling a nonce.

These are emission/ownership primitives, not authentication. The nonce alone
grants nothing. Private managed-worker admission is described below; authenticated
software-keyboard registration and Lua held-key handles still require their full
broker/consumer contract. Public `SyntheticInput` remains disabled. Model tests cover explicit
repeat and byte-prefix cancellation; the owned Cage fixture renders provider
isolation and physical-modifier overlap, but does not render repeat or establish
software authorization or native consumer delivery.

## Private managed software producers

The supervisor's private `Software` command addresses one exact fence,
generated-source incarnation, publication marker and consumer. Preparing,
unavailable or mismatched destinations refuse new work without updating another
slot. This uses the existing sole writer, not a separate input device or worker.

Each nonzero producer nonce opens once with request sequence 1. Normal accepted
requests advance that producer's sequence by exactly one; rejection does not
advance it. Permanent `Close` accepts a higher sequence so cleanup need not wait
for pending admission replies; an old/equal sequence cannot close a newer lease.
Press IDs increase and are never reused. `Press`, `Repeat` and normal `Release`
queue in order, preserving a fast down/up pair even before the first write.
`Close`, execution-epoch loss and lease expiry instead discard unwritten work
and reconcile written prefixes. A retired nonce cannot open again in this output.

`Open` and `Renew` require an absolute monotonic expiry after now and within
250 ms, no later than the current context approval, consumer evidence and seat
registration deadlines. Fresh metadata cannot revive an expired lease. Active
leases advertise wake deadlines even with no input frames or pending writes.
Local cancellation retains same-seat cleanup barriers within the existing
250 ms drain bound. The slot's separate consumer-drain acknowledgement still
fences final writer removal and original-reader release.

Queued software operations capture a key-domain boundary after the last accepted
input frame. Earlier raw reports finish first; later ordered reports wait for the
software operation. Unrelated pointer/touch lanes remain independent outside
neutral cleanup. Software requests never invent a physical source or key event.
At most 64 producer leases share the actor's existing 4096 bindings, 64 pending
operations and 512 KiB accounting budget; provider, press and prefix charges are
reclaimed after cleanup. This is not process-RSS acceptance.

`SoftwareResult.accepted` acknowledges queue admission only, not execution,
consumer delivery or permission to retry a possibly executed effect. Public
broker/session registration and Lua input handles remain unavailable. The nonce
and consumer fields are correlation data, not authenticated UI credentials.

`oslo make test-software-worker` checks actor ordering/leases, exact slot scopes,
strict packet schemas and actual private-channel/native-ABI writes on protected
ordinary files, without kernel devices. The nondefault test harness also drives
thirteen owned Cage actor phases, including lease expiry without new input.
Its authority/readiness remain modeled and it does not test private software IPC,
public authorization or repeat rendering. Default application packages expose no
fixture admission method.

## Bounds and verification

There are at most 4096 active owners, 32 keys per generated owner, 1024 unretired
configuration receipt entries, 4096 retired source identities and 4096 retired
software-producer identities. Individual cancellation tracking is bounded by
accepted held/pending claims and reclaimed
after cleanup. Serial receipt storage does not grow with the number of completed
remaps or actions. Exhausting source retirement capacity seals and drains the
whole output; `retiring()`
reports that state. After capacity exhaustion, new holds/chords are denied.

- `oslo make test-key-output`: device-free ownership, prefix, backpressure,
  cancellation, fencing, bounds and native-byte-prefix tests.
- `oslo make test-key-output-cage`: the actual driver, two independently owned
  protocol keyboards, causal app receipts and fifteen rendered phases in each
  config dialect, including same-source task isolation, context cancellation/
  reentry and delayed old-press rejection. The explicit test-only Aquamarine
  startup override remains separate from unmodified-stock/native/physical acceptance.
