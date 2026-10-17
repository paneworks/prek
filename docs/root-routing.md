# User-launched isolated kernel routing tests

From a terminal in this checkout, run as your normal user:

```sh
./tests/run-root-routing.sh
```

The script first builds an immutable Nix runner, then prompts for sudo. Only
its fixed probe is elevated. All three test executables, the importer and udev tool are
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
version-3 root receipt, the exact fourteen routing, eight recovery and one repeat
characterization inventories, all twenty-three tests actually executed and passed,
cleanup flags and the explicit scope limits. The recovery summary must match its
eight per-case results. The repeat case must supply its complete typed observation
record and exactly one passing test result; the receipt explicitly retains
`repeat_passthrough_tested=false`. Older version-2 receipts are not current acceptance.
Skips and zero-test runs
cannot pass. Tell the agent when the script finishes so it can inspect these files.

`oslo make test-root-routing` runs the unprivileged parser, actual owned-process
and temporary-directory safety tests plus explicitly modeled privileged
orchestration. It does not execute the twenty-three kernel cases or establish root
acceptance. The Nix `root-routing-receipts` check also lints the launchers.
`oslo make build-root-routing` builds the immutable runner without launching it.

The separate `tests/run-root-handoff.sh` remains the native Morf/root/logind
probe. Ordinary builds and Cage runs execute neither root script automatically.
