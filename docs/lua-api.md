# Lua API

All Lua code runs on the main thread. Callbacks must return promptly. The core
logs exceptions and warns after callbacks exceed `watchdog_ms`; the watchdog
measures elapsed time and does not interrupt Lua code. Use the asynchronous
helpers for child processes and Unix sockets.

## Custom sources

```lua
local bar = require("cybar")
local uptime = bar.custom {
  name = "uptime",
  source = {
    type = "poll", interval = 1,
    poll = function(handle) return { seconds = os.time() } end,
  },
  render = function(data, handle)
    return data and tostring(data.seconds) or "Starting"
  end,
}
```

`source.poll(handle)` returns the next data value. `fn` is an alias for `poll`.
`interval` is in seconds and defaults to 1; it must be finite, positive, and
at most one year (365 days).
Callbacks can return any Lua value appropriate for the module's renderer.

Command sources run `/bin/sh -c` asynchronously:

```lua
bar.custom {
  name = "kernel",
  source = { type = "command", command = "uname -r", interval = 60 },
  render = function(data) return data or "Starting" end,
}
```

Data is stdout with trailing line endings removed. Commands do not overlap while
the previous invocation is still running. Nonzero exits are logged. Stream
commands update for each newline and are restarted when they exit:

```lua
bar.custom {
  name = "events",
  source = { type = "stream", command = "my-event-producer" },
  render = function(line) return line or "Waiting" end,
}
```

A stream source must contain exactly one of `command`, `file`, or `socket`.
Unix socket streams update on newline-delimited data. File streams reread the
whole file when its modification time or size changes. File notifications trigger
reads, with an occasional metadata check; if notifications are unavailable,
`interval` controls polling (default 1 second). They do not tail an append-only file. A socket
source requires the external endpoint to exist when the bar is prepared.

```lua
source = { type = "stream", file = "status.txt", interval = 0.5 }
source = { type = "stream", socket = "/run/user/1000/example.sock" }
```

Any source may include `parse(value, handle)` to transform each update before
it becomes module data. For example, convert command output with `parse =
function(output) return tonumber(output) end`. If parsing raises an exception,
Cybar logs it and keeps the previous data.

## Render return values

`render(data, handle)` can return a string, number, `nil`, a display table, or an
array of display tables:

```lua
return {
  text = "Connected", icon = "network-wired", state = "active",
  style = { foreground = "#a6e3a1", padding = 10 },
}
```

`state` is `normal`, `hover`, `active`, `urgent`, or `warning`. A table can include
the style fields directly or in `style`. `icon=false` removes an inherited icon.
`workspace=<id>` associates a displayed item with a workspace for click context.
The automatic hover state takes precedence while the pointer is over a module.
Persistent handle text/icon overrides take precedence over render output.

## Module handles and handlers

| Method | Behavior |
| --- | --- |
| `handle:get_data()` or `handle:data()` | Latest source data |
| `handle:set_data(value)` | Replace data and mark the bar dirty |
| `handle:set_text(text)` | Persistent text override; `nil` restores rendered text |
| `handle:set_icon(icon)` | Persistent icon override; `nil` removes the icon |
| `handle:set_style(table)` | Merge validated style fields into the current style |
| `handle:set_state(state)` | Set persistent state; `"normal"` restores automatic states |
| `handle:redraw()` | Mark the bar dirty |

Handlers can be module fields or entries in `handlers={...}`. Direct fields take
precedence. A handler can be `false` to disable a built-in default.

| Callback | Arguments |
| --- | --- |
| `on_click_left`, `on_click_middle`, `on_click_right` | `(handle, context)` |
| `on_click_button` | `(handle, button, context)` |
| `on_scroll` | `(handle, dx, dy)` |
| `on_hover_enter`, `on_hover_leave` | `(handle)` |
| `on_message` | `(handle, text)` |

Click context contains `button` and, for workspace items, `workspace`. Scroll
units are wheel notches; XInput2 can deliver fractions. Positive `dy` means down,
positive `dx` means right. Vertical input is accumulated for `scroll_coalesce_ms`
and delivered with `dx=0`; horizontal input is immediate with `dy=0`. Setting
the window to zero delivers each event. Set `scroll_coalesce_ms` directly on a
module to override the global window. XInput2 scroll emulation is suppressed
when the server/device supplies smooth valuators; legacy buttons 4–7 are the
fallback.

`cybar request <name> <text...>` converts `on_message`'s return value into JSON.
Return strings, booleans, numbers, or serializable tables. For replicated modules,
use `name@monitor` to target an output. An unqualified name selects the primary
monitor's copy, or the first matching instance. `cybar msg` first checks that the
target exists, then invokes the same callback through a message that receives
no reply. Handler errors appear on daemon stderr. Use [examples/custom.lua](../examples/custom.lua)
to try `cybar request status get` and `cybar msg status set Connected`.

## Non-blocking helpers

```lua
bar.spawn("uname -r", function(stdout, exit_code, stderr)
  handle:set_text(stdout)
end)
bar.exec("notify-send 'Hello from Cybar'")
local later = bar.after(2, function() handle:set_text("Later") end)
local repeating = bar.every(10, function() handle:redraw() end)
repeating:cancel()
later:cancel()
```

`spawn` captures stdout and stderr and reports a numeric exit code; a launch
failure reports `-1`. `exec` fires and forgets; its launched commands continue
through reloads and after Cybar quits. Cybar reaps completed commands while it
runs. Timer callbacks receive no
arguments and return values are ignored. Seconds must be finite, positive, and
at most one year. Child output and socket queues are bounded to 1 MiB.

```lua
local socket = bar.socket.connect("/run/user/1000/service.sock", {
  on_data = function(socket, bytes) handle:set_text(bytes) end,
  on_close = function(socket) handle:set_state("warning") end,
})
socket:send("status\n")
```

Socket handles expose `fd()`, `send(bytes)`, `write(bytes)` (alias), `read()`, and
`close()`. Writes queue without blocking. `read()` returns available bytes, `""`
when no data is currently available, and `nil` after EOF. With `on_data`, the
callback receives chunks instead of placing them in the read queue. A callback
can receive partial lines or multiple lines; keep a buffer when framing matters.
Connections are Unix stream sockets. Cybar expands `~` in helper socket paths;
other relative helper paths use the process working directory.

Timers and captured `spawn` jobs belong to one configuration generation and are
retired with it; detached `exec` jobs continue. Place side effects in callbacks rather than
configuration construction: `cybar check` evaluates Lua, including any direct
Lua I/O and socket connection calls.
