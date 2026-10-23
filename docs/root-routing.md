# User-launched isolated kernel routing tests

From a terminal in this checkout, run as your normal user:

```sh
./tests/run-root-routing.sh
```

The script prompts for sudo at launch and keeps it cached while it builds an
immutable Nix runner. Only its fixed probe is elevated. All four test executables, the importer and udev tool are
pinned store paths; neither a caller-selected command nor user output path is
passed to root. `/dev/uinput` must already exist. The probe does not load modules
or enable production routing.

The root helper temporarily installs a unique runtime udev rule matching only
`prek/test/*` physical markers, with `PREK_TEST=1`, `PREK_VIRTUAL=1` and
`LIBINPUT_IGNORE_DEVICE=1` established before device add. It reloads rules
without triggering existing devices. The retained root-owned registry and rule
directories reject symlinks and writable untrusted parents. Its directory lock
excludes another packaged isolated run; existing test metadata must be inspected
before starting. Existing production registry files/directories are preserved.

Every opened event node comes from a synthetic device created by the tests.
Receivers verify the actual marker, metadata and capabilities, then exclusively
grab the node before injecting. The backlog experiment briefly releases and
reacquires that grab while its sole test producer is idle. The fanout witness
case emits pre-grab motion only on a libinput-ignored REL-only source with no
key/switch capability. No test writes a key while its node is ungrabbed. No
physical input nodes are opened.

Routing and recovery receivers also require the production processed-metadata gate before
grabbing or injecting: completed initialization, a nonce-bound `PREK_PUBLICATION`
receipt and coherent protected metadata must match the actual owned device.
The full physical marker changes on each creation, while the private registry
retains one exclusive slot per source. This uses version-2 pre-add records;
legacy records are preserved and refused, not migrated or removed automatically.
See [virtual publication identities](evdev-boundary.md#virtual-publication-identities).

One additional test-only [repeat characterization](feedback.md#repeat-admission-remains-closed)
constructs an intentionally unsupported `EV_REP` keyboard. Its constructor is
compiled only into the native library test binary, not the application. It uses
the same nonce-bound private publication and protected completion parser, checks
its fixed keyboard identity/capabilities and grabs its own node before injecting.
It observes duplicate software repeats and reverse setting requests; it does not
implement or establish repeat passthrough. The production repeat gate stays closed.

A second test-only [fuzz characterization](evdev-boundary.md#nonzero-fuzz-is-not-an-invertible-transport)
checks re-filtering, skipped slot state and a zero-fuzz control on isolated
two-slot devices. The unsupported constructor exists only in the test binary;
it cannot enable production fuzz routing. Every owned node must pass processed
metadata/ABI verification and be grabbed before injection. This case requires
an actual user-launched run; building its binary is not execution evidence.

A generated-keyboard gate uses the real key output driver, role-separated native
publication and readiness preparation. Its own isolated evdev receiver is grabbed
before any key is emitted. It checks overlapping remap owners, modifier chords,
value-2 repeat delivery, cancel-before-write, sibling/context cleanup, stale press
rejection, final kernel key neutrality and owned-device removal. It opens no
physical input and does not prove partial native writes, compositor delivery,
worker integration or broker authorization. This test is compiled but has not
been executed by the agent; launch the same root script below to obtain evidence.

The managed-worker gate adds a separate immutable executable built with the
nondefault `native-test-harness` feature. Its exec-thread child uses the actual
Supervisor/Heartbeat identity, private Stream, native managed Slot, canonical
Routing projection and raw/generated uinput outputs. A second authenticated
fixture socket sequences test input; ordinary subscription/transport replies use
the production private control protocol. The parent owns only named test producers
and downstream receivers; only the child holds source grabs and output writer fds.
Producer and modeled-original source IDs are disjoint, and output metadata must
name the exact supervising worker's PID, birth and epoch.

Eight logged kernel phases check software down/repeat/up, overlapping producers,
sibling survival, idle lease expiry, mixed down/repeat/unrelated-key/up projection,
a generated modifier before a second source's raw report, software retirement
while a physical remap remains held, then capability revocation, consumed repeat/up
draining and neutral ordinary reentry. Each
`PREK_MANAGED_KEY_WORKER_PHASE` line retains exact key edges and monotonic kernel
report timestamps. Cross-output timestamp ties do not count as ordering evidence.
Twenty-one `PREK_MANAGED_KEY_WORKER_SOFTWARE` admission records retain the exact
private request and correlated bounded response clocks, including five required
rejections: wrong consumer, physical-source alias, closed producer reuse, expired
renewal and expired producer reuse. A fresh context/transport cannot revive an
expired producer. No source frames or software requests drive the idle expiry;
ordinary context/transport renewal keeps the independent seat authority live.
Normal emission is settled before producer cancellation; admission alone is not
an execution acknowledgement. This tests private control, not public software
keyboard authentication or a Lua input service.
Success also requires neutral receivers, output removal, actual source closure
acknowledgements and a successful reaped worker without leftover owned metadata.
Failure cleanup never removes unrelated registry entries.

The generated writer remains alive after its final local release. The receiver
drains complete kernel reports to `EAGAIN`, reads live key state, then replies to
the worker's exact private fence/publication/consumer/nonce challenge. The shared
250 ms deadline includes both local writes and consumer waiting. A separate
`PREK_MANAGED_KEY_WORKER_DRAIN` record correlates the final consumed key-up,
actual neutral sample and subsequent ordinary reentry. No removed-device ioctl
or fixture-owned writer duplicate substitutes for that acknowledgement.

This is an explicitly modeled desktop context and isolated source normalization,
not production discovery, the production reader/cutover loop or broker grant.
It does not prove stock compositor delivery, consumer retirement, native partial
writes or injected worker-failure recovery. The production consumer boundary and
public synthetic-input grant remain closed. Building/inventory checking this
fixture is not a kernel execution result; its body is user-launched only.

Fourteen routing gates cover keyboard/mouse/touchpad forwarding, keyboard LED feedback,
including the actual bounded source-write bridge, intact partial hardware frames,
queued key releases and recovery after a wrapped uinput feedback ring,
configured touch consumption/fallback and cadence, reader error/panic teardown,
four evdev boundary experiments and sole-reader prefix/cutover/tail ordering.
Desktop context in configured routing tests
is explicitly modeled. The tests do not establish stock-compositor retirement,
physical takeover, live broker admission or phone acceptance.

Eight additional recovery gates hold a keyboard chord or touch contact while
requesting orderly shutdown, killing the worker, killing its supervisor, or
stopping worker progress. They check that the virtual output disappears and its
retained event descriptor becomes unusable, the original synthetic source grab
is released, and the source's actual held state remains intact until its producer
releases it. Orderly shutdown must deliver neutral state before output retirement.
Crash recovery removes only metadata matching that worker's PID, birth and epoch;
unrelated live and dead worker lifetimes are preserved. These are kernel/device
gates, not compositor-consumer retirement or physical keyboard/touch acceptance.

Recovery processes exchange bounded control records in a private root-owned
directory with an exact device/inode binding. Each case uses an exclusive child
directory, retained directory descriptors and no-follow opens; cleanup validates
every observed file before unlinking. Unknown files and replacements are retained
and fail the run. Process signals use pinned handles rather than unverified PIDs.
The runner compares the unrelated registry entries before and after all cases.

The boundary experiments compare input queued before and after a grab with a
separate state probe, and inspect a clock change with pending data. Linux
v6.18's [`evdev_set_clk_type`](https://github.com/torvalds/linux/blob/v6.18/drivers/input/evdev.c)
flushes a nonempty client queue and inserts `SYN_DROPPED`. This requires loss
reconciliation; it is not being adopted as a production admission shortcut.
The user-run result will determine whether the expected behavior holds on the
actual host kernel. The production post-grab backlog gate stays closed.

The [fanout-prefix witness](evdev-boundary.md) cases also retain concurrent
post-grab motion and a held post-grab key through a separate state-query client.
They challenge the source-derived prefix classifier on the actual kernel; they
do not test console input-handler cutover or complete consumer/neutral/error
handoff. The private routing worker still does not opt into the cutover callback.
The additional sole-reader case uses the real Linux loop on named REL-only
sources, with an explicit model sink rather than live production admission. It
requires one observed prefix, one boundary at its delivered input clock, one
downstream tail and virtual-output teardown before the original grab releases.

The root helper captures test output on errors, interruption and its 90-second
timeout, kills its owned process group and removes only the original rule inode.
Parent-death handling also stops the test child if its root supervisor dies.
It reloads rules after removing its temporary rule, and removes only empty
directories it created. Unexpected replacements or remaining test metadata fail
the run and are retained for inspection.

Logs belong to your regular user under `target/root-routing/run-*`; `latest`
points to that attempt. `console.log` records build, wrapper and cleanup output;
`kernel.log` retains root probe stdout. `status.json` records running/passed/failed
and the final exit code. `receipt.json` is written only after checking one complete
version-9 root receipt, the exact fourteen routing, eight recovery, two transport
characterization, one generated-keyboard and one managed-worker inventory, all twenty-six tests actually executed and passed,
cleanup flags and the explicit scope limits. The recovery summary must match its
eight per-case results. The repeat case must supply its complete typed observation
record and exactly one passing test result; the receipt explicitly retains
`repeat_passthrough_tested=false`. The fuzz case likewise requires its complete
typed observation record and exactly one passing result, while retaining
`fuzz_passthrough_tested=false`. The generated-keyboard case requires a complete
typed record and one passing result, retaining false authorization, compositor,
worker-binding and partial-native-write scope flags. The separate managed-worker
record requires actual worker binding, nine correlated typed kernel phases,
all twenty-one software admission records and a complete hybrid report receipt.
The latter correlates the actual grabbed keyboard/relative source, one counterpart
event node and exact observed kernel capabilities, generated remap/repeat/release
ordering, an indivisible relative tail and unmatched whole-report fallback within
the original 20 ms scheduler slack. Its input clocks come from the worker's real
source reader, not requested injection timestamps. No native hybrid MT or
compositor delivery is claimed. This is still a user-run acceptance gate: a
build or parser-model pass does not establish the new kernel behavior.
The managed record retains false public-grant, production-loop and compositor scope flags.
Older version-2/3/4/5/6/7/8 receipts
are not current acceptance.
Skips and zero-test runs
cannot pass. Tell the agent when the script finishes so it can inspect these files.

`oslo make test-root-routing` runs the unprivileged parser, actual owned-process
and temporary-directory safety tests plus explicitly modeled privileged
orchestration. It does not execute the twenty-six kernel cases or establish root
acceptance. The Nix `root-routing-receipts` check also lints the launchers.
`oslo make build-root-routing` builds the immutable runner without launching it.
`oslo make test-native-managed-model` runs pure fixture models, compiles the
custom harness and checks its inventory without invoking the kernel body.

The separate `tests/run-root-handoff.sh` remains the native Morf/root/logind
probe. Ordinary builds and Cage runs execute neither root script automatically.
