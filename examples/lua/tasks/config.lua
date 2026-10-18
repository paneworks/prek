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

local observer = prek.scheduler.spawn(function()
    local state = task:join()
    prek.log.info("task-joined", {state = state})
end)

prek.events.on("stop", function()
    observer:cancel()
    task:cancel()
end)

return prek.config.preset("minimal")
