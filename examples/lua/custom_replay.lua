local p = require('prek')
local trigger = p.recognizers.define('custom-replay', {
    selectors = {'keys'}, inputs = {'key'}, reply_within = '100ms',
}, function(ticket)
    if ticket.cause == 'cancel' then return nil end
    return {phase = 'discrete', data = {example = true}}
end)

local config = p.config.new {
    policy = {allow_input_events = true, allowed_devices = {'keys'}},
    devices = {keys = {keys = {'KEY_A'}}},
    rules = {{id = 'custom-replay', devices = {'keys'}, contexts = {'desktop'},
        scope = 'everywhere', delivery = 'observe', trigger = trigger,
        action = p.actions.event {name = 'custom-replay'}}},
}

local records = {
        [[{"record":"header","version":1}]],
        [[{"record":"context","at":{"secs":0,"nanos":0},"binding":{"seat":"seat0","session":"recorded","uid":1000,"compositor_pid":42,"compositor_start":1,"kind":"user"},"snapshot":{"generation":1,"revision":1,"captured_at":{"secs":0,"nanos":0},"state":"desktop","outputs":[],"windows":[],"regions":[],"secure_overlay":false}}]],
        [[{"record":"device","device":{"source":{"device":1,"generation":1,"seat":"seat0"},"identity":"fixture","name":"Fixture","path":"","physical":"","unique":"","vendor":1,"product":1,"properties":[],"capabilities":{"1":[30]},"axes":{},"virtual_device":false},"keys":[],"contacts":[]}]],
        [[{"record":"frame","frame":{"source":{"device":1,"generation":1,"seat":"seat0"},"sequence":1,"at":{"secs":0,"nanos":10000000},"events":[{"kind":1,"code":30,"value":1},{"kind":0,"code":0,"value":0}]}}]],
        [[{"record":"expected_rules","events":[{"outcome":"matched","version":1,"rule":"custom-replay","id":1,"ordinal":1,"source":{"device":1,"generation":1,"seat":"seat0"},"at":{"secs":0,"nanos":10000000},"config_generation":1,"session_generation":1,"stage":"discrete","action":{"kind":"event","name":"custom-replay"},"evidence":{"kind":"custom","recognizer":"custom-replay","data":{"example":true}},"window":null,"output":null,"workspace":null}]}]],
}
local replay, index, timer
timer = p.scheduler.every('1ms', function()
    if not replay then return end
    if records[index] then
        replay:push(records[index])
        index = index + 1
    else
        local result = replay:finish(true)
        replay = nil
        timer:cancel()
        p.log.info('custom-replay-checked', {outcomes = #result.outcomes})
    end
end)
p.events.on('ready', function()
    if index then return end
    replay = p.replay.new(config)
    index = 1
end)

return config
