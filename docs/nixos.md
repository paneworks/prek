# NixOS setup

## Local development

The flake supplies Rust, Python and ShellCheck; no Ubuntu packages are needed.
From this checkout:

```sh
mkdir -p target/cargo-home
export CARGO_HOME="$PWD/target/cargo-home"
export CARGO_BUILD_JOBS=2 RUST_TEST_THREADS=2
nix develop --no-write-lock-file
oslo make verify
```

Only the user launches privileged tests, as a regular user without a sudo
prefix: `./tests/run-root-routing.sh` for isolated synthetic kernel devices, or
`./tests/run-takeover-trial.sh` for one physical device in the running Wayland
session. Logs remain in `target/root-routing/latest` or
`target/takeover-trial/latest`.

## System service

The upstream module provides supervised **observation**, approved Lua callbacks,
permitted normalized input, acknowledged rule/layer/mode edits, and an opt-in
route mode described below. Enabling the module with its defaults selects no
devices and declares no rules.

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
            compositorExecutable = "/ABSOLUTE/PATH/TO/COMPOSITOR-ELF";
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

`compositorExecutable` is required: the actual Wayland compositor ELF, not a
wrapper script. For a Nix-wrapped program the ELF is usually the `.NAME-wrapped`
file beside the wrapper in the package's `bin`. The process listening on the
session's `WAYLAND_DISPLAY` socket must run this exact file. `logindLibrary`
defaults to the NixOS systemd package's absolute `libsystemd.so.0`.

For reusable Lua modules, `luaModules."bindings.keyboard" = ''...''` produces
`modules/bindings/keyboard.lua` beside the entry file. Module names follow the
same bounded dotted-identifier contract as the runtime. Alternatively set
`configFile = ./prek/config.lua`; its sibling `modules/` directory is copied with
it. File-based and inline configuration cannot be combined. Source files are
copied as regular files into an immutable tree and checked with the selected
`prek check` during the build, which also requires the configuration's mode to
match `services.prek.mode`. Lua still compiles in Prek's disposable unprivileged
compiler when the root broker starts.

`mode = "route"` takes over the configuration's allowed devices held by the session's
Wayland compositor at a neutral idle cut, from boot, so it also requires
`allowPhysicalTakeover = true`. It widens the unit to `char-input rw` (LED
feedback), `/dev/uinput rw` and a writable `/run/prek` device registry. Try one
device with `tests/run-takeover-trial.sh` before enabling it here. Set
`session.commands = true` so the session agent runs the configuration's argv
actions for the gestures Prek consumes, and `session.keys = true` so its key and
remap actions type through a generated keyboard; both require route mode.

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
session**, for example from the compositor's startup hook.
The launcher starts `prek session --reconnect` with observation,
verified context and Lua enabled. An authority-free guardian re-executes retired
agents in that same login session; every replacement registers afresh. It inherits `XDG_RUNTIME_DIR` and
`WAYLAND_DISPLAY` from that session. It does not create a systemd
user service: the shared user manager does not prove owning logind membership,
and its agents are deliberately rejected.

A compositor that runs as a systemd user service outside any login session is
bound to your active local display session only while logind records that exact
compositor process as the session's controller; the agent must then share the
compositor's systemd unit. Starting `prek-session` from the compositor's startup
hook does that; a launcher that moves it into its own unit breaks the binding.
Any other compositor without an active logind user session cannot establish
production registration.

`services.prek.session.input = true` adds `--input`; the protected Lua policy
must also set `allow_input_events=true`. `session.ruleMutations = true` adds
`--rule-mutations`, requiring `allow_runtime_rules=true`.
`session.recognizers = true` adds `--recognizers` for source-bound custom
callbacks under the same protected input permission. All three options require
`session.lua=true`; they default false. `session.shortcuts` names a user shortcut
file the agent layers over the system rules and reapplies on change, for example
`"\${XDG_CONFIG_HOME:-$HOME/.config}/prek/init.lua"`; the launcher's shell expands
it, and it requires `session.ruleMutations` (see
[user shortcut files](lua-api.md#user-shortcut-files)). These options cannot widen protected
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

## Route compositor-held devices

1. Find the device's exact kernel name with `grep '^N: Name=' /proc/bus/input/devices`.
   Interfaces that share one name, such as a KVM's keyboard and mouse, are
   selected together.
2. Try it for one minute from a terminal of your Wayland session, then read
   `target/takeover-trial/latest/broker.log`:

   ```sh
   ./tests/run-takeover-trial.sh --device "<name>" --seconds 60
   ```

3. Enable it:

   ```nix
   services.prek = {
     enable = true;
     mode = "route";
     allowPhysicalTakeover = true;
     compositorExecutable = "/ABSOLUTE/PATH/TO/COMPOSITOR-ELF";
     luaConfig = ''
       local p = require("prek")
       return p.config.new {
         mode = "route",
         policy = {allowed_devices={"keyboard"}},
         devices = {keyboard={name="<name>"}},
       }
     '';
   };
   ```

After you log in and `prek-session` registers, the broker mirrors each allowed
device and takes it over once the compositor holds the mirror's event node and
the device has been neutral and idle for 100 ms. `journalctl -u prek.service -b |
grep -E 'taken over|stays with'` shows each outcome.

Only the session's Wayland compositor is a supported consumer. Console
keyboards, devices with nonzero axis fuzz (most touchpads) and unclassified
absolute pointers stay with the compositor. A runtime rule change briefly
releases routed devices until each is neutral and idle again. To roll back,
`sudo systemctl stop prek.service` releases every grab and removes the mirrors;
then set `mode = "observe"` or `enable = false`.

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
releases its grabs and destroys its mirrors, returning each device to the compositor;
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
