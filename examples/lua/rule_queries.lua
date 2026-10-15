local p = require("prek")

p.events.on("ready", function()
    local rule = assert(p.rules.get("example-a"), "example-a is unavailable")
    p.log.info("rule-state", {
        id = rule.id,
        enabled = rule.enabled,
        revision = p.rules.revision(),
    })
    local copy = p.rules.list()
    copy[1].enabled = true
    local current = assert(p.rules.get("example-a"), "example-a is unavailable")
    assert(not current.enabled)
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
