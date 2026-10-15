local p = require("prek")
local transaction

p.events.on("ready", function()
    local available, result = pcall(p.rules.transaction)
    if not available then
        p.log.info("rule-mutation-unavailable", {reason = tostring(result)})
        return
    end
    transaction = result
    transaction:disable("example-a")
    transaction:commit()
end)

p.events.on("rules_changed", function(event)
    if transaction then
        p.log.info("rule-transaction", transaction:status())
    end
end)

-- This permission takes effect only in the protected system configuration.
-- The session must also request --lua --rule-mutations.
return p.config.new {
    policy = {allow_runtime_rules = true},
    rules = {
        p.rules.define {
            id = "example-a",
            trigger = p.triggers.key {key = "KEY_A"},
            action = p.actions.event {name = "example-a"},
        },
    },
}
