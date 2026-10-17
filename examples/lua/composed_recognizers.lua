local p = require('prek')

local function recognize(ticket)
    if ticket.cause == 'cancel' then return nil end
    return {phase = 'discrete', data = {step = ticket.recognizer}}
end

local first = p.recognizers.define('first', {
    selectors = {'keyboard'}, inputs = {'key'}, reply_within = '100ms',
}, recognize)
local last = p.recognizers.define('last', {
    selectors = {'keyboard'}, inputs = {'key'}, reply_within = '100ms',
}, recognize)

return p.config.new {
    policy = {allow_input_events = true, allow_runtime_rules = true,
        allowed_devices = {'keyboard'}},
    devices = {keyboard = {keys = {'KEY_A'}}},
    rules = {{id = 'composed', devices = {'keyboard'}, contexts = {'desktop'},
        scope = 'everywhere', delivery = 'observe',
        trigger = p.triggers.sequence {steps = {first, last}, timeout = '1s'},
        action = p.actions.event {name = 'composed'}}},
}
