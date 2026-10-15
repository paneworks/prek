# Morf integration

Prek's broker can consume Morf's native lock evidence with
`broker --morf-executable /path/to/the/actual/morf-ELF`. The NixOS equivalent is
`services.prek.morfExecutable`. Use a Morf build containing the native provider;
a shell wrapper is not an executable identity. No desktop/root deployment is
performed by building this project.

The upstream implementation lives in `../mold` under
`crates/frontend/morf-host/src/lock/provider.rs`. It creates a private native
endpoint when the real Wayland lock loop starts. Compositor identity comes from
that connection's socket credentials and process birth. Lua cannot supply or
advance a native phase. The provider reports `pending`, the actual `locked`
acknowledgement, and `unlock_dispatched` after the backend flushed unlock.

Prek verifies endpoint ownership/permissions, Morf's exact executable and process
lifetime, active logind membership, and the bound compositor before accepting
the report. Its sampler independently confirms that the compositor is unlocked
before retiring inhibition; it then acknowledges the exact provider epoch and
revision. Socket/process loss never proves unlock. Morf preserves completed
evidence for a sampled observer for at most two seconds, dropping its rendering
resources before waiting. Only root may acknowledge completion; a same-user
reader can inspect the native report without releasing it.

This supplies native lock evidence. Morf shell/keyboard rendered regions,
typed drawer/workspace actions and progress, synthetic keyboard ownership and
the global-recognizer handoff still require implementation. The read-only
`prek.morf.providers()` query exposes verified native shell connection metadata;
the other Morf runtime operations remain pending. Root/logind and
physical authentication acceptance remain unverified by local model fixtures.

The optional owned local test is:

```sh
nix run .#cage-test -- --modes --input --power --windows \
  --morf /absolute/path/to/morf/target/debug/morf
```

Pass the actual ELF and its normal runtime library environment. The test runs
Morf on the owned nested compositor, compares native reports with actual stock
lock state, checks process/compositor/epoch/revision identity, refuses an
unprivileged release and checks endpoint cleanup. Its small fixture explicitly
authorizes unlock through a timer after observing the compositor lock. It does
not exercise PAM, root logind registration, host devices or the host display.
`--visible` repeats the same checks in the visible owned nested session.

Reports are saved beside the normal Cage receipts in `morf-lock-receipts.json`
and `morf-lock.log`. The report explicitly identifies fixture authentication
and the absence of a root/logind execution claim. Tests use short private paths
under `/tmp/pc-*` so native Unix sockets still fit when launched from a Nix
development shell with a long `TMPDIR`.

The companion Caelestia shell also supplies typed ordinary controller IPC,
documented upstream in `../mold/docs/PREK_CONTROLS.md`. Prek's strict wire types
live in `src/morf/controls.rs`: frozen output, epoch/gesture/ordinal correlation,
discrete/continuous action validation and bounded finite motion. They provide
no execution grant and do not enable Morf action capability negotiation.
Native controller binding, rendered context, recognizer handoff and generation
retirement remain required before production capability negotiation.

To extend the same owned Cage test to the actual shell controllers, pass the
upstream source shell and an optimized Morf ELF containing the native lock
provider:

```sh
nix run --no-write-lock-file .#cage-test -- --modes --input --power --windows \
  --morf /absolute/path/to/mold/target/release/morf \
  --morf-shell /absolute/path/to/mold/examples/shells/caelestia/shell/init.lua
```

This starts the full shell with its source library and plugins disabled inside
the private compositor. It checks the actual shell layer and ordinary IPC
controller state while driving panels, keyboard layout, duplicate refusal and
workspace progress/cancellation. `morf-controls-receipts.json` and
`morf-controls.log` record this layer. UI/layout tests upstream additionally
cover both themes, drawer axes, workspace partial return/commit, expiry, hard
deadline and authentication cancellation. Ordinary shell IPC success does not
establish broker grants, recognizer ownership, rendered-region authority or
physical application delivery; those are explicitly pending.

Native shell bindings are separate 0600 `*-shell-provider.sock` endpoints under
the private runtime. The real per-output Wayland client supplies compositor
credentials, process birth and its actual output name. Only native host code
advances pending/1 to configured/2 after the primary configure witness. Backend
replacement removes the old endpoint and creates a new epoch; ordinary Lua
reload on the same connection does not itself replace that epoch. No ordinary
IPC callback can supply or advance these fields. The broker applies the same
executable, lifetime, directory, compositor and logind checks used for native
lock discovery, then includes matching output bindings in fresh desktop context.
Connection metadata never supplies lock completion or rendered widget regions.
The owned shell trial now also verifies the native endpoint identity and its
removal after an ordinary graceful shutdown.

Upstream Morf also implements bounded native root controller leases, queued
requests, read-only receipts and revocation on that shell endpoint. A real
SO_PEERCRED root identity, pidfd lifetime, session/configuration fence, native
connection epoch and Lua VM generation bound each lease. Leases last at most
one second; each queued request expires within 250 ms. Expiry, revocation,
issuer death and VM replacement retire authorization. Workspace completion
checks the live token after animation before issuing its command. These are
upstream receiver foundations; Prek's production action negotiation remains
disabled pending its executor, rendered context and neutral recognizer handoff.

The owned Cage helper queries the native limits and confirms that actual
unprivileged lease/submit/revoke attempts are refused. Its receipt records
`native_root_operations_refused=true` and `root_grant_tested=false`. Positive
grant tests upstream model root credentials around real pidfds; they do not
establish actual root/logind acceptance. No VM or host device is used.

Prek's typed native transport is in `src/morf/native.rs`. A discovered
`ShellClient` negotiates read-only limits with `control_capabilities`; its
`lease_control`, `submit_control`, `control_receipt` and `revoke_control` methods
require the actual root broker. The negotiated endpoint/VM and broker fence
form a `Scope`; a submission contains a positive request ID, absolute monotonic
deadline and the existing typed controller payload. Payload output must match
the native provider. Decimal identities/deadlines retain all u64 bits.

Each operation rechecks the protected endpoint, exact Morf executable/process,
active logind registration and actual compositor before and after its exchange.
Sampling and exchange share 200 ms; earlier lease/grant deadlines shorten that
budget. Future/regressed generations, mismatched IDs/epochs, unknown statuses,
fields or errors, oversized replies and incomplete frames fail closed. Failed
streams are shut down so a late response cannot become the next request's
reply. A `ui_replaced` refusal can report a newer generation, requiring fresh
negotiation. Native connection state remains distinct from Lua VM generation.

A returned `queued` receipt means admitted work; `dispatched` means controller
acceptance, not observed animation/compositor completion. `Error.uncertain`
means mutation bytes were sent and an effect cannot be excluded. No mutation is
automatically retried. A newly verified connection may query the same receipt;
missing/evicted receipts cannot justify repeating a mutation. Read-only receipt
errors and local refusals have `uncertain=false`. Socket-pair tests exercise
the real framing/typed operation code with modeled replies, not root/logind
acceptance. This transport does not yet connect production Morf action offers
or enable action/recognizer ownership negotiation.

The native phone-touch handoff uses `handoff_state`, `quiesce_handoff`,
`activate_handoff` and `confirm_handoff_release`. The typed state distinguishes
native, quiescing, quiesced, owned, releasing and lost. Activation checks the
exact endpoint/VM/ownership generation, UI acknowledgement and a native-neutral
observation less than 250 ms old. Read-only status polling refreshes that
observation. Submission requires owned handoff at the native receiver.

`confirm_handoff_release` transmits root's release assertion; its caller must
first stop recognition, drain input and confirm all owned device descriptors
are closed. Transport does not itself supply that proof. Morf persists
inhibition across worker restart and preserves its native policy across Lua VM
replacement; loss or expiry cannot enable a competing recognizer. Local widget
and keyboard gestures remain available while duplicated global phone gestures
are quiesced. An external unmanaged lisgd cannot acknowledge takeover.

This supplies the Morf receiver and broker wire contract. The production
broker's physical cutover, activation and complete release orchestration,
rendered context and root/logind/phone acceptance remain required before action
negotiation can be enabled. Cage checks inspect actual native handoff state
and refuse unprivileged mutations; positive ownership remains explicit model
evidence, not physical takeover.

The input worker now reports original-reader closure separately from source
removal. It retires virtual output first, drops the original descriptor, then
sends a private `Released` acknowledgement containing the worker lifetime,
broker/configuration/session fence and exact source generation. Seat replacement
preserves queued closure acknowledgements. Backpressure or a missing message
cannot establish release; an actual reaped-worker report supplies the fallback.

The broker's ownership ledger can enroll a seat/fence before its first consumer
readiness. It records every source subsequently offered readiness and yields
opaque `ReleasedInputs` evidence only after that scope retires and all recorded
sources close, or its exact worker is reaped. The ledger bounds pending scopes
and sources without evicting unconfirmed entries. Public session messages
cannot construct this evidence. Tests use real socket EOF for closure ordering
and actual supervised/reaped child processes for exit evidence; no evdev access
is involved.

The trusted broker API now connects enrollment and native inhibition:
`Broker::begin_morf_handoff(fence, output)` requires the root routing broker,
a fresh bound desktop without a secure overlay, a selected native shell and
no preceding transport readiness. It negotiates the native UI generation,
enrolls the worker ledger, retains the compositor/provider bindings outside the
agent connection, and requests a lease and quiescence. The broker polls native
acknowledgements and renews the lease while the original session remains valid.
`morf_handoff_status` reports the local phase and ownership generation.

Agent loss, context expiry, locking, output removal or configuration replacement
retires the local session and stops renewal. It does not restore native globals.
Virtual consumer readiness includes the exact per-output native ownership
requirements. It grants no physical admission. The CLI does not invoke this
entry point or enable production route mode yet. Up to eight retained handoffs
are allowed, without evicting unresolved
cleanup obligations. A new handoff cannot adopt an already inhibited provider.

The routing worker requests native activation only after its metadata,
consumer, drained software buffer and physical-neutral preflight. Its private
`MorfReady` message binds the actual worker lifetime, broker/configuration/
session fence, source generation, per-source attempt, observation time and all
selected output/endpoint/UI/ownership generations. The broker checks the
authenticated worker channel and exact live ledger enrollment, then checks the
owning desktop registration and native quiescence. It activates each native
owner once and observes its owned state. Later source requests sample existing
ownership rather than repeating the native activation operation. Here `owned`
means the native recognizer acknowledgement, not a completed kernel cutover.

A private `MorfActivated` grant is bounded by the original observation's 250 ms
lifetime, native ownership freshness and desktop context freshness. The worker
accepts it only for its current pending attempt and checks physical neutrality
and grant validity again before grab. New input or an incomplete software frame
retires pre-grab permission. Old, foreign, expired or repeated grants cannot
authorize a replacement attempt or source. Transport replies cannot introduce
or remove ownership requirements within a source lifetime, or substitute for
the first neutral admission grant. Public session requests cannot use either
private message.

Active watched sources request periodic lease refresh through the same private
transport. A renewal may preserve an already admitted, unexpired source; it
cannot revive expired authority. Native ownership loss, context expiry or a
missing renewal makes the worker recover the route, retire its virtual output
and reopen the original reader with a fresh generation. Admission is checked
before forwarding as well as before and after grab. The pre-grab kernel backlog
race and complete consumer cutover boundary remain unresolved, so this bridge
does not enable production routing or establish physical takeover acceptance.

After the supervisor actually reaps the bound worker, `input_worker_exited`
consumes its opaque ledger evidence and submits native release under a fresh
cleanup fence. It checks the exact original fence, seat and worker lifetime.
Per-source closure alone is insufficient for this path, because it does not
establish that recognition and all buffered work stopped. Lost replies are not
retried. An accepted release means queued restoration; only a subsequent native
state observation can establish restoration. The broker's normal exit can leave
Morf completing that queued operation at its own neutral UI boundary. Failure
is reported while native inhibition remains in place.

This evidence covers only the enrolled worker's descriptors. It does not prove
consumer neutrality, external lisgd shutdown, a safe kernel admission boundary,
or completed physical ownership. The kernel admission boundary, finer per-seat live-worker draining,
recovery across provider replacement and actual root/logind/phone acceptance
remain required; production Morf action negotiation stays disabled.
