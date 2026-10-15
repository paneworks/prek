local p = require("prek")
local function phase(payload)
    p.log.info("rule-phase", {
        rule=payload.event.rule, stage=payload.event.stage,
        id=payload.event.id, ordinal=payload.event.ordinal,
    })
end
local rule = p.rules.define {
    id="watch-a", contexts={"desktop"}, delivery="observe",
    trigger=p.triggers.key {key="KEY_A"},
    action=p.actions.event {name="a-observed"},
    on_begin=phase, on_end=phase,
}
-- The returned subscription can be disposed inside a callback.
p.rules.on("watch-a", "cancel", function(payload)
    p.log.info("rule-cancelled", {rule=payload.event.rule, id=payload.event.id})
end)
return p.config.new {rules={rule}}
