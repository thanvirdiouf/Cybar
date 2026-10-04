// SPDX-License-Identifier: GPL-3.0-or-later
//! Capture a real Cybar X11 window, without desktop screenshot dependencies.
//! cargo run --example capture -- output.png [bar-name] [--demo]
//! --demo supplies illustrative EWMH metadata and is restricted to isolated :117.
use anyhow::{Context, Result, bail, ensure};
use image::{Rgba, RgbaImage};
use std::{collections::VecDeque, path::PathBuf, time::Duration};
use x11rb::{
    connection::Connection, protocol::xproto::*, rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

fn atom(conn: &RustConnection, name: &str) -> Result<u32> {
    Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
}
fn bytes(conn: &RustConnection, window: u32, property: u32) -> Vec<u8> {
    conn.get_property(false, window, property, AtomEnum::ANY, 0, 1024)
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| r.value)
        .unwrap_or_default()
}
fn channel(pixel: u32, mask: u32) -> u8 {
    if mask == 0 {
        return 0;
    }
    let shift = mask.trailing_zeros();
    let maximum = mask >> shift;
    ((((pixel & mask) >> shift) as u64 * 255 + maximum as u64 / 2) / maximum as u64) as u8
}
fn demo(conn: &RustConnection, screen: usize) -> Result<Vec<u32>> {
    ensure!(
        std::env::var("DISPLAY").as_deref() == Ok(":117"),
        "--demo is restricted to isolated Xvfb display :117"
    );
    let s = &conn.setup().roots[screen];
    let mut windows = Vec::new();
    for workspace in [0, 1, 2, 4] {
        let window = conn.generate_id()?;
        conn.create_window(
            s.root_depth,
            window,
            s.root,
            300,
            150,
            300,
            200,
            0,
            WindowClass::INPUT_OUTPUT,
            s.root_visual,
            &CreateWindowAux::new(),
        )?
        .check()?;
        conn.change_property32(
            PropMode::REPLACE,
            window,
            atom(conn, "_NET_WM_DESKTOP")?,
            AtomEnum::CARDINAL,
            &[workspace],
        )?;
        if workspace == 4 {
            conn.change_property32(
                PropMode::REPLACE,
                window,
                atom(conn, "_NET_WM_STATE")?,
                AtomEnum::ATOM,
                &[atom(conn, "_NET_WM_STATE_DEMANDS_ATTENTION")?],
            )?;
        }
        conn.change_property8(
            PropMode::REPLACE,
            window,
            atom(conn, "_NET_WM_NAME")?,
            atom(conn, "UTF8_STRING")?,
            "Cybar · Lua-powered desktop".as_bytes(),
        )?;
        conn.change_property8(
            PropMode::REPLACE,
            window,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            b"preview\0Preview\0",
        )?;
        windows.push(window);
    }
    conn.change_property32(
        PropMode::REPLACE,
        s.root,
        atom(conn, "_NET_NUMBER_OF_DESKTOPS")?,
        AtomEnum::CARDINAL,
        &[5],
    )?;
    conn.change_property32(
        PropMode::REPLACE,
        s.root,
        atom(conn, "_NET_CURRENT_DESKTOP")?,
        AtomEnum::CARDINAL,
        &[1],
    )?;
    let names = (1..=5).map(|id| format!("{id}\0")).collect::<String>();
    conn.change_property8(
        PropMode::REPLACE,
        s.root,
        atom(conn, "_NET_DESKTOP_NAMES")?,
        atom(conn, "UTF8_STRING")?,
        names.as_bytes(),
    )?;
    conn.change_property32(
        PropMode::REPLACE,
        s.root,
        atom(conn, "_NET_CLIENT_LIST")?,
        AtomEnum::WINDOW,
        &windows,
    )?;
    conn.change_property32(
        PropMode::REPLACE,
        s.root,
        atom(conn, "_NET_ACTIVE_WINDOW")?,
        AtomEnum::WINDOW,
        &[windows[1]],
    )?;
    conn.flush()?;
    Ok(windows)
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let demo_mode = args.iter().any(|a| a == "--demo");
    let positional: Vec<_> = args.iter().filter(|a| a.as_str() != "--demo").collect();
    if positional.is_empty() || positional.len() > 2 {
        bail!("usage: capture output.png [bar-name] [--demo]");
    }
    let output = PathBuf::from(positional[0]);
    let requested = positional.get(1).map(|s| s.as_str());
    let (conn, screen_number) = x11rb::connect(None).context("cannot connect to X11 display")?;
    let screen = &conn.setup().roots[screen_number];
    let demo_windows = if demo_mode {
        demo(&conn, screen_number)?
    } else {
        Vec::new()
    };
    if demo_mode {
        std::thread::sleep(Duration::from_millis(600));
    }
    let class = atom(&conn, "WM_CLASS")?;
    let name = atom(&conn, "_NET_WM_NAME")?;
    let mut search = VecDeque::from([(screen.root, 0)]);
    let mut found = None;
    while let Some((parent, depth)) = search.pop_front() {
        let Ok(tree) = conn.query_tree(parent)?.reply() else {
            continue;
        };
        for window in tree.children {
            let title = String::from_utf8_lossy(&bytes(&conn, window, name)).into_owned();
            let is_bar = bytes(&conn, window, class)
                .split(|b| *b == 0)
                .any(|part| part.eq_ignore_ascii_case(b"cybar"));
            if is_bar && requested.is_none_or(|bar| title == format!("Cybar: {bar}")) {
                found = Some((window, title));
                break;
            }
            if depth < 8 {
                search.push_back((window, depth + 1));
            }
        }
        if found.is_some() {
            break;
        }
    }
    let (window, title) =
        found.context("no matching Cybar window found; start the daemon on this DISPLAY")?;
    let geometry = conn.get_geometry(window)?.reply()?;
    let attributes = conn.get_window_attributes(window)?.reply()?;
    ensure!(
        attributes.map_state == MapState::VIEWABLE,
        "the selected Cybar window is hidden"
    );
    let visual = screen
        .allowed_depths
        .iter()
        .flat_map(|d| &d.visuals)
        .find(|v| v.visual_id == attributes.visual)
        .context("window visual unavailable")?;
    ensure!(
        visual.class == VisualClass::TRUE_COLOR,
        "capture needs a TrueColor visual"
    );
    let format = conn
        .setup()
        .pixmap_formats
        .iter()
        .find(|f| f.depth == geometry.depth)
        .context("pixel format unavailable")?;
    ensure!(
        matches!(format.bits_per_pixel, 16 | 24 | 32),
        "unsupported pixel format"
    );
    let pixels = conn
        .get_image(
            ImageFormat::Z_PIXMAP,
            window,
            0,
            0,
            geometry.width,
            geometry.height,
            u32::MAX,
        )?
        .reply()?;
    let bytes_per_pixel = usize::from(format.bits_per_pixel / 8);
    let padding = usize::from(format.scanline_pad / 8);
    let stride = (geometry.width as usize * bytes_per_pixel).div_ceil(padding) * padding;
    ensure!(
        pixels.data.len() >= stride * geometry.height as usize,
        "incomplete X11 image reply"
    );
    let little = conn.setup().image_byte_order == ImageOrder::LSB_FIRST;
    let mut image = RgbaImage::new(geometry.width.into(), geometry.height.into());
    for y in 0..geometry.height as usize {
        for x in 0..geometry.width as usize {
            let offset = y * stride + x * bytes_per_pixel;
            let mut encoded = [0u8; 4];
            if little {
                encoded[..bytes_per_pixel]
                    .copy_from_slice(&pixels.data[offset..offset + bytes_per_pixel]);
            } else {
                encoded[4 - bytes_per_pixel..]
                    .copy_from_slice(&pixels.data[offset..offset + bytes_per_pixel]);
            }
            let pixel = if little {
                u32::from_le_bytes(encoded)
            } else {
                u32::from_be_bytes(encoded)
            };
            image.put_pixel(
                x as u32,
                y as u32,
                Rgba([
                    channel(pixel, visual.red_mask),
                    channel(pixel, visual.green_mask),
                    channel(pixel, visual.blue_mask),
                    255,
                ]),
            );
        }
    }
    if let Some(directory) = output.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(directory)?;
    }
    image
        .save(&output)
        .with_context(|| format!("cannot save {}", output.display()))?;
    for window in demo_windows {
        conn.destroy_window(window)?;
    }
    if demo_mode {
        for property in [
            "_NET_CLIENT_LIST",
            "_NET_ACTIVE_WINDOW",
            "_NET_NUMBER_OF_DESKTOPS",
            "_NET_CURRENT_DESKTOP",
            "_NET_DESKTOP_NAMES",
        ] {
            conn.delete_property(screen.root, atom(&conn, property)?)?;
        }
    }
    conn.flush()?;
    println!(
        "Captured {title}: {}×{} → {}",
        geometry.width,
        geometry.height,
        output.display()
    );
    Ok(())
}
