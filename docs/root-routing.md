# User-launched isolated kernel routing tests

From a terminal in this checkout, run as your normal user:

```sh
./tests/run-root-routing.sh
```

The script first builds an immutable Nix runner, then prompts for sudo. Only
its fixed probe is elevated. The test executable, importer and udev tool are
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
reacquires that grab while its sole test producer is idle. No test writes a key
while that node is ungrabbed. No physical input nodes are opened.

Eleven gates cover keyboard/mouse/touchpad forwarding, keyboard LED feedback,
configured touch consumption/fallback and cadence, reader error/panic teardown,
and two evdev boundary experiments. Desktop context in configured routing tests
is explicitly modeled. The tests do not establish stock-compositor retirement,
physical takeover, live broker admission or phone acceptance.

The boundary experiments compare input queued before and after a grab with a
separate state probe, and inspect a clock change with pending data. Linux
v6.18's [`evdev_set_clk_type`](https://github.com/torvalds/linux/blob/v6.18/drivers/input/evdev.c)
flushes a nonempty client queue and inserts `SYN_DROPPED`. This requires loss
reconciliation; it is not being adopted as a production admission shortcut.
The user-run result will determine whether the expected behavior holds on the
actual host kernel. The production post-grab backlog gate stays closed.

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
root receipt, the exact eleven-test inventory, all tests actually executed and
passed, cleanup flags and the explicit scope limits. Skips and zero-test runs
cannot pass. Tell the agent when the script finishes so it can inspect these files.

The separate `tests/run-root-handoff.sh` remains the native Morf/root/logind
probe. Ordinary builds and Cage runs execute neither root script automatically.
