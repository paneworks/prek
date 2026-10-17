# Local Cage acceptance

Run each recipe from the checkout through `oslo make`. The flake supplies the
tools; no global installation, VM or root session is needed. The recipes use one
Nix build job and two build cores. `VISIBLE=0` keeps the default owned headless
session explicit; `RENDER_NODE` can select an accessible render node.

```sh
VISIBLE=0 oslo make test-cage-stock
VISIBLE=0 oslo make test-cage-lua-stock
VISIBLE=0 oslo make test-cage-wayland-fix
VISIBLE=0 oslo make test-cage-lua-wayland-fix
VISIBLE=0 oslo make test-cage-touch
oslo make test-cage-startup
```

## Fixture boundaries

| Recipe | Compositor dependencies | Config and checks |
| --- | --- | --- |
| `test-cage-stock` | Pinned Cage 0.3.1, Hyprland 0.56.2, stock Aquamarine 0.15.1 | Legacy compositor config, layer edits, input, power and frozen-target window effects |
| `test-cage-lua-stock` | Same stock dependencies | Actual Lua compositor config, mode edits and the same effects |
| `test-cage-wayland-fix` | Explicit test-only Aquamarine startup patch | Separate legacy-config mode/layer sessions and the same effects |
| `test-cage-lua-wayland-fix` | Same explicit test-only patch | Separate Lua-config mode/layer sessions and the same effects |
| `test-cage-touch` | Test-only Cage touch source | Two contacts, coordinates, motion, ordered frames and neutral releases |
| `test-cage-startup` | No compositor started | Readiness, config, rendered-state and library-provenance models |

The original `test-cage` keeps its older pinned Hyprland 0.54.3 dependencies.
None of these recipes opens physical evdev devices or proves production broker
authority, kernel routing, physical display behavior or phone acceptance.
Application delivery uses prepared Wayland input inside the private compositor;
the routing/action fixture grants are explicitly modeled.

Each run creates private HOME/XDG/runtime directories and a non-activating
session bus. Parent input is masked by the existing version-bridging Wayland
proxy. Focus, output power, workspace moves and close target only the owned
compositor. Its processes are reaped after success or failure.

## Provenance and outcomes

The stock wrapper selects no replacement library. It declares the expected
stock package and checks the actual compositor's `/proc/PID/maps`. A missing,
foreign, patched, mixed, deleted or prefix-only library match fails; a stock and
test-only claim cannot be combined. The patched wrapper still verifies its
separately declared override. Version strings alone do not establish provenance.

Receipts live under `target/cage-host/run-*`:

- `aquamarine.json` and `hyprland-libraries.txt`: exact declared and loaded
  libraries, captured before output readiness so failed startups retain proof.
- `hyprland-version.json` and `hyprland-process.json`: actual IPC version/commit
  and owned compositor PID/executable, also captured before output readiness.
- `cage.log`, `hyprland.log`, `hyprland-runtime.log` and `proxy.log`: startup,
  renderer, protocol and action diagnostics.
- `nested.json`: usable owned socket and compositor identity after startup.
- `receipts.jsonl` and `receiver-*.png`: application state and actual compositor
  captures after the submitted-state frame watermark is acknowledged.
- `hyprland-status.json`: independent actual provider confirmation for either
  Hyprland 0.56.2 config, captured before output readiness.
- `power-receipts.json` and `hyprland-receipts.json`: acknowledged effects and
  observed output/window state. Missing files do not imply success.

On the NixOS/NVIDIA test host on 2026-10-17, both modern stock recipes in headless
mode fail before receiver startup: nested Wayland output remains unavailable and the fallback
headless output cannot allocate GBM buffers. This is an **open acceptance gate**;
the recipes return nonzero and never silently substitute the patched fixture.
The patched recipes separately pass local rendered/action checks, not stock
acceptance. Driver/backend changes require fresh evidence rather than assuming
this host-specific result applies everywhere.

A bounded protocol-ping experiment reached initial stock output/receiver
startup but stalled before acknowledging later rendered input. That workaround
was removed. The shipped proxy has no injected heartbeat, and the rendering
watermark/timeout checks remain enabled. Neither initial configuration, a first
frame callback nor key receipts alone prove continued rendered delivery.

## Visible variant

From an existing Wayland desktop terminal, `VISIBLE=1 oslo make test-cage-stock`
uses an owned nested Hyprland window through the private proxy instead of outer
headless Cage. The caller must supply `WAYLAND_DISPLAY`; the runner does not
guess another desktop socket. Parent interaction is still masked, and test
focus, workspace and power operations target the private compositor only.
Headless results do not establish this different backend's result.
