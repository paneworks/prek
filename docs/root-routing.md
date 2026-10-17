# User-launched isolated kernel routing tests

From a terminal in this checkout, run as your normal user:

```sh
./tests/run-root-routing.sh
```

The script first builds an immutable Nix runner, then prompts for sudo. Only
its fixed probe is elevated. Both test executables, the importer and udev tool are
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

Thirteen routing gates cover keyboard/mouse/touchpad forwarding, keyboard LED feedback,
configured touch consumption/fallback and cadence, reader error/panic teardown,
and four evdev boundary experiments. Desktop context in configured routing tests
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
handoff. The production reader still does not use this component.

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
version-2 root receipt, the exact thirteen routing and eight recovery inventories,
all twenty-one tests actually executed and passed, cleanup flags and the explicit
scope limits. The recovery summary must match its eight per-case results.
Skips and zero-test runs
cannot pass. Tell the agent when the script finishes so it can inspect these files.

`oslo make test-root-routing` runs the unprivileged parser, actual owned-process
and temporary-directory safety tests plus explicitly modeled privileged
orchestration. It does not execute the twenty-one kernel cases or establish root
acceptance. The Nix `root-routing-receipts` check also lints the launchers.

The separate `tests/run-root-handoff.sh` remains the native Morf/root/logind
probe. Ordinary builds and Cage runs execute neither root script automatically.
