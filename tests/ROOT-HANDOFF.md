# User-run native Morf handoff check

From your active local Wayland terminal or logged-in TTY on Tron:

```sh
./tests/run-root-handoff.sh --local-wayland-fix
```

Do not prefix the script with sudo. It builds unprivileged, checks library
compatibility and the actual local logind session, then requests your password.
Only the fixed native probe runs as root. The
compositors and actual Morf ELF run as your regular user, on private sockets.
The script never installs packages or opens evdev/uinput devices.

SSH alone is insufficient: private headless Cage does not create an active
local login seat. The preflight rejects remote, seatless or inactive sessions
before sudo or compositor startup. It does not switch sessions or touch the
host greeter. An active local TTY works without an existing Wayland desktop.

The fixed probe tries host system library paths before its packaged Nix
fallback. Each candidate must pass the unchanged root-ownership, protected-mode
and ABI checks in `Logind::open`. A user-owned Nix store is not root-trusted.
The preflight uses the same test ELF and library policy as the privileged probe;
successful loading or login inspection alone is not handoff acceptance.

The local variant uses Cage0.3.1/Hyprland0.56.2 with the **explicit test-only
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
