local prek = require("prek")

prek.events.on("ready", function()
    local configuration = prek.config.current()
    local policy = prek.policy.current()
    if configuration and policy then
        assert(configuration.revision == policy.revision)
        prek.log.info("configuration inspected", {
            revision = configuration.revision,
            mode = configuration.config.mode,
            runtime_rules_permitted = policy.policy.allow_runtime_rules,
        })
    end
    local status = prek.status.current()
    if status then
        prek.log.info("broker status", { state = status.state, revision = status.revision })
    end
end)

return prek.config.preset("minimal")
