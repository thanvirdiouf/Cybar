// SPDX-License-Identifier: GPL-3.0-or-later
//! Device-pixel layout and rasterization. All public style dimensions are logical
//! pixels, and fonts are points (96 DPI at scale 1). X11 owns the window shape.
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, ensure};
use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping, SwashCache, Wrap};
use tiny_skia::{FillRule, Mask, Paint, PathBuilder, Pixmap, Transform};
use unicode_segmentation::UnicodeSegmentation;

use crate::model::{
    Alignment, BarSpec, Color, DisplayItem, Font, Frame, Geometry, Hit, Rect, SectionSpec, Theme,
    validate_framebuffer,
};

const TEXT_CACHE_LIMIT: usize = 512;
const ICON_CACHE_LIMIT: usize = 128;
// Layout adds/subtracts fractional shaped advances. A tiny tolerance prevents
// a naturally sized label from eliding because of floating-point cancellation.
const WIDTH_EPSILON: f32 = 0.05;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct TextKey {
    text: String,
    family: String,
    fallbacks: Vec<String>,
    pixels: u32,
}

struct TextLayout {
    buffer: Buffer,
    width: f32,
    height: f32,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct IconKey {
    name: String,
    theme: String,
    pixels: u32,
    scale: u64,
}

#[derive(Clone)]
struct ItemSize {
    width: f32,
    height: f32,
    text_width: f32,
    text_height: f32,
    icon_size: f32,
}

struct SectionLayout {
    rect: Rect,
    items: Vec<ItemSize>,
    gaps: Vec<f32>,
    separator_size: f32,
}

pub struct Renderer {
    fonts: FontSystem,
    svg_fonts: std::sync::Arc<resvg::usvg::fontdb::Database>,
    glyphs: SwashCache,
    text: HashMap<TextKey, TextLayout>,
    icons: HashMap<IconKey, Option<Pixmap>>,
    warned_overlaps: HashSet<(String, String, String)>,
    default_icon_theme: String,
}

impl Renderer {
    pub fn new() -> Result<Self> {
        let mut svg_fonts = resvg::usvg::fontdb::Database::new();
        svg_fonts.load_system_fonts();
        Ok(Self {
            fonts: FontSystem::new(),
            svg_fonts: std::sync::Arc::new(svg_fonts),
            glyphs: SwashCache::new(),
            text: HashMap::new(),
            icons: HashMap::new(),
            warned_overlaps: HashSet::new(),
            default_icon_theme: current_icon_theme(),
        })
    }

    /// Re-read icon files and desktop theme settings after a successful config
    /// reload. The expensive font database and reusable glyphs stay loaded.
    pub fn clear_caches(&mut self) {
        self.text.clear();
        self.icons.clear();
        self.warned_overlaps.clear();
        self.default_icon_theme = current_icon_theme();
    }

    pub fn render(
        &mut self,
        bar: &BarSpec,
        theme: &Theme,
        geometry: Geometry,
        scale: f64,
        sections: &[Vec<DisplayItem>],
    ) -> Result<Frame> {
        ensure!(
            scale.is_finite() && scale > 0.0,
            "render scale must be positive and finite"
        );
        validate_framebuffer(geometry)?;
        let mut pixmap = Pixmap::new(geometry.width, geometry.height)
            .ok_or_else(|| anyhow!("cannot allocate bar framebuffer"))?;
        // Alpha is accepted by the parser for forward compatibility. V1 renders
        // an opaque window; rounded bar edges are an X Shape region, not alpha.
        pixmap.fill(skia_color(
            bar.background.unwrap_or(theme.colors.background),
        ));
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            width: geometry.width as f32,
            height: geometry.height as f32,
        };
        let vertical = bar.position.vertical();
        let scale = scale as f32;
        let mut layouts = Vec::with_capacity(bar.sections.len());
        for (index, spec) in bar.sections.iter().enumerate() {
            layouts.push(self.layout_section(
                bar,
                spec,
                theme,
                bounds,
                scale,
                sections.get(index).map(Vec::as_slice).unwrap_or_default(),
            )?);
        }
        for i in 0..layouts.len() {
            for j in i + 1..layouts.len() {
                let a = intersection(layouts[i].rect, bounds);
                let b = intersection(layouts[j].rect, bounds);
                if a.zip(b).is_some_and(|(a, b)| intersection(a, b).is_some()) {
                    let key = (
                        bar.name.clone(),
                        bar.sections[i].name.clone(),
                        bar.sections[j].name.clone(),
                    );
                    if self.warned_overlaps.insert(key) {
                        eprintln!(
                            "cybar: bar {:?}: sections {:?} and {:?} overlap",
                            bar.name, bar.sections[i].name, bar.sections[j].name
                        );
                    }
                }
            }
        }
        let mut hits = Vec::new();
        let mut tray = None;
        for (section_index, layout) in layouts.iter().enumerate() {
            let spec = &bar.sections[section_index];
            let Some(clip) = intersection(layout.rect, bounds) else {
                continue;
            };
            let Some(items) = sections.get(section_index) else {
                continue;
            };
            let mut mask = Mask::new(geometry.width, geometry.height)
                .ok_or_else(|| anyhow!("cannot allocate section clipping mask"))?;
            let mut path = PathBuilder::new();
            if let Some(rect) = skia_rect(clip) {
                path.push_rect(rect);
                mask.fill_path(
                    &path.finish().unwrap(),
                    FillRule::Winding,
                    false,
                    Transform::identity(),
                );
            }
            let padding = spec.padding * scale;
            let mut cursor = if vertical {
                layout.rect.y
            } else {
                layout.rect.x
            } + padding;
            let mut previous_module = None;
            for (item_index, (item, size)) in items.iter().zip(&layout.items).enumerate() {
                if item.tray && item.tray_count == 0 {
                    continue;
                }
                if let Some(gap) = layout.gaps.get(item_index) {
                    if previous_module.is_some() {
                        if previous_module != Some(item.module) && bar.separator.is_some() {
                            self.draw_separator(
                                &mut pixmap,
                                bar,
                                theme,
                                layout,
                                cursor,
                                *gap,
                                clip,
                                scale,
                            )?;
                        }
                        cursor += gap;
                    }
                }
                let rect = if vertical {
                    let cross = (layout.rect.width - 2.0 * padding).max(0.0);
                    let width = size.width.min(cross);
                    Rect {
                        x: layout.rect.x
                            + padding
                            + aligned_offset(item.style.alignment, cross, width),
                        y: cursor,
                        width,
                        height: size.height,
                    }
                } else {
                    Rect {
                        x: cursor,
                        y: layout.rect.y + padding,
                        width: size.width,
                        height: (layout.rect.height - 2.0 * padding).max(0.0),
                    }
                };
                if let Some(hit_rect) = intersection(rect, clip) {
                    hits.push(Hit {
                        rect: hit_rect,
                        module: item.module,
                        workspace: item.workspace,
                    });
                    if item.tray {
                        // The tray region excludes module padding, so XEMBED
                        // children have exactly icon_size + gap spacing.
                        let inner = inset_main(rect, item.style.padding * scale, vertical);
                        tray = intersection(inner, clip);
                    } else {
                        let (foreground, background) = item.style.colors(theme, item.state);
                        if let Some(background) = background {
                            fill_rounded(
                                &mut pixmap,
                                rect,
                                item.style.corner_radius * scale,
                                background,
                                &mask,
                            );
                        }
                        self.draw_item(
                            &mut pixmap,
                            item,
                            size,
                            rect,
                            clip,
                            foreground,
                            theme,
                            scale,
                            vertical,
                        )?;
                    }
                }
                cursor += if vertical { size.height } else { size.width };
                previous_module = Some(item.module);
            }
        }
        Ok(Frame {
            width: geometry.width,
            height: geometry.height,
            pixels: pixmap.take(),
            hits,
            tray,
        })
    }

    fn layout_section(
        &mut self,
        bar: &BarSpec,
        section: &SectionSpec,
        theme: &Theme,
        bounds: Rect,
        scale: f32,
        items: &[DisplayItem],
    ) -> Result<SectionLayout> {
        let vertical = bar.position.vertical();
        let mut sizes = Vec::with_capacity(items.len());
        let separator = if let Some(separator) = &bar.separator {
            if separator.icon.is_some() {
                theme.font.size * 96.0 / 72.0 * scale
            } else if let Some(text) = &separator.text {
                let key = self.shape(text, &theme.font, scale);
                let layout = &self.text[&key];
                if vertical {
                    layout.height
                } else {
                    layout.width
                }
            } else {
                0.0
            }
        } else {
            0.0
        };
        let mut gaps = Vec::with_capacity(items.len());
        let mut previous = None;
        let mut main_size = 2.0 * section.padding * scale;
        for item in items {
            let font = item.style.font.as_ref().unwrap_or(&theme.font);
            let key = self.shape(&item.text, font, scale);
            let text_layout = &self.text[&key];
            let icon_size = if item.icon.is_some() {
                item.style.icon_size * scale
            } else {
                0.0
            };
            let padding = item.style.padding * scale;
            let icon_gap = if !item.text.is_empty() && icon_size > 0.0 {
                item.style.gap * scale
            } else {
                0.0
            };
            let mut width = text_layout.width
                + icon_size
                + icon_gap
                + if vertical { 0.0 } else { 2.0 * padding };
            let mut height =
                text_layout.height.max(icon_size) + if vertical { 2.0 * padding } else { 0.0 };
            if item.tray {
                let count = item.tray_count as f32;
                let tray_size = count * item.style.icon_size * scale
                    + (count - 1.0).max(0.0) * item.style.gap * scale;
                if vertical {
                    width = item.style.icon_size * scale;
                    height = tray_size + 2.0 * padding;
                } else {
                    width = tray_size + 2.0 * padding;
                    height = item.style.icon_size * scale;
                }
            }
            width = width.max(item.style.min_width * scale);
            if let Some(max_width) = item.style.max_width {
                width = width.min(max_width * scale);
            }
            let size = ItemSize {
                width,
                height,
                text_width: text_layout.width,
                text_height: text_layout.height,
                icon_size,
            };
            if item.tray && item.tray_count == 0 {
                sizes.push(size);
                gaps.push(0.0);
                continue;
            }
            let mut gap = if previous.is_some() {
                section.gap * scale
            } else {
                0.0
            };
            if previous.is_some() && previous != Some(item.module) {
                if let Some(spec) = &bar.separator {
                    gap += separator + spec.spacing * scale * 2.0;
                }
            }
            main_size += gap + if vertical { height } else { width };
            sizes.push(size);
            gaps.push(gap);
            previous = Some(item.module);
        }
        let bar_padding = bar.padding * scale;
        let (natural_width, natural_height) = if vertical {
            ((bounds.width - 2.0 * bar_padding).max(0.0), main_size)
        } else {
            (main_size, (bounds.height - 2.0 * bar_padding).max(0.0))
        };
        let width = section
            .width
            .map_or(natural_width, |n| {
                n.resolve(bounds.width as f64, scale as f64) as f32
            })
            .max(0.0);
        let height = section
            .height
            .map_or(natural_height, |n| {
                n.resolve(bounds.height as f64, scale as f64) as f32
            })
            .max(0.0);
        let main_available = if vertical {
            bounds.height
        } else {
            bounds.width
        };
        let main_length = if vertical { height } else { width };
        let anchored = match section.name.as_str() {
            "middle" => (main_available - main_length) / 2.0,
            "right" => main_available - bar_padding - main_length,
            _ => bar_padding,
        };
        let x = section
            .x
            .map_or(if vertical { bar_padding } else { anchored }, |n| {
                n.resolve(bounds.width as f64, scale as f64) as f32
            });
        let y = section
            .y
            .map_or(if vertical { anchored } else { bar_padding }, |n| {
                n.resolve(bounds.height as f64, scale as f64) as f32
            });
        Ok(SectionLayout {
            rect: Rect {
                x,
                y,
                width,
                height,
            },
            items: sizes,
            gaps,
            separator_size: separator,
        })
    }

    // Geometry and frame resources are explicit inputs to this drawing primitive.
    #[allow(clippy::too_many_arguments)]
    fn draw_item(
        &mut self,
        target: &mut Pixmap,
        item: &DisplayItem,
        measured: &ItemSize,
        rect: Rect,
        clip: Rect,
        foreground: Color,
        theme: &Theme,
        scale: f32,
        vertical: bool,
    ) -> Result<()> {
        let padding = item.style.padding * scale;
        let Some(content) = intersection(inset_main(rect, padding, vertical), clip) else {
            return Ok(());
        };
        let content_clip = content;
        let font = item.style.font.as_ref().unwrap_or(&theme.font);
        let icon_size = measured
            .icon_size
            .min(content.width)
            .min(content.height)
            .max(0.0);
        let gap = if icon_size > 0.0 && !item.text.is_empty() {
            item.style.gap * scale
        } else {
            0.0
        };
        let available_text = (content.width - icon_size - gap).max(0.0);
        let key = if measured.text_width > available_text + WIDTH_EPSILON {
            self.ellipsize(&item.text, font, scale, available_text)
        } else {
            self.shape(&item.text, font, scale)
        };
        let text_width = self.text[&key].width.min(available_text);
        let text_height = measured.text_height;
        let total = text_width + icon_size + gap;
        let start = content.x + aligned_offset(item.style.alignment, content.width, total);
        let (text_x, icon_x) = if item.style.icon_after {
            (start, start + text_width + gap)
        } else {
            (start + icon_size + gap, start)
        };
        if let Some(name) = &item.icon {
            if icon_size >= 1.0 {
                let icon_key = self.load_icon(name, icon_size.round() as u32, scale, theme)?;
                if let Some(Some(icon)) = self.icons.get(&icon_key) {
                    blit(
                        target,
                        icon,
                        icon_x.round() as i32,
                        (content.y + (content.height - icon_size) / 2.0).round() as i32,
                        content_clip,
                    );
                }
            }
        }
        if available_text > 0.0 {
            self.draw_text(
                target,
                &key,
                text_x,
                content.y + (content.height - text_height) / 2.0,
                foreground,
                content_clip,
            );
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_separator(
        &mut self,
        target: &mut Pixmap,
        bar: &BarSpec,
        theme: &Theme,
        section: &SectionLayout,
        cursor: f32,
        gap: f32,
        clip: Rect,
        scale: f32,
    ) -> Result<()> {
        let Some(separator) = &bar.separator else {
            return Ok(());
        };
        let color = separator.color.unwrap_or(theme.colors.separator);
        let main = cursor + (gap - section.separator_size) / 2.0;
        if let Some(name) = &separator.icon {
            let size = (theme.font.size * 96.0 / 72.0 * scale)
                .min(section.rect.width)
                .min(section.rect.height)
                .max(1.0)
                .round() as u32;
            let key = self.load_icon(name, size, scale, theme)?;
            if let Some(Some(icon)) = self.icons.get(&key) {
                let (x, y) = if bar.position.vertical() {
                    (
                        section.rect.x + (section.rect.width - size as f32) / 2.0,
                        main,
                    )
                } else {
                    (
                        main,
                        section.rect.y + (section.rect.height - size as f32) / 2.0,
                    )
                };
                blit(target, icon, x.round() as i32, y.round() as i32, clip);
            }
        } else if let Some(text) = &separator.text {
            let key = self.shape(text, &theme.font, scale);
            let layout = &self.text[&key];
            let (x, y) = if bar.position.vertical() {
                (
                    section.rect.x + (section.rect.width - layout.width) / 2.0,
                    main,
                )
            } else {
                (
                    main,
                    section.rect.y + (section.rect.height - layout.height) / 2.0,
                )
            };
            self.draw_text(target, &key, x, y, color, clip);
        }
        Ok(())
    }

    fn shape(&mut self, text: &str, font: &Font, scale: f32) -> TextKey {
        let pixels = (font.size * 96.0 / 72.0 * scale).max(1.0);
        let key = TextKey {
            text: text.replace(['\n', '\r', '\t'], " "),
            family: font.family.clone(),
            fallbacks: font.fallback.clone(),
            pixels: pixels.to_bits(),
        };
        if self.text.contains_key(&key) {
            return key;
        }
        if self.text.len() >= TEXT_CACHE_LIMIT {
            self.text.clear();
        }
        let height = pixels * 1.3;
        let mut buffer = Buffer::new(&mut self.fonts, Metrics::new(pixels, height));
        buffer.set_size(&mut self.fonts, None, Some(height));
        buffer.set_wrap(&mut self.fonts, Wrap::None);
        let names: Vec<&str> = std::iter::once(key.family.as_str())
            .chain(key.fallbacks.iter().map(String::as_str))
            .collect();
        let ids: Vec<_> = names
            .iter()
            .map(|name| {
                self.fonts.db().query(&cosmic_text::fontdb::Query {
                    families: &[font_family(name)],
                    ..Default::default()
                })
            })
            .collect();
        // Select one family for each grapheme cluster, in configured order.
        // Clusters stay together so combining marks and emoji sequences retain
        // advanced shaping. Cosmic's script fallback handles remaining glyphs.
        let mut spans: Vec<(&str, Attrs<'_>)> = Vec::new();
        let mut start = 0;
        let mut previous = None;
        for (offset, grapheme) in key.text.grapheme_indices(true) {
            let selected = ids
                .iter()
                .position(|id| {
                    id.is_some_and(|id| {
                        self.fonts
                            .db()
                            .with_face_data(id, |data, index| {
                                cosmic_text::ttf_parser::Face::parse(data, index)
                                    .ok()
                                    .is_some_and(|face| {
                                        grapheme.chars().all(|ch| {
                                            ch.is_control()
                                                || matches!(
                                                    ch,
                                                    '\u{200d}' | '\u{fe0e}' | '\u{fe0f}'
                                                )
                                                || face.glyph_index(ch).is_some()
                                        })
                                    })
                            })
                            .unwrap_or(false)
                    })
                })
                .unwrap_or(0);
            if previous.is_some_and(|n| n != selected) {
                spans.push((
                    &key.text[start..offset],
                    Attrs::new().family(font_family(names[previous.unwrap()])),
                ));
                start = offset;
            }
            previous = Some(selected);
        }
        if !key.text.is_empty() {
            spans.push((
                &key.text[start..],
                Attrs::new().family(font_family(names[previous.unwrap_or(0)])),
            ));
        }
        let attrs = Attrs::new().family(font_family(&key.family));
        buffer.set_rich_text(&mut self.fonts, spans, &attrs, Shaping::Advanced, None);
        buffer.shape_until_scroll(&mut self.fonts, false);
        let width = buffer
            .layout_runs()
            .map(|run| run.line_w)
            .fold(0.0, f32::max);
        self.text.insert(
            key.clone(),
            TextLayout {
                buffer,
                width,
                height,
            },
        );
        key
    }

    fn ellipsize(&mut self, text: &str, font: &Font, scale: f32, width: f32) -> TextKey {
        let full = self.shape(text, font, scale);
        if self.text[&full].width <= width + WIDTH_EPSILON {
            return full;
        }
        let ellipsis = self.shape("…", font, scale);
        if self.text[&ellipsis].width > width + WIDTH_EPSILON {
            return self.shape("", font, scale);
        }
        let boundaries: Vec<usize> = text
            .grapheme_indices(true)
            .map(|(i, _)| i)
            .chain(std::iter::once(text.len()))
            .collect();
        let mut low = 0;
        let mut high = boundaries.len().saturating_sub(1);
        while low < high {
            let mid = (low + high).div_ceil(2);
            let candidate = self.shape(&format!("{}…", &text[..boundaries[mid]]), font, scale);
            if self.text[&candidate].width <= width + WIDTH_EPSILON {
                low = mid;
            } else {
                high = mid - 1;
            }
        }
        self.shape(&format!("{}…", &text[..boundaries[low]]), font, scale)
    }

    fn draw_text(
        &mut self,
        target: &mut Pixmap,
        key: &TextKey,
        x: f32,
        y: f32,
        color: Color,
        clip: Rect,
    ) {
        let Some(layout) = self.text.get(key) else {
            return;
        };
        let x = x.round() as i32;
        let y = y.round() as i32;
        let target_width = target.width();
        let target_height = target.height();
        let bytes = target.data_mut();
        layout.buffer.draw(
            &mut self.fonts,
            &mut self.glyphs,
            cosmic_text::Color::rgb(color.0, color.1, color.2),
            |gx, gy, w, h, color| {
                let alpha = color.a();
                let premultiplied = [
                    premultiply(color.r(), alpha),
                    premultiply(color.g(), alpha),
                    premultiply(color.b(), alpha),
                    alpha,
                ];
                for row in 0..h {
                    for column in 0..w {
                        let px = gx + x + column as i32;
                        let py = gy + y + row as i32;
                        if px >= 0
                            && py >= 0
                            && px < target_width as i32
                            && py < target_height as i32
                            && clip.contains(px as f32 + 0.5, py as f32 + 0.5)
                        {
                            let offset = (py as usize * target_width as usize + px as usize) * 4;
                            blend(&mut bytes[offset..offset + 4], &premultiplied);
                        }
                    }
                }
            },
        );
    }

    fn load_icon(&mut self, name: &str, pixels: u32, scale: f32, theme: &Theme) -> Result<IconKey> {
        let icon_theme = theme
            .icon_theme
            .as_deref()
            .unwrap_or(&self.default_icon_theme);
        let key = IconKey {
            name: name.into(),
            theme: icon_theme.into(),
            pixels,
            scale: (scale as f64).to_bits(),
        };
        if self.icons.contains_key(&key) {
            return Ok(key);
        }
        if self.icons.len() >= ICON_CACHE_LIMIT {
            self.icons.clear();
        }
        let path = expanded_path(name);
        let path = if path.is_file() || name.contains('/') || name.starts_with('~') {
            Some(path)
        } else {
            let lookup_scale = scale.ceil().clamp(1.0, u16::MAX as f32) as u16;
            let logical_size = (pixels as f32 / lookup_scale as f32)
                .round()
                .clamp(1.0, u16::MAX as f32) as u16;
            freedesktop_icons::lookup(name)
                .with_theme(icon_theme)
                .with_size(logical_size)
                .with_scale(lookup_scale)
                .with_cache()
                .find()
        };
        let icon = match path {
            Some(path) => match rasterize_icon(&path, pixels, &self.svg_fonts) {
                Ok(icon) => Some(icon),
                Err(error) => {
                    eprintln!("cybar: icon {name:?}: {error:#}");
                    None
                }
            },
            None => {
                eprintln!("cybar: icon {name:?} not found in theme {icon_theme:?} or hicolor");
                None
            }
        };
        self.icons.insert(key.clone(), icon);
        Ok(key)
    }
}

fn rasterize_icon(
    path: &Path,
    pixels: u32,
    fonts: &std::sync::Arc<resvg::usvg::fontdb::Database>,
) -> Result<Pixmap> {
    let mut pixmap = Pixmap::new(pixels, pixels).ok_or_else(|| anyhow!("invalid icon size"))?;
    if path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("svg"))
    {
        let data =
            std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
        let options = resvg::usvg::Options {
            resources_dir: path
                .canonicalize()
                .ok()
                .and_then(|path| path.parent().map(Path::to_path_buf)),
            fontdb: fonts.clone(),
            ..Default::default()
        };
        let tree = resvg::usvg::Tree::from_data(&data, &options).context("invalid SVG")?;
        let factor =
            (pixels as f32 / tree.size().width()).min(pixels as f32 / tree.size().height());
        let x = (pixels as f32 - tree.size().width() * factor) / 2.0;
        let y = (pixels as f32 - tree.size().height() * factor) / 2.0;
        let transform = Transform::from_scale(factor, factor).post_translate(x, y);
        resvg::render(&tree, transform, &mut pixmap.as_mut());
    } else {
        let image = image::ImageReader::open(path)
            .with_context(|| format!("cannot read {}", path.display()))?
            .with_guessed_format()?
            .decode()
            .context("invalid image")?;
        let image = image
            .resize(pixels, pixels, image::imageops::FilterType::Lanczos3)
            .to_rgba8();
        let x = (pixels - image.width()) / 2;
        let y = (pixels - image.height()) / 2;
        for (ix, iy, rgba) in image.enumerate_pixels() {
            let offset = (((y + iy) * pixels + x + ix) * 4) as usize;
            let alpha = rgba[3];
            pixmap.data_mut()[offset..offset + 4].copy_from_slice(&[
                premultiply(rgba[0], alpha),
                premultiply(rgba[1], alpha),
                premultiply(rgba[2], alpha),
                alpha,
            ]);
        }
    }
    Ok(pixmap)
}

fn font_family(name: &str) -> Family<'_> {
    match name {
        "monospace" => Family::Monospace,
        "sans-serif" | "sans" => Family::SansSerif,
        "serif" => Family::Serif,
        "cursive" => Family::Cursive,
        "fantasy" => Family::Fantasy,
        name => Family::Name(name),
    }
}

fn current_icon_theme() -> String {
    if let Ok(theme) = std::env::var("XDG_ICON_THEME") {
        if !theme.is_empty() {
            return theme;
        }
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    for path in [
        config.join("gtk-4.0/settings.ini"),
        config.join("gtk-3.0/settings.ini"),
        home.join(".gtkrc-2.0"),
    ] {
        if let Ok(contents) = std::fs::read_to_string(path) {
            for line in contents.lines() {
                if let Some((key, value)) = line.split_once('=') {
                    if key.trim() == "gtk-icon-theme-name" {
                        let value = value.trim().trim_matches(['\'', '"']);
                        if !value.is_empty() {
                            return value.into();
                        }
                    }
                }
            }
        }
    }
    "hicolor".into()
}

fn expanded_path(name: &str) -> PathBuf {
    if let Some(rest) = name.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(name)
}

fn intersection(a: Rect, b: Rect) -> Option<Rect> {
    let x = a.x.max(b.x);
    let y = a.y.max(b.y);
    let width = (a.x + a.width).min(b.x + b.width) - x;
    let height = (a.y + a.height).min(b.y + b.height) - y;
    (width > 0.0 && height > 0.0).then_some(Rect {
        x,
        y,
        width,
        height,
    })
}

fn inset_main(rect: Rect, padding: f32, vertical: bool) -> Rect {
    if vertical {
        Rect {
            y: rect.y + padding,
            height: (rect.height - padding * 2.0).max(0.0),
            ..rect
        }
    } else {
        Rect {
            x: rect.x + padding,
            width: (rect.width - padding * 2.0).max(0.0),
            ..rect
        }
    }
}

fn aligned_offset(alignment: Alignment, available: f32, used: f32) -> f32 {
    let remaining = (available - used).max(0.0);
    match alignment {
        Alignment::Start => 0.0,
        Alignment::Center => remaining / 2.0,
        Alignment::End => remaining,
    }
}

fn skia_rect(rect: Rect) -> Option<tiny_skia::Rect> {
    tiny_skia::Rect::from_xywh(rect.x, rect.y, rect.width, rect.height)
}

fn skia_color(color: Color) -> tiny_skia::Color {
    tiny_skia::Color::from_rgba8(color.0, color.1, color.2, 255)
}

fn fill_rounded(target: &mut Pixmap, rect: Rect, radius: f32, color: Color, mask: &Mask) {
    let Some(skia_rect) = skia_rect(rect) else {
        return;
    };
    let radius = radius.max(0.0).min(rect.width / 2.0).min(rect.height / 2.0);
    let mut builder = PathBuilder::new();
    if radius < 0.5 {
        builder.push_rect(skia_rect);
    } else {
        let (x, y, w, h) = (rect.x, rect.y, rect.width, rect.height);
        let r = radius;
        let c = r * 0.552_284_8;
        builder.move_to(x + r, y);
        builder.line_to(x + w - r, y);
        builder.cubic_to(x + w - r + c, y, x + w, y + r - c, x + w, y + r);
        builder.line_to(x + w, y + h - r);
        builder.cubic_to(x + w, y + h - r + c, x + w - r + c, y + h, x + w - r, y + h);
        builder.line_to(x + r, y + h);
        builder.cubic_to(x + r - c, y + h, x, y + h - r + c, x, y + h - r);
        builder.line_to(x, y + r);
        builder.cubic_to(x, y + r - c, x + r - c, y, x + r, y);
        builder.close();
    }
    let mut paint = Paint::default();
    paint.set_color(skia_color(color));
    if let Some(path) = builder.finish() {
        target.fill_path(
            &path,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            Some(mask),
        );
    }
}

fn premultiply(channel: u8, alpha: u8) -> u8 {
    ((u16::from(channel) * u16::from(alpha) + 127) / 255) as u8
}

fn blend(destination: &mut [u8], source: &[u8]) {
    let remaining = 255 - u16::from(source[3]);
    for i in 0..3 {
        destination[i] = (u16::from(source[i])
            + (u16::from(destination[i]) * remaining + 127) / 255)
            .min(255) as u8;
    }
    destination[3] = 255;
}

fn blit(target: &mut Pixmap, image: &Pixmap, x: i32, y: i32, clip: Rect) {
    let tw = target.width() as i32;
    let th = target.height() as i32;
    let bytes = target.data_mut();
    for iy in 0..image.height() as i32 {
        let py = y + iy;
        if py < 0 || py >= th {
            continue;
        }
        for ix in 0..image.width() as i32 {
            let px = x + ix;
            if px < 0 || px >= tw || !clip.contains(px as f32 + 0.5, py as f32 + 0.5) {
                continue;
            }
            let source = (iy as usize * image.width() as usize + ix as usize) * 4;
            let destination = (py as usize * tw as usize + px as usize) * 4;
            blend(
                &mut bytes[destination..destination + 4],
                &image.data()[source..source + 4],
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Dimension, Edge, ModuleState, Style};

    fn section(name: &str) -> SectionSpec {
        SectionSpec {
            name: name.into(),
            x: None,
            y: None,
            width: None,
            height: None,
            padding: 0.0,
            gap: 4.0,
            modules: Vec::new(),
        }
    }
    fn bar(sections: Vec<SectionSpec>) -> BarSpec {
        BarSpec {
            name: "test".into(),
            monitor: "primary".into(),
            position: Edge::Top,
            thickness: Dimension::Pixels(40.0),
            length: Dimension::Percent(100.0),
            offset_x: Dimension::Pixels(0.0),
            offset_y: Dimension::Pixels(0.0),
            reserve_space: true,
            corner_radius: 4.0,
            scale: None,
            padding: 2.0,
            gap: 4.0,
            background: None,
            sections,
            overrides: None,
            separator: None,
        }
    }
    fn item(module: usize, text: &str) -> DisplayItem {
        DisplayItem {
            module,
            text: text.into(),
            icon: None,
            style: Style::default(),
            state: ModuleState::Normal,
            workspace: None,
            tray: false,
            tray_count: 0,
        }
    }
    fn geometry(width: u32, height: u32) -> Geometry {
        Geometry {
            x: 0,
            y: 0,
            width,
            height,
        }
    }

    #[test]
    fn builtins_anchor_and_workspace_hits_keep_identity() {
        let mut renderer = Renderer::new().unwrap();
        let spec = bar(vec![section("left"), section("middle"), section("right")]);
        let mut workspace = item(0, "1");
        workspace.workspace = Some(7);
        let frame = renderer
            .render(
                &spec,
                &Theme::default(),
                geometry(600, 40),
                1.0,
                &[
                    vec![workspace],
                    vec![item(1, "clock")],
                    vec![item(2, "right")],
                ],
            )
            .unwrap();
        assert_eq!(frame.hits.len(), 3);
        assert_eq!(frame.hits[0].workspace, Some(7));
        assert_eq!(frame.hits[0].rect.x, 2.0);
        assert!((frame.hits[1].rect.x + frame.hits[1].rect.width / 2.0 - 300.0).abs() < 0.1);
        assert!((frame.hits[2].rect.x + frame.hits[2].rect.width - 598.0).abs() < 0.1);
        assert!(frame.pixels.chunks_exact(4).all(|rgba| rgba[3] == 255));
        let first = frame.hits[0].rect;
        let background = Theme::default().colors.background;
        let painted = (first.y as u32..(first.y + first.height) as u32).any(|y| {
            (first.x as u32..(first.x + first.width) as u32).any(|x| {
                let offset = ((y * frame.width + x) * 4) as usize;
                frame.pixels[offset..offset + 3] != [background.0, background.1, background.2]
            })
        });
        assert!(
            painted,
            "naturally sized single-character modules must render their glyph"
        );
    }

    #[test]
    fn percent_sections_clip_paint_and_hits() {
        let mut renderer = Renderer::new().unwrap();
        let mut custom = section("custom");
        custom.x = Some(Dimension::Percent(50.0));
        custom.width = Some(Dimension::Pixels(30.0));
        let spec = bar(vec![custom]);
        let mut module = item(0, "a very long module title");
        module.style.background = Some(Color(255, 0, 0, 50));
        let frame = renderer
            .render(
                &spec,
                &Theme::default(),
                geometry(200, 40),
                2.0,
                &[vec![module]],
            )
            .unwrap();
        assert_eq!(frame.hits[0].rect.x, 100.0);
        assert_eq!(frame.hits[0].rect.width, 60.0);
        let background = Theme::default().colors.background;
        let sample = (20 * frame.width as usize + 170) * 4;
        assert_eq!(
            &frame.pixels[sample..sample + 4],
            &[background.0, background.1, background.2, 255]
        );
    }

    #[test]
    fn vertical_rows_and_empty_tray_do_not_consume_space() {
        let mut renderer = Renderer::new().unwrap();
        let mut spec = bar(vec![section("left")]);
        spec.position = Edge::Left;
        let mut tray = item(0, "");
        tray.tray = true;
        let frame = renderer
            .render(
                &spec,
                &Theme::default(),
                geometry(80, 500),
                1.0,
                &[vec![tray, item(1, "one"), item(2, "two")]],
            )
            .unwrap();
        assert_eq!(frame.hits.len(), 2);
        assert_eq!(frame.hits[0].rect.y, 2.0);
        assert!(frame.hits[1].rect.y >= frame.hits[0].rect.y + frame.hits[0].rect.height);
        assert!(frame.tray.is_none());
    }

    #[test]
    fn tray_reservation_includes_icon_gaps_but_excludes_module_padding() {
        let mut renderer = Renderer::new().unwrap();
        let mut spec = bar(vec![section("left")]);
        let mut tray = item(0, "");
        tray.tray = true;
        tray.tray_count = 3;
        let expected = 3.0 * tray.style.icon_size + 2.0 * tray.style.gap;
        let frame = renderer
            .render(
                &spec,
                &Theme::default(),
                geometry(400, 40),
                1.0,
                &[vec![tray.clone()]],
            )
            .unwrap();
        let region = frame.tray.unwrap();
        assert_eq!(region.width, expected);
        assert_eq!(region.x, 2.0 + tray.style.padding);
        assert_eq!(
            frame.hits[0].rect.width,
            expected + 2.0 * tray.style.padding
        );
        spec.position = Edge::Left;
        let frame = renderer
            .render(
                &spec,
                &Theme::default(),
                geometry(80, 400),
                1.0,
                &[vec![tray.clone()]],
            )
            .unwrap();
        let region = frame.tray.unwrap();
        assert_eq!(region.height, expected);
        assert_eq!(region.y, 2.0 + tray.style.padding);
        assert_eq!(region.width, tray.style.icon_size);
    }

    #[test]
    fn ellipsis_preserves_graphemes_and_stays_in_budget() {
        let mut renderer = Renderer::new().unwrap();
        let font = Font::default();
        let full = renderer.shape("hello e\u{301} world", &font, 1.0);
        let width = renderer.text[&full].width / 2.0;
        let short = renderer.ellipsize("hello e\u{301} world", &font, 1.0, width);
        assert!(renderer.text[&short].width <= width);
        assert!(short.text.ends_with('…'));
        assert_ne!(short.text, full.text);
        let scaled = renderer.shape("hello", &font, 2.0);
        let normal = renderer.shape("hello", &font, 1.0);
        assert!((renderer.text[&scaled].width / renderer.text[&normal].width - 2.0).abs() < 0.1);
    }

    #[test]
    fn svg_and_raster_are_centered_and_premultiplied() {
        let renderer = Renderer::new().unwrap();
        let directory = std::env::temp_dir().join(format!("cybar-renderer-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let svg = directory.join("icon.svg");
        std::fs::write(&svg, r##"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="10"><rect width="20" height="10" fill="#ff0000" fill-opacity="0.5"/></svg>"##).unwrap();
        let pixmap = rasterize_icon(&svg, 20, &renderer.svg_fonts).unwrap();
        assert_eq!(pixmap.pixel(10, 1).unwrap().alpha(), 0);
        let center = pixmap.pixel(10, 10).unwrap();
        assert_eq!(center.red(), center.alpha());
        assert!((126..=129).contains(&center.alpha()));
        let raster = directory.join("icon.png");
        image::RgbaImage::from_pixel(20, 10, image::Rgba([0, 255, 0, 128]))
            .save(&raster)
            .unwrap();
        let pixmap = rasterize_icon(&raster, 20, &renderer.svg_fonts).unwrap();
        assert_eq!(pixmap.pixel(10, 1).unwrap().alpha(), 0);
        assert_eq!(pixmap.pixel(10, 10).unwrap().green(), 128);
        std::fs::remove_file(svg).unwrap();
        std::fs::remove_file(raster).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
}
