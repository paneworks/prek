# Latest Cage and Hyprland on Ubuntu

From this checkout, as the regular desktop user:

```sh
./tests/install-ubuntu-compositors.sh --check
./tests/install-ubuntu-compositors.sh
```

The installer resolves **latest stable**, not development HEAD, through the
official GitHub releases API. It selects a fixed Nixpkgs unstable commit and
checks Cage's version against upstream. Hyprland uses the matching Nixpkgs
package or the pinned upstream release flake when Nixpkgs is behind. A stale
Cage package fails explicitly rather than silently installing an older version.
The manifest records the selected releases, source references and store paths.

Nix, curl and jq must already be installed. Builds run unprivileged, with one
Nix job and two cores by default (`PREK_BUILD_CORES=1..8` overrides cores).
Only installation prompts for sudo. Do not invoke the entire script with sudo.

Commands are exposed system-wide as `/usr/local/bin/cage`, `Hyprland`, `hyprctl`
and, when packaged, `start-hyprland`. They use a managed installation under
`/opt/prek-compositors/current`. Other existing `/usr/local/bin` commands are
never overwritten. Previous managed releases and Nix garbage-collection roots
are retained. Ubuntu's `/usr/bin/Hyprland` package remains unchanged.

The script does not launch a compositor, change the display manager or login
session, install a login-session entry, change GPU drivers, grant input-device
permissions, or run root tests. Installing these tools is not evidence that
nested rendering or physical routing works.

Logs and status are in `target/compositor-install/latest/`. After installation:

```sh
hash -r
/usr/local/bin/cage -v
/usr/local/bin/Hyprland --version
```

On non-NixOS systems hardware rendering may need nixGL; do not replace Ubuntu's
GPU drivers to make a nested test work. Software-rendered isolated tests and
native GPU use must be verified separately. See the official
[Hyprland instructions for other distributions](https://wiki.hypr.land/nix/hyprland-on-any-distro-using-nix/)
and [Cage releases](https://github.com/cage-kiosk/cage/releases).

The Cage test drivers default to software rendering. `--hardware-rendering`
stops forcing Mesa software rendering when using a matching GPU wrapper. Their
headless parent explicitly creates one output; startup requires positive output
dimensions and a rendered receiver frame before injecting test input. A mapped
surface or a successful compositor process exit alone is not a rendering pass.

## Explicit nested-Wayland test variant

`cage-test` keeps its original pinned, unpatched compositor dependencies. The
separate `cage-test-wayland-fix` runner pins Cage 0.3.1, Hyprland 0.56.2 and an
**explicitly patched Aquamarine 0.15.1**. It announces newly created Wayland
outputs on the core idle queue only after acknowledging their first real
`xdg_surface.configure`, and flushes the initial parent connection requests.
This preserves Wayland's configure-before-buffer ordering without startup deadlock.
Retired backends and outputs are excluded from the callback.

```sh
nix build .#cage-test-wayland-fix --out-link target/cage-wayland-fix
target/cage-wayland-fix/bin/prek-cage-host-test --modes --input --power
```

On NixOS, run `oslo make test-cage-wayland-fix` directly with the system graphics
driver; no nixGL wrapper is needed. For NVIDIA hardware on other distributions,
launch that command through a matching nixGL wrapper and add
`--hardware-rendering`. This runner changes only its process-local library
search path; it never installs or replaces a compositor. It prints the test-only
warning and verifies the actual loaded Aquamarine paths from its owned
Hyprland's `/proc/PID/maps`. Each run retains `nested.json` and
`hyprland-libraries.txt`. A missing, foreign or mixed override fails the run.
The positive-output, rendered-receiver and parent-input-isolation guards remain
enabled. A passing variant run is **not unmodified stock compositor acceptance**.
The newer variant does not support `--windows`, whose action adapter remains
pinned to Hyprland 0.54.3.

Pass `--morf /absolute/path/to/morf --morf-shell /absolute/path/to/shell/init.lua`
to exercise the real Morf lock provider and Caelestia controllers. An unwrapped
Nix-built Morf ELF also needs its own runtime libraries: invoke the runner from
Morf's Nix development shell rather than changing system libraries. The packaged
runner supplies `grim` to capture the **private owned output only** at the
dashboard and both keyboard layouts. PNGs and their dimensions, SHA-256 hashes,
output and process identities are retained alongside the controller receipts.
The receiver also has to complete a real baseline screencopy before the Prek
fixture begins; an app-side buffer submission is not compositor presentation.
Inspect those PNGs separately; IPC receipts alone do not establish visual quality,
kernel ownership, root handoff, physical touch or real authentication.
