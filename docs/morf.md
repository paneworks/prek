# Morf integration

Prek's broker can consume Morf's native lock evidence with
`broker --morf-executable /path/to/the/actual/morf-ELF`. The NixOS equivalent is
`services.prek.morfExecutable`. Use a Morf build containing the native provider;
a shell wrapper is not an executable identity. No desktop/root deployment is
performed by building this project.

The upstream implementation lives in `../morf` (formerly `../mold`) under
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

Native shell endpoints also provide read-only `rendered-context` when
`control-capabilities.rendered_context` is true. It exposes the primary GPU
frame's named scene bounding boxes, surface size/scale, endpoint epoch, UI
generation, revision and original monotonic observation time. Prek verifies and
transfers fresh observations to `prek.morf.regions()`; old providers return no
report. The report is bounded to 256 names/128 KiB, excludes hidden/transparent,
masked and exiting subtrees, and follows native transforms and clipping.
Repeated names are preserved under separate opaque native node identities,
scoped by the endpoint epoch and UI generation. A skipped or layout-only frame cannot
supply evidence, and reading the endpoint cannot extend its freshness.

This is submitted-frame inspection. Rounded/rotated shapes are represented by
bounding boxes; auxiliary surfaces and compositor stacking/presentation are not
covered. The broker does not convert these observations into gesture scopes,
keyboard absence or authentication. Those semantic and ownership integrations
remain required. See the exact shape in [the Lua reference](lua-api.md).

When `semantic_regions=true`, named bounds can also contain a typed `semantic`
annotation. Morf configs attach `input_context={role,layout?}` to the actual scene
node; they never submit coordinates. The eva shell labels its keyboard panel with the
current layout, authentication/session drawers as secure overlays, ordinary
drawers as local controls and persistent top/bottom/side geometry as shell roles.
The semantic Items take no input and remain laid out while native recognizers
are inhibited. Annotations describe visible primary-frame nodes only. Absence
does not prove a hidden keyboard, and these fields cannot fabricate unlock or
input ownership. Prek's output snapshots now retain `keyboard_known=false` for
missing keyboard context; awake spatial/visibility-dependent rules then pass
through with `KeyboardUnavailable` rather than treating missing data as hidden.

Providers advertising `layer_space` include `frame.surface` with the immutable
Wayland namespace, native logical viewport size and `local_scale` (compositor
logical units per Morf layout unit). Prek samples stock layer inventories around
the native read and matches exactly one output, producer PID and namespace with
an unchanged address, level and box. A matching report includes an optional
`layout` projection with `basis="stock_layer_layout"`, the producer's process
birth, output geometry generation and projected global logical bounds.

These coordinates describe the compositor's layout box. Hyprland's stock
[`layers` IPC](https://github.com/hyprwm/Hyprland/blob/v0.54.3/src/debug/HyprCtl.cpp)
exports `m_geometry`; its
[renderer](https://github.com/hyprwm/Hyprland/blob/v0.54.3/src/render/Renderer.cpp)
arranges that box in global logical coordinates. The
[rendered surface box](https://github.com/hyprwm/Hyprland/blob/v0.54.3/src/desktop/view/LayerSurface.cpp)
uses animated position and size instead. A plain layout projection therefore
cannot establish the surface's current animated position or grant gesture
authority.

In both pinned stock versions (0.54.3 and 0.56.2), `CLayerSurface::onMap` maps
the surface and starts the `layersIn` animation in one dispatch, and
`onCommit` warps later geometry changes once that animation has finished.
Frame callbacks are only sent for a mapped surface. Morf therefore reports
`frame.surface.mapped_ns`, its CLOCK_MONOTONIC receipt of the primary layer's
first frame callback, as an upper bound of the map time. The layer batch now
also reads stock `animations` and `getoption animations:enabled`, and
`map_in_ns` requires both bracketing samples to agree. The stock tick
(`AnimationManager::tick`, both versions) warps every value while the global
switch or the `layersIn` node is off, so the bound is then zero. Otherwise a
listed `layersIn` cubic bezier lasts `speed` tenths of a second (rounded up for
the two-decimal reply). Stock replies print each node's own values, so a
`layersIn` that was never configured uses its nearest overridden ancestor
(`layers`, then `global`, stock speed 8). The captured 0.56.2 reply in
`src/hyprland/animations/stock-0.56.2.json` checks this. Springs, unknown curves and values over ten seconds
give no bound. If
`mapped_ns + map_in_ns + 100ms <= started_ns`, the projection's `basis` is
`settled_stock_layer`, where `started_ns` precedes the first layer sample.
Such a box is the layer's current position. It is still not presentation,
stacking or an input mask, and stock IPC cannot tell a spring that reuses a
listed bezier's name apart from that bezier. Settled projections become gesture
scopes as described under [settled shell scopes](#settled-shell-scopes).

The separate `keyboard_registry` capability supplies `frame.keyboard` with
`scope="registered_morf_primary"`, a declaration count and `state` of `unknown`,
`hidden` or `visible`. Known states require one declared keyboard under the
primary root and include its native `node` and `layout`. Extra declarations,
foreign roots, exiting nodes and incomplete visible geometry remain unknown.
Hidden describes that registered node only; another program's keyboard remains
outside this coverage. Only an exclusive output, below, lets the registry
change `keyboard_known` in a gesture snapshot.

## Settled shell scopes

A full-output shell layer such as eva's `eva-drawers` primary surface appears in the
stock snapshot as one interactive `layer:<output>:<address>` region. That
region blocks application-content gestures and every reserved edge rule on the
whole output. With `input_region=true`, Morf also reports the rectangles of the
input region it committed with the witnessed frame, including unnamed
interactive items and the reload-error band. A configured mask or more than 512
rectangles is reported as unknown.

After each native refresh the broker refines the desktop snapshot it publishes.
A view qualifies only if its projection is `settled_stock_layer`, it describes
its input, it still validates against the current output, and it names the
exact coarse layer. The coarse region is then replaced by the outward-rounded
input rectangles, local controls and the semantic scope regions. Otherwise the
coarse region stays. On an output where neither bracketing sample showed
another process's bottom, top or overlay layer, the registered keyboard state
becomes `keyboard_known`, with its node's bounds when visible. A visible secure
overlay node always sets `secure_overlay`. The unit fixtures prove that a
reserved G11 strip and G05's physical bottom become eligible only after this
refinement. Physical delivery, presentation and production action negotiation
are not established by it.

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

The companion eva shell also supplies typed ordinary controller IPC,
documented upstream in `../morf/docs/PREK_CONTROLS.md`. Prek's strict wire types
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
  --morf /absolute/path/to/morf/target/release/morf \
  --morf-shell /absolute/path/to/morf/examples/shells/eva/shell/init.lua
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

## User-launched root probe

From your Wayland desktop terminal in this checkout, run:

```sh
./tests/run-root-handoff.sh
```

Run it as your normal desktop user. After the Nix build finishes, the runner
prompts for your sudo password and elevates only a fixed probe from the Nix
store. The compositor, Morf and log writer remain your user. The root probe
clears its environment, uses the packaged libsystemd, and selects one exact
ignored test; it cannot select an arbitrary command or test filter.

The local default uses `../morf` (falling back to `../mold`), prefers its
`target/release/morf`, then the newest available verified layout, context,
cancellation or wallpaper build, and the eva shell in that checkout (falling
back to the older Caelestia path). Override `PREK_MORF_REPO`, `PREK_MORF_BINARY` or `PREK_MORF_SHELL` for
another local build. The script enters the Morf development environment for its
graphics dependencies. It uses the existing visible nested session with parent
input isolation.

`target/root-handoff/latest` points to the unique retained run directory.
`console.log` contains build and runner output; `status.json` records running,
passed or failed with the final exit code. Under `cage/run-*`,
`morf-native-root.log` retains the root test's output even on timeout or failure,
and `morf-native-root-receipts.json` exists only after a successful, correlated
receipt. The remaining compositor, shell and cleanup logs are beside it.

This probe checks actual root credentials, logind registration and the native
quiesce → activate → restore sequence, with a genuinely supervised and reaped
child as release evidence. While owned, it also submits one keyboard-hide
request and waits for its native controller receipt. This exercises the queue
transport, without claiming a rendered keyboard transition. It opens no
evdev/uinput devices and does not test
physical takeover, a broker admission grant or phone input. The packaged test
executable is in the separate `nativeTests` output, outside the normal runtime
installation. Default Cage tests never invoke sudo. To use just the packaged
runner with explicit local paths, run `nix run --no-write-lock-file
.#cage-root-test -- --visible --morf /absolute/path/to/morf --morf-shell
/absolute/path/to/shell/init.lua`.

## Broker dispatch and native receipts

The trusted host entry `Broker::submit_morf_control` requires an already
offered action, its delivered execution grant, negotiated and ready Morf
capabilities, fresh desktop registration/context and an owned native handoff.
The action's output, gesture, ordinal, phase and logical pan displacement and
velocity are frozen from the original worker event. Static actions cannot be
substituted. A named Lua handler may select a Morf action only within its
declared Morf effect permission; it cannot replace the event's target or
motion. Refreshing context cannot extend the original execution grant.
Coalesced worker ordinals remain the authorization/deduplication identities;
the native controller receives consecutive ordinals for submitted attempts.
Cumulative displacement is preserved across that translation.

The native session reserves a monotonic request ID before sending. Its opaque
ticket includes the broker fence and native output/endpoint/UI/ownership
generations. `Broker::take_morf_control` consumes a terminal result once;
unknown, queued, foreign and already-consumed tickets return no result. A
queued reply is not controller acceptance. `Dispatched` means the native
controller accepted the request, without proving presentation or animation
completion. Rejected, unavailable and uncertain outcomes remain distinct.

Each handoff retains at most eight pending requests, 64 unconsumed receipt
records and 4096 gesture histories. Full queues reject before sending and never
evict unresolved records. Receipt reads rotate fairly; they expire to uncertain
one second after the dispatch deadline. Taking a receipt cannot permit replay.
Repeated/retired progress, changed actions, foreign output/epoch and deadlines
outside the current grant or ownership freshness fail before submission.
Failed mutations are never repeated. Ownership loss, retirement and release
turn unresolved work into uncertain results; terminal outcomes are retained.

The session sends typed Morf selections and cancellation through the broker;
it cannot supply a native payload. Public `MorfActions` negotiation stays
gated. Native rendered-context authority and full physical cutover remain required
before production negotiation. The opt-in root script tests the native backend
directly, not the broker's live action-grant path; its receipt keeps
`broker_grant_tested=false` and `native_action_rendering_tested=false`.

Native capabilities may advertise `cancel_gesture=true`. Its absence means
unavailable; source-bound action submissions refuse before writing unless the
provider supports it. The `cancel-gesture` operation carries the exact gesture
epoch and ID under the existing root-only endpoint/UI/fence binding. It retires
only that gesture's pending jobs and native UI guard, without revoking the
owning lease or another source's gesture. Acceptance confirms permission
retirement, not rendered cancellation or undoing an already-started effect.

The broker binds source-backed submissions to the original device, connection
generation and seat. Source invalidation and gesture cancellation schedule only
matching native gestures, including an accepted begin whose receipt was already
consumed. A later accepted begin retires older continuous histories under
Morf's one-controller-per-output contract. Pending requests remain bounded.
Cancellation attempts are recorded before sending. Confirmed and failed
attempts cannot be resubmitted, and unresolved effects remain uncertain.

Loss of the entire desktop/fence/output schedules one native `revoke` instead.
The broker processes current client messages before polling native work, so a
known context loss cannot renew its former authority in that step. Revocation
retires queued work while native recognizers stay inhibited. Only the separate
worker-reaping proof and neutral release sequence can restore recognition.
No root I/O occurs from `Drop`; failed/uncertain exchanges stop without retry.

The root script checks scoped cancellation when the provider advertises it,
verifies a different gesture still dispatches on the same lease, then checks
scope revocation before worker-exit restoration. Its receipt records
`native_gesture_cancel_supported`, `native_gesture_cancel_tested` and
`native_scope_revocation_tested`; a legacy provider cannot be reported as a
scoped-cancellation pass. Production negotiation, native rendered-context
authority, full ownership orchestration and physical/phone acceptance remain
unfinished.

The session's `--morf` opt-in installs a metadata-only submission adapter. It
opens no native socket and cannot turn provider presence into an effect grant.
The broker currently refuses public negotiation until the rendered-context and
physical ownership gates are complete. Typed transport and host-fixture tests
therefore do not establish a working production route.

After an exact delivered grant, the agent sends `MorfAction` with only its
fence, action ID/ordinal and action enum. The broker reconstructs the frozen
target, phase and logical motion, revalidates authority and submits once. Each
offered ordinal admits at most one Morf effect. It retains up to 64 outstanding
native submissions, and the agent counts native work against its eight-effect
asynchronous bound. Native receipts are polled every 5 ms while work is pending;
ownership sampling remains at 100 ms. Idle receipt polling stops when empty.

`MorfResult` completes the agent's effect group only after a terminal native
outcome. Native controller acceptance maps to `Applied`, without claiming
presentation. Lost/ambiguous or refused native work is conservative and cannot
manufacture success. Mixed named-handler plans join that result with their
owned command children; partial effects remain uncertain. A missing result two seconds
after the original grant deadline schedules cancellation and reports uncertainty, never resubmission.

`MorfCancel` references only known owning gesture history and introduces no
new effect. Repeated cleanup is suppressed. Stale generation results cannot
complete a replacement plan; duplicate/late known results cannot decrement a
group twice. The broker accepts a native action completion only after its
correlated result was actually sent. Subscriber overflow removes that subscriber
and retires its native scope while the input worker remains independent.
