#!/usr/bin/env python3
"""Check installed Lua metadata against real positive/negative LuaLS diagnostics."""
import argparse
import json
import pathlib
import shutil
import subprocess
import tempfile
import urllib.parse

POSITIVE = '''local p = require("prek")
local preset = p.config.preset("minimal")
preset.modes = {p.modes.define {id = "normal", active = true}, p.modes.define {id = "navigate"}}
preset.rules[1] = p.rules.define {
    id = "editor-example", trigger = p.triggers.key {kind = "key", key = "KEY_A"},
    action = p.actions.event {name = "example"},
    on_begin = function(payload) p.log.info(payload.event.rule) end,
    output = "eDP-1",
    when_mode = "normal",
}
local triggers = {
    p.triggers.button_taps {key = "KEY_A", count = 2},
    p.triggers.button_hold {key = "KEY_A", after = "600ms", ["repeat"] = {delay = "1s", interval = "100ms"}},
    p.triggers.chord {keys = {"KEY_A", "KEY_B"}, ordered = true, timeout = "300ms"},
    p.triggers.wheel {horizontal = false, minimum = 1},
    p.triggers.touch_taps {fingers = 2, count = 1},
    p.triggers.touch_hold {fingers = 2, after = "600ms"},
    p.triggers.swipe {fingers = 2, direction = "down", travel = "12mm", fallback_travel = "0.1norm"},
    p.triggers.pan {fingers = 2, direction = "any", travel = "12mm", fallback_travel = "0.1norm"},
    p.triggers.pinch {fingers = 2, ratio = 0.2},
    p.triggers.rotate {fingers = 2, degrees = 30},
    p.triggers.shape {name = "circle", fingers = 2},
    p.triggers.sequence {steps = {p.triggers.key {key = "KEY_A"}}, timeout = "1s"},
}
p.rules.on("editor-example", "cancel", function(payload) p.log.info(payload.event.rule) end)
local actions = {
    p.actions.command {argv = {"printf", "%s", "hello"}},
    p.actions.keys {kind = "keys", keys = {"KEY_LEFTMETA", "KEY_A"}}, p.actions.remap {key = "KEY_B"},
    p.actions.morf {action = "dashboard"}, p.actions.hyprland {action = "close"},
    p.actions.power {action = "wake"},
    p.actions.define("editor-handler", {requires = {"observations"}}, function(payload)
        return p.actions.event {name = payload.event.rule}
    end),
}
p.events.on("ready", function(payload)
    ---@type string
    local identity = payload.fence.configuration
    local tx = p.layers.transaction()
    tx:activate_layer("navigation"); tx:deactivate_layer("navigation"); tx:reset_layer("navigation")
    tx:select_mode("navigate"); tx:reset_mode()
    p.modes.list(); p.modes.get("normal"); p.modes.current(); p.modes.revision(); p.modes.transaction():cancel()
    tx:enable("editor-example"); tx:disable("editor-example"); tx:replace(preset.rules[1]); tx:remove("editor-example")
    tx:commit()
    local status = tx:status()
    p.log.info(status.state, {identity = identity, layers = p.layers.list(), revision = p.rules.revision()})
    local snapshot = p.context.snapshot()
    local output = p.outputs.get("fixture-output")
    if output then
        local point = p.coordinates.convert(output.reference, {x=0.5,y=0.5}, "normalized", "global")
        for _, view in ipairs(p.regions.at(point or {x=0,y=0}) or {}) do
            p.log.info(view.region.id, {revision=view.revision})
        end
    end
    p.outputs.list(); p.regions.list(); p.regions.get("widget")
    local capabilities = p.capabilities.current()
    if capabilities then
        ---@type string
        local generation = capabilities.fence.session
        p.log.info(generation, {negotiated = capabilities.negotiated, ready = capabilities.ready})
    end
    p.capabilities.has("future_capability"); p.capabilities.ready("power_actions")
    if snapshot then
        ---@type string
        local revision = snapshot.revision
        p.log.info(revision, {x = snapshot.outputs[1].geometry.origin.x})
    end
end)
p.events.on("matched", function(payload)
    if payload.event.evidence.kind == "key" then
        p.log.info(payload.event.rule, {down = payload.event.evidence.down})
    end
end)
p.events.on("context_changed", function(payload) p.log.info(payload.state, {revision = payload.revision}) end)
p.events.on("rules_changed", function(payload) p.log.info("receipt", {accepted = payload.accepted}) end)
p.events.on("input", function(payload)
    local info = p.devices.get(payload.input.source)
    if info then
        local axis = info.device.axes[53]
        local position = p.devices.position(payload.input.source, 0, 0)
        if position and position["local"] then p.log.info("position", {x = position["local"].x}) end
        p.log.info(info.device.name, {maximum = axis and axis.maximum, revision = p.devices.revision()})
    end
    local seat = p.seats.current()
    if seat then p.log.info(seat.binding.seat) end
    if payload.input.kind == "frame" then
        ---@type string
        local sequence = payload.input.sequence
        for _, change in ipairs(payload.input.changes) do
            if change.kind == "key" then p.log.info(sequence, {code = change.code}) end
        end
    else p.log.info(payload.input.reason, {keys = payload.input.keys}) end
end)
local timer = p.scheduler.after("1s", function(payload) p.log.info("tick", {id = payload.timer.id}) end)
timer:cancel()
local task = p.scheduler.spawn(function(payload)
    local wake = p.scheduler.sleep("10ms")
    p.log.info("wake", {at = wake.at.secs, id = payload.task.id})
end)
task:cancel()
return preset
'''

NEGATIVE = '''local p = require("prek")
p.triggers.key {key = 30} -- expect: assign-type-mismatch
p.triggers.button_hold {key = "KEY_A"} -- expect: missing-fields
p.rules.no_such_api() -- expect: undefined-field
p.events.on("ready", function(payload)
    ---@type integer
    local wrong = payload.fence.configuration -- expect: assign-type-mismatch
    p.layers.transaction():activate_layer(12) -- expect: param-type-mismatch
    p.modes.transaction():select_mode(false) -- expect: param-type-mismatch
end)
p.events.on("bogus", function() end) -- expect: param-type-mismatch
p.modes.define {id = "bad", active = "yes"} -- expect: assign-type-mismatch
p.rules.on("editor-example", "ready", function() end) -- expect: param-type-mismatch
p.rules.on("editor-example", "begin", false) -- expect: param-type-mismatch
p.coordinates.convert({}, {x=0,y=0}, "local", "global") -- expect: missing-fields
p.events.on("ready", function()
    local output = p.outputs.get("x")
    if output then p.coordinates.convert(output.reference, {x=0,y=0}, "pixels", "global") end -- expect: param-type-mismatch
end)
'''


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=pathlib.Path, required=True)
    parser.add_argument("--definitions", type=pathlib.Path, required=True)
    parser.add_argument("--examples", type=pathlib.Path, required=True)
    parser.add_argument("--language-server", required=True)
    parser.add_argument("--workdir", type=pathlib.Path, required=True)
    args = parser.parse_args()
    args.workdir.mkdir(parents=True, exist_ok=True)
    root = pathlib.Path(tempfile.mkdtemp(prefix="run-", dir=args.workdir)).resolve()
    workspace = root / "workspace"
    library = workspace / "library"
    library.mkdir(parents=True)
    metadata = args.definitions.read_bytes()
    exported = subprocess.run([str(args.binary.resolve()), "api", "--lua"], check=True,
                              capture_output=True, timeout=10).stdout
    assert exported == metadata, "installed CLI/editor artifact mismatch"
    (library / "prek.lua").write_bytes(metadata)
    shutil.copytree(args.examples, workspace / "examples")
    (workspace / "positive.lua").write_text(POSITIVE)
    (workspace / "negative.lua").write_text(NEGATIVE)
    entries = [path for path in sorted((workspace / "examples").rglob("*.lua"))
               if "modules" not in path.relative_to(workspace / "examples").parts]
    for entry in [*entries, workspace / "positive.lua"]:
        checked = subprocess.run([str(args.binary.resolve()), "check", "--config", str(entry), "--json"],
                                 capture_output=True, timeout=10)
        assert checked.returncode == 0, f"{entry}: {checked.stderr.decode(errors='replace')}"
        assert json.loads(checked.stdout)["status"] == "valid", str(entry)
    module_paths = [str(path.relative_to(workspace) / "?.lua")
                    for path in sorted(workspace.glob("examples/**/modules"))]
    (workspace / ".luarc.json").write_text(json.dumps({
        "runtime.version": "Lua 5.4", "runtime.path": ["?.lua", "?/init.lua", *module_paths],
        "workspace.library": ["library"], "workspace.checkThirdParty": False,
        "diagnostics.disable": ["unused-local", "unused-function"],
        "diagnostics.libraryFiles": "Enable",
    }))
    report = root / "diagnostics.json"
    result = subprocess.run([
        args.language_server, f"--check={workspace}", "--check_format=json",
        f"--check_out_path={report}", f"--logpath={root / 'log'}", f"--metapath={root / 'meta'}",
    ], capture_output=True, timeout=60)
    (root / "server.log").write_bytes(result.stdout + result.stderr)
    assert result.returncode in (0, 1), f"LuaLS failed: {root / 'server.log'}"
    diagnostics = json.loads(report.read_text()) if report.exists() else {}
    expected = {(index, line.split("-- expect: ", 1)[1])
                for index, line in enumerate(NEGATIVE.splitlines()) if "-- expect: " in line}
    actual = set()
    for uri, items in diagnostics.items():
        path = pathlib.Path(urllib.parse.unquote(urllib.parse.urlparse(uri).path))
        for item in items:
            assert path == workspace / "negative.lua", f"unexpected diagnostic in {path}: {item}"
            actual.add((item["range"]["start"]["line"], item["code"]))
    assert actual == expected, f"expected {expected}, got {actual}; report {report}"
    print(json.dumps({"status": "passed", "negative_diagnostics": len(actual),
                      "examples": len(list((workspace / 'examples').rglob('*.lua'))),
                      "validated_configurations": len(entries) + 1,
                      "report": str(report)}))


if __name__ == "__main__":
    main()
