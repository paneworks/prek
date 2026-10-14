# prek

Low-level Linux input tools, named after Albanian **preke** — “touch it”.

Prek reads evdev input below Wayland. Its recognition core is independent of Morf
and the compositor. The current CLI supports read-only observation, private
recordings, deterministic offline replay, declarative configuration validation,
and configured rule observation.

The library additionally provides seat-scoped chords, layers, repeatable holds,
reference-counted logical keys, armed path capture and shape matching. These
primitives are tested. Configured key edges, button taps/holds/repeats, chords,
layers, wheels and touch gestures now run in observation and replay. Configured
shape capture and sequence composition remain pending; enabling those rules
produces an explicit runtime/preflight error. Exclusive input routing, a
broker/session service and desktop actions are still in progress.
The current executable does not grab devices or execute configured actions.

The library now also has virtual-device preflight and a bounded raw-frame router.
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
This proves session membership, while lock-provider acknowledgements and the
broker/session protocol are still required before desktop actions can run.

## Commands

Build with `cargo build --locked`, or run commands through Cargo:

```sh
cargo run --locked -- devices --json
cargo run --locked -- watch /dev/input/by-id/YOUR-DEVICE --gestures-only --json
cargo run --locked -- init --preset desktop --output prek.toml
cargo run --locked -- check --config prek.toml --json
cargo run --locked -- doctor --config prek.toml --json
cargo run --locked -- run --config prek.toml --mode observe --duration 10 --json
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
use private sockets and subprocesses. The disposable VM also verifies idle
observation, input descriptors owned only by the worker, graceful shutdown and
forced teardown of a stopped observer within two seconds. Recovery of real grabs
and held virtual input is also tested with isolated keyboard/touch sources:
orderly stop delivers a neutral frame, while worker/parent death and stalls release
the original grab and remove the virtual device. Application state after device
removal and production broker handoff still require downstream-consumer gates.

Supervised virtual devices carry their worker's process birth and supervision
epoch in the private runtime registry. Complete metadata is published atomically.
Cleanup after a reaped worker preserves other lifetimes; restart cleanup also
checks kernel process handles before reclaiming orphaned records. Live and
unmanaged publications remain intact. These APIs do not enable route mode.

Event loss cancels that source's recognition. The current live observer removes
a lost/disconnected source; other selected sources continue. Automatic reopening
and live kernel-state reconciliation remain pending. Offline replay supports
explicit reconciliation and mapping-generation changes.

## Configuration

`init` writes schema version 1 as TOML. Presets are `minimal`, `desktop`, and
`phone`; each round-trips through `check`. Existing files are preserved unless
`--force` is supplied. New configurations and recordings use private permissions.

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
session provider. The current CLI has no live provider registration, so it reports
those rules as skipped rather than guessing the session or app.

The library's context model freezes the first target and validates later fingers
against it. A focus change cannot redirect a swipe. Session/lock changes, stale
snapshots, target removal, relevant geometry changes and overlays cancel it.
Compositor visibility is distinct from a mapped window on another workspace.
The model requires 250 ms snapshot freshness and a one-second provider lease.
The Linux session library supplies verified socket/process/logind membership
for agents launched inside the compositor's session. Service integration,
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
`tests/recovery.rs` runs only in the explicit isolated VM. Its eight cases cover
held keyboard modifiers and touch slots during orderly stop, worker death,
supervisor death and stopped forwarding. The supervising process retains no
input descriptors; a separate harness owns each synthetic source generator.

`nix build .#checks.x86_64-linux.routing` runs those gates in a disposable NixOS VM
without host device passthrough. The gates check metadata/capabilities, ordinary
keyboard/mouse/touchpad frames, consumed touch, replay timing, LED feedback,
supervised observation shutdown and kernel recovery. Building the normal
package installs its udev rule in the package output; it does not activate it on
the host or start input routing. ARM kernel execution remains a separate gate.

`nix build .#checks.x86_64-linux.compositor` tests the client against stock
Hyprland in a separate disposable VM. It checks virtual keyboard acceptance,
processed metadata, keymap verification and removal. Ordinary IPC tests cover
endpoint replacement, executable/permission checks, malformed replies and timeouts.
The VM also checks actual logind registration, rejection of another process with
the same UID outside the session, agent death, disconnect and active VT changes.

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
Linux. Build natively on the corresponding architecture, or use a configured
cross/remote builder. Both x86-64 and GNU ARM64 release binaries build, and
the full library and CLI test suites pass on each (ARM64 under QEMU). The x86-64 Nix
package also builds and runs. Physical phone tests and distribution/cache outputs
remain pending. CI is configured to build/test natively on both architectures,
including the declared Rust 1.88 minimum; those remote jobs have not run yet.

For Nix builds of a Git checkout, add new source files to Git's index so they are
included. The package source excludes recordings, sockets and local build output.

MIT. See [LICENSE](LICENSE).
