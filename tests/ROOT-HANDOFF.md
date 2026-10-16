# User-run native Morf handoff check

## SSH-only Tron

```sh
./tests/run-root-handoff.sh --remote-session
```

Run as your regular user, without a sudo prefix. The script builds first, then
asks for your password. This explicit mode temporarily stops the display manager,
creates a genuine `PAMName=login` user session on unused `tty9`, activates that
session through logind, and runs the private headless Cage fixture there. No
physical console access, VM, autologin configuration or fake logind facts are used.

The root coordinator accepts only the fixed native probe's eight bindings from
the launch user's actual owned session scope. Kernel socket credentials and a
peer pidfd bind each caller. The unprivileged fixture verifies that the server
is root. No sudoers rule or password forwarding to the new TTY is needed.

A separate systemd `ExecStopPost` cleanup removes the owned session, restores
the original VT and starts the display manager again only if it was initially
active. The controller has a four-minute runtime bound and a two-minute cleanup
bound. Cleanup also runs after controller failure; SSH sessions are never
terminated. Another local user session or an allocated `tty9` causes a refusal
before stopping the display manager. The script never runs `pkill` or stops SSH.

The console log prints the unique `prek-handoff-<id>.service` guard name. To stop
a running attempt explicitly, run `sudo systemctl stop` with that exact name;
the cleanup hook still runs. A disconnected SSH terminal does not cancel the
systemd timeout. Root-owned audit state remains under the printed `/run` path;
the privileged socket and exclusive reservation are removed on successful cleanup.
The `PREK_ROOT_SESSION_RECEIPT` line reports host cleanup, not native acceptance.
Cleanup failure retains its reservation and reports failure instead of permitting
another overlapping run.

If the cleanup log reports that the display manager could not restart, the
SSH-side recovery command is `sudo systemctl start display-manager.service`.
Keep the failed run's logs and reservation for diagnosis; do not remove the
reservation or overlap attempts while its printed guard unit is still running.

## Existing local login

From your active local Wayland terminal or logged-in TTY on Tron:

```sh
./tests/run-root-handoff.sh --local-wayland-fix
```

Do not prefix the script with sudo. It builds unprivileged, checks library
compatibility and the actual local logind session, then requests your password.
Only the fixed native probe runs as root. The
compositors and actual Morf ELF run as your regular user, on private sockets.
The script never installs packages or opens evdev/uinput devices.

Without `--remote-session`, SSH alone is insufficient: private headless Cage does not create an active
local login seat. The preflight rejects remote, seatless or inactive sessions
before sudo or compositor startup. It does not switch sessions or touch the
host greeter. An active local TTY works without an existing Wayland desktop.

The fixed probe tries host system library paths before its packaged Nix
fallback. Each candidate must pass the unchanged root-ownership, protected-mode
and ABI checks in `Logind::open`. A user-owned Nix store is not root-trusted.
The preflight uses the same test ELF and library policy as the privileged probe;
successful loading or login inspection alone is not handoff acceptance.

Both headless variants use Cage0.3.1/Hyprland0.56.2 with the **explicit test-only
Aquamarine startup patch**, not an unmodified stock compositor result. Morf's
development shell supplies runtime libraries and matching nixGL/nixVulkan
wrappers. The loaded NVIDIA kernel version is detected read-only; an explicit
`NVIDIA_VERSION` takes precedence. The older `--windows` adapter is not used.

The default Morf checkout is `../morf`, with a legacy `../mold` fallback if that
directory is absent. `target/release/morf` takes precedence over named legacy
verified artifacts. Override `PREK_MORF_REPO`, `PREK_MORF_BINARY` and
`PREK_MORF_SHELL` for other paths. The binary must be the actual ELF, not a
wrapper. No source is built or edited in Morf by this script.

With no arguments the original visible desktop fixture remains available; it
requires your own Wayland desktop and retains its older unpatched dependencies.

Logs and final status:

```text
target/root-handoff/latest/console.log
target/root-handoff/latest/status.json
target/root-handoff/latest/cage/run-*/morf-native-root.log
target/root-handoff/latest/cage/run-*/morf-native-root-receipts.json
```

A skipped native test, missing receipt, wrong process/session binding or failed
restoration is not a pass. A successful run proves only the receipt's declared
native handoff/control scope, not physical input takeover, real authentication,
phone readiness or native ARM execution. The unprivileged fixture also retains
its actual private-output screenshots and loaded test-library evidence.
