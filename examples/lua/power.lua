-- Protected routing configuration declaration; offline check performs no effects.
-- Production takeover remains gated. Select your own keyboard/output explicitly.
local p = require("prek")
local screen = p.actions.define("screen", {requires={"power_actions"}}, function(payload)
    if payload.event.stage ~= "begin" then return end
    return p.actions.power {action=payload.event.rule}
end)
return p.config.new {
    mode="route",
    policy={allowed_devices={"keyboard"}, allow_power_actions=true},
    devices={keyboard={name="Example keyboard"}},
    rules={
        p.rules.define {id="blank", devices={"keyboard"}, contexts={"desktop"},
            output="eDP-1", screen="on", delivery="reserve",
            trigger=p.triggers.key {key="KEY_F11"}, action=screen},
        p.rules.define {id="wake", devices={"keyboard"}, contexts={"desktop"},
            output="eDP-1", screen="off", delivery="reserve",
            trigger=p.triggers.key {key="KEY_F12"}, action=screen},
    },
}
