-- SPDX-License-Identifier: GPL-3.0-or-later
-- Run with `cybar --config examples/custom.lua`.
local bar = require("cybar")

return {
  bars = {{
    name = "custom", monitor = "primary",
    sections = {
      left = {
        bar.custom {
          name = "status",
          render = function(data) return data or "Ready" end,
          on_message = function(handle, message)
            if message == "get" then return handle:get_data() or "Ready" end
            local value = string.match(message, "^set (.*)$")
            if value then handle:set_data(value); return value end
            return "Use get or set <text>"
          end,
          on_click_left = function(handle)
            handle:set_data("Clicked")
          end,
        },
      },
      middle = {
        bar.custom {
          name = "poll-example",
          source = {
            type = "poll", interval = 1,
            poll = function() return os.date("%H:%M:%S") end,
          },
          render = function(data) return data or "Starting" end,
        },
      },
      right = {
        bar.custom {
          name = "command-example",
          source = { type = "command", interval = 60, command = "uname -r" },
          render = function(data) return "Kernel " .. (data or "…") end,
        },
        bar.custom {
          name = "stream-example",
          source = {
            type = "stream",
            command = "while :; do date '+%H:%M:%S'; sleep 5; done",
          },
          render = function(data) return "Stream " .. (data or "…") end,
        },
      },
    },
  }},
}
