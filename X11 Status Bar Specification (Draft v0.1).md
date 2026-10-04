# Cybar: X11 Status Bar Specification (Draft v0.2)

Draft of October 3, 2026. Decisions marked **\[Decided\]** came from the planning Q&A. The remaining implementation decisions are resolved in section 11.

## 1. Overview

A configurable status bar for X11 desktops and window managers, in the same family as polybar and waybar. It is written in Rust and configured in Lua from `~/.config/`. The configuration is easy to write by hand and also supports full scripting.

**Goals**

- A bar on any screen edge (top, bottom, left, right) with configurable size and offset.
- Sections (left, middle, right, plus any number of custom sections) that hold modules with per-module alignment.
- Five built-in modules in v1, plus a custom-module API.
- Mouse interaction on every module: hover, left, middle, and right click, other mouse buttons, and scrolling.
- Icons from image files (PNG, JPG, SVG, and others) and from the system icon theme by name.
- Fonts, font sizes, and a full color scheme.

**Non-goals for v1**

- Wayland support.
- Tooltips and popup windows (a future version may add them).
- Gradients, shadows, and blur.
- A graphical config editor.

## 2. Technology Decisions

| Area | Decision |
| --- | --- |
| Language | Rust **\[Decided\]** |
| Lua binding | `mlua` crate **\[Decided\]** |
| Lua runtime | LuaJIT or Lua 5.4, chosen at build time through mutually exclusive Cargo features (`luajit`, `lua54`). Offered as two builds or release binaries, not a runtime switch; LuaJIT is the default **\[Decided\]** |
| X11 binding | `x11rb` (pure Rust) **\[Decided\]** |
| Rendering | `tiny-skia` (2D raster) **\[Decided\]** |
| Text | `cosmic-text` (shaping, fallback fonts, layout) **\[Decided\]** |
| SVG | `resvg` (shares tiny-skia, so rendering stays consistent) **\[Decided\]** |
| Raster images | `image` crate (PNG, JPG, and others) |
| Event loop | Single-threaded, `calloop` or `mio`, with timer-driven polling **\[Decided\]** |
| Appearance | Solid colors and rounded corners **\[Decided\]** |

## 3. Architecture

```
  X server  <-->  x11 layer (x11rb)  <-->  event loop (calloop)  <-->  Lua VM (mlua)
                       |                        |
                  window/struts            timers, fds, child processes
                       |                        |
                  renderer (tiny-skia + cosmic-text + resvg)  <--  module state
```

- **Core:** owns the event loop, X connection, window lifecycle, and rendering.
- **Config layer:** loads and validates the Lua config, converts it into Rust structs, and keeps Lua callbacks as registry references.
- **Module runtime:** each module has a data source and a renderer. Sources feed state into modules, and modules mark themselves dirty.
- **Renderer:** redraws only when something is dirty, and ideally only the dirty region, into a pixmap that is then copied to the bar window.
- **Single-threaded rule:** all Lua code runs on the main thread. Anything slow must go through non-blocking helpers (section 8).

## 4. Configuration

**Location:** `~/.config/cybar/config.lua`, honoring `XDG_CONFIG_HOME`. The program is named **Cybar** **\[Decided\]**. The binary, crate, and config directory are all `cybar`.

**Style \[Decided\]:** hybrid. The core is a declarative table returned from the config file. Because it is plain Lua, users can build the table dynamically with loops, functions, conditionals, and `require`d files. The docs should explain that the declarative API is ordinary Lua tables plus a few helper constructors.

**Example (illustrative; field names are provisional)**

```lua
local bar = require("cybar")

return {
  theme = {
    font = { family = "JetBrains Mono", size = 11 },
    colors = {
      background = "#1e1e2e", text = "#cdd6f4", text_active = "#89b4fa",
      text_dim = "#6c7086", separator = "#45475a", urgent = "#f38ba8", hover = "#b4befe", warning = "#f9e2af",
    },
  },

  bars = {
    {
      monitor = "primary",
      position = "top",              -- top | bottom | left | right
      size = { thickness = 28, length = "100%" },
      offset = { x = 0, y = 0 },
      reserve_space = true,          -- false = floating overlay
      corner_radius = 0,

      sections = {
        left   = { bar.workspaces{}, bar.window_title{ max_length = 60 } },
        middle = { bar.clock{ format = "%a %d %b  %H:%M" } },
        right  = {
          bar.cpu{ icon = "~/.config/cybar/icons/cpu.svg" },
          bar.ram{ icon = "~/.config/cybar/icons/ram.svg" },
          bar.systray{},
        },
        -- any number of custom sections, each with its own name, position, and size
        custom = { { name = "stats", position = { x = "70%", y = 0 }, size = { width = 200 }, modules = {} } },
      },
    },
  },
}
```

## 5. Layout and Geometry

### 5.1 Bar placement

- `position`: `top`, `bottom`, `left`, or `right`. Left and right bars are vertical.
- `size`: thickness (cross axis) and length (main axis).
- `offset`: x and y offsets from the anchored edge, for floating or inset bars.
- **Units \[Decided\]:** every dimension and position value accepts either an integer (pixels) or a string like `"50%"`. This applies to sizes and positions of the bar and of any section.
- Percentages resolve against the parent: the monitor for the bar, the bar for a section.

### 5.2 Space reservation \[Decided\]

`reserve_space` is configurable. When true, the bar is a dock window (`_NET_WM_WINDOW_TYPE_DOCK`) and sets `_NET_WM_STRUT_PARTIAL` and `_NET_WM_STRUT` so windows avoid it. When false, it floats as an overlay without struts. Struts must account for the offset.

### 5.3 Multi-monitor \[Decided\]

One bar instance per monitor, with per-monitor overrides. A config entry can target monitors by name (RandR output, e.g. `HDMI-1`), by `primary`, or by `all`, with overrides layered on top of a shared base. Monitor hotplug is handled through RandR events: bars are created, moved, or destroyed as outputs change.

### 5.4 Sections

- Built-in sections: `left`, `middle`, `right`.
- **Custom sections \[Decided\]:** any number of named custom sections (`custom = { ... }`), each with its own position and size, using the units rule above. Every section, built-in or custom, holds any number of modules. Overlapping sections are allowed, and the layout engine logs a warning when it detects overlap.
- Each module has an alignment within its section: `start`, `center`, or `end`. Modules in the same section are laid out in order along the main axis.
- Padding and gaps are configurable per bar, section, and module.

## 6. Appearance

- **Fonts:** a global default family and size, with per-module overrides, plus ordered fallback families.
- **Colors:** background, text, active text, dim text, separator, and urgent, each overridable per module. Colors are given as hex strings (`#rrggbb`, with optional alpha parsed even though transparency is not a v1 feature).
- **Shapes \[Decided\]:** solid colors and rounded corners, configurable per bar and per module. There is no transparency, border, or gradient support in v1.
- **Separators:** a separator element between modules, with configurable glyph or image, color, and spacing.
- **Icons \[Decided\]:** any module can take an `icon` given as either a file path (PNG, JPG, SVG, and others, with `~` expansion) or a system icon theme name, looked up through the freedesktop icon theme specification with the user's current theme and a `hicolor` fallback. SVG is rasterized at the display size. Icons are cached and re-rasterized when the size or scale changes. Icon size and position (before or after the text) are configurable.
- **Per-state colors \[Decided\]:** every module supports colors for the states `normal`, `hover`, `active`, `urgent`, and `warning`, inheriting from the theme and overridable per module. The `hover` state is applied automatically while the pointer is over a module, in addition to calling `on_hover_enter`. Lua callbacks (section 8) can change a module's state or style at runtime, for example switching to `warning` when CPU usage is high. Built-in modules set states where it makes sense: the focused workspace is `active`, and a workspace holding an urgent window is `urgent`.

* **Scaling \[Decided\]:** automatic by default (from `Xft.dpi` in the X resources, falling back to the RandR physical size, per monitor), with a manual `scale` factor that overrides it. The factor can be set globally, per bar, or per monitor. Pixel values are logical and are multiplied by the scale, while percentages are unaffected and font sizes in points follow the DPI.

## 7. Modules

### 7.1 Unified module interface \[Decided\]

Every module, built-in or custom, follows one interface with a pluggable data source:

- **source:** where data comes from.
- **render:** turns the latest data into a display description (text, icon, and style).
- **handlers:** `on_click_left`, `on_click_middle`, `on_click_right`, `on_click_button` (any other button, with the button number passed in), `on_scroll` (receives `dx` and `dy`), `on_hover_enter`, `on_hover_leave`, and `on_message` (receives text sent with `cybar msg` or `cybar request`; for a request, its return value becomes the reply).
- **style and alignment:** per-state colors, font, padding, and alignment.

### 7.2 Data sources \[Decided\]

| Source type | Behavior |
| --- | --- |
| `poll` | A Lua function called every `interval` seconds, returning a value or table |
| `command` | A shell command run every `interval` seconds (non-blocking), with its stdout parsed or passed to `render` |
| `stream` | Event-driven: watches a file, a Unix socket, or a long-running command's stdout and updates on each new line or change |

Built-in modules use internal sources (X11 events, `/proc`, timers) behind the same interface. Users can override a built-in module's `render` or handlers.

### 7.3 Built-in modules for v1

1. **Workspaces:** shows workspace names or numbers from `_NET_DESKTOP_NAMES`, `_NET_NUMBER_OF_DESKTOPS`, and `_NET_CURRENT_DESKTOP`. Clicking a workspace sends a `_NET_CURRENT_DESKTOP` client message to switch. Highlights the active workspace and optionally marks occupied or urgent ones.
2. **Window title:** shows the title of the active window (`_NET_ACTIVE_WINDOW`, `_NET_WM_NAME`) with truncation or ellipsis and a configurable empty-state text.
3. **Systray:** implements the XEMBED system tray protocol (`_NET_SYSTEM_TRAY_S<n>` selection, dock requests, icon embedding and sizing). Only one bar owns the tray, so a `tray_monitor` option is needed with multi-monitor. Click events must reach the tray icons.
4. **Clock:** a `strftime`-style format string with timer-driven updates (per second only when the format needs it).
5. **CPU and RAM:** read from `/proc/stat` and `/proc/meminfo`. Configurable interval and display format (percentage, used and total, or both). They can be configured as one module or two.

### 7.4 WM integration \[Decided\]

EWMH is the default and the only backend that ships in v1. The workspaces and window title modules never talk to the WM directly. They go through a backend contract, so any WM can be supported by supplying a Lua backend instead of changing the Rust code. The built-in EWMH backend implements the same contract.

**Backend contract \[Decided\]:** a backend is a Lua module whose `connect(opts)` function returns a **connection object** with these methods:

| Method | Purpose |
| --- | --- |
| `conn:get_workspaces()` | Returns a list of workspaces, each with `id`, `name`, `index`, `monitor`, and the flags `focused`, `visible`, `urgent`, and `occupied`. |
| `conn:get_active_window()` | Returns the active window as `{ id, title, class }`, or `nil` when no window has focus. |
| `conn:command(name, ...)` | Sends a command to the WM and returns `ok, err`. Standard names include `switch_workspace` (used by the workspaces module). Backends may also accept raw WM-specific command strings. |
| `conn:subscribe(event, callback)` and `conn:on(event, callback)` | Aliases. Register a callback for an event and return a handle for unsubscribing. |
| `conn:main()` | Processes pending WM messages and dispatches events to the registered callbacks. |
| `conn:fd()` | Returns the file descriptor the core polls. The core calls `main()` when it becomes readable. |

**Standard events:** `workspace_changed` fires when the workspace list or the focused workspace changes (switch, create, remove, rename, or urgency change). `window_title_changed` fires whenever the title the bar should show changes, including when focus moves to another window. Modules react by calling `get_workspaces()` or `get_active_window()` again. Backends may emit extra events that Lua callbacks can subscribe to.

**Event loop:** the core owns the single event loop, so `main()` must not block in a loop of its own. It handles everything pending and returns. Backends talk to the WM through the core's non-blocking socket helper (`bar.socket`, section 8), because plain Lua has no socket support. Errors raised inside a backend or its callbacks are caught and logged to the CLI, and never stop the bar.

```lua
-- my_backend.lua
local bar = require("cybar")
local M = {}

function M.connect(opts)
  local sock = bar.socket.connect(opts.path)
  local handlers = {}
  local conn = {}

  function conn:get_workspaces() end      -- query the WM, return a list
  function conn:get_active_window() end   -- return { id, title, class } or nil
  function conn:command(name, ...) end    -- send a command to the WM
  function conn:on(event, cb)
    handlers[event] = handlers[event] or {}
    table.insert(handlers[event], cb)
  end
  conn.subscribe = conn.on
  function conn:fd() return sock:fd() end
  function conn:main() end                -- read pending data, call handlers

  return conn
end

return M
```

In the config, a backend is selected with something like `wm = { backend = require("my_backend"), opts = { path = "..." } }`. The default is the built-in EWMH backend.

## 8. Interaction and Lua API

**Events \[Decided\]:** hover, left, middle, and right click, other mouse buttons (X buttons 8 and above), and scrolling. All behavior is a Lua callback (`on_hover_enter`, `on_hover_leave`, `on_click_left`, `on_click_middle`, `on_click_right`, `on_click_button`, `on_scroll`, `on_message`), with no built-in tooltip or popup in v1. Built-in modules ship with default handlers (clicking a workspace switches to it, and scrolling over the workspaces module cycles through them) that users can override or disable.

- **Scroll signature \[Decided\]:** one handler, `on_scroll(handle, dx, dy)`, replaces separate up and down handlers. Units are wheel notches, so 1.0 is one notch of a classic wheel and touchpads deliver fractions.
- **Coalescing \[Decided\]:** vertical scroll is accumulated over a window of about 40 ms and delivered as one call. The window is configurable (`scroll_coalesce_ms`, with a global default and a per-module override), and `0` turns coalescing off. Horizontal scroll is passed through immediately. Vertical calls arrive with `dx = 0`, and horizontal calls with `dy = 0`.
- **Smooth scrolling \[Decided\]:** the bar uses XInput2 smooth-scroll valuators when the server and device provide them (fractional deltas). Otherwise it falls back to legacy button 4 to 7 events as whole-notch steps. Legacy events that XInput2 emulates are ignored, so one physical scroll never fires twice.

Each callback receives a **module handle** that can:

- Change text, icon, and style (colors, font) at runtime.
- Read current module data.
- Trigger a redraw.

**Because the event loop is single-threaded, callbacks must not block.** The API provides non-blocking helpers:

- `bar.spawn(cmd, callback)` runs a command asynchronously and calls back with its output.
- `bar.after(seconds, fn)` and `bar.every(seconds, fn)` schedule work on the event loop.
- `bar.exec(cmd)` fires and forgets.

* `bar.socket.connect(path, handlers)` opens a non-blocking Unix socket registered with the event loop and calls back on data and on close. WM backends use it because plain Lua has no socket support. The name is provisional.

A watchdog should log a warning when a Lua callback runs longer than a configurable threshold.

## 9. Config Reloading \[Decided\]

- **Signal or CLI:** `SIGUSR1` reloads the config, and a CLI subcommand (for example `cybar reload`) does the same.
- **Auto-reload:** watch the config file (and `require`d Lua files) with inotify, debounced.
- **Failure behavior \[Decided\]:** if the new config fails to load or validate, keep running the previous config and log the error to the CLI: the daemon's standard error (the terminal when it is started from one), and `cybar reload` also prints the error it receives back. Nothing is shown on the bar and no desktop notification is sent.

## 10. Risks and Hard Parts

- **Systray:** XEMBED by hand on x11rb is the single hardest module. Plan time for it and for compatibility testing with common tray apps.
- **Text quality and performance:** `cosmic-text` is capable but needs glyph caching and dirty-region redraw to stay light on CPU and memory.
- **Color emoji and fallback fonts:** verify that `cosmic-text` handles the fonts users will actually use.
- **Lua runtime split:** two Cargo feature builds must both be tested in CI, and Lua code that works on one runtime (for example 5.4 integer division or `goto`) may not on the other. Because LuaJIT is the default, documentation and examples target the common subset (Lua 5.1 semantics plus what 5.4 also supports).
- **Strut correctness across WMs:** behavior varies, so test across several window managers.
- **Blocking Lua:** one slow callback freezes the bar, so the non-blocking API design matters.

* **Icon theme lookup:** the freedesktop icon theme specification has many edge cases (theme inheritance, size matching, scalable directories). Prefer an existing crate over rewriting it.
* **Scaling:** mixing logical pixels, percentages, and per-monitor scale needs one consistent coordinate model from the start.
* **IPC security:** the control socket must be private to the user (in `$XDG_RUNTIME_DIR`, with restrictive permissions).
* **License compatibility:** with GPL-3.0-or-later, every dependency must be checked, and the check should run in CI.

- **WM backend stalls:** a backend's `main()` must stay non-blocking, and a slow or buggy backend can still stall the bar. Document this clearly and apply the callback watchdog (section 8) to backend calls too.
- **XInput2 scrolling:** smooth scrolling needs the XInput extension through `x11rb` (version 2.1 or later on the server) and careful handling of emulated legacy button events. Behavior differs between mice and touchpads.

## 11. Resolved Implementation Decisions

1. **Backend `main()` semantics [Decided]:** the core polls `conn:fd()` and calls `conn:main()` when readable. `main()` drains pending messages and returns without waiting; it never owns a blocking event loop. Backend calls use the callback watchdog and error logging.
2. **Request timeout [Decided]:** clients default to 1000 ms. `--timeout <ms>` overrides this for requests, including reload, quit, and toggle. A timeout exits nonzero and late replies are discarded.
3. **Startup failure [Decided]:** invalid configuration at first launch exits nonzero and prints the error to stderr. Failed reloads preserve the active configuration.

## 12. Suggested Milestones

1. Window creation, struts, and rendering of a solid bar, with position, size, offset, and rounded corners.
2. Lua config loading, layout engine for sections, and px/% units.
3. Text rendering, fonts, colors, per-state colors, and icons (raster, SVG, and icon theme lookup).
4. Clock, CPU and RAM modules, and the unified module and source interface.
5. Workspaces and window title through EWMH, with mouse events and Lua callbacks.
6. Custom modules with all three source types, plus the non-blocking API.
7. Systray (XEMBED).
8. Multi-monitor support, reloading, DPI scaling, the CLI and IPC (reload, quit, toggle, msg), and polish. Then packaging and release (section 14).

## 13. CLI and IPC \[Decided\]

The `cybar` binary is both the daemon and the control client. Running `cybar` with no subcommand starts the bar (optionally with `--config <path>`). Every other subcommand connects to the running instance.

| Command | Wire form | Behavior |
| --- | --- | --- |
| `cybar reload` | request `reload` | Reload the config (same effect as `SIGUSR1`). The reply arrives when the reload finishes. On failure the error is returned and printed. |
| `cybar quit` | request `quit` | Shut down all bars cleanly, releasing struts and the systray selection. |
| `cybar toggle [target]` | request `toggle` | Show or hide a bar. The optional target is a bar or monitor name, defaulting to all bars. Hidden bars release their struts. |
| `cybar msg <module> <text...>` | msg `module.message` | Fire-and-forget. Delivered to the module's `on_message` handler. The client exits as soon as the message is written. |
| `cybar request <module> <text...>` | request `module.message` | Same delivery, but the client waits for a reply, which is the return value of `on_message`. Accepts `--timeout <ms>`. |

- Custom modules need a unique `name` to be addressable by `msg` and `request`.
- **Transport \[Decided\]:** a Unix domain socket in `$XDG_RUNTIME_DIR` (one per X display), readable and writable only by the user.
- **Protocol \[Decided\]:** JSON Lines. Each message is one UTF-8 JSON object on one line, in both directions.
- **Symmetric envelope \[Decided\]:** both sides use the same envelope, `{ "id", "type", "method", "params", "result", "error" }`. The `type` is `msg` (fire-and-forget, never answered), `request` (expects a reply), or `reply` (answers the request with the same `id`, carrying either `result` or `error`). The `id` is chosen by the sender and is unique per connection. In v1 only clients send messages and requests, and only the daemon sends replies, but the envelope allows daemon-initiated traffic later.
- **Timeouts \[Decided\]:** `request` has a client-side timeout. When it expires the client exits non-zero with a timeout error, and a late reply is discarded.
- The client exits with a non-zero status and a clear message when no instance is running or the target does not exist.

Example exchange:

```json
{"id": 1, "type": "request", "method": "module.message", "params": {"module": "vpn", "text": "status"}}
{"id": 1, "type": "reply", "result": "connected"}
{"type": "msg", "method": "module.message", "params": {"module": "vpn", "text": "refresh"}}
{"id": 2, "type": "request", "method": "reload"}
{"id": 2, "type": "reply", "error": "config.lua:12: unexpected symbol near '}'"}
```

## 14. Project, Licensing, and Distribution \[Decided\]

- **Name:** Cybar. The binary, crate, and config directory are all `cybar`.
- **License:** GPL-3.0-or-later. Ship a `LICENSE` file, put SPDX identifiers in source headers, and set `license = "GPL-3.0-or-later"` in `Cargo.toml`. Every dependency's license must be checked for GPLv3 compatibility (for example with `cargo deny`). Most crates in this stack are MIT or Apache-2.0, which are compatible, but verify each one, including `resvg`.
- **Cargo (crates.io):** `cargo install cybar` must work without system Lua. The default build bundles LuaJIT through `mlua`'s vendored feature (which needs a C compiler at build time), and `--no-default-features --features lua54` builds with Lua 5.4 instead.
- **Debian package (.deb):** built from the release with `cargo-deb` or equivalent. Runtime dependencies stay minimal because the X11 and rendering stack is pure Rust.
- **AUR:** a release package (`cybar`) and a development package (`cybar-git`), both with a `PKGBUILD` that builds from source.
- **Both Lua runtimes \[Decided\]:** LuaJIT is the default, for ecosystem compatibility and for the JIT on hot callbacks. Lua 5.4 is available behind the `lua54` build feature, and the two features are mutually exclusive. CI builds and tests both. For the deb and AUR packages, the default package is the LuaJIT build, with a separate Lua 5.4 variant (for example `cybar-lua54`) that conflicts with it.
- **Shipped with every package:** a man page, shell completions, and an example config in the documentation directory.
