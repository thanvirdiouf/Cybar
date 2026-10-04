# Implementation status

This release targets Linux with X11. It contains bar geometry/struts, Lua
configuration, raster/text/SVG rendering, built-in and custom modules, mouse
callbacks, RandR rebuilding, EWMH/Lua backends, XEMBED docking, automatic reload,
and JSON Lines control IPC. The original open decisions are resolved: backend
`main()` drains pending data and returns; requests default to 1000 ms with an
override; invalid first-start configuration exits nonzero.

Practical limits:

- Rendering redraws each visible bar when its generation is dirty. Text and icon
  caches are bounded, but partial dirty-region repainting is not implemented.
- Each framebuffer is limited to 256 MiB. Physical bar dimensions are capped at
  32767 pixels; positions must fit X11's signed 16-bit coordinates. Invalid
  geometry is rejected during preparation so a failed reload retains the bar.
- EWMH desktops are global. Output-specific workspace association requires a
  backend that supplies monitor names; filtering cannot invent associations.
- One bar owns the X11 system tray selection. Another tray owner prevents Cybar
  from taking it. The implementation supports embedding and mapped-state updates,
  but actual application/WM/device compatibility needs real-desktop testing.
- File sources represent file snapshots, not append-only tail streams. Unix
  stream sources require a live endpoint at preparation and do not automatically
  reconnect after EOF. Command streams are restarted after their child exits.
- The callback watchdog reports slow execution after it returns. Lua code that
  blocks or loops indefinitely still stalls the main thread.
- Pixel alpha is accepted but rendered opaque. Tooltips, popups, transparency,
  blur, shadows, gradients, and Wayland are outside this version's scope.
- Package recipes and CI are provided. No crates.io/AUR publication or release
  repository is assumed. AUR templates need the actual repository URL and tagged
  releases before distribution.

The parser, geometry, IPC, runtime, renderer, and isolated X11 tests are described
in [BUILD.md](../BUILD.md#formatting-tests-and-linting), along with the CI matrix
and daemon/CLI smoke tests.

IPC addresses replicated modules with `name@monitor`; unqualified names select
the primary copy or the first matching instance. File streams use notifications
when available and fall back to polling. Source parsers keep the previous data
after errors. Detached `exec` commands continue through reload and shutdown.

Xvfb tests validate the implemented protocol behavior against a controlled X
server. They do not establish compatibility with every WM, tray app, font, or
physical smooth-scroll device.
