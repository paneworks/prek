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
| `poll(routing, at)` | Revalidate live work before writing or flushing; cancel stale scopes, advance one pending frame, and process at most eight immediately completed queue operations. |
| `repeat(handles, frame, index, at)` | Accept the exact value-2 event supplied by the trusted input coordinator. Deduplicate its source/sequence/event position and emit once per distinct mapped target, with no new owner or value-1 press. |
| `cancel(fence, event, at)` | Cancel the exact recognized gesture in this coordinator. Discard unwritten tails and compensate only written prefixes; preserve sibling owners. |
| `retire_source(source, at)` | Cancel work whose retained scope depends on this source, then tombstone its incarnation. Preserve unrelated sources. |
| `pause(routing, at)` | Revoke this coordinator's synthetic-grant epoch and cancel accepted work. A different coordinator is rejected before mutation. |
| `resume(routing, at)` | Require drained local output and fresh matching authority. This does not replace native consumer/neutral checks by the caller. |
| `retire(at)` / `drain(at)` | Permanently reject new work and drain release-only cleanup without retaining a removed Routing. Cleanup has a 250 ms bound. |
| `blocks_raw(source, sequence)` | Report local write barriers for this raw input position. It is not a consumer-delivery acknowledgement. |
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

The forwarding loop must interleave raw and generated writes, not drain every
generated action before forwarding ordinary input. For consuming work, prior raw
sequences must be acknowledged first and the action blocks its own and later
cutpoints. Observing work also waits for its own raw frame and blocks later ones.
An in-flight write retains the barrier until full local completion. This
preserves separate-frame, same-source down / ordinary typing / up order under
local backpressure. It does **not** establish within-frame or cross-source
ordering, stock-compositor cross-fd ordering, or selective consumption.
Whole-device raw gesture groups still cannot selectively remap a held key while
preserving every unrelated key. Per-key projection and the native consumer
boundary remain required before production capability negotiation is enabled.

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

`oslo make test-key-positions` runs device-free original-position, edge-ordered
activation, retained-cancellation and held-touch continuation regressions.
`oslo make test-key-worker` runs device-free scope, ordering, ownership,
cancellation, timeout and bounds regressions. `oslo make test-key-worker-cage`
drives the actual scheduler through owned protocol keyboards and a rendered app
with both Hyprland configuration dialects. Its authority/readiness fixtures are
explicitly modeled and its rules observe raw input; neither test grants native
takeover or establishes selective-consuming remapping.

For fixture development, `test-key-worker-cage-dev --runner=/nix/store/.../bin/prek-cage-host-test`
requires an explicitly supplied, already built immutable compositor runner. It
builds and selects the current Cargo test executable, prints its SHA-256, and
runs both configuration dialects without rebuilding the unrelated release
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
write fds. Scope teardown retains retiring output until release-only draining
finishes or its 250 ms deadline fails. Shutdown attempts bounded cleanup before
original reader release. Neither local write completion nor descriptor removal
is a consumer-neutral acknowledgement. There is no automatic in-place resume of
a retired native slot in its old coordinator.

This binding does not complete native acceptance. The stock consumer boundary
still refuses takeover, public `SyntheticInput` negotiation stays disabled, and
the shared per-key/input-position projector remains missing. Mixed press/release
and repeat frames are explicitly refused rather than silently reordered.
Provider-owned repeat, neutral reentry and the complete Lua input-effect APIs
also remain open.

`oslo make test-native-key-worker` runs slot models, actual sole-worker dispatch
through the native ABI writer on isolated private **regular files**, broker
ledger/publication tests and the owned service-hook test. Its fixture rejects
nonregular files and never creates a uinput device. It does not prove kernel
injection, authenticated broker registration or physical consumer behavior.

## Operations

| Operation | Behavior |
| --- | --- |
| `hold(claim, keys, at)` | Own 1–32 distinct supported keys; physical remaps own one target. Emit a down only for the first owner of each output code. |
| `tap(claim, keys, at)` | Synthetic-only chord. Press in supplied order, then release in reverse order after the press frame completes. |
| `release(claim, at)` | Release only the exact stored claim. A stale configuration/origin cannot release a rebound owner. Emit an up only for the last owner. |
| `repeat(claim, at)` | Forward value `2` for an owned physical remap without adding an owner or another press. Ignore an unowned repeat. |
| `cancel(claim, at)` | Cancel only this exact accepted held/pending claim, discarding its unwritten press tail. Preserve other claims, including those from the same source. |
| `cancel_all(at)` | Cancel all currently accepted claims without permanently retiring their source/configuration. New work still requires the caller's current authority. |
| `retire_source(source, at)` | Permanently retire that device's source generations through the specified incarnation, including origin-bound generated input. Other sources remain independent. |
| `retire_generation(through, at)` | Retire all remap and generated claims through that configuration and reclaim its serial receipts. |
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

## Bounds and verification

There are at most 4096 active owners, 32 keys per generated owner, 1024 unretired
configuration receipt entries, and 4096 retired source identities. Individual
cancellation tracking is bounded by accepted held/pending claims and reclaimed
after cleanup. Serial receipt storage does not grow with the number of completed
remaps or actions. Exhausting source retirement capacity seals and drains the
whole output; `retiring()`
reports that state. After capacity exhaustion, new holds/chords are denied.

- `oslo make test-key-output`: device-free ownership, prefix, backpressure,
  cancellation, fencing, bounds and native-byte-prefix tests.
- `oslo make test-key-output-cage`: the actual driver, two independently owned
  protocol keyboards, causal app receipts and eleven rendered phases in each
  config dialect, including same-source task isolation, context cancellation/
  reentry and delayed old-press rejection. The explicit test-only Aquamarine
  startup override remains separate from unmodified-stock/native/physical acceptance.
