local p = require("prek")

p.events.on("ready", function()
    local view = p.capabilities.current()
    if view then
        p.log.info("session capabilities", {
            fence = view.fence,
            negotiated = view.negotiated,
            ready = view.ready,
        })
    end
end)

p.events.on("context_changed", function()
    -- A true readiness result still requires a broker grant for each effect.
    p.log.info("power availability", {
        negotiated = p.capabilities.has("power_actions"),
        ready = p.capabilities.ready("power_actions"),
    })
end)

return p.config.preset("minimal")
