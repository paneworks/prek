local prek = require("prek")

return {
    rules = function()
        return {
            prek.rules.define {
                id = "modular-key-a",
                devices = { "keyboard" },
                trigger = prek.triggers.key { key = "KEY_A" },
                action = prek.actions.event { name = "modular_key_a" },
            },
        }
    end,
}
