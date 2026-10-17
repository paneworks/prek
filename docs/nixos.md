# NixOS setup

## Local development and Cage tests

The flake supplies Rust, Python and ShellCheck; no Ubuntu packages or global
Cage installation are needed. From this checkout:

```sh
mkdir -p target/cargo-home
export CARGO_HOME="$PWD/target/cargo-home"
export CARGO_BUILD_JOBS=2 RUST_TEST_THREADS=2
nix develop --no-write-lock-file
oslo make verify
oslo make test-cage-touch
oslo make test-cage-wayland-fix
```

The Cage recipes use the NixOS graphics driver directly, with one Nix build job
and two build cores. `hardware.graphics.enable` must already be configured;
`RENDER_NODE` overrides `/dev/dri/renderD128`. They create private headless
compositors, homes, sockets and session buses, not a VM or a host login. Receipts
are under `target/cage-touch/run-*` and `target/cage-host/run-*`. The touch fixture
is a test-only Cage patch; the ordinary keyboard fixture uses stock dependencies.
Neither opens physical input devices or enables production routing.

`test-cage-wayland-fix` explicitly selects the patched Aquamarine fixture
described in [the compositor notes](ubuntu-compositors.md#explicit-nested-wayland-test-variant).
It passed the keyboard, Lua, mode-change and DPMS checks on NixOS with NVIDIA
595.71.05. Its results are not unmodified stock compositor acceptance.
The recipe runs separate mode/layer sessions and exercises frozen-target window
moves, workspace follow and close with another owned window focused. The receiver uses a submitted-state sequence
in each frame callback; keyboard batches and each touch frame must reach that
watermark. The nested test config disables animations so its screencopy is not
an intermediate opening frame. Frame callbacks establish compositor pacing, not
physical display presentation; the keyboard fixture separately saves actual
compositor pixels in `receiver-baseline.png`, `receiver-routed-keys.png` and
`receiver-changed-keys.png`. Exact Hyprland 0.56.2 window/power effects require a
verified hyprlang config provider; Lua/unknown providers refuse before mutation.
`oslo make test-cage` retains the original unpatched Hyprland 0.54.3 fixture and
adds window-action checks. On this NVIDIA setup it timed out waiting for usable
nested output dimensions; the runner does not silently substitute the patch.

Only the user launches privileged tests, as a regular user without a sudo
prefix: `./tests/run-root-routing.sh` for isolated synthetic kernel devices, or
`./tests/run-root-handoff.sh --remote-session` for the bounded native PAM/logind
handoff. The latter requires a separately built Morf ELF and shell; it refuses
an occupied local user session. NixOS skips nixGL/nixVulkan wrappers and the
root-session coordinator uses immutable Nix paths for systemd and console tools.
Logs remain in `target/root-routing/latest` or `target/root-handoff/latest`.
Do not launch the remote handoff from an active desktop: it can temporarily stop
the display manager, with an owned cleanup guard for restoration.

The driver setup is described in the [NixOS graphics documentation](https://wiki.nixos.org/wiki/Graphics).

## System service

The upstream module currently provides supervised **observation**, approved Lua
callbacks, permitted normalized input, and acknowledged rule/layer/mode edits.
Production input routing and physical phone takeover remain unavailable. Enabling
the module with its defaults selects no devices and declares no rules.

Import the module from the same flake revision as the package:

```nix
{
  inputs.prek.url = "path:/absolute/path/to/prek";

  outputs = { self, nixpkgs, prek, ... }: {
    nixosConfigurations.my-machine = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux"; # or aarch64-linux
      modules = [
        prek.nixosModules.default
        ./configuration.nix
        {
          services.prek = {
            enable = true;
            luaConfig = ''
              local p = require("prek")
              return p.config.new {
                policy = {allowed_devices={"keyboard"}},
                devices = {keyboard={name="YOUR EXACT KEYBOARD NAME"}},
                rules = {
                  p.rules.define {
                    id="a", trigger=p.triggers.key{key="KEY_A"},
                    action=p.actions.event{name="a"},
                  },
                },
              }
            '';
          };
        }
      ];
    };
  };
}
```

The example uses your local checkout. Once a repository/revision has been
published, use that repository's normal input/update flow.
`services.prek.package` defaults to the imported Prek flake's
package for the host architecture; it can be overridden with another tested
package. The broker and session launcher always use that same executable.

`compositorPackage` defaults to `config.programs.hyprland.package`. It must be the
package that runs the session. The build checks the actual ELF, selecting
`.Hyprland-wrapped` for the standard Nix wrapper layout; a nonstandard layout can
set `compositorExecutable` explicitly. `logindLibrary` defaults to the NixOS
systemd package's absolute `libsystemd.so.0`. Set `morfExecutable` to Morf's actual
ELF when native lock-provider verification is required.

For reusable Lua modules, `luaModules."bindings.keyboard" = ''...''` produces
`modules/bindings/keyboard.lua` beside the entry file. Module names follow the
same bounded dotted-identifier contract as the runtime. Alternatively set
`configFile = ./prek/config.lua`; its sibling `modules/` directory is copied with
it. File-based and inline configuration cannot be combined. Source files are
copied as regular files into an immutable tree and checked with the selected
`prek check` during the build. A route-mode configuration is rejected. Lua still
compiles in Prek's disposable unprivileged compiler when the root broker starts.

The generated tree is available as `services.prek.configurationPackage`, with
`config.lua`, `modules/` and `validation.json`. The service reads its store path
directly so module ownership and no-symlink checks retain their normal meaning.
Changing the configuration restarts the service onto the new checked tree.
For configuration or executable replacement, restart the session agent as well;
the broker authenticates the exact agent executable.

## Start the owning session agent

The module installs `prek-session`. Run it **from the compositor's own login
session**, for example in the Hyprland configuration:

```text
exec-once = /run/current-system/sw/bin/prek-session
```

For a Lua-configured compositor, use its equivalent existing startup hook.
The launcher directly execs the installed session agent with observation,
verified context and Lua enabled. It inherits `XDG_RUNTIME_DIR` and
`HYPRLAND_INSTANCE_SIGNATURE` from that session. It does not create a systemd
user service: the shared user manager does not prove owning logind membership,
and its agents are deliberately rejected. A compositor run without an active
logind user session also cannot establish production registration. Local Cage
acceptance uses explicit model seat authority and separate owned sockets.

`services.prek.session.input = true` adds `--input`; the protected Lua policy
must also set `allow_input_events=true`. `session.ruleMutations = true` adds
`--rule-mutations`, requiring `allow_runtime_rules=true`.
`session.recognizers = true` adds `--recognizers` for source-bound custom
callbacks under the same protected input permission. All three options require
`session.lua=true`; they default false. These options cannot widen protected
source policy. `session.enable=false` omits the launcher from the system profile
while retaining the broker, useful when an existing session hook starts the
selected `prek` executable directly. `session.package` exposes the generated
launcher to other upstream modules.

The `prek.service` root broker starts at `multi-user.target`, after logind and
udev. Its supervised worker alone opens allowlisted input devices. The unit
permits read access to input event devices, creates `/run/prek`, and stops the
whole process group on shutdown. It retains process visibility for cross-UID
PID/executable verification and permission to drop compiler credentials. The
module installs Prek's udev metadata rule but does not enable uinput, grant users
input-group membership, or activate routing. `runtimeDirectory` changes the
single directory name under `/run` and the launcher's socket together.

## Check, stop and recover

Build the system through its usual configuration workflow before activation.
Invalid Lua/modules, incompatible configuration choices and route mode fail
before this module can activate a replacement service.

```sh
systemctl status prek.service
journalctl -u prek.service -b
prek status --json             # from the verified compositor login session
sudo systemctl stop prek.service
sudo systemctl start prek.service
```

Stopping the broker retires its worker and registration; a session agent should
then be restarted from the compositor's own startup hook. Do not launch a second
agent while the first remains registered. Observation does not consume input,
so stopping it preserves ordinary application delivery. Physical routing,
failure-neutralization and rollback acceptance remain separate implementation
and hardware gates.

Installed references, editor definitions, examples and three Lua presets are
under `/run/current-system/sw/share/prek` and `share/doc/prek`. Copy a preset into
your configuration before editing; Nix store files are immutable. The phone
preset declares route mode and is a reference for pending phone integration,
not an observation configuration to enable unchanged.

To import the module from an already built package instead of the flake, import
`"${prekPackage}/share/prek/nixos/module.nix"` and set
`services.prek.package = prekPackage`. The local verification command is
`nix build .#checks.x86_64-linux.nixos-module` (use `aarch64-linux` on native ARM).
It evaluates module variants, checks generated Lua and installed launchers, and
verifies unit syntax without starting a service, creating a VM, or opening input.
