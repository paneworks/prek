local prek = require("prek")

local task = prek.scheduler.spawn(function(wake)
    prek.log.info("task-start", {
        generation = wake.context.fence.configuration,
    })
    local next = prek.scheduler.sleep("100ms")
    prek.log.info("task-resumed", {
        generation = next.context.fence.configuration,
    })
end)

prek.events.on("stop", function()
    task:cancel()
end)

return prek.config.preset("minimal")
