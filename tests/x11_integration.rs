// SPDX-License-Identifier: GPL-3.0-or-later
//! Run explicitly against an isolated server, e.g.
//! CYBAR_TEST_DISPLAY=:99 cargo test --test x11_integration -- --nocapture
use anyhow::{Context, Result, bail};
use cybar::{
    model::*,
    x11::{Platform, XEvent},
};
use std::time::{Duration, Instant};
use x11rb::{
    CURRENT_TIME, NONE,
    connection::Connection,
    protocol::{Event, xproto::*},
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

fn atom(conn: &RustConnection, name: &str) -> Result<u32> {
    Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
}
fn integers(conn: &RustConnection, window: u32, name: &str) -> Result<Vec<u32>> {
    let reply = conn
        .get_property(false, window, atom(conn, name)?, AtomEnum::ANY, 0, 128)?
        .reply()?;
    Ok(reply.value32().map(|v| v.collect()).unwrap_or_default())
}
fn wait_event(platform: &mut Platform, predicate: impl Fn(&XEvent) -> bool) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if platform.poll_events()?.iter().any(&predicate) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("expected X11 event did not arrive");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn isolated_x11_window_ewmh_and_tray_lifecycle() -> Result<()> {
    let Ok(display) = std::env::var("CYBAR_TEST_DISPLAY") else {
        eprintln!("skipping X11 integration: set CYBAR_TEST_DISPLAY to an isolated X server");
        return Ok(());
    };
    let mut platform = Platform::connect_to(Some(&display))?;
    let (client, screen_number) = x11rb::connect(Some(&display))?;
    let screen = &client.setup().roots[screen_number];
    let root = screen.root;
    let monitors = platform.monitors()?;
    let monitor = monitors.first().context("X server has no monitors")?;
    let spec = BarSpec {
        name: "integration".into(),
        monitor: "primary".into(),
        position: Edge::Top,
        thickness: Dimension::Pixels(28.0),
        length: Dimension::Pixels(320.0),
        offset_x: Dimension::Pixels(20.0),
        offset_y: Dimension::Pixels(6.0),
        reserve_space: true,
        corner_radius: 4.0,
        scale: Some(1.0),
        padding: 0.0,
        gap: 0.0,
        background: None,
        sections: Vec::new(),
        overrides: None,
        separator: None,
    };
    let bar = platform.create_bar(&spec, monitor, None)?;
    let children = client.query_tree(root)?.reply()?.children;
    let mut invalid = spec.clone();
    invalid.length = Dimension::Pixels(32767.0);
    invalid.thickness = Dimension::Pixels(32767.0);
    assert!(
        platform
            .create_bar(&invalid, monitor, None)
            .unwrap_err()
            .to_string()
            .contains("256 MiB")
    );
    invalid = spec.clone();
    invalid.offset_x = Dimension::Pixels(f64::MAX);
    assert!(
        platform
            .create_bar(&invalid, monitor, None)
            .unwrap_err()
            .to_string()
            .contains("X11 coordinate range")
    );
    assert_eq!(
        client.query_tree(root)?.reply()?.children,
        children,
        "invalid geometry allocated an X11 window before validation"
    );
    assert_eq!(
        integers(&client, bar.window, "_NET_WM_WINDOW_TYPE")?,
        vec![atom(&client, "_NET_WM_WINDOW_TYPE_DOCK")?]
    );
    let strut = integers(&client, bar.window, "_NET_WM_STRUT_PARTIAL")?;
    assert_eq!(strut.len(), 12);
    assert_eq!(strut[2], (bar.geometry.y + 28) as u32);
    assert_eq!(
        (strut[8], strut[9]),
        (bar.geometry.x as u32, bar.geometry.x as u32 + 319)
    );
    assert_eq!(integers(&client, bar.window, "_NET_WM_STRUT")?, strut[..4]);
    platform.set_visible(bar.window, false)?;
    assert!(integers(&client, bar.window, "_NET_WM_STRUT_PARTIAL")?.is_empty());
    assert_eq!(
        client.get_window_attributes(bar.window)?.reply()?.map_state,
        MapState::UNMAPPED
    );
    platform.set_visible(bar.window, true)?;
    assert_eq!(
        integers(&client, bar.window, "_NET_WM_STRUT_PARTIAL")?,
        strut
    );

    let pixels =
        [255, 0, 0, 255].repeat(bar.geometry.width as usize * bar.geometry.height as usize);
    platform.paint(
        bar.window,
        &Frame {
            width: bar.geometry.width,
            height: bar.geometry.height,
            pixels,
            hits: Vec::new(),
            tray: None,
        },
    )?;
    let image = client
        .get_image(ImageFormat::Z_PIXMAP, bar.window, 10, 10, 1, 1, u32::MAX)?
        .reply()?;
    let format = client
        .setup()
        .pixmap_formats
        .iter()
        .find(|f| f.depth == screen.root_depth)
        .unwrap();
    let mut data = [0u8; 4];
    let bytes = usize::from(format.bits_per_pixel / 8);
    let little = client.setup().image_byte_order == ImageOrder::LSB_FIRST;
    if little {
        data[..bytes].copy_from_slice(&image.data[..bytes]);
    } else {
        data[4 - bytes..].copy_from_slice(&image.data[..bytes]);
    }
    let pixel = if little {
        u32::from_le_bytes(data)
    } else {
        u32::from_be_bytes(data)
    };
    let visual = screen
        .allowed_depths
        .iter()
        .flat_map(|d| &d.visuals)
        .find(|v| v.visual_id == screen.root_visual)
        .unwrap();
    assert_eq!(
        pixel & (visual.red_mask | visual.green_mask | visual.blue_mask),
        visual.red_mask
    );

    // Build a tiny EWMH fixture rather than relying on an installed window manager.
    let app = client.generate_id()?;
    client
        .create_window(
            screen.root_depth,
            app,
            root,
            50,
            50,
            100,
            50,
            0,
            WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &CreateWindowAux::new(),
        )?
        .check()?;
    client.change_property32(
        PropMode::REPLACE,
        root,
        atom(&client, "_NET_NUMBER_OF_DESKTOPS")?,
        AtomEnum::CARDINAL,
        &[2],
    )?;
    client.change_property32(
        PropMode::REPLACE,
        root,
        atom(&client, "_NET_CURRENT_DESKTOP")?,
        AtomEnum::CARDINAL,
        &[1],
    )?;
    client.change_property8(
        PropMode::REPLACE,
        root,
        atom(&client, "_NET_DESKTOP_NAMES")?,
        atom(&client, "UTF8_STRING")?,
        b"one\0two\0",
    )?;
    client.change_property32(
        PropMode::REPLACE,
        root,
        atom(&client, "_NET_CLIENT_LIST")?,
        AtomEnum::WINDOW,
        &[app],
    )?;
    client.change_property32(
        PropMode::REPLACE,
        root,
        atom(&client, "_NET_ACTIVE_WINDOW")?,
        AtomEnum::WINDOW,
        &[app],
    )?;
    client.change_property32(
        PropMode::REPLACE,
        app,
        atom(&client, "_NET_WM_DESKTOP")?,
        AtomEnum::CARDINAL,
        &[1],
    )?;
    client.change_property32(
        PropMode::REPLACE,
        app,
        atom(&client, "_NET_WM_STATE")?,
        AtomEnum::ATOM,
        &[atom(&client, "_NET_WM_STATE_DEMANDS_ATTENTION")?],
    )?;
    client.change_property8(
        PropMode::REPLACE,
        app,
        atom(&client, "_NET_WM_NAME")?,
        atom(&client, "UTF8_STRING")?,
        "Fixture — UTF-8".as_bytes(),
    )?;
    client.change_property8(
        PropMode::REPLACE,
        app,
        AtomEnum::WM_CLASS,
        AtomEnum::STRING,
        b"fixture\0CybarFixture\0",
    )?;
    client.flush()?;
    wait_event(&mut platform, |e| matches!(e, XEvent::WmChanged))
        .context("initial EWMH fixture")?;
    let workspaces = platform.workspaces()?;
    assert_eq!(workspaces.len(), 2);
    assert_eq!((workspaces[0].index, workspaces[1].index), (0, 1));
    assert_eq!(workspaces[0].name, "one");
    assert_eq!(workspaces[1].name, "two");
    assert!(!workspaces[0].focused && !workspaces[0].occupied);
    assert!(
        workspaces[1].focused
            && workspaces[1].visible
            && workspaces[1].occupied
            && workspaces[1].urgent
    );
    let active = platform.active_window()?.unwrap();
    assert_eq!(active.id, app);
    assert_eq!(active.title, "Fixture — UTF-8");
    assert_eq!(active.class, "CybarFixture");
    client.change_property8(
        PropMode::REPLACE,
        app,
        atom(&client, "_NET_WM_NAME")?,
        atom(&client, "UTF8_STRING")?,
        b"Updated",
    )?;
    client.flush()?;
    wait_event(&mut platform, |e| matches!(e, XEvent::WmChanged)).context("active title change")?;
    assert_eq!(platform.active_window()?.unwrap().title, "Updated");
    client
        .change_window_attributes(
            root,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::SUBSTRUCTURE_NOTIFY),
        )?
        .check()?;
    platform.switch_workspace(0)?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(Event::ClientMessage(e)) = client.poll_for_event()? {
            if e.type_ == atom(&client, "_NET_CURRENT_DESKTOP")? {
                assert_eq!(e.data.as_data32()[0], 0);
                break;
            }
        }
        if Instant::now() >= deadline {
            bail!("workspace switch client message missing");
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    platform.claim_tray(bar.window, false, 16)?;
    assert!(platform.owns_tray(bar.window));
    let selection = atom(&client, &format!("_NET_SYSTEM_TRAY_S{screen_number}"))?;
    let manager = client.get_selection_owner(selection)?.reply()?.owner;
    assert_ne!(manager, NONE);
    assert_eq!(
        integers(&client, manager, "_NET_SYSTEM_TRAY_ORIENTATION")?,
        vec![0]
    );
    let icon = client.generate_id()?;
    client
        .create_window(
            screen.root_depth,
            icon,
            root,
            0,
            0,
            32,
            32,
            0,
            WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &CreateWindowAux::new()
                .event_mask(EventMask::STRUCTURE_NOTIFY | EventMask::PROPERTY_CHANGE),
        )?
        .check()?;
    let embed_info = atom(&client, "_XEMBED_INFO")?;
    client.change_property32(PropMode::REPLACE, icon, embed_info, embed_info, &[0, 1])?;
    let request = ClientMessageEvent::new(
        32,
        manager,
        atom(&client, "_NET_SYSTEM_TRAY_OPCODE")?,
        [CURRENT_TIME, 0, icon, 0, 0],
    );
    client.send_event(false, manager, EventMask::NO_EVENT, request)?;
    client.flush()?;
    wait_event(&mut platform, |e| matches!(e, XEvent::TrayChanged)).context("initial icon dock")?;
    assert_eq!(platform.tray_count(), 1);
    platform.layout_tray(
        bar.window,
        Some(Rect {
            x: 280.0,
            y: 4.0,
            width: 16.0,
            height: 16.0,
        }),
    )?;
    assert_eq!(client.query_tree(icon)?.reply()?.parent, manager);
    let geometry = client.get_geometry(icon)?.reply()?;
    assert_eq!((geometry.width, geometry.height), (16, 16));
    assert_eq!(
        client.get_window_attributes(icon)?.reply()?.map_state,
        MapState::VIEWABLE
    );
    // An embedded client cannot resize itself beyond the host's configured slot.
    client
        .configure_window(icon, &ConfigureWindowAux::new().width(80).height(40))?
        .check()?;
    client.get_input_focus()?.reply()?;
    platform.poll_events()?;
    let geometry = client.get_geometry(icon)?.reply()?;
    assert_eq!((geometry.width, geometry.height), (16, 16));
    // XEMBED_MAPPED is authoritative, including toggles without destroying the icon.
    client.change_property32(PropMode::REPLACE, icon, embed_info, embed_info, &[0, 0])?;
    client.flush()?;
    wait_event(&mut platform, |e| matches!(e, XEvent::TrayChanged))
        .context("mapped flag change")?;
    assert_eq!(platform.tray_count(), 0);
    assert_eq!(
        client.get_window_attributes(icon)?.reply()?.map_state,
        MapState::UNMAPPED
    );
    platform.release_tray()?;
    assert_eq!(client.get_selection_owner(selection)?.reply()?.owner, NONE);
    assert_eq!(client.query_tree(icon)?.reply()?.parent, root);
    assert!(!platform.owns_tray(bar.window));

    // A tray generation change preserves the external application's icon window.
    platform.claim_tray(bar.window, true, 12)?;
    let manager = client.get_selection_owner(selection)?.reply()?.owner;
    assert_eq!(
        integers(&client, manager, "_NET_SYSTEM_TRAY_ORIENTATION")?,
        vec![1]
    );
    client.change_property32(PropMode::REPLACE, icon, embed_info, embed_info, &[0, 1])?;
    client.send_event(
        false,
        manager,
        EventMask::NO_EVENT,
        ClientMessageEvent::new(
            32,
            manager,
            atom(&client, "_NET_SYSTEM_TRAY_OPCODE")?,
            [CURRENT_TIME, 0, icon, 0, 0],
        ),
    )?;
    client.flush()?;
    wait_event(&mut platform, |e| matches!(e, XEvent::TrayChanged))
        .context("icon dock after selection replacement")?;
    assert!(platform.owns_tray(bar.window));
    assert_eq!(platform.tray_count(), 1);
    platform.layout_tray(
        bar.window,
        Some(Rect {
            x: 300.0,
            y: 0.0,
            width: 12.0,
            height: 24.0,
        }),
    )?;
    let geometry = client.get_geometry(icon)?.reply()?;
    assert_eq!((geometry.width, geometry.height), (12, 12));
    assert_eq!(client.query_tree(icon)?.reply()?.parent, manager);
    platform.release_tray()?;
    assert_eq!(client.query_tree(icon)?.reply()?.parent, root);

    client.destroy_window(icon)?;
    client.destroy_window(app)?;
    client.get_input_focus()?.reply()?;
    assert!(platform.active_window()?.is_none());
    assert!(platform.workspaces()?.iter().all(|w| !w.occupied));

    // A stale root client ID can become valid again. Failed initial event
    // subscriptions must be retried so the new client's title stays live.
    let mut reconnected = Platform::connect_to(Some(&display))?;
    client
        .create_window(
            screen.root_depth,
            app,
            root,
            50,
            50,
            100,
            50,
            0,
            WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &CreateWindowAux::new(),
        )?
        .check()?;
    client.change_property32(
        PropMode::REPLACE,
        root,
        atom(&client, "_NET_CLIENT_LIST")?,
        AtomEnum::WINDOW,
        &[app],
    )?;
    client.flush()?;
    wait_event(&mut reconnected, |e| matches!(e, XEvent::WmChanged))
        .context("reused client ID subscription")?;
    client.change_property8(
        PropMode::REPLACE,
        app,
        atom(&client, "_NET_WM_NAME")?,
        atom(&client, "UTF8_STRING")?,
        b"Reused client title",
    )?;
    client.flush()?;
    wait_event(&mut reconnected, |e| matches!(e, XEvent::WmChanged))
        .context("reused client ID title change")?;
    assert_eq!(
        reconnected.active_window()?.unwrap().title,
        "Reused client title"
    );
    client.destroy_window(app)?;
    for name in [
        "_NET_NUMBER_OF_DESKTOPS",
        "_NET_CURRENT_DESKTOP",
        "_NET_DESKTOP_NAMES",
        "_NET_CLIENT_LIST",
        "_NET_ACTIVE_WINDOW",
    ] {
        client.delete_property(root, atom(&client, name)?)?;
    }
    client.flush()?;
    platform.destroy_bar(bar.window)?;
    assert!(client.get_window_attributes(bar.window)?.reply().is_err());
    Ok(())
}
