-- SPDX-License-Identifier: GPL-3.0-or-later
-- Run with `cybar --config examples/horizontal.lua`.
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
    name = "preview", monitor = "primary", position = "top",
    size = { thickness = 44, length = "100%" }, reserve_space = true,
    padding = 10, gap = 8,
    sections = {
      left = {
        bar.workspaces { name = "workspaces", numbers = true,
          style = { padding = 9, corner_radius = 5,
            normal = { foreground = "#8e9db8" },
            active = { foreground = "#8aebdc", background = "#273e47" },
            urgent = { foreground = "#f59db2", background = "#3b2a3b" },
          },
        },
        bar.window_title { name = "title", max_length = 27, empty_text = "Cybar", style = { padding = 10 } },
      },
      middle = { bar.clock { name = "clock", format = "%a %d %b  %H:%M", style = { padding = 10, foreground = "#bac6dd" } } },
      right = {
        bar.cpu { name = "cpu", interval = 1, format = "CPU {percent}%", style = { padding = 9, foreground = "#8aebdc" } },
        bar.ram { name = "ram", interval = 1, format = "RAM {percent}%", style = { padding = 9, foreground = "#b1c8ff" } },
        bar.custom { name = "lua", render = function() return "Lua ready" end,
          style = { padding = 10, corner_radius = 5, foreground = "#f9d58b", background = "#303044" },
        },
      },
    },
  }},
}
