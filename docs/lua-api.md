# Lua configuration API

Prek embeds [onix-os/luna](https://github.com/onix-os/luna), pinned to version
0.5.1. Files ending in `.lua` return one configuration table. Legacy TOML is
also accepted. Configuration evaluation declares actions and does not execute
them. Live production routing remains disabled.

This reference covers the implemented configuration bindings. Runtime callbacks,
custom recognizers, event subscriptions, timers and live adapter bindings are
required ongoing work in `PLAN.md`; they are not advertised as implemented.
Run `prek api --json` for the installed manifest or inspect `prek.api` in Lua.

```lua
local prek = require("prek")
local config = prek.config.preset("minimal")
config.policy.allowed_devices = { "keyboard" }
config.devices.keyboard = { keys = { "KEY_A" } }
config.rules = {
    prek.rules.define {
        id = "a-held", devices = { "keyboard" },
        trigger = prek.triggers.button_hold {
            key = "KEY_A", after = prek.units.duration("600ms"),
        },
        action = prek.actions.event { name = "a_held" },
    },
}
return config
```

Create a preset with `prek init --preset minimal --output config.lua`, validate
it with `prek check --config config.lua`, and use it with `replay`, `doctor`,
`run` or `broker`. The broker validates its protected system file and transfers
the compiled Rust model to the device worker; that worker evaluates no Lua
while reading or forwarding input. The current compiler runs synchronously;
isolated asynchronous compilation and runtime callback generations remain work.

## Configuration and rule constructors

`require("prek")` returns the configuration module. Its `api_version` is 1;
`version` is the application version, and `api` contains the manifest.

- `config.new(table)` supplies `version = 1` if absent and returns the table.
- `config.preset(name)` returns a mutable `minimal`, `desktop` or `phone` preset.
- `rules.define(table)` returns a rule declaration.
- `units.duration(string)` and `units.distance(string)` require a string with
  explicit units; model validation checks its unit and range.

The returned configuration accepts `version`, `mode`, `timing`, `shape_capture`,
`policy`, `devices`, `layers`, `shapes` and `rules`. Rust schema validation remains
authoritative, including unknown fields, references, priorities, resource bounds,
activation keys, permitted seats and source selectors. A rule accepts `id`,
`enabled`, `priority`, `devices`, `contexts`, `app`, `scope`, `screen`,
`keyboard_visible`, `require_keys`, `keys_throughout`, `layer`, `delivery`,
`trigger` and `action`. Existing defaults and exact units remain in effect.

## Trigger constructors

All constructors take one table of named fields and return that table with the
correct `kind`. A conflicting `kind` is an error.

| Constructor | Fields |
|---|---|
| `triggers.key` | `key` |
| `triggers.button_taps` | `key`, `count` |
| `triggers.button_hold` | `key`, `after`, optional `repeat = {delay, interval}` |
| `triggers.chord` | `keys`, `ordered`, `timeout` |
| `triggers.wheel` | `horizontal`, `minimum` |
| `triggers.touch_taps` | `fingers`, `count` |
| `triggers.touch_hold` | `fingers`, `after` |
| `triggers.swipe` | `fingers`, `direction`, `travel`, `fallback_travel`, optional `continuous` |
| `triggers.pan` | Swipe fields; supplies `continuous = true` |
| `triggers.pinch` | `fingers`, `ratio` |
| `triggers.rotate` | `fingers`, `degrees` |
| `triggers.shape` | `name`, optional `fingers`, optional mouse `button` |
| `triggers.sequence` | `steps` containing trigger declarations, `timeout` |

Keys use Linux names/aliases or `code:N`; touch counts, source scope and timing
conditions retain their existing validated meaning. Pan is the continuous swipe
representation in the Rust model. Constructors never create a physical owner.

## Action constructors

| Constructor | Fields |
|---|---|
| `actions.event` | `name` |
| `actions.keys` | `keys` |
| `actions.remap` | `key` |
| `actions.command` | `argv` containing exact arguments |
| `actions.morf` | `action` containing a supported Morf action name |
| `actions.hyprland` | `action` containing a supported window action name |
| `actions.power` | `action` containing a supported power action name |

Declaring an action does not establish availability, authorize a target or run
it. Observation and replay execute no live effect. Actions require the applicable
session capability, context and ownership checks in routing mode. Functions in
the configuration result are rejected until runtime handler registration exists.

## Limits, libraries and errors

The configuration VM includes Luna's core libraries and the `prek` module.
Ambient `io`, `os`, `package` and `debug` are absent. `print`, `warn`, `dofile`,
`loadfile`, `load` and `collectgarbage` are disabled. Other modules are unavailable
until a controlled module loader is implemented. The initial random seed is
deterministic. A configuration cannot yield or return multiple values.

Source is limited to 1 MiB, execution to 1,000,000 instruction-fuel units and
250 ms for user source evaluation, arena memory to a 32 MiB check between VM slices, and returned values to
64 nesting levels. Memory checks are slice-granular and do not promise a hard
allocation ceiling. Compilation/process isolation remains a separate requirement.
Trusted VM/API setup is separate from the user-source budget; its bootstrap has
a separate bounded execution window. Expanded model size and total process
memory need the isolated compiler limits planned for the next integration stage.
Syntax/runtime errors include the named Lua chunk; schema errors identify invalid
data. A rejected model does not become the worker's active configuration.
