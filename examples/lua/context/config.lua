-- Run with an authenticated broker and session --lua --json.
local p = require("prek")

p.events.on("context_changed", function(event)
    local snapshot = p.context.snapshot()
    local binding = p.context.binding()
    if not snapshot or not binding then
        p.log.info("context unavailable", {
            state = p.context.state(),
            reason = p.context.error(),
        })
        return
    end

    p.log.info("session geometry", {
        seat = binding.seat,
        session = binding.session,
        revision = snapshot.revision,
        outputs = #snapshot.outputs,
    })
end)

return p.config.preset("minimal")
