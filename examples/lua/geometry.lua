-- Queries are read-only and require fresh broker-provided desktop context.
local p = require("prek")
p.events.on("context_changed", function()
    for _, output in ipairs(p.outputs.list() or {}) do
        local center = p.coordinates.convert(output.reference, {x=0.5, y=0.5}, "normalized", "global")
        if center then
            p.log.info("output-center", {name=output.state.geometry.name, point=center})
        end
    end
end)
return p.config.new {}
