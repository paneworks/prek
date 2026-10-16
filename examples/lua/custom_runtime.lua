local p = require("prek")

local pressed = p.recognizers.define("runtime_pressed", {
    selectors = { "keyboard" },
    inputs = { "key" },
    reply_within = "250ms",
}, function(event)
    if event.cause == "cancel" then return nil end
    ---@cast event prek.RecognitionTicket
    for _, change in ipairs(event.input.changes) do
        if change.kind == "key" and change.code == 30 and change.state == "press" then
            return { phase = "discrete", data = { device = event.input.source.device } }
        end
    end
    return nil
end)

p.events.on("ready", function()
    local tx = p.rules.transaction()
    tx:replace(p.rules.define {
        id = "runtime_pressed_rule",
        devices = { "keyboard" },
        contexts = { "desktop" },
        trigger = pressed,
        action = p.actions.event { name = "runtime_pressed" },
    })
    tx:commit()
end)

p.events.on("rules_changed", function(event)
    p.log.info("runtime rule receipt", { accepted = event.accepted, revision = event.revision })
end)

return p.config.new {
    policy = { allow_input_events = true, allow_runtime_rules = true, allowed_devices = { "keyboard" } },
    devices = { keyboard = { keys = { "KEY_A" } } },
}
