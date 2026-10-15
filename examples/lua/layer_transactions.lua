local p = require("prek")
local transaction

p.events.on("ready", function()
    local available, result = pcall(p.layers.transaction)
    if not available then
        p.log.info("layer-mutation-unavailable", {reason = tostring(result)})
        return
    end
    transaction = result
    transaction:activate_layer("navigation")
    transaction:commit()
end)

p.events.on("rules_changed", function(event)
    local available, layer = pcall(p.layers.get, "navigation")
    p.log.info("layer-transaction", {
        accepted = event.accepted,
        revision = event.revision,
        activation = available and layer and layer.activation or "unavailable",
    })
end)

p.events.on("matched", function(event)
    if event.event.rule == "navigation-a" and event.event.stage == "end"
        and transaction and transaction:status().state == "accepted" then
        transaction = p.layers.transaction()
        transaction:reset_layer("navigation")
        transaction:commit()
    end
end)

-- Permission is effective only in the protected system source. The owning
-- desktop session must request --lua --rule-mutations. This observes input.
return p.config.new {
    policy = {allow_runtime_rules = true, allowed_devices = {"keyboard"}},
    devices = {keyboard = {keys = {"KEY_A", "KEY_LEFTMETA"}}},
    layers = {p.layers.define {id = "navigation", key = "KEY_LEFTMETA"}},
    rules = {
        p.rules.define {
            id = "navigation-a",
            devices = {"keyboard"},
            layer = "navigation",
            trigger = p.triggers.key {key = "KEY_A"},
            action = p.actions.event {name = "navigation-a"},
        },
    },
}
