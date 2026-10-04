// SPDX-License-Identifier: GPL-3.0-or-later
//! Lua configuration loading, validation, and monitor-specific layering.
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use mlua::{Lua, Table, Value};

use crate::model::*;

fn lua_result<T>(result: mlua::Result<T>) -> Result<T> {
    result.map_err(|error| anyhow!(error.to_string()))
}

pub fn default_path() -> PathBuf {
    env::var_os("XDG_CONFIG_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
                .join(".config")
        })
        .join("cybar/config.lua")
}

pub fn expand_path(path: &str) -> PathBuf {
    if path == "~" || path.starts_with("~/") {
        if let Some(home) = env::var_os("HOME") {
            return PathBuf::from(home).join(path.strip_prefix("~/").unwrap_or(""));
        }
    }
    PathBuf::from(path)
}

/// Constructors are ordinary Lua functions returning ordinary options tables.
pub fn install_constructors(lua: &Lua, api: &Table) -> mlua::Result<()> {
    for kind in [
        "clock",
        "cpu",
        "ram",
        "cpu_ram",
        "workspaces",
        "window_title",
        "systray",
        "custom",
    ] {
        api.set(
            kind,
            lua.create_function(move |lua, options: Option<Table>| {
                let options = match options {
                    Some(table) => table,
                    None => lua.create_table()?,
                };
                options.set("kind", kind)?;
                Ok(options)
            })?,
        )?;
    }
    let package: Table = lua.globals().get("package")?;
    let loaded: Table = package.get("loaded")?;
    loaded.set("cybar", api.clone())?;
    let preload: Table = package.get("preload")?;
    let api = api.clone();
    preload.set(
        "cybar",
        lua.create_function(move |_, _: Value| Ok(api.clone()))?,
    )?;
    Ok(())
}

pub fn load(lua: &Lua, path: &Path) -> Result<Config> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("read configuration {}", path.display()))?;
    let package: Table = lua_result(lua.globals().get("package"))?;
    let old_path: String = lua_result(package.get("path"))?;
    let directory = path.parent().unwrap_or(Path::new(".")).to_string_lossy();
    lua_result(package.set(
        "path",
        format!("{directory}/?.lua;{directory}/?/init.lua;{old_path}"),
    ))?;
    let value: Value = lua_result(
        lua.load(&text)
            .set_name(format!("@{}", path.display()))
            .eval(),
    )
    .with_context(|| format!("load configuration {}", path.display()))?;
    let root = as_table(value, "configuration must return a table")?;
    let directory = path.parent().unwrap_or(Path::new("."));
    let directory = if directory.is_absolute() {
        directory.to_owned()
    } else {
        env::current_dir()?.join(directory)
    };
    normalize_paths(&root, &directory, &mut HashSet::new())?;
    parse_config(lua, &root).with_context(|| format!("invalid configuration {}", path.display()))
}

fn normalize_paths(input: &Table, directory: &Path, seen: &mut HashSet<usize>) -> Result<()> {
    if !seen.insert(input.to_pointer() as usize) {
        return Ok(());
    }
    if let Some(icon) = string(input, "icon")? {
        let path = expand_path(&icon);
        let extension = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !path.is_absolute()
            && (icon.contains('/')
                || [
                    "svg", "png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "tif", "tiff",
                ]
                .contains(&extension.as_str()))
        {
            lua_result(input.set("icon", directory.join(path).to_string_lossy().to_string()))?;
        }
    }
    if let Some(source) = table(input, "source")? {
        for key in ["file", "socket"] {
            if let Some(path) = string(&source, key)? {
                let path = expand_path(&path);
                let path = if path.is_absolute() {
                    path
                } else {
                    directory.join(path)
                };
                lua_result(source.set(key, path.to_string_lossy().to_string()))?;
            }
        }
    }
    for pair in input.pairs::<Value, Value>() {
        if let Value::Table(value) = lua_result(pair)?.1 {
            normalize_paths(&value, directory, seen)?;
        }
    }
    Ok(())
}

fn get(table: &Table, key: &str) -> Result<Value> {
    lua_result(table.get(key))
}
fn table(table: &Table, key: &str) -> Result<Option<Table>> {
    match get(table, key)? {
        Value::Nil => Ok(None),
        Value::Table(value) => Ok(Some(value)),
        _ => bail!("{key} must be a table"),
    }
}
fn as_table(value: Value, message: &str) -> Result<Table> {
    match value {
        Value::Table(table) => Ok(table),
        _ => bail!("{message}"),
    }
}
fn string(table: &Table, key: &str) -> Result<Option<String>> {
    match get(table, key)? {
        Value::Nil => Ok(None),
        Value::String(value) => Ok(Some(lua_result(value.to_str())?.to_owned())),
        _ => bail!("{key} must be a string"),
    }
}
fn nonempty(table: &Table, key: &str) -> Result<Option<String>> {
    let value = string(table, key)?;
    if let Some(value) = &value {
        ensure!(
            !value.trim().is_empty() && !value.contains('\0'),
            "{key} must be a nonempty string without NUL characters"
        );
    }
    Ok(value)
}
fn boolean(table: &Table, key: &str, default: bool) -> Result<bool> {
    match get(table, key)? {
        Value::Nil => Ok(default),
        Value::Boolean(value) => Ok(value),
        _ => bail!("{key} must be a boolean"),
    }
}
fn number_value(value: Value, key: &str) -> Result<Option<f64>> {
    let number = match value {
        Value::Nil => return Ok(None),
        Value::Integer(n) => n as f64,
        Value::Number(n) => n,
        _ => bail!("{key} must be a number"),
    };
    ensure!(number.is_finite(), "{key} must be finite");
    Ok(Some(number))
}
fn number(table: &Table, key: &str) -> Result<Option<f64>> {
    number_value(get(table, key)?, key)
}
fn positive(table: &Table, key: &str) -> Result<Option<f64>> {
    let value = number(table, key)?;
    if let Some(value) = value {
        ensure!(value > 0.0, "{key} must be positive");
    }
    Ok(value)
}
fn interval(table: &Table) -> Result<Option<f64>> {
    let value = positive(table, "interval")?;
    if let Some(value) = value {
        ensure!(
            value <= 365.0 * 24.0 * 60.0 * 60.0,
            "interval must be at most one year (365 days)"
        );
    }
    Ok(value)
}
fn manual_scale(table: &Table) -> Result<Option<f64>> {
    let value = positive(table, "scale")?;
    if let Some(value) = value {
        ensure!(
            (0.25..=16.0).contains(&value),
            "manual scale must be between 0.25 and 16"
        );
    }
    Ok(value)
}
fn nonnegative(table: &Table, key: &str, default: f32) -> Result<f32> {
    let value = number(table, key)?.unwrap_or(default as f64);
    ensure!(
        value >= 0.0 && value <= f32::MAX as f64,
        "{key} must be a finite nonnegative number"
    );
    Ok(value as f32)
}
fn integer(table: &Table, key: &str) -> Result<Option<u64>> {
    let value = number(table, key)?;
    if let Some(value) = value {
        ensure!(
            value >= 0.0 && value.fract() == 0.0 && value < u64::MAX as f64,
            "{key} must be a nonnegative integer"
        );
    }
    Ok(value.map(|n| n as u64))
}
fn name(table: &Table, default: String) -> Result<String> {
    let value = nonempty(table, "name")?.unwrap_or(default);
    ensure!(
        !value.contains(['\n', '\r']) && value.len() <= 256,
        "name must contain at most 256 bytes and no newlines"
    );
    Ok(value)
}
fn color(table: &Table, key: &str) -> Result<Option<Color>> {
    string(table, key)?
        .map(|value| Color::parse(&value).map_err(|error| anyhow!("{key}: {error}")))
        .transpose()
}
fn dimension(value: Value, key: &str, is_size: bool) -> Result<Option<Dimension>> {
    let value = match value {
        Value::Nil => return Ok(None),
        Value::String(s) => {
            let s = lua_result(s.to_str())?;
            let number = s
                .strip_suffix('%')
                .ok_or_else(|| anyhow!("{key} must be integer pixels or a percentage string"))?
                .trim()
                .parse::<f64>()
                .with_context(|| format!("invalid percentage for {key}"))?;
            ensure!(
                number.is_finite() && number.abs() <= i32::MAX as f64,
                "{key} must be finite and within supported coordinates"
            );
            Dimension::Percent(number)
        }
        value => {
            let number = number_value(value, key)?.unwrap();
            ensure!(
                number.fract() == 0.0 && number.abs() <= i32::MAX as f64,
                "{key} pixel dimensions must be integers within supported coordinates"
            );
            Dimension::Pixels(number)
        }
    };
    let numeric = match value {
        Dimension::Pixels(n) | Dimension::Percent(n) => n,
    };
    if is_size {
        ensure!(numeric > 0.0, "{key} must be positive");
    }
    Ok(Some(value))
}
fn dim(table: &Table, key: &str, is_size: bool) -> Result<Option<Dimension>> {
    dimension(get(table, key)?, key, is_size)
}

/// Reject holes rather than silently truncating a Lua sequence at its first nil.
fn array(table: &Table, label: &str, allow_metadata: bool) -> Result<Vec<Value>> {
    let mut values = BTreeMap::new();
    for pair in table.pairs::<Value, Value>() {
        let (key, value) = lua_result(pair)?;
        match key {
            Value::Integer(index) if index > 0 => {
                values.insert(index, value);
            }
            Value::String(_) if allow_metadata => (),
            _ => bail!("{label} must be a contiguous array with positive integer indices"),
        }
    }
    for (expected, index) in (1_i64..).zip(values.keys()) {
        ensure!(
            expected == *index,
            "{label} contains a hole at index {expected}"
        );
    }
    Ok(values.into_values().collect())
}

fn parse_font(input: &Table, base: &Font) -> Result<Font> {
    let family = nonempty(input, "family")?.unwrap_or_else(|| base.family.clone());
    let size = positive(input, "size")?.unwrap_or(base.size as f64);
    ensure!(size <= 4096.0, "font size must be at most 4096 points");
    let fallback = if let Some(list) = table(input, "fallback")? {
        array(&list, "font fallback", false)?
            .into_iter()
            .map(|value| {
                let Value::String(value) = value else {
                    bail!("font fallback entries must be strings")
                };
                let value = lua_result(value.to_str())?.to_owned();
                ensure!(
                    !value.trim().is_empty(),
                    "font fallback families must be nonempty"
                );
                Ok(value)
            })
            .collect::<Result<Vec<_>>>()?
    } else {
        base.fallback.clone()
    };
    Ok(Font {
        family,
        size: size as f32,
        fallback,
    })
}

fn parse_state(table: &Table, base: &StateStyle) -> Result<StateStyle> {
    Ok(StateStyle {
        foreground: color(table, "foreground")?
            .or(color(table, "text")?)
            .or(color(table, "color")?)
            .or(base.foreground),
        background: color(table, "background")?.or(base.background),
    })
}

pub fn parse_style(input: &Table, base: &Style) -> Result<Style> {
    let mut style = base.clone();
    if let Some(font) = table(input, "font")? {
        style.font = Some(parse_font(
            &font,
            style.font.as_ref().unwrap_or(&Font::default()),
        )?);
    }
    style.foreground = color(input, "foreground")?
        .or(color(input, "text")?)
        .or(color(input, "color")?)
        .or(style.foreground);
    style.background = color(input, "background")?.or(style.background);
    style.padding = nonnegative(input, "padding", style.padding)?;
    style.gap = nonnegative(input, "gap", style.gap)?;
    style.corner_radius = nonnegative(input, "corner_radius", style.corner_radius)?;
    style.icon_size = nonnegative(input, "icon_size", style.icon_size)?;
    ensure!(style.icon_size > 0.0, "icon_size must be positive");
    style.icon_after = boolean(input, "icon_after", style.icon_after)?;
    if let Some(position) = string(input, "icon_position")? {
        style.icon_after = match position.as_str() {
            "before" => false,
            "after" => true,
            _ => bail!("icon_position must be before or after"),
        };
    }
    if let Some(alignment) = string(input, "alignment")?.or(string(input, "align")?) {
        style.alignment = match alignment.as_str() {
            "start" => Alignment::Start,
            "center" => Alignment::Center,
            "end" => Alignment::End,
            _ => bail!("alignment must be start, center, or end"),
        };
    }
    style.min_width = nonnegative(input, "min_width", style.min_width)?;
    if number(input, "max_width")?.is_some() {
        style.max_width = Some(nonnegative(input, "max_width", 0.0)?);
    }
    if let Some(max) = style.max_width {
        ensure!(
            max >= style.min_width && max > 0.0,
            "max_width must be positive and at least min_width"
        );
    }
    let states = table(input, "states")?;
    let colors = table(input, "colors")?;
    if let Some(colors) = &colors {
        style.foreground = color(colors, "foreground")?
            .or(color(colors, "text")?)
            .or(style.foreground);
        style.background = color(colors, "background")?.or(style.background);
    }
    for (name, state) in [
        ("normal", &mut style.normal),
        ("hover", &mut style.hover),
        ("active", &mut style.active),
        ("urgent", &mut style.urgent),
        ("warning", &mut style.warning),
    ] {
        if let Some(colors) = &colors {
            match get(colors, name)? {
                Value::Nil => (),
                Value::Table(value) => *state = parse_state(&value, state)?,
                Value::String(value) => {
                    state.foreground = Some(
                        Color::parse(&lua_result(value.to_str())?)
                            .map_err(|error| anyhow!("colors.{name}: {error}"))?,
                    )
                }
                _ => bail!("colors.{name} must be a color string or style table"),
            }
        }
        if let Some(states) = &states {
            if let Some(value) = table(states, name)? {
                *state = parse_state(&value, state)?;
            }
        }
        if let Some(value) = table(input, name)? {
            *state = parse_state(&value, state)?;
        }
    }
    Ok(style)
}

fn parse_theme(input: Option<Table>) -> Result<Theme> {
    let mut theme = Theme::default();
    let Some(input) = input else { return Ok(theme) };
    if let Some(font) = table(&input, "font")? {
        theme.font = parse_font(&font, &theme.font)?;
    }
    theme.icon_theme = nonempty(&input, "icon_theme")?;
    if let Some(colors) = table(&input, "colors")? {
        theme.colors.background = color(&colors, "background")?.unwrap_or(theme.colors.background);
        theme.colors.text = color(&colors, "text")?.unwrap_or(theme.colors.text);
        theme.colors.active = color(&colors, "text_active")?
            .or(color(&colors, "active")?)
            .unwrap_or(theme.colors.active);
        theme.colors.dim = color(&colors, "text_dim")?
            .or(color(&colors, "dim")?)
            .unwrap_or(theme.colors.dim);
        theme.colors.separator = color(&colors, "separator")?.unwrap_or(theme.colors.separator);
        theme.colors.urgent = color(&colors, "urgent")?.unwrap_or(theme.colors.urgent);
        theme.colors.hover = color(&colors, "hover")?.unwrap_or(theme.colors.hover);
        theme.colors.warning = color(&colors, "warning")?.unwrap_or(theme.colors.warning);
    }
    Ok(theme)
}

const HANDLERS: &[&str] = &[
    "on_click_left",
    "on_click_middle",
    "on_click_right",
    "on_click_button",
    "on_scroll",
    "on_hover_enter",
    "on_hover_leave",
    "on_message",
];

fn validate_source(input: &Table) -> Result<()> {
    let kind = nonempty(input, "type")?
        .or(nonempty(input, "kind")?)
        .ok_or_else(|| anyhow!("source.type is required"))?;
    lua_result(input.set("type", kind.clone()))?;
    interval(input)?;
    match kind.as_str() {
        "poll" => {
            let poll = get(input, "poll")?;
            let function = if poll.is_nil() {
                get(input, "fn")?
            } else {
                poll
            };
            ensure!(
                matches!(function, Value::Function(_)),
                "poll source requires poll or fn function"
            );
            lua_result(input.set("poll", function))?;
        }
        "command" => {
            ensure!(
                nonempty(input, "command")?.is_some(),
                "command source requires command"
            );
        }
        "stream" => {
            let paths = [
                nonempty(input, "file")?,
                nonempty(input, "socket")?,
                nonempty(input, "command")?,
            ];
            ensure!(
                paths.iter().filter(|path| path.is_some()).count() == 1,
                "stream source requires exactly one of file, socket, or command"
            );
        }
        _ => bail!("source.type must be poll, command, or stream"),
    }
    ensure!(
        matches!(get(input, "parse")?, Value::Nil | Value::Function(_)),
        "source.parse must be a function"
    );
    Ok(())
}

fn parse_module(input: Table, default_name: String, theme_font: &Font) -> Result<ModuleSpec> {
    let kind_name = nonempty(&input, "kind")?
        .or(nonempty(&input, "type")?)
        .unwrap_or_else(|| "custom".into());
    let kind = match kind_name.as_str() {
        "clock" => ModuleKind::Clock,
        "cpu" => ModuleKind::Cpu,
        "ram" => ModuleKind::Ram,
        "cpu_ram" => ModuleKind::CpuRam,
        "workspaces" => ModuleKind::Workspaces,
        "window_title" => ModuleKind::WindowTitle,
        "systray" => ModuleKind::Systray,
        "custom" => ModuleKind::Custom,
        _ => bail!("unknown module kind {kind_name}"),
    };
    let name = name(&input, default_name)?;
    lua_result(input.set("name", name.clone()))?;
    lua_result(input.set("kind", kind_name))?;
    interval(&input)?;
    integer(&input, "max_length")?;
    if let Some(format) = string(&input, "format")? {
        ensure!(!format.contains('\0'), "format may not contain NUL");
        if kind == ModuleKind::Clock {
            ensure!(
                !chrono::format::StrftimeItems::new(&format)
                    .any(|item| matches!(item, chrono::format::Item::Error)),
                "clock format contains an unsupported strftime directive"
            );
        }
    }
    string(&input, "empty_text")?;
    if let Some(source) = table(&input, "source")? {
        validate_source(&source).context("invalid module source")?;
    }
    if let Some(handlers) = table(&input, "handlers")? {
        for handler in HANDLERS {
            if get(&input, handler)?.is_nil() {
                lua_result(input.set(*handler, get(&handlers, handler)?))?;
            }
        }
    }
    for handler in HANDLERS {
        ensure!(
            matches!(
                get(&input, handler)?,
                Value::Nil | Value::Function(_) | Value::Boolean(false)
            ),
            "{handler} must be a function or false"
        );
    }
    let render = get(&input, "render")?;
    ensure!(
        matches!(render, Value::Nil | Value::Function(_)),
        "render must be a function"
    );
    if kind == ModuleKind::Custom {
        ensure!(
            matches!(render, Value::Function(_)),
            "custom module {name} requires a render function"
        );
    }
    // Resolve the theme before applying partial overrides so both runtime
    // mutations and rendering inherit the same complete font.
    let base = Style {
        font: Some(theme_font.clone()),
        ..Style::default()
    };
    let mut style = parse_style(&input, &base)?;
    if let Some(overrides) = table(&input, "style")? {
        style = parse_style(&overrides, &style)?;
    }
    let icon = nonempty(&input, "icon")?;
    let scroll_coalesce_ms = integer(&input, "scroll_coalesce_ms")?;
    Ok(ModuleSpec {
        kind,
        name,
        options: input,
        style,
        icon,
        scroll_coalesce_ms,
    })
}

fn parse_section(
    input: Table,
    section_name: String,
    bar_name: &str,
    custom: bool,
    bar_gap: f32,
    theme_font: &Font,
) -> Result<SectionSpec> {
    let section_name = if custom {
        name(&input, section_name)?
    } else {
        section_name
    };
    let position = table(&input, "position")?;
    let size = table(&input, "size")?;
    let x = if let Some(position) = &position {
        dim(position, "x", false)?
    } else {
        None
    }
    .or(dim(&input, "x", false)?);
    let y = if let Some(position) = &position {
        dim(position, "y", false)?
    } else {
        None
    }
    .or(dim(&input, "y", false)?);
    let width = if let Some(size) = &size {
        dim(size, "width", true)?
    } else {
        None
    }
    .or(dim(&input, "width", true)?);
    let height = if let Some(size) = &size {
        dim(size, "height", true)?
    } else {
        None
    }
    .or(dim(&input, "height", true)?);
    let padding = nonnegative(&input, "padding", 0.0)?;
    let gap = nonnegative(&input, "gap", bar_gap)?;
    let module_list = table(&input, "modules")?.unwrap_or_else(|| input.clone());
    let mut modules = vec![];
    for (index, value) in array(&module_list, "section modules", true)?
        .into_iter()
        .enumerate()
    {
        let module = as_table(value, "module entries must be tables")?;
        let module = parse_module(
            module,
            format!("{bar_name}.{section_name}.{}", index + 1),
            theme_font,
        )
        .with_context(|| format!("module {} in section {section_name}", index + 1))?;
        modules.push(module);
    }
    Ok(SectionSpec {
        name: section_name,
        x,
        y,
        width,
        height,
        padding,
        gap,
        modules,
    })
}

fn parse_separator(input: &Table) -> Result<Option<Separator>> {
    let Some(input) = table(input, "separator")? else {
        return Ok(None);
    };
    let text = string(&input, "text")?.or(string(&input, "glyph")?);
    let icon = nonempty(&input, "icon")?;
    Ok(Some(Separator {
        text: Some(text.unwrap_or_else(|| "│".into())).filter(|s| !s.is_empty()),
        icon,
        color: color(&input, "color")?,
        spacing: nonnegative(&input, "spacing", 6.0)?,
    }))
}

fn parse_bar(
    input: &Table,
    index: usize,
    global_scale: Option<f64>,
    theme_font: &Font,
) -> Result<BarSpec> {
    let name = name(input, format!("bar-{index}"))?;
    let monitor = nonempty(input, "monitor")?.unwrap_or_else(|| "primary".into());
    let position = match string(input, "position")?.as_deref().unwrap_or("top") {
        "top" => Edge::Top,
        "bottom" => Edge::Bottom,
        "left" => Edge::Left,
        "right" => Edge::Right,
        _ => bail!("position must be top, bottom, left, or right"),
    };
    let size = table(input, "size")?;
    let thickness = if let Some(size) = &size {
        dim(size, "thickness", true)?
    } else {
        None
    }
    .or(dim(input, "thickness", true)?)
    .unwrap_or(Dimension::Pixels(28.0));
    let length = if let Some(size) = &size {
        dim(size, "length", true)?
    } else {
        None
    }
    .or(dim(input, "length", true)?)
    .unwrap_or(Dimension::Percent(100.0));
    let offset = table(input, "offset")?;
    let offset_x = if let Some(offset) = &offset {
        dim(offset, "x", false)?
    } else {
        None
    }
    .unwrap_or(Dimension::Pixels(0.0));
    let offset_y = if let Some(offset) = &offset {
        dim(offset, "y", false)?
    } else {
        None
    }
    .unwrap_or(Dimension::Pixels(0.0));
    let gap = nonnegative(input, "gap", 6.0)?;
    let mut sections = vec![];
    if let Some(input_sections) = table(input, "sections")? {
        for section in ["left", "middle", "right"] {
            if let Some(value) = table(&input_sections, section)? {
                sections.push(parse_section(
                    value,
                    section.into(),
                    &name,
                    false,
                    gap,
                    theme_font,
                )?);
            }
        }
        if let Some(custom) = table(&input_sections, "custom")? {
            for (index, value) in array(&custom, "custom sections", false)?
                .into_iter()
                .enumerate()
            {
                let section = as_table(value, "custom sections must be tables")?;
                ensure!(
                    nonempty(&section, "name")?.is_some(),
                    "custom sections require a name"
                );
                sections.push(parse_section(
                    section,
                    format!("custom-{}", index + 1),
                    &name,
                    true,
                    gap,
                    theme_font,
                )?);
            }
        }
        for pair in input_sections.pairs::<Value, Value>() {
            let (key, _) = lua_result(pair)?;
            let Value::String(key) = key else {
                bail!("sections must use named fields")
            };
            let key = lua_result(key.to_str())?;
            ensure!(
                ["left", "middle", "right", "custom"].contains(&key.as_ref()),
                "unknown section {key}; put named sections in sections.custom"
            );
        }
    }
    let mut section_names = HashSet::new();
    let mut module_names = HashSet::new();
    for section in &sections {
        ensure!(
            section_names.insert(section.name.clone()),
            "duplicate section name {}",
            section.name
        );
        for module in &section.modules {
            ensure!(
                module_names.insert(module.name.clone()),
                "duplicate module name {}",
                module.name
            );
        }
    }
    Ok(BarSpec {
        name,
        monitor,
        position,
        thickness,
        length,
        offset_x,
        offset_y,
        reserve_space: boolean(input, "reserve_space", true)?,
        corner_radius: nonnegative(input, "corner_radius", 0.0)?,
        scale: manual_scale(input)?.or(global_scale),
        padding: nonnegative(input, "padding", 0.0)?,
        gap,
        background: color(input, "background")?,
        sections,
        overrides: None,
        separator: parse_separator(input)?,
    })
}

fn clone_value(
    lua: &Lua,
    value: Value,
    seen: &mut HashMap<usize, Table>,
    depth: usize,
) -> Result<Value> {
    let Value::Table(input) = value else {
        return Ok(value);
    };
    let id = input.to_pointer() as usize;
    if let Some(table) = seen.get(&id) {
        return Ok(Value::Table(table.clone()));
    }
    ensure!(depth < 64, "configuration tables are nested too deeply");
    let output = lua_result(lua.create_table())?;
    seen.insert(id, output.clone());
    for pair in input.pairs::<Value, Value>() {
        let (key, value) = lua_result(pair)?;
        lua_result(output.set(key, clone_value(lua, value, seen, depth + 1)?))?;
    }
    Ok(Value::Table(output))
}
fn clone_table(lua: &Lua, input: &Table) -> Result<Table> {
    as_table(
        clone_value(lua, Value::Table(input.clone()), &mut HashMap::new(), 0)?,
        "expected table",
    )
}
fn is_sequence(input: &Table) -> Result<bool> {
    for pair in input.pairs::<Value, Value>() {
        if matches!(lua_result(pair)?.0, Value::Integer(_) | Value::Number(_)) {
            return Ok(true);
        }
    }
    Ok(false)
}
fn merge(lua: &Lua, base: &Table, override_table: &Table, depth: usize) -> Result<()> {
    ensure!(depth < 64, "monitor override tables are nested too deeply");
    for pair in override_table.pairs::<Value, Value>() {
        let (key, value) = lua_result(pair)?;
        if let Value::String(key) = &key {
            if lua_result(key.to_str())?.starts_with("_cybar_") {
                continue;
            }
        }
        let previous: Value = lua_result(base.get(key.clone()))?;
        match (previous, value) {
            (Value::Table(previous), Value::Table(value))
                if !is_sequence(&value)? && !is_sequence(&previous)? =>
            {
                merge(lua, &previous, &value, depth + 1)?
            }
            (_, value) => {
                lua_result(base.set(key, clone_value(lua, value, &mut HashMap::new(), 0)?))?
            }
        }
    }
    Ok(())
}

fn parse_config(lua: &Lua, root: &Table) -> Result<Config> {
    let theme = parse_theme(table(root, "theme")?)?;
    let scale = manual_scale(root)?;
    let bars_table = table(root, "bars")?.ok_or_else(|| anyhow!("bars is required"))?;
    let mut bars = vec![];
    let mut bar_names = HashSet::new();
    let mut module_names = HashSet::new();
    for (index, value) in array(&bars_table, "bars", false)?.into_iter().enumerate() {
        let input = as_table(value, "bar entries must be tables")?;
        let mut bar = parse_bar(&input, index + 1, scale, &theme.font)
            .with_context(|| format!("bar {}", index + 1))?;
        ensure!(
            bar_names.insert(bar.name.clone()),
            "duplicate bar name {}",
            bar.name
        );
        for section in &bar.sections {
            for module in &section.modules {
                ensure!(
                    module_names.insert(module.name.clone()),
                    "duplicate module name {}",
                    module.name
                );
            }
        }
        if let Some(overrides) = table(&input, "overrides")?.or(table(&input, "monitor_overrides")?)
        {
            let base = clone_table(lua, &input)?;
            lua_result(base.set("overrides", Value::Nil))?;
            lua_result(base.set("monitor_overrides", Value::Nil))?;
            lua_result(base.set("name", bar.name.clone()))?;
            if let Some(scale) = bar.scale {
                lua_result(base.set("scale", scale))?;
            }
            // Validate every alternate monitor configuration before accepting reload.
            for pair in overrides.pairs::<Value, Value>() {
                let (key, value) = lua_result(pair)?;
                let Value::String(key) = key else {
                    bail!("monitor overrides must be keyed by monitor name")
                };
                let key = lua_result(key.to_str())?.to_owned();
                ensure!(
                    !key.trim().is_empty() && !key.starts_with("_cybar_"),
                    "invalid monitor override name {key}"
                );
                as_table(value, "each monitor override must be a table")?;
                // Check both possible primary statuses for named outputs, since
                // the primary output can change while the daemon is running.
                for primary in [false, true] {
                    if key == "all" && primary || key == "primary" && !primary {
                        continue;
                    }
                    let monitor_name = if key == "all" || key == "primary" {
                        "_cybar_validation_output"
                    } else {
                        &key
                    };
                    let merged = merge_monitor(lua, &base, &overrides, monitor_name, primary)?;
                    parse_bar(&merged, index + 1, scale, &theme.font)
                        .with_context(|| format!("override for monitor {key}"))?;
                }
            }
            let overrides = clone_table(lua, &overrides)?;
            lua_result(overrides.set("_cybar_base", base))?;
            let font = lua_result(lua.create_table())?;
            lua_result(font.set("family", theme.font.family.clone()))?;
            lua_result(font.set("size", theme.font.size))?;
            lua_result(font.set(
                "fallback",
                lua_result(lua.create_sequence_from(theme.font.fallback.clone()))?,
            ))?;
            lua_result(overrides.set("_cybar_theme_font", font))?;
            bar.overrides = Some(overrides);
        }
        bars.push(bar);
    }
    ensure!(!bars.is_empty(), "at least one bar is required");
    let wm = table(root, "wm")?;
    if let Some(wm) = &wm {
        match get(wm, "backend")? {
            Value::Nil => (),
            Value::String(name) if lua_result(name.to_str())? == "ewmh" => {
                lua_result(wm.set("backend", Value::Nil))?
            }
            Value::Table(backend) => ensure!(
                matches!(get(&backend, "connect")?, Value::Function(_)),
                "wm.backend requires connect function"
            ),
            _ => bail!("wm.backend must be ewmh or a Lua backend table"),
        }
        table(wm, "opts")?;
    }
    let watchdog_ms = integer(root, "watchdog_ms")?.unwrap_or(100);
    ensure!(watchdog_ms > 0, "watchdog_ms must be positive");
    Ok(Config {
        theme,
        bars,
        scale,
        tray_monitor: nonempty(root, "tray_monitor")?.unwrap_or_else(|| "primary".into()),
        scroll_coalesce_ms: integer(root, "scroll_coalesce_ms")?.unwrap_or(40),
        watchdog_ms,
        reload_debounce_ms: integer(root, "reload_debounce_ms")?.unwrap_or(150),
        auto_reload: boolean(root, "auto_reload", true)?,
        wm,
    })
}

/// Overrides layer in the order all, primary (when applicable), exact output name.
/// Named maps merge recursively; module and custom-section arrays replace the base.
pub fn for_monitor(lua: &Lua, spec: &BarSpec, monitor: &Monitor) -> Result<BarSpec> {
    ensure!(
        monitor.scale.is_finite() && monitor.scale > 0.0,
        "monitor scale must be finite and positive"
    );
    let mut result = spec.clone();
    if let Some(overrides) = &spec.overrides {
        let base = table(overrides, "_cybar_base")?
            .ok_or_else(|| anyhow!("monitor override base is missing"))?;
        let font = table(overrides, "_cybar_theme_font")?
            .ok_or_else(|| anyhow!("monitor override theme font is missing"))?;
        let font = parse_font(&font, &Font::default())?;
        let merged = merge_monitor(lua, &base, overrides, &monitor.name, monitor.primary)?;
        result = parse_bar(&merged, 1, spec.scale, &font)?;
        result.overrides = spec.overrides.clone();
    }
    result.scale = Some(result.scale.unwrap_or(monitor.scale));
    Ok(result)
}

fn merge_monitor(
    lua: &Lua,
    base: &Table,
    overrides: &Table,
    monitor_name: &str,
    primary: bool,
) -> Result<Table> {
    let merged = clone_table(lua, base)?;
    let mut keys = vec!["all"];
    if primary {
        keys.push("primary");
    }
    if !keys.contains(&monitor_name) {
        keys.push(monitor_name);
    }
    for key in keys {
        if let Some(value) = table(overrides, key)? {
            merge(lua, &merged, &value, 0)?;
        }
    }
    Ok(merged)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(text: &str) -> Result<(Lua, Config)> {
        let lua = Lua::new();
        let api = lua_result(lua.create_table())?;
        lua_result(install_constructors(&lua, &api))?;
        let root: Table = lua_result(lua.load(text).eval())?;
        let config = parse_config(&lua, &root)?;
        Ok((lua, config))
    }
    fn assert_font(font: &Font, family: &str, size: f32, fallback: &[&str]) {
        assert_eq!(font.family, family);
        assert_eq!(font.size, size);
        assert_eq!(
            font.fallback.iter().map(String::as_str).collect::<Vec<_>>(),
            fallback
        );
    }
    #[test]
    fn partial_module_fonts_inherit_the_theme_and_preserve_override_precedence() {
        let (_, config) = parse(
            r#"local b=require('cybar'); return {
                theme={font={family='DejaVu Sans',size=17,fallback={'Noto Sans','Emoji'}}},
                bars={{sections={left={
                    b.cpu{},
                    b.cpu{font={size=13}},
                    b.ram{style={font={family='Module Face'}}},
                    b.ram{font={family='Direct Face',size=15},style={font={fallback={}}}},
                    b.cpu{font={fallback={'CJK'}}},
                    b.cpu{font={family='Direct Face'},style={font={size=14}}},
                }}}},
            }"#,
        )
        .unwrap();
        let modules = &config.bars[0].sections[0].modules;
        let fallback = ["Noto Sans", "Emoji"];
        assert_font(
            modules[0].style.font.as_ref().unwrap(),
            "DejaVu Sans",
            17.0,
            &fallback,
        );
        assert_font(
            modules[1].style.font.as_ref().unwrap(),
            "DejaVu Sans",
            13.0,
            &fallback,
        );
        assert_font(
            modules[2].style.font.as_ref().unwrap(),
            "Module Face",
            17.0,
            &fallback,
        );
        assert_font(
            modules[3].style.font.as_ref().unwrap(),
            "Direct Face",
            15.0,
            &[],
        );
        assert_font(
            modules[4].style.font.as_ref().unwrap(),
            "DejaVu Sans",
            17.0,
            &["CJK"],
        );
        assert_font(
            modules[5].style.font.as_ref().unwrap(),
            "Direct Face",
            14.0,
            &fallback,
        );
    }
    #[test]
    fn monitor_module_replacements_inherit_the_original_theme_font() {
        let (lua, config) = parse(
            r#"local b=require('cybar'); return {
                theme={font={family='DejaVu Sans',size=17,fallback={'Noto Sans','Emoji'}}},
                bars={
                    {name='main',sections={
                        left={b.cpu{font={family='Base Face',size=22}}},
                        middle={b.clock{}},right={b.ram{}},
                    },overrides={
                        all={sections={left={b.cpu{font={size=13}}}}},
                        primary={sections={right={b.ram{style={font={family='Primary Face'}}}}}},
                        ['HDMI-1']={sections={middle={b.clock{style={font={fallback={'CJK'}}}}}}},
                    }},
                    {name='empty',overrides={
                        primary={sections={left={b.cpu{style={font={size=19}}}}}},
                    }},
                },
            }"#,
        )
        .unwrap();
        let monitor = Monitor {
            name: "HDMI-1".into(),
            geometry: Geometry {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            primary: true,
            scale: 1.0,
        };
        let bar = for_monitor(&lua, &config.bars[0], &monitor).unwrap();
        let fallback = ["Noto Sans", "Emoji"];
        assert_font(
            bar.sections[0].modules[0].style.font.as_ref().unwrap(),
            "DejaVu Sans",
            13.0,
            &fallback,
        );
        assert_font(
            bar.sections[1].modules[0].style.font.as_ref().unwrap(),
            "DejaVu Sans",
            17.0,
            &["CJK"],
        );
        assert_font(
            bar.sections[2].modules[0].style.font.as_ref().unwrap(),
            "Primary Face",
            17.0,
            &fallback,
        );
        assert_font(
            config.bars[0].sections[0].modules[0]
                .style
                .font
                .as_ref()
                .unwrap(),
            "Base Face",
            22.0,
            &fallback,
        );
        let empty = for_monitor(&lua, &config.bars[1], &monitor).unwrap();
        assert_font(
            empty.sections[0].modules[0].style.font.as_ref().unwrap(),
            "DejaVu Sans",
            19.0,
            &fallback,
        );
    }
    #[test]
    fn runtime_partial_fonts_preserve_the_resolved_module_font() {
        let (lua, config) = parse(
            r#"local b=require('cybar');return {
                theme={font={family='DejaVu Sans',size=17,fallback={'Noto Sans','Emoji'}}},
                bars={{sections={left={b.cpu{font={size=13}}}}}},
            }"#,
        )
        .unwrap();
        let base = &config.bars[0].sections[0].modules[0].style;
        let change: Table = lua.load("return {font={size=14}}").eval().unwrap();
        let resized = parse_style(&change, base).unwrap();
        assert_font(
            resized.font.as_ref().unwrap(),
            "DejaVu Sans",
            14.0,
            &["Noto Sans", "Emoji"],
        );
        let change: Table = lua
            .load("return {font={family='Runtime Face'}}")
            .eval()
            .unwrap();
        let changed = parse_style(&change, &resized).unwrap();
        assert_font(
            changed.font.as_ref().unwrap(),
            "Runtime Face",
            14.0,
            &["Noto Sans", "Emoji"],
        );
        assert_font(
            base.font.as_ref().unwrap(),
            "DejaVu Sans",
            13.0,
            &["Noto Sans", "Emoji"],
        );
    }
    #[test]
    fn declarative_config_and_helpers() {
        let (_, config) = parse(r##"local b=require('cybar'); return {theme={colors={text_active='#112233'}},bars={{sections={left={b.workspaces{}},middle={b.clock{format='%H:%M'}},right={b.cpu{style={hover={foreground='#abcdef'},alignment='end'}}},custom={{name='stats',position={x='70%',y=-2},size={width=200},modules={b.ram{}}}}}}}}"##).unwrap();
        assert_eq!(config.theme.colors.active, Color(17, 34, 51, 255));
        assert_eq!(config.bars[0].sections.len(), 4);
        assert_eq!(config.bars[0].sections[3].x, Some(Dimension::Percent(70.0)));
        assert_eq!(
            config.bars[0].sections[2].modules[0].style.hover.foreground,
            Some(Color(171, 205, 239, 255))
        );
    }
    #[test]
    fn invalid_dimensions_sources_and_callbacks_are_rejected() {
        for fields in [
            "size={thickness=0}",
            "size={length='nan%'}",
            "scale=-1",
            "sections={left={{kind='cpu',interval=0}}}",
            "sections={left={{kind='window_title',max_length=-1}}}",
            "sections={left={{kind='custom',render=function()end,source={type='poll',interval=1}}}}",
            "sections={left={{kind='cpu',on_scroll='oops'}}}",
            "sections={left={{kind='cpu',style={max_width=2,min_width=4}}}}",
        ] {
            assert!(
                parse(&format!("return {{bars={{{{{fields}}}}}}}")).is_err(),
                "accepted {fields}"
            );
        }
    }
    #[test]
    fn clock_directives_are_validated_before_startup() {
        assert!(parse("local b=require('cybar');return {bars={{sections={middle={b.clock{format='%Q'}}}}}}").is_err());
        assert!(parse("local b=require('cybar');return {bars={{sections={middle={b.clock{format='%H:%M:%-S'}}}}}}").is_ok());
    }
    #[test]
    fn practical_scale_and_interval_limits_are_enforced() {
        for scale in [0.24, 16.01] {
            assert!(parse(&format!("return {{scale={scale},bars={{{{}}}}}}")).is_err());
            assert!(
                parse(&format!(
                    "return {{bars={{{{overrides={{primary={{scale={scale}}}}}}}}}}}"
                ))
                .is_err()
            );
        }
        for scale in [0.25, 16.0] {
            assert!(parse(&format!("return {{scale={scale},bars={{{{}}}}}}")).is_ok());
            assert!(
                parse(&format!(
                    "return {{bars={{{{overrides={{primary={{scale={scale}}}}}}}}}}}"
                ))
                .is_ok()
            );
        }
        assert!(parse("local b=require('cybar');return {bars={{sections={left={b.cpu{interval=31536001}}}}}}").is_err());
        assert!(parse("local b=require('cybar');return {bars={{sections={left={b.custom{source={type='command',command='true',interval=31536001},render=function(v)return v end}}}}}}").is_err());
        assert!(parse("local b=require('cybar');return {bars={{sections={left={b.cpu{interval=31536000}}}}}}").is_ok());
    }
    #[test]
    fn duplicate_names_and_holey_arrays_are_rejected() {
        assert!(parse("return {bars={{sections={left={{kind='cpu',name='same'},{kind='ram',name='same'}}}}}}").is_err());
        assert!(parse("return {bars={[1]={},[3]={}}}").is_err());
        assert!(
            parse("return {bars={{sections={custom={{name='left',modules={}}},left={}}}}}")
                .is_err()
        );
    }
    #[test]
    fn monitor_overrides_merge_and_scale_without_mutating_base() {
        let (lua, config) = parse("local b=require('cybar');return {scale=1.5,bars={{name='main',monitor='all',size={thickness=28,length='100%'},sections={left={b.cpu{}}},overrides={all={padding=4},primary={size={thickness=32}},['HDMI-1']={scale=2,size={length='80%'}}}}}}").unwrap();
        let monitor = Monitor {
            name: "HDMI-1".into(),
            geometry: Geometry {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            primary: true,
            scale: 1.0,
        };
        let bar = for_monitor(&lua, &config.bars[0], &monitor).unwrap();
        assert_eq!(bar.scale, Some(2.0));
        assert_eq!(bar.thickness, Dimension::Pixels(32.0));
        assert_eq!(bar.length, Dimension::Percent(80.0));
        assert_eq!(bar.padding, 4.0);
        assert_eq!(config.bars[0].thickness, Dimension::Pixels(28.0));
        assert_eq!(config.bars[0].scale, Some(1.5));
        assert_eq!(
            bar.sections[0].modules[0].name,
            config.bars[0].sections[0].modules[0].name
        );
    }
    #[test]
    fn invalid_unconnected_monitor_override_still_fails_validation() {
        assert!(
            parse("return {bars={{overrides={['not-connected']={size={thickness=-3}}}}}}").is_err()
        );
    }
    #[test]
    fn source_fn_alias_and_handler_tables_are_normalized() {
        let (_lua, config) = parse("local b=require('cybar');return {bars={{sections={left={b.custom{name='test',source={type='poll',fn=function()return 1 end},render=function(v)return tostring(v)end,handlers={on_message=function()return 'ok'end},on_scroll=false}}}}}}").unwrap();
        let options = &config.bars[0].sections[0].modules[0].options;
        assert!(matches!(
            get(options, "on_message").unwrap(),
            Value::Function(_)
        ));
        let source = table(options, "source").unwrap().unwrap();
        assert!(matches!(get(&source, "poll").unwrap(), Value::Function(_)));
    }
    #[test]
    fn combined_monitor_overrides_are_validated() {
        // Each override is valid alone, but together they duplicate a section.
        assert!(parse("return {bars={{overrides={all={sections={left={}}},primary={sections={custom={{name='left',modules={}}}}}}}}}").is_err());
        assert!(parse("return {bars={{overrides={all={sections={left={}}},['HDMI-1']={sections={custom={{name='left',modules={}}}}}}}}}").is_err());
    }
    #[test]
    fn relative_paths_and_required_lua_modules_resolve_from_config_directory() {
        let directory = env::temp_dir().join(format!(
            "cybar-config-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("helper.lua"), "return 'helper-loaded'").unwrap();
        fs::write(directory.join("config.lua"), "local b=require('cybar');return {bars={{gap=9,sections={left={b.custom{name=require('helper'),icon='icons/cpu.svg',source={type='stream',file='status.txt'},render=function(v)return v end}}}}}}").unwrap();
        let lua = Lua::new();
        install_constructors(&lua, &lua.create_table().unwrap()).unwrap();
        let config = load(&lua, &directory.join("config.lua")).unwrap();
        let section = &config.bars[0].sections[0];
        let module = &section.modules[0];
        assert_eq!(section.gap, 9.0);
        assert_eq!(module.name, "helper-loaded");
        assert_eq!(
            module.icon.as_deref(),
            Some(directory.join("icons/cpu.svg").to_str().unwrap())
        );
        assert_eq!(
            string(&table(&module.options, "source").unwrap().unwrap(), "file")
                .unwrap()
                .as_deref(),
            Some(directory.join("status.txt").to_str().unwrap())
        );
        fs::remove_dir_all(directory).unwrap();
    }
}
