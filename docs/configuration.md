# Configuration reference

Configuration is ordinary Lua returning one table. `require("cybar")` provides
constructors for `clock`, `cpu`, `ram`, `cpu_ram`, `workspaces`, `window_title`,
`systray`, and `custom`. A constructor sets `kind` on its options and returns the
table. `{ kind = "clock", ... }` is equivalent to `bar.clock{ ... }`. Local Lua
files can be loaded with `require`: Cybar adds the configuration directory's
`?.lua` and `?/init.lua` patterns to `package.path`.

## Top-level options

| Field | Default | Meaning |
| --- | --- | --- |
| `bars` | Required nonempty array | Bar declarations |
| `theme.font` | `{ family="monospace", size=11 }` | Point size and optional ordered `fallback` families |
| `theme.colors` | Example palette | `background`, `text`, `text_active`, `text_dim`, `separator`, `urgent`, `hover`, `warning` |
| `theme.icon_theme` | Discovered system theme | Freedesktop icon theme name |
| `scale` | Automatic | Manual display scale between 0.25 and 16 |
| `tray_monitor` | `"primary"` | Output or bar name owning the single tray |
| `scroll_coalesce_ms` | `40` | Vertical scroll accumulation window; `0` delivers immediately |
| `watchdog_ms` | `100` | Warning threshold for Lua execution |
| `auto_reload` | `true` | Watch the config and required Lua files |
| `reload_debounce_ms` | `150` | Debounce interval after a watched change |
| `wm` | EWMH | `{ backend=require("my_backend"), opts={...} }` |

Colors accept `#rrggbb` or `#rrggbbaa`. Alpha is parsed for compatibility; the
current renderer produces opaque solid colors.

## Bars and monitors

```lua
{
  name = "main", monitor = "all", position = "bottom",
  size = { thickness = 30, length = "100%" },
  offset = { x = 0, y = 8 },
  reserve_space = true, corner_radius = 6,
  padding = 4, gap = 6, background = "#202020",
  overrides = {
    all = { padding = 6 },
    primary = { size = { thickness = 32 } },
    ["HDMI-1"] = { scale = 1.5 },
  },
  sections = { ... },
}
```

`monitor` selects `primary`, `all`, or a RandR output name. RandR changes rebuild
the bars using the current monitor list. Automatic scale uses `Xft.dpi` when
available, then the output's physical size, then 1. Manual scales range from
0.25 to 16 inclusive. A global `scale` is overridden
by the bar's scale, then monitor overrides. Overrides layer `all`, `primary` for
the primary output, and finally the exact output name. Named tables merge
recursively; arrays of modules or custom sections replace the base array.

Position accepts `top`, `bottom`, `left`, or `right`. Left and right are vertical.
`size.thickness` defaults to 28, and `size.length` to `"100%"`. Offsets default
to zero and measure inward from the anchored edge. Negative offsets are allowed.
Sizes must be positive; pixel values must be integers. All dimensions also accept
percentage strings. Bar percentages resolve against monitor dimensions; section
percentages resolve against bar dimensions. Font sizes are points and follow DPI.

When `reserve_space` is true, Cybar creates a dock window and publishes EWMH
struts, including its offset. Floating bars omit these struts. Hiding a bar with
`cybar toggle` releases its reserved space.

Bar names default to `bar-1`, `bar-2`, and so on. Give modules explicit names for
IPC. Otherwise names use `<bar>.<section>.<index>`. Module names must be unique
across configured entries; section names must be unique inside a bar. An `all`
entry creates a separate live copy on each monitor. Address a particular copy
with `name@monitor`, for example `cybar request status@HDMI-1 get`. An unqualified
name selects the primary-monitor instance, or the first available instance when
there is no matching copy on the primary monitor.

## Sections

Built-in sections accept either a module array or a table with `modules`:

```lua
sections = {
  left = { bar.workspaces{}, bar.window_title{} },
  middle = { padding = 2, gap = 8, modules = { bar.clock{} } },
  right = { bar.cpu{}, bar.ram{} },
  custom = {{
    name = "stats",
    position = { x = "70%", y = 0 },
    size = { width = 200, height = "100%" },
    padding = 2, gap = 4,
    modules = { bar.cpu_ram{} },
  }},
}
```

Custom sections require a name. Position and size values follow the same units
as the bar. Built-in sections anchor at the start, center, and end of the main
axis; custom sections accept explicit positions. Section gaps inherit the bar's
gap, and section padding defaults to zero. Modules retain their order. Overlap
is allowed and logged to stderr.

## Module style and separators

```lua
bar.cpu {
  name = "cpu",
  icon = "icons/cpu.svg",
  style = {
    font = { family = "monospace", size = 11, fallback = { "sans-serif" } },
    foreground = "#ffffff", background = "#303030",
    padding = 8, gap = 6, corner_radius = 4,
    icon_size = 16, icon_position = "before",
    alignment = "start", min_width = 40, max_width = 160,
    hover = { foreground = "#89b4fa" },
    warning = { foreground = "#f9e2af", background = "#402020" },
  },
}
```

Style may appear directly on a module or in `style`; the nested table takes
precedence. Partial font tables inherit the theme's family, size, and fallback
families; nested `style.font` then overrides the module's direct font. A specified
fallback array replaces the inherited list, and `{}` clears it. State tables are
`normal`, `hover`, `active`, `urgent`, and `warning`.
Each accepts `foreground` and `background`. `states={...}` is an equivalent
grouping, and `colors` accepts general colors plus state colors. Alignment is
`start`, `center`, or `end`. It places content within the module's available
extent; `min_width` can create room for alignment. Module `gap` separates text
from its icon and also separates tray icons. Section `gap` separates modules.

Icons are paths or freedesktop theme names. `~` expands to the home directory;
relative file icons and source file/socket paths resolve against the configuration
directory. Raster formats include PNG, JPEG, GIF, WebP, BMP, ICO, and TIFF. SVG is
rasterized at the display size. Theme icons use inherited themes and `hicolor`
fallback. Runtime-generated relative icon paths are relative to the process's
working directory; use an absolute path in a render function.

Optional bar separators draw between modules:

```lua
separator = { text = "│", color = "#45475a", spacing = 6 }
-- Or: separator = { icon = "icons/divider.svg", spacing = 4 }
```

## Built-in options and data

All built-ins support `render(data, handle)`, handlers, `style`, `icon`, and an
optional source override. CPU/RAM data uses bytes for `used` and `total`.

| Module | Options | Data |
| --- | --- | --- |
| `clock` | `format` in strftime syntax; optional `interval` in seconds | `{text,timestamp}` |
| `cpu` | `interval`; `format` with `{percent}` or `{usage}` | `{text,percent,usage,cpu,ram,used,total}` |
| `ram` | `interval`; `format` with `{percent}`, `{used}`, `{total}` | Same fields; `percent` is RAM usage |
| `cpu_ram` | `interval`; `format` with `{cpu}`, `{ram}`, `{used}`, `{total}` | Combined CPU and RAM fields |
| `workspaces` | `numbers=true` displays 1-based indices; `monitor_only=true` filters known output labels | Array of workspace tables from the backend |
| `window_title` | `max_length` (default 80 graphemes); `empty_text` | `{text,id,title,class}` when focused, or `{text}` when empty |
| `systray` | Style `icon_size`, `gap`, `padding` | `{count}` |

CPU sampling starts at zero until a second `/proc/stat` sample is available.
RAM uses `/proc/meminfo`'s `MemAvailable` or a free/buffers/cache fallback.
Intervals must be finite, positive, and at most one year (365 days). Unsupported
clock directives are rejected during validation. The clock updates on second or
minute boundaries according to its format.
Default workspaces handlers switch on left click and cycle on scroll. Set
`on_click_left=false` or `on_scroll=false` to disable these defaults.

Custom modules require a render function. See [the Lua API](lua-api.md) for
source tables and callback signatures.
