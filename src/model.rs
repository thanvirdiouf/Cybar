// SPDX-License-Identifier: GPL-3.0-or-later
use mlua::Table;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Dimension {
    Pixels(f64),
    Percent(f64),
}
impl Dimension {
    pub fn resolve(self, parent: f64, scale: f64) -> f64 {
        match self {
            Self::Pixels(n) => n * scale,
            Self::Percent(n) => n * parent / 100.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Top,
    Bottom,
    Left,
    Right,
}
impl Edge {
    pub fn vertical(self) -> bool {
        matches!(self, Self::Left | Self::Right)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}
impl Rect {
    pub fn contains(self, x: f32, y: f32) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.width && y < self.y + self.height
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

pub const MAX_FRAME_BYTES: u64 = 256 * 1024 * 1024;

/// Shared by window preparation and rendering, before either allocates storage.
pub fn validate_framebuffer(geometry: Geometry) -> anyhow::Result<()> {
    anyhow::ensure!(
        geometry.width > 0 && geometry.height > 0,
        "bar framebuffer dimensions must be positive"
    );
    let bytes = u64::from(geometry.width)
        .checked_mul(u64::from(geometry.height))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| anyhow::anyhow!("bar framebuffer dimensions overflow"))?;
    anyhow::ensure!(bytes <= MAX_FRAME_BYTES, "bar framebuffer exceeds 256 MiB");
    Ok(())
}

#[derive(Clone, Debug)]
pub struct Monitor {
    pub name: String,
    pub geometry: Geometry,
    pub primary: bool,
    pub scale: f64,
}

pub fn bar_geometry(spec: &BarSpec, monitor: &Monitor, scale: f64) -> Geometry {
    let m = monitor.geometry;
    let vertical = spec.position.vertical();
    let length = spec
        .length
        .resolve(
            if vertical {
                m.height as f64
            } else {
                m.width as f64
            },
            scale,
        )
        .round()
        .max(1.0) as u32;
    let thickness = spec
        .thickness
        .resolve(
            if vertical {
                m.width as f64
            } else {
                m.height as f64
            },
            scale,
        )
        .round()
        .max(1.0) as u32;
    // Clamp the physical dimensions before anchoring to the bottom/right edge.
    let (width, height) = if vertical {
        (thickness.min(32767), length.min(32767))
    } else {
        (length.min(32767), thickness.min(32767))
    };
    // Float-to-integer casts saturate; wide arithmetic prevents accepted finite
    // configuration offsets from overflowing before X11 placement validation.
    let ox = spec.offset_x.resolve(m.width as f64, scale).round() as i64;
    let oy = spec.offset_y.resolve(m.height as f64, scale).round() as i64;
    let mx = i64::from(m.x);
    let my = i64::from(m.y);
    let (x, y) = match spec.position {
        Edge::Top | Edge::Left => (mx.saturating_add(ox), my.saturating_add(oy)),
        Edge::Bottom => (
            mx.saturating_add(ox),
            (my + i64::from(m.height) - i64::from(height)).saturating_sub(oy),
        ),
        Edge::Right => (
            (mx + i64::from(m.width) - i64::from(width)).saturating_sub(ox),
            my.saturating_add(oy),
        ),
    };
    Geometry {
        x: x.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
        y: y.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
        width,
        height,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color(pub u8, pub u8, pub u8, pub u8);
impl Color {
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.strip_prefix('#').ok_or("color must start with #")?;
        if s.len() != 6 && s.len() != 8 {
            return Err("color must be #rrggbb or #rrggbbaa".into());
        }
        let n = u32::from_str_radix(s, 16).map_err(|_| "invalid hex color")?;
        Ok(if s.len() == 6 {
            Self((n >> 16) as u8, (n >> 8) as u8, n as u8, 255)
        } else {
            Self((n >> 24) as u8, (n >> 16) as u8, (n >> 8) as u8, n as u8)
        })
    }
}

#[derive(Clone, Debug)]
pub struct Font {
    pub family: String,
    pub size: f32,
    pub fallback: Vec<String>,
}
impl Default for Font {
    fn default() -> Self {
        Self {
            family: "monospace".into(),
            size: 11.0,
            fallback: vec![],
        }
    }
}

#[derive(Clone, Debug)]
pub struct Palette {
    pub background: Color,
    pub text: Color,
    pub active: Color,
    pub dim: Color,
    pub separator: Color,
    pub urgent: Color,
    pub hover: Color,
    pub warning: Color,
}
impl Default for Palette {
    fn default() -> Self {
        Self {
            background: Color(30, 30, 46, 255),
            text: Color(205, 214, 244, 255),
            active: Color(137, 180, 250, 255),
            dim: Color(108, 112, 134, 255),
            separator: Color(69, 71, 90, 255),
            urgent: Color(243, 139, 168, 255),
            hover: Color(180, 190, 254, 255),
            warning: Color(249, 226, 175, 255),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Theme {
    pub font: Font,
    pub colors: Palette,
    pub icon_theme: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ModuleState {
    #[default]
    Normal,
    Hover,
    Active,
    Urgent,
    Warning,
}
impl ModuleState {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "normal" => Some(Self::Normal),
            "hover" => Some(Self::Hover),
            "active" => Some(Self::Active),
            "urgent" => Some(Self::Urgent),
            "warning" => Some(Self::Warning),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Alignment {
    #[default]
    Start,
    Center,
    End,
}

#[derive(Clone, Debug, Default)]
pub struct StateStyle {
    pub foreground: Option<Color>,
    pub background: Option<Color>,
}

#[derive(Clone, Debug)]
pub struct Style {
    pub font: Option<Font>,
    pub foreground: Option<Color>,
    pub background: Option<Color>,
    pub normal: StateStyle,
    pub hover: StateStyle,
    pub active: StateStyle,
    pub urgent: StateStyle,
    pub warning: StateStyle,
    pub padding: f32,
    pub gap: f32,
    pub corner_radius: f32,
    pub icon_size: f32,
    pub icon_after: bool,
    pub alignment: Alignment,
    pub min_width: f32,
    pub max_width: Option<f32>,
}
impl Default for Style {
    fn default() -> Self {
        Self {
            font: None,
            foreground: None,
            background: None,
            normal: StateStyle::default(),
            hover: StateStyle::default(),
            active: StateStyle::default(),
            urgent: StateStyle::default(),
            warning: StateStyle::default(),
            padding: 8.0,
            gap: 6.0,
            corner_radius: 0.0,
            icon_size: 16.0,
            icon_after: false,
            alignment: Alignment::Start,
            min_width: 0.0,
            max_width: None,
        }
    }
}
impl Style {
    pub fn colors(&self, theme: &Theme, state: ModuleState) -> (Color, Option<Color>) {
        let (s, base) = match state {
            ModuleState::Normal => (&self.normal, theme.colors.text),
            ModuleState::Hover => (&self.hover, theme.colors.hover),
            ModuleState::Active => (&self.active, theme.colors.active),
            ModuleState::Urgent => (&self.urgent, theme.colors.urgent),
            ModuleState::Warning => (&self.warning, theme.colors.warning),
        };
        (
            s.foreground.or(self.foreground).unwrap_or(base),
            s.background.or(self.background),
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModuleKind {
    Clock,
    Cpu,
    Ram,
    CpuRam,
    Workspaces,
    WindowTitle,
    Systray,
    Custom,
}

#[derive(Clone, Debug)]
pub struct ModuleSpec {
    pub kind: ModuleKind,
    pub name: String,
    pub options: Table,
    pub style: Style,
    pub icon: Option<String>,
    pub scroll_coalesce_ms: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct SectionSpec {
    pub name: String,
    pub x: Option<Dimension>,
    pub y: Option<Dimension>,
    pub width: Option<Dimension>,
    pub height: Option<Dimension>,
    pub padding: f32,
    pub gap: f32,
    pub modules: Vec<ModuleSpec>,
}

#[derive(Clone, Debug)]
pub struct Separator {
    pub text: Option<String>,
    pub icon: Option<String>,
    pub color: Option<Color>,
    pub spacing: f32,
}

#[derive(Clone, Debug)]
pub struct BarSpec {
    pub name: String,
    pub monitor: String,
    pub position: Edge,
    pub thickness: Dimension,
    pub length: Dimension,
    pub offset_x: Dimension,
    pub offset_y: Dimension,
    pub reserve_space: bool,
    pub corner_radius: f32,
    pub scale: Option<f64>,
    pub padding: f32,
    pub gap: f32,
    pub background: Option<Color>,
    pub sections: Vec<SectionSpec>,
    pub overrides: Option<Table>,
    pub separator: Option<Separator>,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub theme: Theme,
    pub bars: Vec<BarSpec>,
    pub scale: Option<f64>,
    pub tray_monitor: String,
    pub scroll_coalesce_ms: u64,
    pub watchdog_ms: u64,
    pub reload_debounce_ms: u64,
    pub auto_reload: bool,
    pub wm: Option<Table>,
}

#[derive(Clone, Debug)]
pub struct DisplayItem {
    pub module: usize,
    pub text: String,
    pub icon: Option<String>,
    pub style: Style,
    pub state: ModuleState,
    pub workspace: Option<u32>,
    pub tray: bool,
    pub tray_count: usize,
}

#[derive(Clone, Debug)]
pub struct Hit {
    pub rect: Rect,
    pub module: usize,
    pub workspace: Option<u32>,
}

pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
    pub hits: Vec<Hit>,
    pub tray: Option<Rect>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Workspace {
    pub id: u32,
    pub name: String,
    pub index: u32,
    pub monitor: String,
    pub focused: bool,
    pub visible: bool,
    pub urgent: bool,
    pub occupied: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActiveWindow {
    pub id: u32,
    pub title: String,
    pub class: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    fn bar(position: Edge) -> BarSpec {
        BarSpec {
            name: "geometry-test".into(),
            monitor: "primary".into(),
            position,
            thickness: Dimension::Pixels(28.0),
            length: Dimension::Pixels(200.0),
            offset_x: Dimension::Pixels(0.0),
            offset_y: Dimension::Pixels(0.0),
            reserve_space: true,
            corner_radius: 0.0,
            scale: None,
            padding: 0.0,
            gap: 0.0,
            background: None,
            sections: Vec::new(),
            overrides: None,
            separator: None,
        }
    }
    fn monitor() -> Monitor {
        Monitor {
            name: "test".into(),
            primary: true,
            scale: 1.0,
            geometry: Geometry {
                x: 1920,
                y: 120,
                width: 1920,
                height: 1080,
            },
        }
    }
    #[test]
    fn dimension_scales_pixels_only() {
        assert_eq!(Dimension::Pixels(20.0).resolve(200.0, 2.0), 40.0);
        assert_eq!(Dimension::Percent(50.0).resolve(200.0, 2.0), 100.0);
    }
    #[test]
    fn color_rejects_invalid_input() {
        assert!(Color::parse("#abc").is_err());
        assert_eq!(Color::parse("#1e1e2e").unwrap(), Color(30, 30, 46, 255));
        assert_eq!(Color::parse("#11223344").unwrap(), Color(17, 34, 51, 68));
    }

    #[test]
    fn extreme_finite_offsets_stay_outside_x11_range_without_overflow() {
        for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
            for offset in [f64::MAX, -f64::MAX, i32::MAX as f64, i32::MIN as f64] {
                let mut spec = bar(edge);
                spec.offset_x = Dimension::Pixels(offset);
                spec.offset_y = Dimension::Pixels(offset);
                let geometry = bar_geometry(&spec, &monitor(), 1.0);
                assert!(
                    i16::try_from(geometry.x).is_err() || i16::try_from(geometry.y).is_err(),
                    "extreme {edge:?} offset {offset} became a representable position: {geometry:?}"
                );
            }
        }
    }

    #[test]
    fn bottom_and_right_anchor_after_dimension_clamping() {
        let mut right = bar(Edge::Right);
        right.thickness = Dimension::Pixels(40000.0);
        let geometry = bar_geometry(&right, &monitor(), 1.0);
        assert_eq!(geometry.width, 32767);
        assert_eq!(i64::from(geometry.x) + i64::from(geometry.width), 3840);
        let mut bottom = bar(Edge::Bottom);
        bottom.thickness = Dimension::Pixels(40000.0);
        let geometry = bar_geometry(&bottom, &monitor(), 1.0);
        assert_eq!(geometry.height, 32767);
        assert_eq!(i64::from(geometry.y) + i64::from(geometry.height), 1200);
    }

    #[test]
    fn normal_geometry_preserves_monitor_offsets_scaling_and_percentages() {
        let mut spec = bar(Edge::Bottom);
        spec.offset_x = Dimension::Percent(5.0);
        spec.offset_y = Dimension::Pixels(6.0);
        assert_eq!(
            bar_geometry(&spec, &monitor(), 2.0),
            Geometry {
                x: 2016,
                y: 1132,
                width: 400,
                height: 56
            }
        );
        spec.position = Edge::Right;
        assert_eq!(
            bar_geometry(&spec, &monitor(), 2.0),
            Geometry {
                x: 3688,
                y: 132,
                width: 56,
                height: 400
            }
        );
    }

    #[test]
    fn framebuffer_prevalidation_checks_limit_zero_dimensions_and_overflow() {
        let geometry = |width, height| Geometry {
            x: 0,
            y: 0,
            width,
            height,
        };
        assert!(validate_framebuffer(geometry(8192, 8192)).is_ok());
        assert!(
            validate_framebuffer(geometry(8193, 8192))
                .unwrap_err()
                .to_string()
                .contains("256 MiB")
        );
        assert!(validate_framebuffer(geometry(0, 28)).is_err());
        assert!(
            validate_framebuffer(geometry(u32::MAX, u32::MAX))
                .unwrap_err()
                .to_string()
                .contains("overflow")
        );
    }
}
