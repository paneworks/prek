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
