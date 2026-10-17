# Native raw-group chains

The Rust `router::Router` transport model can retain several physical groups
under one consuming decision. `routing::Routing` uses it for consuming built-in
native sequences and coordinated custom-step ownership. Full compiled plans
now survive private worker subscriptions, rule edits and configuration reloads.
Public source validation accepts composed custom declarations. Offline configured
replay runs isolated real callbacks without raw transport or live effect authority.
Production routing remains gated.

The embedding must verify the active rule step, source incarnation, protected
policy, frozen target and transport readiness before admitting a group. A token
proves router state, not those independent permissions. No token or receipt is
deserialized from a callback or IPC request.

## Native API

| Method/type | Contract |
| --- | --- |
| `start_chain(frame, candidate, at)` | Start a native `Buffer` or `Reserve` lease. `Observe` is rejected. The normal candidate ID, authorization, source, physical start, frame and timing checks apply. An existing live chain must first finish or be cancelled. |
| `continue_chain(frame, previous, at)` | Admit one fresh neutral-to-active physical group using the most recent `ChainToken`. The token must identify this exact source generation, authorization, lease and latest segment. An overlapping contact or replayed predecessor is rejected before advancing the stream. |
| `ChainAdmission::Held(token)` | The new segment is pending or claimed under that original lease. Inspect `chain_ownership` to distinguish the states. |
| `ChainAdmission::Forwarded` | The frame was processed on the ordinary path; the buffer decision elapsed. It must not be submitted again. |
| `chain_ownership(token, at)` | Read-only native ownership of an admitted segment. Invalid, retired, backdated or expired scopes return `None`. This does not advance input or delivery clocks. |
| `chain_deadline(token)` | Original pending buffer deadline, never the later segment's join deadline or an older queued delivery. Claimed/invalid leases have no pending decision. |
| `claim` / `claim_finished` | Claim every pending segment in the logical lease, not only the latest press. Only a physically neutral interaction may use `claim_finished`. |
| `cancel` | Pending chains fall back once through the scheduled FIFO. Claimed chains drain their remaining physical releases without replaying consumed input. |
| `seal_chain(latest, at)` | Create a `ClaimedChain` receipt only after the entire lease is claimed. An older predecessor cannot seal later segments. The receipt records source, authorization, candidate and a compact segment range. |
| `chain_receipt_valid(receipt)` | Check the original source incarnation and transport authorization remain alive. A later ordinary group does not retarget or erase a completed claim. Revoke/recovery or a different source/authorization invalidates it. This is not current context, target, grant or action validation. |

While a group is physically held, feed subsequent frames through the ordinary
`push(frame, None, at)` path. The lease retains its neutral completion until the
native coordinator resolves it. Starting a later ordinary group without an
explicit continuation token ends retention: a pending prefix falls back, and
the new group cannot be stolen by the old lease.

## Time and delivery

The buffer decision stays anchored to the **first** segment. Continuing a chain
does not renew the decision or accumulate another per-group delivery offset.
The configured offset ceiling remains unchanged. The newest held segment gets
its own contact-join window; a fully neutral recognized chain may finish early.

Fallback preserves every retained raw frame's order and physical timestamp.
All segments use one fixed delivery offset and the existing acknowledged FIFO.
Claiming a chain clears only its pending frames, never older queued application
releases. The delivery epoch ends only when physical state, delivered state,
the queue and the logical lease are all neutral. Latent absolute-axis state from
consumed segments is repaired for the next ordinary group.

`Reserve` has no buffer deadline, but still requires the embedding's explicit
noninteractive-scope admission and parent/session lifetime checks. Cancellation
consumes the remainder through full neutral; it does not resurrect the prefix.

## Bounds and failures

`Limits.frames` bounds queued plus pending raw frames and also the number of
segments in a retained lease, including a reserve that stores no pending frames.
`Limits.events` counts all pending/queued raw events and inserted absolute-state
repairs. Segment identities are monotonic; a chain stores a compact range rather
than an unbounded token history. Existing scheduler-slack and offset checks apply.

Malformed input, backwards time, capacity/counter overflow or missed delivery
stops the transport. Backend teardown and recovery are mandatory; a partial or
uncertain write is never retried as ordinary input. A foreign/stale/reordered
native token or invalid continuation start is rejected before physical state,
sequence or decision changes. If a once-valid pending token reaches its original
decision boundary during `continue_chain`, the old prefix and new frame follow
the ordinary path instead of obtaining a fresh lease.

`oslo make test-composition` includes deterministic router component checks for
keyboard/touch chains, exact-once fallback, claim/drain, original deadlines,
latest join clocks, native token/receipt lifetime, latent axes and resource
limits. These are component checks, not kernel capture, rendered delivery,
composed callback admission or physical takeover acceptance.

## Configured native sequences

Native candidate/sequence scopes retain per-source admission watermarks. Only a
fresh group admitted by the current step can extend its parent chain. Required
chord presses join a source; unrelated frames do not. If any required prefix
was already ordinary, the parent is retired rather than claiming a later suffix.
All required sources must have matching live tokens before the all-source claim.
The coordinator bounds aggregate native source bindings to 4096.

Final matches waiting for a later group's join window retain their original
parent/decision expiry in the bounded deferred queue. Queued native effects
hold a shared immutable claim receipt, so completing a prefix or forwarding a
later ordinary group cannot retarget the result. Dispatch validates source and
authorization lifetime, the original frozen target/context/config and expiry.
An expired queued begin cancels the series and drains held state; updates cannot
be dispatched as a new series. These paths do not open public custom composition.

## Private custom-step ownership

At ticket issue and reply, the native engine derives the active custom leaf and
its exact sequence parent after advancing the drained input watermark. It
rechecks the parent's frozen target, current step, sources and group deadlines.
Routing requires complete parent source coverage with live source generations,
transport authorizations and latest opaque raw-chain tokens. A claimed parent
winner is translated to the exact leaf ID only after this proof succeeds; the
callback supplies neither parent IDs nor ownership tokens.

The first request binds the leaf to its parent and full source/token map.
Subsequent requests and replies cannot create or replace that binding. A changed
prefix token invalidates the reply even when the current leaf's own source is
unchanged. Aggregate custom scope bindings are limited to 4096 source entries;
candidate and transport retirement remove them. Ordinary input that preceded
step admission cannot become prefix evidence after the fact.

Each pending ticket is independently shortened to the earliest original raw
buffer decision, in addition to its reply window and native parent/leaf expiry.
This contraction updates the saved ticket and byte accounting, not the candidate
lifetime: after a valid claim, continuous updates may outlive the old buffer
decision while the parent and reply windows remain valid. On rejection, the
raw scheduler uses wall time capped by the original decision for FIFO fallback,
without advancing physical input through unread records. Missed output slack
requires transport recovery rather than a renewed epoch or a past-due replay.

`oslo make test-custom-chains` covers configured private Routing models and
ticket contraction; `oslo make test-chain-helper` additionally runs a real
supervised Lua child through native hold/custom/custom Routing and correlated
cleanup. These use explicit model source/readiness/context authority. They are
not the complete sole-routing-worker composition/source-approval path, public
configuration admission, kernel capture or rendered/physical acceptance.

## Compiled plans in the sole worker

The private native backend carries a complete bounded `Config`, not a sequence
injected into one seat after subscription. Public Lua/TOML parsing, source
installation, native snapshots and rule transactions use one structural validator.
There is no validation-mode flag. Ordinary unfenced engines still refuse enabled
callbacks; no callback supplies a native scope or creates a coordinator.

Preflight validates the original enabled graph, including exclusive conflicts,
the 4096 compiled-recognizer bound and strict buffer join timing, before creating
a dormant baseline. Only the fenced owning seat executes custom rules. Private
source transfer keeps existing size, order, expiry and generation checks. A bad
reload leaves the prior graph, tickets and transport intact; a successful reload
retires old ownership and requires a new configuration fence and transport.

Native edits retain the original exact callback declaration catalog across
remove/restore. Broker edits may compose declarations retained by the original
protected source, but cannot add or change callback definitions.
Existing capability, action-handler and source-binding checks remain separate.

`oslo make test-composed-worker` checks complete plans through actual private
channels and Stream command/input ordering, using model readiness and context.
`oslo make test-composed-worker-helper` also runs the actual supervised Lua child
through two complete plan generations, canonical device metadata, reader-delayed
receipts, typed outer results and correlated leaf cleanup. It compiles the full
public source graph and compares the entire worker config with its source model,
not only a declaration catalog. Source preparation and CLI cases also exercise
the public example and strict callback bindings. This is not broker-to-kernel
delivery or rendered/physical acceptance. Separate offline replay tests use native observation coordinators,
not this raw transport. Production route-mode admission stays disabled.
