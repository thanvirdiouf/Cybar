# Window-manager backends

EWMH is the default backend. A Lua backend is selected with:

```lua
wm = {
  backend = require("my_backend"),
  opts = { path = "/run/user/1000/my-wm-bridge.sock" },
}
```

The module exposes `connect(opts)` and returns a connection with these methods:

| Method | Contract |
| --- | --- |
| `conn:get_workspaces()` | Array of `{id,name,index,monitor,focused,visible,urgent,occupied}` |
| `conn:get_active_window()` | `{id,title,class}` or `nil` |
| `conn:command(name, ...)` | Return `ok, err`; implement `switch_workspace` with an integer workspace ID |
| `conn:subscribe(event, callback)` or `conn:on(event, callback)` | Register an event handler and return an unsubscribe handle; provide at least one name, or both as aliases |
| `conn:fd()` | Valid initial descriptor polled by the core; return `-1` after disconnect to disable polling |
| `conn:main()` | Drain pending messages, emit events, and return without blocking |

IDs and indices are unsigned integers; `index` is zero-based. Workspace names,
monitor names, titles, and classes are strings. The four workspace flags are
booleans. An unknown EWMH output association uses the empty monitor string.

Cybar registers redraw callbacks for `workspace_changed` and
`window_title_changed`. Emit the former for changes in workspace existence,
names, focus, visibility, occupation, or urgency; emit the latter when focus or
the active title changes. Callback payloads are backend-defined; Cybar's redraw
callbacks ignore them and read the getters again. Getter results represent the
cached current state.
`fd()` is refreshed during runtime. Keep returning `-1` after closing the
connection until a new connection is available; the core also temporarily
disables polling after a hangup or descriptor error. Cached state remains
available to getters. `main()` never owns a loop that waits for future messages:
Cybar owns the single event loop and calls it for readable, hangup, or error
readiness. Handle EOF and errors without waiting. Exceptions from backend
calls and event callbacks are logged; the watchdog also measures backend calls.
An invalid backend connection at startup is fatal, and a failed backend connection
during reload preserves the active configuration.

The [example backend](../examples/backend.lua) implements the complete contract
using a simple newline protocol over `bar.socket`. It is a reference adapter for
an external bridge, not support for a particular WM. A subscription returns a
handle with `remove()` and `cancel()`:

```lua
local subscription = conn:on("workspace_changed", function() print("changed") end)
subscription:remove()
```

Cybar does not call returned unsubscribe handles on reload or shutdown. Adapters
manage explicit listener cleanup; the example's handle removes its callback from
the adapter's event table.

Socket data must be buffered because reads may split a line. Request/reply WM
protocols need their own asynchronous state machine; never wait synchronously
for an answer inside `connect`, `command`, or `main`.
