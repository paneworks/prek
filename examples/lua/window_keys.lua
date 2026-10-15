-- Global keyboard actions freeze the initial focused window on an exact output.
-- Protected system configuration; session --lua --hyprland. Takeover is gated.
local p = require("prek")
local window = p.actions.define("window", {requires={"hyprland_actions"}}, function(payload)
    if payload.event.stage ~= "begin" then return end
    return p.actions.hyprland {action=payload.event.rule}
end)
return p.config.new {
    mode="route",
    policy={allowed_devices={"keyboard"}, allow_hyprland_actions=true},
    devices={keyboard={name="Example keyboard"}},
    rules={
        p.rules.define {id="move_left_and_follow", priority=10, devices={"keyboard"}, contexts={"desktop"},
            output="eDP-1", delivery="reserve", trigger=p.triggers.key {key="KEY_F9"}, action=window},
        p.rules.define {id="move_right_and_follow", priority=11, devices={"keyboard"}, contexts={"desktop"},
            output="eDP-1", delivery="reserve", trigger=p.triggers.key {key="KEY_F10"}, action=window},
        p.rules.define {id="close", priority=12, devices={"keyboard"}, contexts={"desktop"},
            output="eDP-1", delivery="reserve", trigger=p.triggers.key {key="KEY_F11"}, action=window},
    },
}
