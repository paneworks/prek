-- Queries are read-only and require fresh broker-provided desktop context.
local p = require("prek")
p.events.on("context_changed", function()
    for _, output in ipairs(p.outputs.list() or {}) do
        local center = p.coordinates.convert(output.reference, {x=0.5, y=0.5}, "normalized", "global")
        if center then
            p.log.info("output-center", {name=output.state.geometry.name, point=center})
            for _, view in ipairs(p.regions.at(center) or {}) do
                p.log.info("rendered-region", {id=view.region.id, interactive=view.region.interactive})
            end
        end
    end
end)
return p.config.new {}
