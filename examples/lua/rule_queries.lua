local p = require("prek")

p.events.on("ready", function()
    local rule = p.rules.get("example-a")
    p.log.info("rule-state", {
        id = rule.id,
        enabled = rule.enabled,
        revision = p.rules.revision(),
    })
    local copy = p.rules.list()
    copy[1].enabled = true
    assert(not p.rules.get("example-a").enabled)
end)

return p.config.new {
    rules = {
        p.rules.define {
            id = "example-a",
            enabled = false,
            trigger = p.triggers.key { key = "KEY_A" },
            action = p.actions.event { name = "example-a" },
        },
    },
}
