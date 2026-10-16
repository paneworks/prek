# Native raw-group chains

The Rust `router::Router` transport model can retain several physical groups
under one consuming decision. This is not yet connected to composed rule
admission in `routing::Routing`, the sole input worker or Lua. Public custom
sequence validation and production routing remain gated.

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
