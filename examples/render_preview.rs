// SPDX-License-Identifier: GPL-3.0-or-later
//! Render a sample without an X server:
//! cargo run --example render_preview -- cybar-preview.png [icon-name-or-path]
use anyhow::{Result, bail};
use cybar::{model::*, renderer::Renderer};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let output = args.next().unwrap_or_else(|| "cybar-preview.png".into());
    if output == "--help" || output == "-h" {
        println!("Usage: cargo run --example render_preview -- [output.png] [icon-name-or-path]");
        return Ok(());
    }
    let icon = args.next();
    if args.next().is_some() {
        bail!("expected an output PNG path and optional icon name or file path");
    }
    let section = |name: &str| SectionSpec {
        name: name.into(),
        x: None,
        y: None,
        width: None,
        height: None,
        padding: 0.0,
        gap: 4.0,
        modules: Vec::new(),
    };
    let mut theme = Theme::default();
    theme.font.fallback = vec!["Noto Sans".into(), "Noto Color Emoji".into()];
    let spec = BarSpec {
        name: "preview".into(),
        monitor: "primary".into(),
        position: Edge::Top,
        thickness: Dimension::Pixels(36.0),
        length: Dimension::Percent(100.0),
        offset_x: Dimension::Pixels(0.0),
        offset_y: Dimension::Pixels(0.0),
        reserve_space: true,
        corner_radius: 0.0,
        scale: None,
        padding: 4.0,
        gap: 4.0,
        background: None,
        sections: vec![section("left"), section("middle"), section("right")],
        overrides: None,
        separator: Some(Separator {
            text: Some("│".into()),
            icon: None,
            color: None,
            spacing: 4.0,
        }),
    };
    let item = |module, text: &str, state| DisplayItem {
        module,
        text: text.into(),
        icon: None,
        style: Style::default(),
        state,
        workspace: None,
        tray: false,
        tray_count: 0,
    };
    let mut left = vec![
        item(0, "1", ModuleState::Active),
        item(0, "2", ModuleState::Normal),
        item(0, "3", ModuleState::Urgent),
    ];
    for item in &mut left {
        item.style.background = Some(Color(40, 42, 62, 255));
        item.style.corner_radius = 5.0;
    }
    let mut title = item(
        1,
        "Cybar — a configurable X11 status bar with Lua",
        ModuleState::Normal,
    );
    title.style.max_width = Some(245.0);
    left.push(title);
    let middle = vec![item(2, "Sat 03 Oct  14:42", ModuleState::Normal)];
    let mut cpu = item(3, "CPU 82%", ModuleState::Warning);
    cpu.icon = icon;
    let right = vec![
        cpu,
        item(4, "RAM 4.2 / 16 GiB", ModuleState::Normal),
        item(5, "🦀", ModuleState::Hover),
    ];
    let frame = Renderer::new()?.render(
        &spec,
        &theme,
        Geometry {
            x: 0,
            y: 0,
            width: 1200,
            height: 36,
        },
        1.0,
        &[left, middle, right],
    )?;
    image::save_buffer(
        &output,
        &frame.pixels,
        frame.width,
        frame.height,
        image::ColorType::Rgba8,
    )?;
    println!("Saved {output} ({} × {} pixels)", frame.width, frame.height);
    Ok(())
}
