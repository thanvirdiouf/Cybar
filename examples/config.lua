-- SPDX-License-Identifier: GPL-3.0-or-later
-- Copy to ~/.config/cybar/config.lua, then run `cybar check`.
local bar = require("cybar")

return {
  theme = {
    font = { family = "monospace", size = 11, fallback = { "sans-serif" } },
    colors = {
      background = "#1e1e2e", text = "#cdd6f4", text_active = "#89b4fa",
      text_dim = "#6c7086", separator = "#45475a", urgent = "#f38ba8",
      hover = "#b4befe", warning = "#f9e2af",
    },
  },
  tray_monitor = "primary",
  auto_reload = true,
  reload_debounce_ms = 150,
  watchdog_ms = 100,
  scroll_coalesce_ms = 40,
  bars = {
    {
      name = "main",
      monitor = "primary", -- "all" or an output name, such as "HDMI-1", also works.
      position = "top",
      size = { thickness = 28, length = "100%" },
      offset = { x = 0, y = 0 },
      reserve_space = true,
      padding = 4,
      gap = 4,
      sections = {
        left = {
          bar.workspaces { name = "workspaces", style = { padding = 7 } },
          bar.window_title { name = "title", max_length = 32, empty_text = "" },
        },
        middle = {
          bar.clock { name = "clock", format = "%a %d %b  %H:%M" },
        },
        right = {
          bar.cpu {
            name = "cpu", interval = 1,
            render = function(data)
              data = data or {}
              local usage = data.percent or 0
              return {
                text = string.format("CPU %.0f%%", usage),
                state = usage >= 90 and "warning" or "normal",
              }
            end,
          },
          bar.ram { name = "ram", interval = 2, format = "RAM {percent}%" },
          bar.systray { name = "tray", style = { icon_size = 18, gap = 4 } },
        },
      },
      -- Overrides apply as: all, primary (when appropriate), exact output name.
      -- overrides = { ["HDMI-1"] = { scale = 1.25 } },
    },
  },
}
