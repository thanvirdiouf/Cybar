-- SPDX-License-Identifier: GPL-3.0-or-later
-- Run with `cybar --config examples/vertical.lua`.
-- README screenshot uses illustrative EWMH metadata from capture --demo.
local bar = require("cybar")
return {
  scale = 1,
  auto_reload = false,
  theme = {
    font = { family = "DejaVu Sans Mono", size = 12 },
    colors = {
      background = "#141824", text = "#d8deed", text_active = "#8aebdc",
      text_dim = "#78849b", separator = "#354154", urgent = "#f59db2",
      hover = "#b1c8ff", warning = "#f9d58b",
    },
  },
  bars = {{
    name = "vertical-preview", monitor = "primary", position = "left",
    size = { thickness = 148, length = 650 }, offset = { x = 32, y = 32 },
    reserve_space = false, corner_radius = 9, padding = 12, gap = 6,
    sections = {
      left = {
        bar.custom { name = "brand", render = function() return "CYBAR" end,
          style = { padding = 10, alignment = "center", foreground = "#8aebdc", font = { family = "DejaVu Sans Mono", size = 14 } },
        },
        bar.workspaces { name = "workspaces", numbers = true,
          style = { padding = 8, corner_radius = 5, alignment = "center", min_width = 100,
            normal = { foreground = "#8e9db8" },
            active = { foreground = "#8aebdc", background = "#273e47" },
            urgent = { foreground = "#f59db2", background = "#3b2a3b" },
          },
        },
      },
      middle = {
        bar.custom { name = "date", render = function() return os.date("%a %d %b") end,
          style = { padding = 4, alignment = "center", foreground = "#78849b", font = { family = "DejaVu Sans Mono", size = 11 } },
        },
        bar.clock { name = "clock", format = "%H:%M",
          style = { padding = 6, alignment = "center", foreground = "#bac6dd", font = { family = "DejaVu Sans Mono", size = 16 } },
        },
      },
      right = {
        bar.cpu { name = "cpu", interval = 1, format = "CPU {percent}%", style = { padding = 10, alignment = "center", foreground = "#8aebdc" } },
        bar.ram { name = "ram", interval = 1, format = "RAM {percent}%", style = { padding = 10, alignment = "center", foreground = "#b1c8ff" } },
        bar.custom { name = "lua", render = function() return "Lua ready" end,
          style = { padding = 12, alignment = "center", corner_radius = 5, foreground = "#f9d58b", background = "#303044" },
        },
      },
    },
  }},
}
