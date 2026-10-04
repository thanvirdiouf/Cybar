// SPDX-License-Identifier: GPL-3.0-or-later
//! X11 windows, EWMH, RandR, XI2 input, and the XEMBED tray.
use crate::model::*;
use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::{HashMap, HashSet},
    os::fd::{AsRawFd, RawFd},
};
use x11rb::protocol::{
    randr::ConnectionExt as _, shape::ConnectionExt as _, xinput::ConnectionExt as _,
};
use x11rb::{
    CURRENT_TIME, NONE,
    connection::{Connection, RequestConnection},
    protocol::{Event, randr, shape, xinput, xproto::*},
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

x11rb::atom_manager! {
    Atoms: AtomsCookie {
        UTF8_STRING, MANAGER, WM_PROTOCOLS, WM_DELETE_WINDOW, WM_NAME, WM_CLASS, WM_HINTS,
        RESOURCE_MANAGER, _NET_WM_WINDOW_TYPE, _NET_WM_WINDOW_TYPE_DOCK,
        _NET_WM_STRUT, _NET_WM_STRUT_PARTIAL, _NET_WM_STATE, _NET_WM_STATE_ABOVE,
        _NET_WM_STATE_STICKY, _NET_WM_STATE_SKIP_TASKBAR, _NET_WM_STATE_SKIP_PAGER,
        _NET_WM_STATE_DEMANDS_ATTENTION, _NET_WM_DESKTOP, _NET_WM_NAME,
        _NET_NUMBER_OF_DESKTOPS, _NET_DESKTOP_NAMES, _NET_CURRENT_DESKTOP,
        _NET_DESKTOP_VIEWPORT, _NET_ACTIVE_WINDOW, _NET_CLIENT_LIST,
        _NET_SYSTEM_TRAY_OPCODE, _NET_SYSTEM_TRAY_ORIENTATION,
        _NET_SYSTEM_TRAY_VISUAL, _NET_SYSTEM_TRAY_ICON_SIZE, _XEMBED, _XEMBED_INFO,
    }
}

#[derive(Clone, Copy, Debug)]
pub struct BarWindow {
    pub window: u32,
    pub geometry: Geometry,
    pub scale: f64,
}

#[derive(Clone, Debug)]
pub enum XEvent {
    Expose {
        window: u32,
    },
    Pointer {
        window: u32,
        x: f32,
        y: f32,
    },
    Leave {
        window: u32,
    },
    Button {
        window: u32,
        x: f32,
        y: f32,
        button: u8,
    },
    Scroll {
        window: u32,
        x: f32,
        y: f32,
        dx: f64,
        dy: f64,
    },
    MonitorsChanged,
    WmChanged,
    TrayChanged,
    Close {
        window: u32,
    },
}

struct WindowState {
    geometry: Geometry,
    edge: Edge,
    reserve: bool,
    visible: bool,
    xi: bool,
    gc: u32,
    pixmap: u32,
}
struct TrayIcon {
    window: u32,
    mapped: bool,
}
struct Tray {
    bar: u32,
    manager: u32,
    icons: Vec<TrayIcon>,
    vertical: bool,
    icon_size: u32,
    rect: Option<Rect>,
}
#[derive(Clone, Copy)]
struct ScrollAxis {
    horizontal: bool,
    increment: f64,
    last: f64,
}

pub struct Platform {
    conn: RustConnection,
    screen: usize,
    root: u32,
    atoms: Atoms,
    tray_selection: u32,
    depth: u8,
    visual: u32,
    red_mask: u32,
    green_mask: u32,
    blue_mask: u32,
    bytes_per_pixel: usize,
    scanline_pad: usize,
    little_endian: bool,
    randr: bool,
    shape: bool,
    xi: bool,
    windows: HashMap<u32, WindowState>,
    tray: Option<Tray>,
    scroll_axes: HashMap<(u16, u16), ScrollAxis>,
    watched_clients: HashSet<u32>,
}

impl Platform {
    pub fn connect() -> Result<Self> {
        Self::connect_to(None)
    }

    /// Connect to a specific display, including isolated servers used in integration tests.
    pub fn connect_to(display: Option<&str>) -> Result<Self> {
        let (conn, screen) =
            x11rb::connect(display).context("cannot connect to the X server (check DISPLAY)")?;
        let atoms = Atoms::new(&conn)?.reply()?;
        let s = &conn.setup().roots[screen];
        let visual = s
            .allowed_depths
            .iter()
            .flat_map(|d| &d.visuals)
            .find(|v| v.visual_id == s.root_visual)
            .context("X server root visual is unavailable")?;
        ensure!(
            visual.class == VisualClass::TRUE_COLOR,
            "Cybar requires a TrueColor X11 visual"
        );
        let format = conn
            .setup()
            .pixmap_formats
            .iter()
            .find(|f| f.depth == s.root_depth)
            .context("X server has no pixel format for the root depth")?;
        ensure!(
            matches!(format.bits_per_pixel, 16 | 24 | 32),
            "unsupported X11 pixel format"
        );
        let tray_selection = conn
            .intern_atom(false, format!("_NET_SYSTEM_TRAY_S{screen}").as_bytes())?
            .reply()?
            .atom;
        let root = s.root;
        let mut p = Self {
            root,
            screen,
            atoms,
            tray_selection,
            depth: s.root_depth,
            visual: s.root_visual,
            red_mask: visual.red_mask,
            green_mask: visual.green_mask,
            blue_mask: visual.blue_mask,
            bytes_per_pixel: usize::from(format.bits_per_pixel / 8),
            scanline_pad: usize::from(format.scanline_pad / 8),
            little_endian: conn.setup().image_byte_order == ImageOrder::LSB_FIRST,
            randr: false,
            shape: false,
            xi: false,
            conn,
            windows: HashMap::new(),
            tray: None,
            scroll_axes: HashMap::new(),
            watched_clients: HashSet::new(),
        };
        p.conn
            .change_window_attributes(
                root,
                &ChangeWindowAttributesAux::new().event_mask(
                    EventMask::PROPERTY_CHANGE
                        | EventMask::STRUCTURE_NOTIFY
                        | EventMask::SUBSTRUCTURE_NOTIFY
                        | EventMask::FOCUS_CHANGE,
                ),
            )?
            .check()?;
        p.randr = p
            .conn
            .randr_query_version(1, 5)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some();
        if p.randr {
            p.conn
                .randr_select_input(
                    root,
                    randr::NotifyMask::SCREEN_CHANGE
                        | randr::NotifyMask::CRTC_CHANGE
                        | randr::NotifyMask::OUTPUT_CHANGE
                        | randr::NotifyMask::OUTPUT_PROPERTY,
                )?
                .check()?;
        }
        p.shape = p
            .conn
            .shape_query_version()
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some();
        p.xi = p
            .conn
            .xinput_xi_query_version(2, 1)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some_and(|v| v.major_version > 2 || (v.major_version == 2 && v.minor_version >= 1));
        if p.xi
            && (p
                .conn
                .xinput_xi_select_events(
                    root,
                    &[xinput::EventMask {
                        deviceid: 0,
                        mask: vec![
                            xinput::XIEventMask::DEVICE_CHANGED | xinput::XIEventMask::HIERARCHY,
                        ],
                    }],
                )?
                .check()
                .is_err()
                || p.refresh_scroll_axes().is_err())
        {
            p.xi = false;
        }
        p.watch_clients()?;
        p.conn.flush()?;
        Ok(p)
    }

    pub fn fd(&self) -> RawFd {
        self.conn.stream().as_raw_fd()
    }

    fn resource_scale(&self) -> Option<f64> {
        let resources = self
            .property8(
                self.root,
                self.atoms.RESOURCE_MANAGER,
                AtomEnum::STRING.into(),
            )
            .ok()?;
        parse_xft_scale(&String::from_utf8_lossy(&resources))
    }

    pub fn monitors(&self) -> Result<Vec<Monitor>> {
        let scale = self.resource_scale();
        let mut monitors = Vec::new();
        if self.randr {
            let primary = self
                .conn
                .randr_get_output_primary(self.root)
                .ok()
                .and_then(|c| c.reply().ok())
                .map(|r| r.output);
            if let Ok(resources) = self
                .conn
                .randr_get_screen_resources_current(self.root)?
                .reply()
            {
                for output in resources.outputs {
                    let Ok(info) = self
                        .conn
                        .randr_get_output_info(output, resources.config_timestamp)?
                        .reply()
                    else {
                        continue;
                    };
                    if info.connection != randr::Connection::CONNECTED || info.crtc == NONE {
                        continue;
                    }
                    let Ok(crtc) = self
                        .conn
                        .randr_get_crtc_info(info.crtc, resources.config_timestamp)?
                        .reply()
                    else {
                        continue;
                    };
                    if crtc.width == 0 || crtc.height == 0 {
                        continue;
                    }
                    let rotated = u16::from(crtc.rotation)
                        & u16::from(randr::Rotation::ROTATE90 | randr::Rotation::ROTATE270)
                        != 0;
                    let (mm_width, mm_height) = if rotated {
                        (info.mm_height, info.mm_width)
                    } else {
                        (info.mm_width, info.mm_height)
                    };
                    monitors.push(Monitor {
                        name: String::from_utf8_lossy(&info.name).into_owned(),
                        primary: primary == Some(output),
                        geometry: Geometry {
                            x: crtc.x.into(),
                            y: crtc.y.into(),
                            width: crtc.width.into(),
                            height: crtc.height.into(),
                        },
                        scale: scale.unwrap_or_else(|| {
                            physical_scale(
                                crtc.width.into(),
                                crtc.height.into(),
                                mm_width,
                                mm_height,
                            )
                        }),
                    });
                }
            }
        }
        if monitors.is_empty() {
            let g = self.conn.get_geometry(self.root)?.reply()?;
            let s = &self.conn.setup().roots[self.screen];
            monitors.push(Monitor {
                name: format!("screen-{}", self.screen),
                primary: true,
                geometry: Geometry {
                    x: 0,
                    y: 0,
                    width: g.width.into(),
                    height: g.height.into(),
                },
                scale: scale.unwrap_or_else(|| {
                    physical_scale(
                        g.width.into(),
                        g.height.into(),
                        s.width_in_millimeters.into(),
                        s.height_in_millimeters.into(),
                    )
                }),
            });
        }
        monitors.sort_by_key(|m| (!m.primary, m.geometry.x, m.geometry.y, m.name.clone()));
        // Mirrored outputs share one bar; prefer the primary output's name.
        let mut seen = HashSet::new();
        monitors.retain(|m| {
            seen.insert((
                m.geometry.x,
                m.geometry.y,
                m.geometry.width,
                m.geometry.height,
            ))
        });
        if !monitors.iter().any(|m| m.primary) {
            monitors[0].primary = true;
        }
        Ok(monitors)
    }

    pub fn create_bar(
        &mut self,
        spec: &BarSpec,
        monitor: &Monitor,
        global_scale: Option<f64>,
    ) -> Result<BarWindow> {
        let scale = spec.scale.or(global_scale).unwrap_or(monitor.scale);
        ensure!(
            scale.is_finite() && scale > 0.0,
            "bar scale must be positive and finite"
        );
        let geometry = bar_geometry(spec, monitor, scale);
        let x =
            i16::try_from(geometry.x).context("bar x position is outside X11 coordinate range")?;
        let y =
            i16::try_from(geometry.y).context("bar y position is outside X11 coordinate range")?;
        validate_framebuffer(geometry)?;
        let window = self.conn.generate_id()?;
        let gc = self.conn.generate_id()?;
        let pixmap = self.conn.generate_id()?;
        let base_mask =
            EventMask::EXPOSURE | EventMask::STRUCTURE_NOTIFY | EventMask::PROPERTY_CHANGE;
        let pointer_mask = EventMask::POINTER_MOTION
            | EventMask::ENTER_WINDOW
            | EventMask::LEAVE_WINDOW
            | EventMask::BUTTON_PRESS;
        self.conn
            .create_window(
                self.depth,
                window,
                self.root,
                x,
                y,
                geometry.width as u16,
                geometry.height as u16,
                0,
                WindowClass::INPUT_OUTPUT,
                self.visual,
                &CreateWindowAux::new()
                    .background_pixel(0)
                    .border_pixel(0)
                    .event_mask(base_mask | pointer_mask),
            )?
            .check()?;
        let result = (|| -> Result<bool> {
            self.conn
                .create_gc(gc, window, &CreateGCAux::new().graphics_exposures(0))?
                .check()?;
            self.conn
                .create_pixmap(
                    self.depth,
                    pixmap,
                    window,
                    geometry.width as u16,
                    geometry.height as u16,
                )?
                .check()?;
            self.conn.change_property32(
                PropMode::REPLACE,
                window,
                self.atoms._NET_WM_WINDOW_TYPE,
                AtomEnum::ATOM,
                &[self.atoms._NET_WM_WINDOW_TYPE_DOCK],
            )?;
            self.conn.change_property32(
                PropMode::REPLACE,
                window,
                self.atoms._NET_WM_STATE,
                AtomEnum::ATOM,
                &[
                    self.atoms._NET_WM_STATE_ABOVE,
                    self.atoms._NET_WM_STATE_STICKY,
                    self.atoms._NET_WM_STATE_SKIP_TASKBAR,
                    self.atoms._NET_WM_STATE_SKIP_PAGER,
                ],
            )?;
            self.conn.change_property32(
                PropMode::REPLACE,
                window,
                self.atoms._NET_WM_DESKTOP,
                AtomEnum::CARDINAL,
                &[u32::MAX],
            )?;
            self.conn.change_property8(
                PropMode::REPLACE,
                window,
                self.atoms._NET_WM_NAME,
                self.atoms.UTF8_STRING,
                format!("Cybar: {}", spec.name).as_bytes(),
            )?;
            self.conn.change_property8(
                PropMode::REPLACE,
                window,
                self.atoms.WM_NAME,
                AtomEnum::STRING,
                b"Cybar",
            )?;
            self.conn.change_property8(
                PropMode::REPLACE,
                window,
                self.atoms.WM_CLASS,
                AtomEnum::STRING,
                b"cybar\0Cybar\0",
            )?;
            // ICCCM InputHint = false: dock windows never steal keyboard focus.
            self.conn.change_property32(
                PropMode::REPLACE,
                window,
                self.atoms.WM_HINTS,
                self.atoms.WM_HINTS,
                &[1, 0, 0, 0, 0, 0, 0, 0, 0],
            )?;
            self.conn.change_property32(
                PropMode::REPLACE,
                window,
                self.atoms.WM_PROTOCOLS,
                AtomEnum::ATOM,
                &[self.atoms.WM_DELETE_WINDOW],
            )?;
            if self.shape && spec.corner_radius > 0.0 {
                let rectangles = rounded_rectangles(
                    geometry.width,
                    geometry.height,
                    (spec.corner_radius as f64 * scale) as f32,
                );
                self.conn
                    .shape_rectangles(
                        shape::SO::SET,
                        shape::SK::BOUNDING,
                        ClipOrdering::Y_SORTED,
                        window,
                        0,
                        0,
                        &rectangles,
                    )?
                    .check()?;
            }
            let xi = self.xi
                && self
                    .conn
                    .xinput_xi_select_events(
                        window,
                        &[xinput::EventMask {
                            deviceid: 1,
                            mask: vec![
                                xinput::XIEventMask::MOTION
                                    | xinput::XIEventMask::BUTTON_PRESS
                                    | xinput::XIEventMask::ENTER
                                    | xinput::XIEventMask::LEAVE,
                            ],
                        }],
                    )?
                    .check()
                    .is_ok();
            if xi {
                self.conn
                    .change_window_attributes(
                        window,
                        &ChangeWindowAttributesAux::new().event_mask(base_mask),
                    )?
                    .check()?;
            }
            Ok(xi)
        })();
        let xi = match result {
            Ok(xi) => xi,
            Err(error) => {
                let _ = self.conn.free_gc(gc);
                let _ = self.conn.free_pixmap(pixmap);
                let _ = self.conn.destroy_window(window);
                return Err(error);
            }
        };
        self.windows.insert(
            window,
            WindowState {
                geometry,
                edge: spec.position,
                reserve: spec.reserve_space,
                visible: true,
                xi,
                gc,
                pixmap,
            },
        );
        self.update_struts(window)?;
        self.conn.map_window(window)?.check()?;
        self.conn.flush()?;
        Ok(BarWindow {
            window,
            geometry,
            scale,
        })
    }

    fn update_struts(&self, window: u32) -> Result<()> {
        let state = self.windows.get(&window).context("unknown bar window")?;
        if state.reserve && state.visible {
            let g = self.conn.get_geometry(self.root)?.reply()?;
            let partial =
                strut_partial(state.geometry, state.edge, g.width.into(), g.height.into());
            self.conn.change_property32(
                PropMode::REPLACE,
                window,
                self.atoms._NET_WM_STRUT_PARTIAL,
                AtomEnum::CARDINAL,
                &partial,
            )?;
            self.conn.change_property32(
                PropMode::REPLACE,
                window,
                self.atoms._NET_WM_STRUT,
                AtomEnum::CARDINAL,
                &partial[..4],
            )?;
        } else {
            self.conn
                .delete_property(window, self.atoms._NET_WM_STRUT_PARTIAL)?;
            self.conn
                .delete_property(window, self.atoms._NET_WM_STRUT)?;
        }
        Ok(())
    }

    pub fn destroy_bar(&mut self, window: u32) -> Result<()> {
        if self.tray.as_ref().is_some_and(|tray| tray.bar == window) {
            self.release_tray()?;
        }
        if let Some(s) = self.windows.remove(&window) {
            self.conn
                .delete_property(window, self.atoms._NET_WM_STRUT_PARTIAL)?;
            self.conn
                .delete_property(window, self.atoms._NET_WM_STRUT)?;
            self.conn.unmap_window(window)?;
            self.conn.free_pixmap(s.pixmap)?;
            self.conn.free_gc(s.gc)?;
            self.conn.destroy_window(window)?;
        }
        self.conn.flush()?;
        Ok(())
    }

    pub fn set_visible(&mut self, window: u32, visible: bool) -> Result<()> {
        self.windows
            .get_mut(&window)
            .context("unknown bar window")?
            .visible = visible;
        self.update_struts(window)?;
        if visible {
            self.conn.map_window(window)?;
        } else {
            self.conn.unmap_window(window)?;
        }
        self.conn.flush()?;
        Ok(())
    }

    pub fn paint(&mut self, window: u32, frame: &Frame) -> Result<()> {
        let s = self.windows.get(&window).context("unknown bar window")?;
        ensure!(
            frame.width == s.geometry.width && frame.height == s.geometry.height,
            "frame dimensions differ from the bar window"
        );
        ensure!(
            frame.pixels.len() == frame.width as usize * frame.height as usize * 4,
            "invalid RGBA frame length"
        );
        // Tile both axes so even a very wide bar fits the server's request limit.
        let max_data = self.conn.maximum_request_bytes().saturating_sub(32).max(4);
        let tile_width = (max_data / self.bytes_per_pixel)
            .min(frame.width as usize)
            .max(1);
        for x in (0..frame.width as usize).step_by(tile_width) {
            let width = tile_width.min(frame.width as usize - x);
            let stride =
                (width * self.bytes_per_pixel).div_ceil(self.scanline_pad) * self.scanline_pad;
            let tile_height = (max_data / stride).max(1).min(frame.height as usize);
            for y in (0..frame.height as usize).step_by(tile_height) {
                let height = tile_height.min(frame.height as usize - y);
                let mut data = vec![0; stride * height];
                for row in 0..height {
                    for col in 0..width {
                        let source = ((y + row) * frame.width as usize + x + col) * 4;
                        let pixel = channel_pixel(frame.pixels[source], self.red_mask)
                            | channel_pixel(frame.pixels[source + 1], self.green_mask)
                            | channel_pixel(frame.pixels[source + 2], self.blue_mask);
                        let bytes = if self.little_endian {
                            pixel.to_le_bytes()
                        } else {
                            pixel.to_be_bytes()
                        };
                        let bytes = if self.little_endian {
                            &bytes[..self.bytes_per_pixel]
                        } else {
                            &bytes[4 - self.bytes_per_pixel..]
                        };
                        let offset = row * stride + col * self.bytes_per_pixel;
                        data[offset..offset + self.bytes_per_pixel].copy_from_slice(bytes);
                    }
                }
                self.conn.put_image(
                    ImageFormat::Z_PIXMAP,
                    s.pixmap,
                    s.gc,
                    width as u16,
                    height as u16,
                    x as i16,
                    y as i16,
                    0,
                    self.depth,
                    &data,
                )?;
            }
        }
        self.conn.copy_area(
            s.pixmap,
            window,
            s.gc,
            0,
            0,
            0,
            0,
            frame.width as u16,
            frame.height as u16,
        )?;
        self.conn.change_window_attributes(
            window,
            &ChangeWindowAttributesAux::new().background_pixmap(s.pixmap),
        )?;
        if let Some(tray) = self.tray.as_ref().filter(|t| t.bar == window) {
            self.conn.clear_area(false, tray.manager, 0, 0, 0, 0)?;
        }
        self.conn.flush()?;
        Ok(())
    }

    fn property32(&self, window: u32, atom: u32, type_: u32) -> Result<Vec<u32>> {
        let reply = self
            .conn
            .get_property(false, window, atom, type_, 0, 65536)?
            .reply()?;
        Ok(reply
            .value32()
            .map(|values| values.collect())
            .unwrap_or_default())
    }
    fn property8(&self, window: u32, atom: u32, type_: u32) -> Result<Vec<u8>> {
        let reply = self
            .conn
            .get_property(false, window, atom, type_, 0, 65536)?
            .reply()?;
        Ok(if reply.format == 8 {
            reply.value
        } else {
            Vec::new()
        })
    }
    fn cardinal(&self, window: u32, atom: u32) -> Option<u32> {
        self.property32(window, atom, AtomEnum::CARDINAL.into())
            .ok()?
            .first()
            .copied()
    }

    pub fn workspaces(&self) -> Result<Vec<Workspace>> {
        let number = self
            .cardinal(self.root, self.atoms._NET_NUMBER_OF_DESKTOPS)
            .unwrap_or(1)
            .clamp(1, 4096);
        let current = self
            .cardinal(self.root, self.atoms._NET_CURRENT_DESKTOP)
            .unwrap_or(0);
        let names = self.property8(
            self.root,
            self.atoms._NET_DESKTOP_NAMES,
            self.atoms.UTF8_STRING,
        )?;
        let names: Vec<_> = names
            .split(|b| *b == 0)
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect();
        let mut workspaces: Vec<_> = (0..number)
            .map(|id| {
                Workspace {
                    id,
                    index: id,
                    name: names
                        .get(id as usize)
                        .filter(|s| !s.is_empty())
                        .cloned()
                        .unwrap_or_else(|| (id + 1).to_string()),
                    // EWMH desktops are global. _NET_DESKTOP_VIEWPORT describes
                    // virtual desktop panning, not a physical output association.
                    monitor: String::new(),
                    focused: id == current,
                    visible: id == current,
                    urgent: false,
                    occupied: false,
                }
            })
            .collect();
        for client in self.property32(
            self.root,
            self.atoms._NET_CLIENT_LIST,
            AtomEnum::WINDOW.into(),
        )? {
            if self.windows.contains_key(&client) {
                continue;
            }
            let desktop = self
                .cardinal(client, self.atoms._NET_WM_DESKTOP)
                .unwrap_or(current);
            let Ok(states) =
                self.property32(client, self.atoms._NET_WM_STATE, AtomEnum::ATOM.into())
            else {
                // A stale WM client list must not mark ghost windows occupied.
                continue;
            };
            let hints = self
                .property32(client, self.atoms.WM_HINTS, self.atoms.WM_HINTS)
                .unwrap_or_default();
            let urgent = states.contains(&self.atoms._NET_WM_STATE_DEMANDS_ATTENTION)
                || hints.first().is_some_and(|h| h & (1 << 8) != 0);
            for workspace in &mut workspaces {
                if desktop == u32::MAX || desktop == workspace.id {
                    workspace.occupied = true;
                    workspace.urgent |= urgent;
                }
            }
        }
        Ok(workspaces)
    }

    pub fn active_window(&self) -> Result<Option<ActiveWindow>> {
        let id = self
            .property32(
                self.root,
                self.atoms._NET_ACTIVE_WINDOW,
                AtomEnum::WINDOW.into(),
            )?
            .first()
            .copied()
            .unwrap_or(0);
        if id == NONE || self.windows.contains_key(&id) {
            return Ok(None);
        }
        if self.conn.get_window_attributes(id)?.reply().is_err() {
            return Ok(None);
        }
        let title = self
            .property8(id, self.atoms._NET_WM_NAME, self.atoms.UTF8_STRING)
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                self.property8(id, self.atoms.WM_NAME, AtomEnum::STRING.into())
                    .unwrap_or_default()
            });
        let class = self
            .property8(id, self.atoms.WM_CLASS, AtomEnum::STRING.into())
            .unwrap_or_default();
        let class = class
            .split(|b| *b == 0)
            .rfind(|s| !s.is_empty())
            .unwrap_or_default();
        Ok(Some(ActiveWindow {
            id,
            title: String::from_utf8_lossy(&title)
                .trim_end_matches('\0')
                .into(),
            class: String::from_utf8_lossy(class).into(),
        }))
    }

    pub fn switch_workspace(&self, id: u32) -> Result<()> {
        let event = ClientMessageEvent::new(
            32,
            self.root,
            self.atoms._NET_CURRENT_DESKTOP,
            [id, CURRENT_TIME, 0, 0, 0],
        );
        self.conn.send_event(
            false,
            self.root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            event,
        )?;
        self.conn.flush()?;
        Ok(())
    }

    fn watch_clients(&mut self) -> Result<()> {
        let mut clients: HashSet<_> = self
            .property32(
                self.root,
                self.atoms._NET_CLIENT_LIST,
                AtomEnum::WINDOW.into(),
            )?
            .into_iter()
            .collect();
        if let Some(id) = self
            .property32(
                self.root,
                self.atoms._NET_ACTIVE_WINDOW,
                AtomEnum::WINDOW.into(),
            )?
            .first()
        {
            clients.insert(*id);
        }
        let mut watched: HashSet<_> = clients
            .intersection(&self.watched_clients)
            .copied()
            .collect();
        for client in clients.difference(&self.watched_clients) {
            if *client == NONE || self.windows.contains_key(client) {
                continue;
            }
            // Clients can disappear between the property read and event selection.
            if let Ok(cookie) = self.conn.change_window_attributes(
                *client,
                &ChangeWindowAttributesAux::new()
                    .event_mask(EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY),
            ) {
                if cookie.check().is_ok() {
                    watched.insert(*client);
                }
            }
        }
        // Failed subscriptions are retried when root properties change. A dead
        // client ID in a stale WM list can be reused by a new X11 connection.
        self.watched_clients = watched;
        Ok(())
    }

    fn refresh_scroll_axes(&mut self) -> Result<()> {
        let devices = self.conn.xinput_xi_query_device(0u16)?.reply()?;
        let mut axes = HashMap::new();
        for device in devices.infos {
            for class in &device.classes {
                if let xinput::DeviceClassData::Scroll(scroll) = &class.data {
                    let Some(value) = device.classes.iter().find_map(|c| match &c.data {
                        xinput::DeviceClassData::Valuator(v) if v.number == scroll.number => {
                            Some(fp3232(v.value))
                        }
                        _ => None,
                    }) else {
                        continue;
                    };
                    let increment = fp3232(scroll.increment);
                    if !increment.is_finite() || increment == 0.0 {
                        continue;
                    }
                    axes.insert(
                        (device.deviceid, scroll.number),
                        ScrollAxis {
                            horizontal: scroll.scroll_type == xinput::ScrollType::HORIZONTAL,
                            increment,
                            last: value,
                        },
                    );
                }
            }
        }
        self.scroll_axes = axes;
        Ok(())
    }

    fn xi_motion(&mut self, event: &xinput::MotionEvent, result: &mut Vec<XEvent>) {
        let x = event.event_x as f32 / 65536.0;
        let y = event.event_y as f32 / 65536.0;
        result.push(XEvent::Pointer {
            window: event.event,
            x,
            y,
        });
        let mut dx = 0.0;
        let mut dy = 0.0;
        let emulated = u32::from(event.flags) & (1 << 16) != 0;
        let mut values = event.axisvalues.iter();
        for (word, mask) in event.valuator_mask.iter().enumerate() {
            for bit in 0..32 {
                if mask & (1 << bit) == 0 {
                    continue;
                }
                let Some(value) = values.next() else {
                    break;
                };
                if let Some(axis) = self
                    .scroll_axes
                    .get_mut(&(event.sourceid, (word * 32 + bit) as u16))
                {
                    let next = fp3232(*value);
                    let delta = (next - axis.last) / axis.increment;
                    axis.last = next;
                    // A physical legacy wheel button can produce emulated valuators too.
                    if !emulated && delta.is_finite() {
                        if axis.horizontal {
                            dx += delta;
                        } else {
                            dy += delta;
                        }
                    }
                }
            }
        }
        if dx != 0.0 || dy != 0.0 {
            result.push(XEvent::Scroll {
                window: event.event,
                x,
                y,
                dx,
                dy,
            });
        }
    }

    pub fn poll_events(&mut self) -> Result<Vec<XEvent>> {
        let mut result = Vec::new();
        let mut wm_changed = false;
        let mut monitor_changed = false;
        let mut tray_changed = false;
        while let Some(event) = self.conn.poll_for_event()? {
            match event {
                Event::Expose(e) if self.windows.contains_key(&e.window) && e.count == 0 => {
                    result.push(XEvent::Expose { window: e.window })
                }
                Event::MotionNotify(e) if self.windows.get(&e.event).is_some_and(|s| !s.xi) => {
                    result.push(XEvent::Pointer {
                        window: e.event,
                        x: e.event_x.into(),
                        y: e.event_y.into(),
                    })
                }
                Event::EnterNotify(e) if self.windows.get(&e.event).is_some_and(|s| !s.xi) => {
                    result.push(XEvent::Pointer {
                        window: e.event,
                        x: e.event_x.into(),
                        y: e.event_y.into(),
                    })
                }
                Event::LeaveNotify(e) if self.windows.get(&e.event).is_some_and(|s| !s.xi) => {
                    result.push(XEvent::Leave { window: e.event })
                }
                Event::ButtonPress(e) if self.windows.get(&e.event).is_some_and(|s| !s.xi) => {
                    push_button(
                        &mut result,
                        e.event,
                        e.event_x.into(),
                        e.event_y.into(),
                        e.detail,
                    );
                }
                Event::XinputMotion(e) if self.windows.get(&e.event).is_some_and(|s| s.xi) => {
                    self.xi_motion(&e, &mut result)
                }
                Event::XinputButtonPress(e) if self.windows.get(&e.event).is_some_and(|s| s.xi) => {
                    // Only wheel buttons are duplicated by smooth-scroll emulation.
                    let smooth_axis = self.scroll_axes.iter().any(|((device, _), axis)| {
                        *device == e.sourceid && axis.horizontal == matches!(e.detail, 6 | 7)
                    });
                    if !(smooth_axis
                        && (4..=7).contains(&e.detail)
                        && u32::from(e.flags) & (1 << 16) != 0)
                    {
                        if let Ok(button) = u8::try_from(e.detail) {
                            push_button(
                                &mut result,
                                e.event,
                                e.event_x as f32 / 65536.0,
                                e.event_y as f32 / 65536.0,
                                button,
                            );
                        }
                    }
                }
                Event::XinputEnter(e) if self.windows.get(&e.event).is_some_and(|s| s.xi) => {
                    // Rebase after scrolling outside the bar; do not replay those deltas.
                    let _ = self.refresh_scroll_axes();
                    result.push(XEvent::Pointer {
                        window: e.event,
                        x: e.event_x as f32 / 65536.0,
                        y: e.event_y as f32 / 65536.0,
                    });
                }
                Event::XinputLeave(e) if self.windows.get(&e.event).is_some_and(|s| s.xi) => {
                    result.push(XEvent::Leave { window: e.event })
                }
                Event::XinputDeviceChanged(_) | Event::XinputHierarchy(_) if self.xi => {
                    let _ = self.refresh_scroll_axes();
                }
                Event::RandrScreenChangeNotify(_) | Event::RandrNotify(_) => monitor_changed = true,
                Event::ConfigureNotify(e) if e.window == self.root => monitor_changed = true,
                Event::PropertyNotify(e) => {
                    if self
                        .tray
                        .as_ref()
                        .is_some_and(|t| t.icons.iter().any(|i| i.window == e.window))
                        && e.atom == self.atoms._XEMBED_INFO
                    {
                        self.update_icon_mapping(e.window)?;
                        tray_changed = true;
                    } else if e.window == self.root || self.watched_clients.contains(&e.window) {
                        wm_changed = true;
                        if e.window == self.root && e.atom == self.atoms.RESOURCE_MANAGER {
                            monitor_changed = true;
                        }
                    }
                }
                Event::FocusIn(_) | Event::FocusOut(_) => wm_changed = true,
                Event::ClientMessage(e) => {
                    if e.type_ == self.atoms._NET_SYSTEM_TRAY_OPCODE
                        && e.format == 32
                        && self
                            .tray
                            .as_ref()
                            .is_some_and(|t| e.window == t.manager || e.window == t.bar)
                        && e.data.as_data32()[1] == 0
                    {
                        let icon = e.data.as_data32()[2];
                        if let Err(error) = self.dock_icon(icon) {
                            eprintln!("cybar: tray icon {icon:#x}: {error:#}");
                        }
                        tray_changed = true;
                    } else if e.type_ == self.atoms.WM_PROTOCOLS
                        && e.data.as_data32()[0] == self.atoms.WM_DELETE_WINDOW
                        && self.windows.contains_key(&e.window)
                    {
                        result.push(XEvent::Close { window: e.window });
                    }
                }
                Event::SelectionClear(e)
                    if e.selection == self.tray_selection
                        && self
                            .tray
                            .as_ref()
                            .is_some_and(|tray| tray.manager == e.owner) =>
                {
                    self.release_tray()?;
                    tray_changed = true;
                }
                Event::DestroyNotify(e) => {
                    if self.remove_icon(e.window) {
                        tray_changed = true;
                    }
                    if self.watched_clients.remove(&e.window) {
                        wm_changed = true;
                    }
                    if self.windows.contains_key(&e.window) {
                        result.push(XEvent::Close { window: e.window });
                    }
                }
                Event::ReparentNotify(e) => {
                    if self.tray.as_ref().is_some_and(|t| {
                        e.parent != t.manager && t.icons.iter().any(|i| i.window == e.window)
                    }) {
                        self.remove_icon(e.window);
                        tray_changed = true;
                    }
                }
                Event::ConfigureRequest(e)
                    if self
                        .tray
                        .as_ref()
                        .is_some_and(|t| t.icons.iter().any(|i| i.window == e.window)) =>
                {
                    self.configure_tray()?;
                }
                Event::MapRequest(e)
                    if self
                        .tray
                        .as_ref()
                        .is_some_and(|t| t.icons.iter().any(|i| i.window == e.window)) =>
                {
                    self.update_icon_mapping(e.window)?;
                    tray_changed = true;
                }
                Event::Error(e) => eprintln!("cybar: asynchronous X11 error: {e:?}"),
                _ => (),
            }
        }
        if wm_changed {
            self.watch_clients()?;
            result.push(XEvent::WmChanged);
        }
        if monitor_changed {
            result.push(XEvent::MonitorsChanged);
        }
        if tray_changed {
            self.configure_tray()?;
            result.push(XEvent::TrayChanged);
        }
        self.conn.flush()?;
        Ok(result)
    }

    pub fn claim_tray(&mut self, window: u32, vertical: bool, icon_size: u32) -> Result<()> {
        self.release_tray()?;
        ensure!(self.windows.contains_key(&window), "unknown tray bar");
        ensure!(
            self.conn
                .get_selection_owner(self.tray_selection)?
                .reply()?
                .owner
                == NONE,
            "another application already owns the X11 system tray"
        );
        let manager = self.conn.generate_id()?;
        self.conn
            .create_window(
                self.depth,
                manager,
                window,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_OUTPUT,
                self.visual,
                &CreateWindowAux::new()
                    .background_pixmap(1)
                    .border_pixel(0)
                    .event_mask(
                        EventMask::SUBSTRUCTURE_NOTIFY
                            | EventMask::SUBSTRUCTURE_REDIRECT
                            | EventMask::PROPERTY_CHANGE,
                    ),
            )?
            .check()?;
        self.conn.change_property32(
            PropMode::REPLACE,
            manager,
            self.atoms._NET_SYSTEM_TRAY_ORIENTATION,
            AtomEnum::CARDINAL,
            &[u32::from(vertical)],
        )?;
        self.conn.change_property32(
            PropMode::REPLACE,
            manager,
            self.atoms._NET_SYSTEM_TRAY_VISUAL,
            AtomEnum::VISUALID,
            &[self.visual],
        )?;
        self.conn.change_property32(
            PropMode::REPLACE,
            manager,
            self.atoms._NET_SYSTEM_TRAY_ICON_SIZE,
            AtomEnum::CARDINAL,
            &[icon_size.max(1)],
        )?;
        self.conn
            .set_selection_owner(manager, self.tray_selection, CURRENT_TIME)?
            .check()?;
        if self
            .conn
            .get_selection_owner(self.tray_selection)?
            .reply()?
            .owner
            != manager
        {
            self.conn.destroy_window(manager)?;
            bail!("failed to acquire the system tray selection");
        }
        let event = ClientMessageEvent::new(
            32,
            self.root,
            self.atoms.MANAGER,
            [CURRENT_TIME, self.tray_selection, manager, 0, 0],
        );
        self.conn
            .send_event(false, self.root, EventMask::STRUCTURE_NOTIFY, event)?;
        self.tray = Some(Tray {
            bar: window,
            manager,
            icons: Vec::new(),
            vertical,
            icon_size: icon_size.clamp(1, 32767),
            rect: None,
        });
        self.conn.flush()?;
        Ok(())
    }

    fn xembed(&self, icon: u32, message: u32, detail: u32, data1: u32, data2: u32) -> Result<()> {
        let event = ClientMessageEvent::new(
            32,
            icon,
            self.atoms._XEMBED,
            [CURRENT_TIME, message, detail, data1, data2],
        );
        self.conn
            .send_event(false, icon, EventMask::NO_EVENT, event)?;
        Ok(())
    }

    fn dock_icon(&mut self, window: u32) -> Result<()> {
        let tray = self.tray.as_ref().context("tray unavailable")?;
        if window == NONE
            || window == tray.bar
            || window == tray.manager
            || self.windows.contains_key(&window)
            || tray.icons.iter().any(|i| i.window == window)
        {
            return Ok(());
        }
        self.conn.get_window_attributes(window)?.reply()?;
        let manager = tray.manager;
        let info = self
            .property32(window, self.atoms._XEMBED_INFO, self.atoms._XEMBED_INFO)
            .unwrap_or_default();
        let mapped = info.get(1).map(|flags| flags & 1 != 0).unwrap_or(true);
        self.conn
            .change_save_set(SetMode::INSERT, window)?
            .check()?;
        let result = (|| -> Result<()> {
            self.conn
                .change_window_attributes(
                    window,
                    &ChangeWindowAttributesAux::new()
                        .event_mask(EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY),
                )?
                .check()?;
            self.conn.unmap_window(window)?;
            self.conn.reparent_window(window, manager, 0, 0)?.check()?;
            self.conn
                .configure_window(window, &ConfigureWindowAux::new().border_width(0))?;
            self.xembed(window, 0, 0, manager, 0)?; // XEMBED_EMBEDDED_NOTIFY, supported version 0.
            self.xembed(window, 1, 0, 0, 0)?; // XEMBED_WINDOW_ACTIVATE.
            Ok(())
        })();
        if let Err(error) = result {
            let _ = self.conn.change_save_set(SetMode::DELETE, window);
            return Err(error);
        }
        self.tray
            .as_mut()
            .unwrap()
            .icons
            .push(TrayIcon { window, mapped });
        self.configure_tray()?;
        Ok(())
    }

    fn update_icon_mapping(&mut self, window: u32) -> Result<()> {
        let info = self
            .property32(window, self.atoms._XEMBED_INFO, self.atoms._XEMBED_INFO)
            .unwrap_or_default();
        if let Some(icon) = self
            .tray
            .as_mut()
            .and_then(|t| t.icons.iter_mut().find(|i| i.window == window))
        {
            icon.mapped = info.get(1).map(|flags| flags & 1 != 0).unwrap_or(true);
        }
        self.configure_tray()
    }

    fn remove_icon(&mut self, window: u32) -> bool {
        let Some(tray) = &mut self.tray else {
            return false;
        };
        let previous = tray.icons.len();
        tray.icons.retain(|i| i.window != window);
        tray.icons.len() != previous
    }

    pub fn tray_count(&self) -> usize {
        self.tray
            .as_ref()
            .map(|t| t.icons.iter().filter(|i| i.mapped).count())
            .unwrap_or(0)
    }

    pub fn owns_tray(&self, window: u32) -> bool {
        self.tray.as_ref().is_some_and(|tray| tray.bar == window)
    }

    pub fn layout_tray(&mut self, window: u32, rect: Option<Rect>) -> Result<()> {
        if let Some(tray) = &mut self.tray {
            if tray.bar != window {
                return Ok(());
            }
            tray.rect = rect.filter(|r| r.width >= 1.0 && r.height >= 1.0);
            self.configure_tray()?;
            self.conn.flush()?;
        }
        Ok(())
    }

    fn configure_tray(&self) -> Result<()> {
        let Some(tray) = &self.tray else {
            return Ok(());
        };
        let Some(rect) = tray.rect else {
            self.conn.unmap_window(tray.manager)?;
            return Ok(());
        };
        let width = rect.width.round().clamp(1.0, 32767.0) as u32;
        let height = rect.height.round().clamp(1.0, 32767.0) as u32;
        self.conn.configure_window(
            tray.manager,
            &ConfigureWindowAux::new()
                .x(rect.x.round() as i32)
                .y(rect.y.round() as i32)
                .width(width)
                .height(height)
                .stack_mode(StackMode::ABOVE),
        )?;
        let size = tray
            .icon_size
            .min(if tray.vertical { width } else { height });
        let count = tray.icons.iter().filter(|i| i.mapped).count() as u32;
        let main = if tray.vertical { height } else { width };
        let gap = if count > 1 {
            main.saturating_sub(count * size) / (count - 1)
        } else {
            0
        };
        let mut offset = 0;
        for icon in &tray.icons {
            if !icon.mapped {
                self.conn.unmap_window(icon.window)?;
                continue;
            }
            let (x, y) = if tray.vertical {
                ((width - size) / 2, offset)
            } else {
                (offset, (height - size) / 2)
            };
            self.conn.configure_window(
                icon.window,
                &ConfigureWindowAux::new()
                    .x(x as i32)
                    .y(y as i32)
                    .width(size)
                    .height(size)
                    .border_width(0),
            )?;
            self.conn.map_window(icon.window)?;
            offset += size + gap;
        }
        self.conn.map_window(tray.manager)?;
        Ok(())
    }

    pub fn release_tray(&mut self) -> Result<()> {
        let Some(tray) = self.tray.take() else {
            return Ok(());
        };
        if self
            .conn
            .get_selection_owner(self.tray_selection)?
            .reply()?
            .owner
            == tray.manager
        {
            self.conn
                .set_selection_owner(NONE, self.tray_selection, CURRENT_TIME)?;
        }
        for icon in tray.icons {
            let _ = self.xembed(icon.window, 2, 0, 0, 0); // XEMBED_WINDOW_DEACTIVATE.
            let _ = self.conn.unmap_window(icon.window);
            let _ = self
                .conn
                .reparent_window(icon.window, self.root, 0, 0)
                .map(|c| c.check());
            let _ = self.conn.change_save_set(SetMode::DELETE, icon.window);
        }
        self.conn.destroy_window(tray.manager)?;
        self.conn.flush()?;
        Ok(())
    }
}

impl Drop for Platform {
    fn drop(&mut self) {
        let _ = self.release_tray();
        for window in self.windows.keys().copied().collect::<Vec<_>>() {
            let _ = self.destroy_bar(window);
        }
    }
}

fn fp3232(value: xinput::Fp3232) -> f64 {
    value.integral as f64 + value.frac as f64 / 4294967296.0
}
fn channel_pixel(value: u8, mask: u32) -> u32 {
    if mask == 0 {
        return 0;
    }
    let shift = mask.trailing_zeros();
    (((((value as u64) * (mask >> shift) as u64 + 127) / 255) as u32) << shift) & mask
}
fn parse_xft_scale(resources: &str) -> Option<f64> {
    resources
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find_map(|(key, value)| {
            if key.trim().eq_ignore_ascii_case("Xft.dpi")
                || key.trim().eq_ignore_ascii_case("Xft*dpi")
            {
                value
                    .trim()
                    .parse::<f64>()
                    .ok()
                    .filter(|dpi| dpi.is_finite() && *dpi > 0.0)
                    .map(|dpi| (dpi / 96.0).clamp(0.5, 4.0))
            } else {
                None
            }
        })
}
fn physical_scale(width: u32, height: u32, mm_width: u32, mm_height: u32) -> f64 {
    if mm_width == 0 || mm_height == 0 {
        return 1.0;
    }
    let dpi_x = width as f64 * 25.4 / mm_width as f64;
    let dpi_y = height as f64 * 25.4 / mm_height as f64;
    if !(50.0..=400.0).contains(&dpi_x) || !(50.0..=400.0).contains(&dpi_y) {
        return 1.0;
    }
    ((dpi_x + dpi_y) / 192.0).clamp(0.5, 4.0)
}

/// EWMH strut distances are measured from the root edges, including monitor offsets.
fn strut_partial(g: Geometry, edge: Edge, root_width: u32, root_height: u32) -> [u32; 12] {
    let mut s = [0; 12];
    let x = i64::from(g.x);
    let y = i64::from(g.y);
    let right = x + i64::from(g.width);
    let bottom = y + i64::from(g.height);
    let clip_x = |n: i64| n.clamp(0, root_width.saturating_sub(1).into()) as u32;
    let clip_y = |n: i64| n.clamp(0, root_height.saturating_sub(1).into()) as u32;
    match edge {
        Edge::Left => {
            s[0] = right.clamp(0, root_width.into()) as u32;
            s[4] = clip_y(y);
            s[5] = clip_y(bottom - 1);
        }
        Edge::Right => {
            s[1] = (i64::from(root_width) - x).clamp(0, root_width.into()) as u32;
            s[6] = clip_y(y);
            s[7] = clip_y(bottom - 1);
        }
        Edge::Top => {
            s[2] = bottom.clamp(0, root_height.into()) as u32;
            s[8] = clip_x(x);
            s[9] = clip_x(right - 1);
        }
        Edge::Bottom => {
            s[3] = (i64::from(root_height) - y).clamp(0, root_height.into()) as u32;
            s[10] = clip_x(x);
            s[11] = clip_x(right - 1);
        }
    }
    s
}

fn rounded_rectangles(width: u32, height: u32, radius: f32) -> Vec<Rectangle> {
    let r = radius
        .min(width as f32 / 2.0)
        .min(height as f32 / 2.0)
        .max(0.0);
    let mut rectangles: Vec<Rectangle> = Vec::new();
    for y in 0..height {
        let center = y as f32 + 0.5;
        let dy = if center < r {
            r - center
        } else if center > height as f32 - r {
            center - (height as f32 - r)
        } else {
            0.0
        };
        let inset = if dy > 0.0 {
            (r - (r * r - dy * dy).max(0.0).sqrt()).ceil() as u16
        } else {
            0
        };
        let w = (width as u16).saturating_sub(inset * 2);
        if w == 0 {
            continue;
        }
        if let Some(previous) = rectangles
            .last_mut()
            .filter(|p| p.x == inset as i16 && p.width == w && p.y as u32 + p.height as u32 == y)
        {
            previous.height += 1;
        } else {
            rectangles.push(Rectangle {
                x: inset as i16,
                y: y as i16,
                width: w,
                height: 1,
            });
        }
    }
    rectangles
}

fn push_button(events: &mut Vec<XEvent>, window: u32, x: f32, y: f32, button: u8) {
    if let Some((dx, dy)) = legacy_scroll(button) {
        events.push(XEvent::Scroll {
            window,
            x,
            y,
            dx,
            dy,
        });
    } else {
        events.push(XEvent::Button {
            window,
            x,
            y,
            button,
        });
    }
}
fn legacy_scroll(button: u8) -> Option<(f64, f64)> {
    match button {
        4 => Some((0.0, -1.0)),
        5 => Some((0.0, 1.0)),
        6 => Some((-1.0, 0.0)),
        7 => Some((1.0, 0.0)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn struts_include_monitor_origin_and_inset() {
        let top = strut_partial(
            Geometry {
                x: 1920,
                y: 30,
                width: 1920,
                height: 28,
            },
            Edge::Top,
            3840,
            1080,
        );
        assert_eq!(top, [0, 0, 58, 0, 0, 0, 0, 0, 1920, 3839, 0, 0]);
        let bottom = strut_partial(
            Geometry {
                x: 1920,
                y: 1022,
                width: 1920,
                height: 28,
            },
            Edge::Bottom,
            3840,
            1080,
        );
        assert_eq!(bottom[3], 58);
        assert_eq!(&bottom[10..], &[1920, 3839]);
        let right = strut_partial(
            Geometry {
                x: 1870,
                y: 200,
                width: 30,
                height: 600,
            },
            Edge::Right,
            3840,
            1080,
        );
        assert_eq!(right[1], 1970);
        assert_eq!(&right[6..8], &[200, 799]);
    }
    #[test]
    fn struts_clip_signed_coordinates_without_wrapping() {
        let s = strut_partial(
            Geometry {
                x: -50,
                y: -10,
                width: 400,
                height: 30,
            },
            Edge::Top,
            1920,
            1080,
        );
        assert_eq!(s[2], 20);
        assert_eq!(s[8], 0);
        assert_eq!(s[9], 349);
    }
    #[test]
    fn smooth_valuator_preserves_negative_fractions() {
        assert_eq!(
            fp3232(xinput::Fp3232 {
                integral: -1,
                frac: 0x80000000
            }),
            -0.5
        );
        assert_eq!(legacy_scroll(4), Some((0.0, -1.0)));
        assert_eq!(legacy_scroll(7), Some((1.0, 0.0)));
        assert_eq!(legacy_scroll(8), None);
    }
    #[test]
    fn xft_resource_dpi_takes_priority_and_rejects_nan() {
        assert_eq!(
            parse_xft_scale("Xft.antialias: 1\nXft.dpi: 144\n"),
            Some(1.5)
        );
        assert_eq!(parse_xft_scale("Xft.dpi: NaN"), None);
        assert_eq!(physical_scale(1920, 1080, 0, 0), 1.0);
    }
    #[test]
    fn visual_masks_encode_rgb565_and_rgb888() {
        assert_eq!(
            channel_pixel(255, 0xf800) | channel_pixel(128, 0x07e0) | channel_pixel(0, 0x001f),
            0xfc00
        );
        assert_eq!(
            channel_pixel(0x12, 0xff0000) | channel_pixel(0x34, 0xff00) | channel_pixel(0x56, 0xff),
            0x123456
        );
    }
}
