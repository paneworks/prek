local p = require("prek")

p.events.on("input", function(payload)
    local input = payload.input
    local info = p.devices.get(input.source)
    if not info then
        p.log.info("device-unavailable", {generation = input.source.generation})
        return
    end
    if input.kind == "reset" then
        p.log.info("device-metadata", {
            name = info.device.name,
            generation = info.device.source.generation,
            revision = p.devices.revision(),
        })
    elseif info.mapping and input.contacts[1] then
        local contact = input.contacts[1]
        local position = p.devices.position(input.source, contact.x, contact.y)
        if position then
            p.log.info("touch-position", {normalized = position.normalized, millimetres = position.millimetres})
        end
    end
end)

-- Protected system source and session --lua --input are required for device
-- metadata. The seat query uses this session's independently verified context.
return p.config.new {
    policy = {allow_input_events = true, allowed_devices = {"touch"}},
    devices = {touch = {absolute_axes = {53, 54}}},
}
