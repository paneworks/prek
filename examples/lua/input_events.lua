local p = require("prek")
local presses = 0

p.events.on("input", function(payload)
    local input = payload.input
    if input.kind == "reset" then
        -- Reset local recognizer state here. Held keys/contacts are snapshots,
        -- not new presses; source generation changes establish a new lifetime.
        p.log.info("input-reset", {reason = input.reason, generation = input.source.generation})
        return
    end
    for _, change in ipairs(input.changes) do
        if change.kind == "key" and change.code == 30 and change.state == "press" then
            presses = presses + 1
            p.log.info("a-press-count", {count = presses, sequence = input.sequence})
        end
    end
end)

-- Install as a protected system source, then use session --lua --input.
-- This observation subscription has no authority to consume or remap input.
return p.config.new {
    policy = {allow_input_events = true, allowed_devices = {"keyboard"}},
    devices = {keyboard = {keys = {"KEY_A"}}},
}
