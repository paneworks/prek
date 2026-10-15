local p = require("prek")
local transaction

p.events.on("ready", function()
    p.log.info("selected-mode", {id = p.modes.current().id})
end)

p.events.on("matched", function(payload)
    if payload.event.rule ~= "switch-mode" or payload.event.stage ~= "end" then return end
    local available, result = pcall(p.modes.transaction)
    if not available then
        p.log.info("mode-mutation-unavailable", {reason = tostring(result)})
        return
    end
    transaction = result
    transaction:select_mode(p.modes.current().id == "normal" and "navigate" or "normal")
    transaction:commit()
end)

p.events.on("rules_changed", function(payload)
    p.log.info("mode-receipt", {
        accepted = payload.accepted,
        revision = p.modes.revision(),
        id = p.modes.current().id,
        state = transaction and transaction:status().state,
    })
end)

-- Use the protected system source and session --lua --rule-mutations.
-- F9 switches acknowledged modes; A emits the selected mode's event.
return p.config.new {
    policy = {allow_runtime_rules = true, allowed_devices = {"keyboard"}},
    devices = {keyboard = {keys = {"KEY_A", "KEY_F9"}}},
    modes = {p.modes.define {id = "normal", active = true}, p.modes.define {id = "navigate"}},
    rules = {
        p.rules.define {id = "switch-mode", trigger = p.triggers.key {key = "KEY_F9"},
            action = p.actions.event {name = "switch-mode"}},
        p.rules.define {id = "normal-a", when_mode = "normal", trigger = p.triggers.key {key = "KEY_A"},
            action = p.actions.event {name = "normal-a"}},
        p.rules.define {id = "navigate-a", when_mode = "navigate", trigger = p.triggers.key {key = "KEY_A"},
            action = p.actions.event {name = "navigate-a"}},
    },
}
