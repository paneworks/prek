-- Offline declaration; live execution currently requires an authorized host
-- embedding to install the verified Hyprland adapter. Public routing is gated.
local p = require("prek")
local window = p.actions.define("window", {requires={"hyprland_actions"}}, function(payload)
    if payload.event.stage ~= "begin" and payload.event.stage ~= "discrete" then return end
    return p.actions.hyprland {action=payload.event.rule}
end)
return p.config.new {
    mode="route",
    policy={allowed_devices={"touchscreen"}},
    devices={touchscreen={name="Example touchscreen", absolute_axes={53,54,57}}},
    rules={
        p.rules.define {id="move_left_and_follow", priority=10, devices={"touchscreen"}, contexts={"desktop"}, scope="app_content", delivery="buffer",
            trigger=p.triggers.swipe {fingers=2, direction="left", travel="12mm", fallback_travel="0.1norm"}, action=window},
        p.rules.define {id="move_right_and_follow", priority=11, devices={"touchscreen"}, contexts={"desktop"}, scope="app_content", delivery="buffer",
            trigger=p.triggers.swipe {fingers=2, direction="right", travel="12mm", fallback_travel="0.1norm"}, action=window},
        p.rules.define {id="close", priority=12, devices={"touchscreen"}, contexts={"desktop"}, scope="app_content", delivery="buffer",
            trigger=p.triggers.swipe {fingers=2, direction="up", travel="12mm", fallback_travel="0.1norm"}, action=window},
    },
}
