local p = require("prek")

-- These declarations request permission; only the protected system source and
-- an owning session with --lua --recognizers can activate the callback.
---@type table<prek.Identity, number>
local held = {}

---@param time prek.MonotonicTime
---@return number
local function milliseconds(time)
    return time.secs * 1000 + time.nanos / 1000000
end

local hold = p.recognizers.define("example_hold", {
    selectors = { "keyboard" },
    inputs = { "key" },
    reply_within = "250ms",
    timeout = "1s",
    wake = "20ms",
}, function(event)
    if event.cause == "cancel" then
        -- Completion also sends cleanup. Candidate identities are strings.
        held[event.candidate] = nil
        return nil
    end
    ---@cast event prek.RecognitionTicket
    local device = p.devices.get(event.input.source)
    if not device then return { phase = "cancel" } end
    if event.cause == "frame" then
        for _, change in ipairs(event.input.changes) do
            if change.kind == "key" and change.code == 30 then -- KEY_A
                if change.state == "press" then
                    held[event.candidate] = milliseconds(event.input.at)
                elseif change.state == "release" then
                    return { phase = "cancel" }
                end
            end
        end
    end
    local started = held[event.candidate]
    if not started then return { phase = "cancel" } end
    local elapsed = milliseconds(event.requested_at) - started
    if elapsed >= 600 then
        return { phase = "discrete", data = { held_ms = elapsed } }
    end
    -- A timer wake has no repeated transitions and retains the last frame time.
    return nil
end)

return p.config.new {
    policy = { allow_input_events = true, allowed_devices = { "keyboard" } },
    devices = { keyboard = { keys = { "KEY_A" } } },
    rules = {
        p.rules.define {
            id = "example_hold_rule",
            devices = { "keyboard" },
            contexts = { "desktop" },
            require_keys = { "KEY_A" },
            keys_throughout = true,
            trigger = hold,
            action = p.actions.event { name = "example_hold" },
            on_discrete = function(event)
                p.log.info("custom hold matched", { rule = event.event.rule })
            end,
        },
    },
}
