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
