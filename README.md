# prek

Low-level Linux input tools, named after Albanian **preke** — “touch it”.

Prek reads evdev input below Wayland. Its recognition core is independent of Morf
and the compositor. The current CLI supports read-only observation, private
recordings, deterministic offline replay, declarative configuration validation,
configured rule observation, and authenticated broker/session status.

The library additionally provides seat-scoped chords, layers, repeatable holds,
reference-counted logical keys, armed path capture and shape matching. These
primitives are tested. Configured key edges, button taps/holds/repeats, chords,
layers, wheels, touch gestures and configured shapes now run in observation and
replay. Ordered temporal sequences now compose those recognizers. Exclusive input
routing, broker/session integration and desktop actions are still in progress.
The current executable does not grab devices or execute configured actions.

The library also has virtual-device preflight, a bounded raw-frame router and
a configured routing coordinator.
Preflight models capability/axis preservation, consumer mapping and acknowledgement,
feedback requirements and neutral handoff. The router models reserve/buffer/observe
ownership, scheduled fallback, slot reuse, delivery acknowledgements and neutral
recovery. The Linux backend now publishes pre-add metadata, creates per-source
uinput devices and writes bounded complete frames. It also reads declared LED
feedback. Real consumer readiness, the reverse-feedback bridge, policy arbitration
and supervised input takeover are still pending. Devices with repeat support or
nonzero axis fuzz are refused until transparent forwarding is verified for those
capabilities.
Production routing remains disabled.

The configured routing core connects rule candidates to reserve/buffer/observe
ownership. Consuming candidates require a current context permit and a negotiated
action consumer; unavailable authority or actions preserve ordinary input.
Finger joining, rule priority and coherent direction determine ownership before
passthrough. One-finger input falls through at the join boundary when no eligible
rule remains, and forwarded input cannot be promoted by later fingers. Swipes
claim on coherent motion before final travel succeeds; failure or extra fingers
after claim drain without replay or a trailing click.

Persistent touch coordinates and the selected slot can change during consumed
input. Before forwarding an ordinary delta frame, the router restores the
cursor and missing coordinates needed by that frame. Explicit raw values take
precedence. Repairs add no contacts, keys or SYN frames, preserve the original
timestamp/sequence and count toward frame/storage limits. Buffered repairs keep
the state of each original frame rather than using a later gesture's position.

Dispatch requests retain gesture/configuration/session identity and frozen
targets, and are checked again when dequeued. Queues and deadlines are bounded;
stalled output or dispatch consumers require transport recovery. The default
decision delay remains 300 ms; explicit configurations may increase it to five
seconds and callers can inspect the scheduled delivery offset. Native replay
and routing-library tests cover configured consumption with model context.
Local application delivery, live broker bridges, pre-grab handoff and remaining
application behaviors are required before the CLI can enable routing.

The Hyprland client verifies the IPC socket's owner, compositor process identity
and executable before querying bounded device inventories. It supports the
verified 0.54/0.55 schemas and checks virtual-device acceptance and reported
keyboard settings. The kernel backend also verifies processed udev metadata
and the resulting evdev capabilities. Settings absent from stock IPC and
complete readiness handoff remain pending.

The session library obtains agent credentials from an established local socket
and verifies that agent and compositor belong to the same active local logind
session and seat. Checks retain kernel process handles and expire after 250 ms.
Process exit, socket disconnect, compositor replacement or session deactivation
revokes registration; returning to the seat requires a new registration.
This proves session membership. Optional live compositor context is sampled
separately; bound lock-provider acknowledgements and action dispatch remain pending.

## Commands

Build with `cargo build --locked`, or run commands through Cargo:

```sh
cargo run --locked -- devices --json
cargo run --locked -- watch /dev/input/by-id/YOUR-DEVICE --gestures-only --json
cargo run --locked -- init --preset desktop
cargo run --locked -- check --config config.lua --json
cargo run --locked -- doctor --config config.lua --json
cargo run --locked -- run --config config.lua --mode observe --duration 10 --json
cargo run --locked -- migrate --config previous.toml --output migrated.lua
cargo run --locked -- session --json
cargo run --locked -- session --observations --json
cargo run --locked -- session --context --observations --json
cargo run --locked -- status --json
sudo target/release/prek reload --json
cargo run --locked -- learn-shape my-v --device /dev/input/by-id/YOUR-MOUSE --mouse --output my-v.lua
cargo run --locked --example main
```

Choose an actual path from the device inventory. Multiple paths can be passed to
`watch`; a stable by-id/by-path link is preferable. Unreadable nodes are listed
with their errors. Reading requires permission on the chosen event nodes.

The observer uses kernel monotonic timestamps and complete input frames. It
recognizes settled button taps/holds and decodes multi-touch slot identities,
multi-finger tap groups, holds, coherent pans/swipes, pinches and rotations.
Without output geometry, touch positions are explicitly normalized device
coordinates. No pixel coordinates or application targets are guessed.

Single/double/triple taps emit only their settled final count. Default tap duration
is 250 ms, tap gap 300 ms, hold 600 ms and touch join window 120 ms. Touch movement
uses reported millimetres, with explicit normalized fallbacks when resolution is
missing. Kernel key repeats do not count as additional taps.

`run` selects readable physical sources through the configuration's device/seat
allowlist. It reports versioned rule matches, skipped candidates and cancellations
without exposing the raw input stream. Matches include a gesture ID, event ordinal,
configuration/session generations and the proposed action. Repeats share a gesture
ID and have distinct ordinals. Observation evaluates each rule; it makes no claim
about exclusive arbitration or input consumption. `--mode route` currently refuses
before device takeover.

Configured observation runs in a supervised worker. Only that worker opens input
devices; the parent watches a private health channel. The input loop reports
progress every 250 ms, including while idle. One second without progress triggers
termination, followed by a forced kill after a 250 ms grace period. Ctrl-C and
SIGTERM request orderly shutdown, and parent death kills the worker. Process tests
use private sockets and subprocesses. Idle observation, exclusive worker input
descriptors and kernel teardown remain guarded acceptance cases for a prepared
isolated Linux environment. Recovery of real grabs
and held virtual input is also tested with isolated keyboard/touch sources:
orderly stop delivers a neutral frame, while worker/parent death and stalls release
the original grab and remove the virtual device. Application state after device
removal and production broker handoff still require downstream-consumer gates.

`broker` runs the same supervised observation worker alongside a privileged
control service. It requires a protected root-owned system configuration and
parent directories, the actual Hyprland ELF path and an absolute system
libsystemd path:

```sh
sudo prek broker --config /etc/prek/prek.toml \
  --compositor-executable /YOUR/SYSTEM/Hyprland-ELF \
  --logind-library /YOUR/SYSTEM/libsystemd.so.0
```

Start `session` from the compositor's own login session. It uses
`HYPRLAND_INSTANCE_SIGNATURE`, or an explicit `--instance`, and maintains a
250 ms acknowledged heartbeat. A second agent for the same seat is refused;
`status` uses a separate authenticated inspection role. A shared user-manager
process without session membership is refused. Both commands accept `--socket`
for a different broker endpoint. Status reports the active agent's current
context, snapshot revision and any context/reload error. Device ownership/latency
counters and desktop action dispatch remain pending.

`session --observations` subscribes to configured outcomes from its verified seat.
The worker acknowledges readiness with `subscribed: true` in JSON output after
reconciling held keys and contacts. Already-held or unread input older than that
activation cannot become a new gesture. Outcomes carry the current broker,
configuration and session fences. Inspection clients receive status only. Session
membership permits context-free observations after a fresh compositor lock check.
Add `--context` to enable rules scoped to the verified desktop or greeter. The
broker samples output geometry, workspaces, DPMS, focus and layers directly from
the bound compositor, rechecks logind, and publishes context on its private worker
channel. Caller-supplied context snapshots remain refused.

Context refreshes run at 100 ms intervals with a shared 200 ms IPC deadline and
250 ms snapshot freshness. Context loss, restoration or a witnessed lock advances
the session fence, discards queued outcomes and reconciles held input before
recognition resumes. Workers expire context independently while idle; delayed
context cannot stop their input reader. Oversized snapshots disable publication
and report an error instead of truncating targets or crashing the broker.

Every observation is checked against the compositor lock before delivery, including
agents without `--context`. A witnessed lock inhibits all observation subscriptions
and remains latched across agent reconnection. A bare compositor unlock cannot
resume observations: the bound lock provider's completed transition is required.
With an adapter-enabled Morf build, pass its actual ELF as `broker --morf-executable`
and enable `PREK_LOCK_PROVIDER=1` for the lock process. The broker verifies the
native endpoint's exact process/executable, active logind session and actual
Wayland compositor identity. It accepts unlock only for the bound native lock
epoch after Morf observed its compositor acknowledgement, flushed unlock, and
the compositor independently reports unlocked. Morf briefly retains its native
endpoint until the broker acknowledges completion; timeout or provider loss keeps
inhibition in place. Caller-supplied Lua lock state and socket disappearance cannot
authorize unlock. Locked/greeter region actions and authentication remain the
provider's responsibility. Stock window coordinates describe animation
goals, so application-content targets remain ineligible until rendered geometry is
proven by a provider. Layer bounds reserve interactive space conservatively;
Morf-specific regions and device-to-output mapping are still required. The native
adapter gate is [tests/morf.nix](tests/morf.nix), which takes a separately built
upstream Morf package; generic Prek builds retain no Morf dependency.

The private worker channel carries subscriptions and semantic outcomes, with no
raw frames or device descriptors. Its health channel remains separate. Progress
updates coalesce while queued, and retained data has packet and byte limits.
For coalesced updates, use total displacement or ratios to track progress; the
latest per-frame delta describes only that frame.
Overflow or an expired observation lease revokes the affected stream while the
worker continues reading input. Disconnect or registration replacement discards
queued outcomes and requires fresh input on the next subscription.

`sudo prek reload` rereads the broker's configured system file. Clients cannot
supply a different path or configuration. The parent validates and compiles one
bounded snapshot, transfers those exact bytes privately, and waits for the
worker to validate against current device state before committing the new
generation. Invalid files leave the active configuration and pending gestures
intact; `status --json` reports the last rejection. Startup uses the same snapshot
transfer, so the worker does not reopen a file that changed after validation.

Successful reload cancels prior gesture progress and discards its queued outcomes.
Sessions receive the new configuration fence, then a fresh subscription readiness
acknowledgement. Already-held triggers must reach their neutral boundary before
starting new recognition. A bounded grace period accepts in-flight heartbeats
from the previous generation; other operations retain strict
fencing. Observation reload also applies changes to device/seat selectors.

The control protocol uses versioned, bounded sequenced packets. Kernel sender
credentials are checked on every message, transferred descriptors are closed
and refused, and packet order/time plus broker/configuration/session generations
are fenced. A private kernel lease permits one broker per runtime directory.
Bad, disconnected or stalled clients lose their queued replies and seat ownership;
the device worker continues independently. Startup, control traffic and device
supervision have native process tests; privileged kernel acceptance remains a separate gate. Route mode still refuses before
runtime/device takeover.

Supervised virtual devices carry their worker's process birth and supervision
epoch in the private runtime registry. Complete metadata is published atomically.
Cleanup after a reaped worker preserves other lifetimes; restart cleanup also
checks kernel process handles before reclaiming orphaned records. Live and
unmanaged publications remain intact. These APIs do not enable route mode.

Event loss cancels that source's recognition. Configured `run`/`broker` observation
rescans the allowlist every 250 ms, waits for initialized udev metadata and reopens
lost or newly selected devices with a fresh source generation and queried held
state. It stays live while no selected source is present. Changed metadata,
ambiguous identities and removed selectors cancel the affected source. Discovery
retains at most 4,096 source identities. Explicit `watch`, `record` and learning
commands keep their selected-source lifetime. Suspend/resume recovery remains
pending. Offline replay supports reconciliation and mapping-generation changes.

## Configuration

Prek embeds onix-os/luna for Lua configuration. `init` writes a minimal
`config.lua` in the current directory by default; `--preset` and `--output`
choose a different preset/path. `.lua` files work with `check`, `doctor`, `replay`, `run`
and the protected broker configuration. `prek api --json` describes implemented
configuration and runtime bindings; the [Lua reference](docs/lua-api.md) documents
fields, constructors, libraries and limits. `prek api --lua` exports the shipped
[Lua Language Server definitions](docs/lua/prek.lua); point `workspace.library`
at the installed `share/prek/lua` directory for completion and type checking.
Supervised session callbacks and timers
are implemented. Named action handlers return bounded command/event plans under
fresh broker grants; `session --lua --commands` opts into that executor. Custom
recognizers and live adapter APIs remain required work.

`events.on("input", callback)` receives permitted normalized input when protected
`policy.allow_input_events` and the owning session's `--lua --input` opt-in agree.
Only explicitly allowed devices on that seat in fresh desktop context are
eligible. See [input subscriptions](docs/lua-api.md#permitted-normalized-input)
and [the example](examples/lua/input_events.lua) for frame/reset types and limits.
These observations do not grant input consumption or action authority.
`devices.list/get/revision/position` expose the received permitted metadata and
validated coordinate mapping; `seats.current` returns the fresh current session
binding. See [device queries](docs/lua-api.md#permitted-device-and-seat-queries).
`outputs` and `regions` query verified compositor geometry;
`coordinates.convert` uses an exact snapshot reference for output coordinates.
See [geometry queries](docs/lua-api.md#output-region-and-coordinate-queries).
Rule declarations support observational `on_begin`, `on_update`, `on_end` and
`on_cancel` hooks; `rules.on` returns a disposable per-rule subscription.
See [rule callbacks](docs/lua-api.md#rule-phase-callbacks).
`session --power` negotiates protected-policy DPMS blank/wake on explicit frozen
outputs with a routing broker; `--lua --power` also enables approved Lua plans.
Production takeover remains gated. `nix run .#cage-test -- --layers --input --power`
verifies the owned nested output; see
[adapter scope and requirements](docs/lua-api.md#experimental-output-scoped-dpms-adapter).
Production routing remains disabled; named-effect execution currently has model
grant and helper-process coverage, without authenticated production routing proof.
Pending effects are canceled by generation, gesture cancellation and source
invalidation. A canceled queued Lua callback does not run; bounded in-flight
results are discarded, while unrelated session work continues. Commands already
started remain owned and reaped. JSON transport preserves floating-point geometry
exactly, and execution grants retain the context snapshot's freshness deadline.

Reusable Lua files live in `modules/` beside `config.lua`, with dotted imports
such as `require("phone.bindings")`. Protected files are captured into the
approved generation and transferred with callbacks; the runtime loads those
bytes without filesystem access. Imports are cached, and cycles, unsafe paths,
permissions and source/count/depth limits are checked. See the Lua reference
for the complete module policy.

`scheduler.spawn` runs bounded coroutine tasks in the unprivileged session;
`scheduler.sleep` yields until a monotonic deadline. Task handles expose
completion/cancellation state, and completed tasks free capacity without
reusing identities. Generation replacement retires their coroutine state.

Runtime callbacks and tasks can query `context.snapshot()`, `binding()`,
`state()`, `fence()` and `error()` for broker-verified session identity and full
output/window/region/keyboard geometry. Results are owned copies, expire from
their original monotonic timestamps, and cannot set authority. The agent
refreshes snapshots every 100 ms; `context_changed` notifies the owning Lua VM.
See [the context reference](docs/lua-api.md#runtime-context) and
[example](examples/lua/context/config.lua).

Presets are `minimal`, `desktop`, and `phone`; each round-trips through `check`.
Explicit TOML input/output remains supported with schema version 1; no removal
date is scheduled. `migrate --config FILE.toml --output FILE.lua` validates the
complete legacy model, emits deterministic Lua declarations and verifies the
same model through the isolated Luna compiler before writing. It preserves the
input and existing output; `--force` permits replacing only the destination.
Migration preserves effective values and rule order; comments and original unit
spelling are normalized. Named handlers require their original approved source
and cannot be recreated from a declarative model. New files use private permissions.

Shape rules name a template with `trigger = { kind = "shape", name = "v",
fingers = 1 }`. Touch shapes follow the centroid of a fixed contact set; all
required fingers must join before the join deadline. A partial lift, replacement
or extra finger cancels capture. All contacts lifting together complete it,
including their final release-frame positions. Generic paths use reported
millimetres where available, with normalized fallback; mapped paths use logical
output coordinates. Matching retains physical aspect ratio without a display map.

For mouse shapes, omit `fingers` and provide an arming key or layer. The default
capture button is `BTN_LEFT`; `button = "BTN_RIGHT"` selects another delimiter.
Press starts capture and release completes it, including release-frame movement.
Paths integrate raw relative counts. They do not infer pointer pixels or a
window under the pointer. The capture button can also be an arming key; other
throughout activation keys must remain held. Releasing a required key from
another selected device cancels immediately. Sampled activation remains valid
for the rest of that capture. Unarmed or already-held sequences wait for neutral.

`[shape_capture]` controls `max_samples`, `timeout`, `minimum_length`,
`minimum_size`, normalized `fallback_length`/`fallback_size`, and
`mouse_minimum_length_counts`/`mouse_minimum_size_counts`. Defaults are 2,048
samples, five seconds, 20 mm length, 12 mm extent, normalized fallbacks of
0.1/0.04 and mouse limits of 20/12 counts. The engine also caps retained shape
samples at 65,536 across all rules/sources. Capture failure or low/ambiguous
confidence produces cancellation and no match.

`trigger = { kind = "sequence", steps = [...], timeout = "5s" }` composes
ordered triggers within one permitted seat. Intermediate steps stay internal;
the completed sequence reports one outcome with evidence for each step. A final
continuous trigger retains begin/update/end, one gesture ID and ordered ordinals.
Tap-then-hold uses the actual last tap release for ordering, so the hold may
already be in progress when the tap count settles. A touch hold may continue
into a swipe or shape only with the same source and contact identities. Other
steps require fresh input starting after the previous step completes.

Timeouts start with candidate input. Nested sequences retain their own deadlines.
Sampled activation applies from the sequence's first step; throughout activation
must remain valid for the whole chain. Source loss, reconciliation, mapping or
session/context changes cancel pending work. Resets suppress already-held input
instead of inventing a new press. Contextual sequences retain their first target;
a focus change cannot redirect the result. Configuration bounds each sequence
to 64 compiled recognizers and the program to 4,096. Shape sample retention
remains globally bounded across sequence steps. These are observation/replay
semantics; exclusive sequence routing remains pending.

`learn-shape NAME --device PATH --output TEMPLATE.lua` explicitly arms one
read-only touch capture. Use `--fingers 2` for a fixed two-contact centroid, or
`--mouse` with optional `--button BTN_RIGHT` for relative mouse capture. Draw
after the reader reports ready, then release. Capture uses the same size,
identity and timeout checks as configured shapes. Rejected strokes can be retried
within `--duration` (30 seconds by default); Ctrl-C cancels without saving.
The first valid stroke is saved privately as a standalone schema-1 template
Lua file with 64 resampled points and no enabled rules. An explicit `.toml`
output keeps the legacy format. Existing output files are
preserved. `--json` reports units, sample count, duration and destination.
`--from-recording RECORDING.jsonl` learns deterministically from the explicitly
named source without opening input. Copy the resulting template into the desired
configuration and choose a binding explicitly.

The phone preset describes the intended Morf/Hyprland gesture bindings and
ownership policies. It does not enable a service or install those bindings yet.
The desktop example is disabled until you enable its rule. Desktop shape templates
include V, circle and zigzag without active action
bindings. `check` validates syntax, units, references, bounded nesting, system
device/seat policy and conflicting exclusive rules. `doctor` additionally checks
readable hardware, stable identity ambiguity and touch capacity, and reports
unconnected runtime features, routing and unavailable adapters as explicit findings.

Timing values have explicit `ms`, `s`, or `m` units. Distances use `mm`, `px`, or
`norm`. Pixel thresholds require mapped output geometry; millimetre thresholds
have explicit normalized fallbacks. Unknown fields are errors. Root policy and
user overlays are separate: an overlay may replace/disable a rule but cannot
expand permitted devices or seats.

The configured engine compiles a whole replacement before activating it, retains
the last good configuration on failure, cancels previous candidates and reconciles
held keys/contacts without inventing presses. Live ownership handoff and the
daemon's `reload` command are not implemented yet.

Rules name device selectors, contexts, scopes, required keys/layers, delivery,
trigger and action. `require_keys` combines devices on the same permitted seat;
`keys_throughout` cancels when an activation key/layer is released. The trigger's
timing overrides apply to that rule, including arbitrary hold durations and repeats.
Ordinary OS repeat is separate from configured repeat-while-held.

An unbound event rule can be observed without a desktop if its scope is everywhere
and it has no context, screen or keyboard condition. Other rules require a verified
session provider. `session --context --observations` supplies verified compositor
context for observation. Rules requiring app targets, keyboard geometry or
Morf regions remain skipped until their providers are integrated.

The library's context model freezes the first target and validates later fingers
against it. A focus change cannot redirect a swipe. Session/lock changes, stale
snapshots, target removal, relevant geometry changes and overlays cancel it.
Compositor visibility is distinct from a mapped window on another workspace.
The model requires 250 ms snapshot freshness and a one-second provider lease.
The Linux session library supplies verified socket/process/logind membership
for agents launched inside the compositor's session. Action integration,
delegation for shared user services and real lock-provider registration remain
pending. The lockscreen/greeter UI and authentication remain Morf's responsibility.

## Recording and replay

Capture only a deliberately selected device:

```sh
prek record --device /dev/input/by-id/YOUR-DEVICE --output input.jsonl --duration 10
prek replay input.jsonl --json
prek replay fixture.jsonl --check
prek replay rule-fixture.jsonl --config prek.toml --check --json
```

The JSONL format is versioned and bounded. It records device metadata, initial
state, complete frames and timer ticks. A test fixture may additionally contain
output mappings, reconciliation snapshots and a final expected-outcomes record.
Linux registration also records persistent absolute-axis state, including the
selected multi-touch slot and coordinates of inactive slots. The `absolute`
record seeds those values before the source's first frame after registration or
reconciliation. It creates no contacts or presses. This preserves coordinates
when the kernel omits unchanged values; existing recordings remain readable.
Generic fixtures use `expected`; configured fixtures use `expected_rules`. The
decoder limits each line to 4 MiB, total input to 64 MiB and records to 100,000.
Configured replay additionally bounds its retained observation queue; live
observation drains it continuously. Context-dependent replay tests currently use
the library's trusted context API; serialized session-context fixtures are pending.

`--check` requires explicit expectations. IDs, counts, phases, timestamps and raw
input remain exact; touch geometry allows only 1e-9 relative floating-point
roundoff. Replaying never accesses input devices, injects events or calls desktop
commands. Raw recordings are an explicit diagnostic operation, not service logs.

## Core structure

- `input` normalizes complete frames, keys, wheels and multi-touch contacts.
- `buttons` handles settled button timing; `keys` adds chords/layers and ownership.
- `gestures` maps coordinates and groups touch interactions.
- `paths` bounds armed trajectories and compares resampled shape templates.
- `config` owns the strict schema, presets, policy and generation model.
- `context` checks session authority, regions and frozen application targets.
- `rules` connects configured recognizers, conditions and context checks.
- `transport` validates virtual-device descriptors and readiness evidence.
- `router` owns sequences and schedules bounded, ordered raw-frame delivery.
- `kernel` owns Linux virtual-device creation, metadata and nonblocking writes.
- `hyprland` verifies compositor IPC identity and virtual-device inventory evidence.
- `session` verifies agent/compositor membership in logind's active seat/session.
- `protocol` and `ipc` bound control messages, generations and kernel sender credentials.
- `service` owns authenticated broker registrations and their bounded leases.
- `supervisor` bounds worker lifetime and handles foreground/service termination.
- `diagnostics` checks device selection/capacity and reports missing consumers.
- `replay` connects frame normalization with button/touch recognition.
- `linux` handles read-only device access; `main` exposes the CLI.

Unit tests live with their modules. Ordinary CLI integration tests live in
`tests/cli.rs`. They use explicit timestamps and synthetic data, requiring no
input permissions or display server. Separate kernel tests in `tests/routing.rs`
are ignored by default: they require a prepared isolated environment, root-owned
runtime metadata and this package's udev rule. Every test event node is created
by that harness; it never opens or grabs a production event node.
`tests/supervisor.rs` runs eleven process gates for healthy/stalled workers,
termination escalation, health-channel failure, worker identity and parent death.
It uses a separate executable so fixtures run on their process's original thread.
`tests/recovery.rs` and the other privileged kernel fixtures remain manual gates
for a prepared isolated Linux environment. They are excluded from ordinary
Cargo and local compositor runs; they must not inject into the host session.

The desktop test setup follows Mold's `tools/sandbox/nested.sh`:

```sh
nix run .#cage-test                     # headless Cage -> nested Hyprland -> receiver
nix run .#cage-test -- --visible        # watch the owned session as a window
nix run .#cage-test -- --layers         # Lua layer change with its rule still enabled
nix run .#cage-test -- --layers --input # Normalized source frames and resets reach Lua
nix run .#cage-test -- --layers --input --power --windows # Lua DPMS and targeted window actions
nix run .#cage-touch-test               # direct headless Cage touch receipts
```

The runner uses private Wayland sockets, a scratch home/XDG directories and a
private session bus with service activation disabled. The system bus and seatd
socket point to nonexistent paths. It injects B and Shift+A into the nested
compositor only, checks six ordered events, text and neutral modifier cleanup,
then stops its Cage, Hyprland, proxy and receiver processes. Logs and receipts
remain under `target/cage-host/run-*`. No host evdev devices are opened.
`RENDER_NODE` selects the rendering device, defaulting to `/dev/dri/renderD128`.
The compatibility proxy is adapted from Mold's sandbox at commit `36261df7`.

The same run now queries that owned Hyprland through Prek's verified IPC client,
transfers complete geometry to its real supervised Luna helper, and routes
synthetic A/B frames through the Lua-configured routing library. The app must
receive only B from that phase; A must produce its semantic callback without
appearing in the receiver. These source/readiness/session authorities are model
fixtures. They do not establish production kernel takeover or logind registration.

The app receipt for that phase now precedes a Lua disable transaction. The
coordinator retires its transports and queued matches, the fixture establishes a
fresh neutral model source incarnation, and Lua receives revision 2 while the
file/session fences stay at 1. A second phase must deliver A and B to the same
real receiver, with neutral keys/modifiers and increasing gesture IDs. Nine
actual compositor snapshots and the correlated Lua receipt are checked.

`--layers` runs the same app-receipt sequence by deactivating A's declared layer
while keeping A's rule enabled. The receiver must then get A and B. This tests
acknowledged Lua layer control through the routing model and actual nested app;
source ownership/readiness remain model fixtures.

`--windows` creates two additional owned receivers. Granted Lua plans move the
starting receiver to the adjacent numeric workspace and back, following it,
then close it after focus switches to the guard receiver. Stock IPC must confirm
the exact window/workspace/focus, the target must acknowledge close, and the
guard must remain alive. The adapter currently supports stock Hyprland 0.54.3
and single-output workspace moves through explicit authorized host installation.
Public action negotiation, production takeover, reorder and scrolling-layout
effects remain pending. [The reference](docs/lua-api.md#experimental-targeted-hyprland-window-adapter)
documents the stock address-selector race between lifetime preflight and dispatch.

The separate touch runner uses a test-only Cage build with a private inherited
socketpair and a wlroots touch device. Its events use Cage's ordinary touch
handlers and must reach the actual receiver as two identified contacts, precise
coordinates, motion, four ordered touch frames and neutral releases. The driver
refuses non-headless backends before startup and terminates on malformed input.
No public input protocol or kernel input device is created. The receiver's first
rendered-frame receipt establishes the mapped target before touch injection.

These local checks prove keyboard/touch delivery and modeled keyboard consumption.
Touch routing/consumption, cancellation, remapping/repeats, broker/lock integration and handoff races retain their own
required acceptance gates. The old NixOS/QEMU test drivers are retired; ARM
builds and execution follow the native-CI and phone cross-build paths below.

`tests/protocol.rs` covers schema/size/order/time bounds, nonblocking backpressure,
descriptor rejection/cleanup, endpoint lifetimes and inherited-socket misuse.
Privileged `learn-shape` checks on owned synthetic devices remain a manual
acceptance gate, including saved templates and idle Ctrl-C cancellation. Offline learning tests reuse the exported template in replay.
Configured shape tests cover touch/mouse V, circle/zigzag, fixed multi-finger
centroids, arming changes, release-frame motion, confidence/size/resource limits
and mapping/contact replacement. CLI shape replay checks a hand-written expected
outcome. This is recognition/replay evidence; physical trials and exclusive
shape routing remain pending.

Shape matching preserves path order and aspect ratio;
rotation/reflection invariance must be enabled explicitly. Too-small, unmatched
and ambiguous paths produce no match.

## Development

The existing Oslo task runner remains available:

```sh
oslo make build
oslo make run
oslo make run --device /dev/input/by-id/YOUR-DEVICE
oslo make example
oslo make verify
```

Direct checks are `cargo fmt --all -- --check`,
`cargo clippy --locked --all-targets --all-features -- -D warnings`,
`cargo test --locked --all-targets --all-features`, and
`RUSTDOCFLAGS='-D warnings' cargo doc --locked --no-deps`.

The flake declares packages/apps and development shells for x86-64 and ARM64
Linux. CI runs builds and tests natively on `ubuntu-24.04` and
`ubuntu-24.04-arm`, including the Rust 1.88 minimum. These jobs are configured;
a local cross-build does not establish native ARM execution or phone behavior.

Like Mold, the phone shell targets postmarketOS/Alpine's musl loader:

```sh
nix develop .#cross-aarch64
cargo build --release --target aarch64-unknown-linux-musl --bin prek
```

The shell supplies the ARM standard library and cross linker. It uses dynamic
musl linking with `/lib/ld-musl-aarch64.so.1`, allowing runtime system libraries
to load on the phone. Run device acceptance on the actual ARM phone; desktop
Cage checks run locally without a machine emulator.

For Nix builds of a Git checkout, add new source files to Git's index so they are
included. The package source excludes recordings, sockets and local build output.

MIT. See [LICENSE](LICENSE).
