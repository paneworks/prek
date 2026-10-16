local prek = require("prek")

-- Supply explicit recording data; this API opens no files or input devices.
local function replay(config)
    local session = prek.replay.new(config)
    session:push('{"record":"header","version":1}')
    session:push('{"record":"tick","at":{"secs":0,"nanos":0}}')
    local result = session:finish()
    assert(not result.checked and #result.outcomes == 0)
    assert(session:status().state == "finished")
    return result.mode
end

assert(replay(nil) == "observations")
assert(replay(prek.config.preset("minimal")) == "rules")

prek.events.on("ready", function()
    assert(replay(nil) == "observations")
    prek.log.info("offline replay completed without live effects")
end)

return prek.config.preset("minimal")
