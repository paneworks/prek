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
oslo make test-cage-lua-wayland-fix
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
`receiver-changed-keys.png`. Exact Hyprland 0.56.2 window/power effects freeze and
verify the hyprlang or Lua provider; unknown/changed providers refuse before
mutation. The Lua recipe starts `hyprland.lua`, verifies actual `j/status`, and
uses typed Lua dispatch for both setup focus and the granted application effects.
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
`prek check` during the build, which also requires the configuration's mode to
match `services.prek.mode`. Lua still compiles in Prek's disposable unprivileged
compiler when the root broker starts.

`mode = "route"` takes over the configuration's allowed devices consumed by stock
Hyprland at a neutral idle cut, from boot, so it also requires
`allowPhysicalTakeover = true`. It widens the unit to `char-input rw` (LED
feedback), `/dev/uinput rw` and a writable `/run/prek` device registry. Try one
device with `tests/run-takeover-trial.sh` before enabling it here.

The generated tree is available as `services.prek.configurationPackage`, with
`config.lua`, `modules/` and `validation.json`. The service reads its store path
directly so module ownership and no-symlink checks retain their normal meaning.
Changing the configuration restarts the service onto the new checked tree.
For configuration or executable replacement, restart the session agent as well;
the broker authenticates the exact agent executable.
Confirmed suspend-clock discontinuities retire live process authority; see
[the clock epoch and recovery limits](suspend-epoch.md).

## Start the owning session agent

The module installs `prek-session`. Run it **from the compositor's own login
session**, for example in the Hyprland configuration:

```text
exec-once = /run/current-system/sw/bin/prek-session
```

For a Lua-configured compositor, use its equivalent existing startup hook.
The launcher starts `prek session --reconnect` with observation,
verified context and Lua enabled. An authority-free guardian re-executes retired
agents in that same login session; every replacement registers afresh. It inherits `XDG_RUNTIME_DIR` and
`HYPRLAND_INSTANCE_SIGNATURE` from that session. It does not create a systemd
user service: the shared user manager does not prove owning logind membership,
and its agents are deliberately rejected.

With uwsm (`programs.hyprland.withUWSM`), Hyprland itself runs as a user service
outside any login session. The broker then binds it to your active local display
session only while logind records that exact Hyprland process as the session's
controller, and requires the agent to share Hyprland's systemd unit. A startup
hook does that; do not wrap `prek-session` in `uwsm app`, which moves it to its
own unit. Any other compositor without an active logind user session cannot
establish production registration. Local Cage
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
The reconnecting launcher also passes the protected `logindLibrary` to its
read-only sleep watcher. Broker pre-sleep coordination and the optional
user-launched delay-only probe are described in
[the sleep contract](suspend-epoch.md#authoritative-logind-preparation).
`session.reconnect=false` restores one-shot foreground behavior. Reconnection
does not retry policy rejection, malformed protocol, orderly exit or an external
agent signal/crash. Broker transport loss and a confirmed retired clock epoch
are retryable, with a five-second startup deadline, 500 ms retry delay, at most
16 launches in 60 seconds and a 60-second unregistered-outage budget. SIGINT or
SIGTERM stops the guardian and reaps its child; abrupt guardian death kills that
child. No old Lua VM, pending action, fence or source generation is reused.

The `prek.service` root broker starts at `multi-user.target`, after logind and
udev. Its supervised worker alone opens allowlisted input devices. The unit
permits read access to input event devices, creates `/run/prek`, and stops the
whole process group on shutdown. It retains process visibility for cross-UID
PID/executable verification and permission to drop compiler credentials. The
module installs Prek's udev metadata rule but does not enable uinput, grant users
input-group membership, or activate routing. `runtimeDirectory` changes the
single directory name under `/run` and the launcher's socket together.
Managed device discovery binds metadata to the opened event descriptor and
requires a stable initialized udev publication. See the
[source re-entry contract](evdev-boundary.md#discovery-and-source-re-entry) for
replacement handling and remaining hardware/consumer gates.

## Route stock-Hyprland devices

1. Find the device's exact kernel name with `grep '^N: Name=' /proc/bus/input/devices`.
   Interfaces that share one name, such as a KVM's keyboard and mouse, are
   selected together.
2. Try it for one minute from any terminal while Hyprland runs (the trial starts
   its agent through Hyprland), then read `target/takeover-trial/latest/broker.log`:

   ```sh
   ./tests/run-takeover-trial.sh --device "<name>" --seconds 60
   ```

3. Enable it:

   ```nix
   services.prek = {
     enable = true;
     mode = "route";
     allowPhysicalTakeover = true;
     luaConfig = ''
       local p = require("prek")
       return p.config.new {
         mode = "route",
         policy = {allowed_devices={"keyboard"}},
         devices = {keyboard={name="<name>",hyprland={repeat_rate=35,repeat_delay=300}}},
       }
     '';
   };
   ```

After you log in and `prek-session` registers, the broker mirrors each allowed
device, copies the device's visible Hyprland settings and the declared
`hyprland` options to the mirror, and takes the device over once it has been
neutral and idle for 100 ms. `journalctl -u prek.service -b | grep -E 'taken
over|stays with'` shows each outcome.

Only stock Hyprland is a supported consumer. Console keyboards, devices with
nonzero axis fuzz (most touchpads) and unclassified absolute pointers stay with
Hyprland. A runtime rule change briefly releases routed devices until each is
neutral and idle again, and settings Hyprland cannot show are trusted from its
write reply. To roll back, `sudo systemctl stop prek.service` releases every
grab and removes the mirrors; then set `mode = "observe"` or `enable = false`.

## Check, stop and recover

Build the system through its usual configuration workflow before activation.
Invalid Lua/modules, incompatible configuration choices, a mode mismatch and an
unconfirmed route mode fail before this module can activate a replacement service.

```sh
systemctl status prek.service
journalctl -u prek.service -b
prek status --json             # from the verified compositor login session
sudo systemctl stop prek.service
sudo systemctl start prek.service
```

Stopping the broker retires its worker and registration. The reconnecting
launcher waits within its bounded recovery budget; stopping only the broker is
not an agent-stop command. Stop the owning `prek-session` process as well when
shutting down the integration. One-shot agents must be restarted from the
compositor's own startup hook. Do not launch a second agent while the first remains registered. Observation does not consume input,
so stopping it preserves ordinary application delivery. Stopping a route broker
releases its grabs and destroys its mirrors, returning each device to Hyprland;
rollback on real hardware has not been accepted yet.

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
