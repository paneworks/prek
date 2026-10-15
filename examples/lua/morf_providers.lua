local p = require("prek")

p.events.on("context_changed", function()
    for _, provider in ipairs(p.morf.providers() or {}) do
        p.log.info("native Morf connection", {
            output = provider.output,
            epoch = provider.epoch,
            phase = provider.phase,
            compositor_birth = provider.compositor_start,
            fence = provider.fence,
        })
    end
end)

return p.config.preset("minimal")
