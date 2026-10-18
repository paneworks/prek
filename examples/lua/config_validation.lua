local prek = require("prek")

local config = prek.config.preset("desktop")
config.timing.tap_gap = "350ms"

local checked = prek.config.validate(config)
assert(checked ~= config)

return checked
