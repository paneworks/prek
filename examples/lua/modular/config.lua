local prek = require("prek")
local keyboard = require("bindings.keyboard")

prek.events.on("ready", function(event)
    prek.log.info("modular-config-ready", {
        generation = event.fence.configuration,
    })
end)

return prek.config.new {
    policy = { allowed_devices = { "keyboard" } },
    devices = { keyboard = { keys = { "KEY_A" } } },
    rules = keyboard.rules(),
}
