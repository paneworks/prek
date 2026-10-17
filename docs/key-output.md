# Generated key output

`keys::output::Output<S>` emits remaps and generated chords through one owned
per-seat keyboard sink. Ordinary input retains its per-device counterpart; do
not feed raw passthrough ownership into this driver's counts.

This is a library execution component, **not an authorization or readiness
gate**. Configured `keys`/`remap` actions are not yet wired through the production
broker/worker. `SyntheticInput` remains ungrantable and native takeover remains
closed. The native `kernel::Virtual` sink adapter is compiled; device-free tests
exercise the driver and native byte-prefix accounting. Its kernel execution is
not verified. The real Cage tests use owned protocol keyboards, not kernel devices.

## Claims

- `Claim.owner`: `Owner::Physical(SourceKey)` for a remap, or
  `Owner::Synthetic { seat, generation, id }` for generated input.
- `Claim.origin`: the exact source for a physical remap; the originating source
  for a generated gesture; `None` for provider-only software-keyboard input.
- `Claim.configuration`: the nonzero configuration generation. A synthetic
  owner's `generation` must match it.
- `Claim.serial`: a nonzero, strictly increasing **output submission serial**
  shared by remaps and generated actions within each configuration. A synthetic
  owner's `id` must equal it. A new physical press gets a new serial, even on the
  same source/key/configuration. Delayed release/repeat/cancel operations cannot
  affect a later press.
- The caller allocates serials in output submission order, not from recognition
  gesture IDs or raw frame sequences. It retains each accepted claim for
  deduplication and lifecycle cleanup. Do not reorder pending normal operations
  or allocate a new serial to retry an uncertain accepted action. Completed or
  canceled serials cannot be replayed. Duplicate holds of the exact still-active
  claim and ordered key list are idempotent.

Claims do not carry broker/session authority. A production caller must validate
the complete current execution fence and source binding before submitting work.

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
