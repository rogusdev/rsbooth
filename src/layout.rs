//! Composing captures into the final sheet.
//!
//! A mode describes a sheet size, a capture grid and an optional footer band.
//! Captures are centre-cropped to the cell aspect ratio (never letterboxed) so
//! the grid is filled edge to edge.

use std::path::Path;

use ab_glyph::{Font, FontVec, PxScale, ScaleFont};
use anyhow::{Context, Result, bail};
use image::imageops::FilterType;
use image::{Rgb, RgbImage, RgbaImage};

use crate::config::{Mode, parse_hex_color};

/// Resampling filter for scaling captures into their cells. Lanczos3 is slow
/// but this runs once per session, off the UI thread.
const RESIZE_FILTER: FilterType = FilterType::Lanczos3;

/// Fraction of the footer band height used as the text line height (ascent
/// to descent).
const FOOTER_TEXT_HEIGHT_RATIO: f32 = 0.45;

/// Fraction of the sheet width the footer text may occupy before it is scaled
/// down to fit.
const FOOTER_TEXT_WIDTH_RATIO: f32 = 0.9;

/// JPEG quality for saved captures and sheets.
pub const JPEG_QUALITY: u8 = 92;

pub fn load_font(path: &Path) -> Result<FontVec> {
    let bytes = std::fs::read(path).with_context(|| format!("reading font {}", path.display()))?;
    FontVec::try_from_vec(bytes).with_context(|| format!("parsing font {}", path.display()))
}

/// Build the final sheet for `mode` from `captures`.
///
/// Fewer captures than the mode calls for leaves the remaining cells at the
/// background color, so a partial session still produces a usable sheet.
pub fn compose(mode: &Mode, captures: &[RgbImage], font: Option<&FontVec>) -> Result<RgbImage> {
    if captures.is_empty() {
        bail!("cannot compose a sheet from zero captures");
    }
    let background = Rgb(parse_hex_color(&mode.background)?);
    let (sheet_width, sheet_height) = (mode.sheet[0], mode.sheet[1]);
    let mut sheet = RgbImage::from_pixel(sheet_width, sheet_height, background);

    let columns = mode.columns.max(1);
    let [cell_width, cell_height] = mode.cell_size()?;

    for (index, capture) in captures.iter().take(mode.captures as usize).enumerate() {
        let column = index as u32 % columns;
        let row = index as u32 / columns;
        let x = mode.margin + column * (cell_width + mode.gap);
        let y = mode.margin + row * (cell_height + mode.gap);
        let cell = fill_cell(capture, cell_width, cell_height);
        image::imageops::overlay(&mut sheet, &cell, i64::from(x), i64::from(y));
    }

    if mode.footer_height > 0 && !mode.footer_text.is_empty() {
        match font {
            Some(font) => draw_footer(&mut sheet, mode, font)?,
            None => tracing::warn!(
                "mode '{}' has footer_text but general.font is unset or unreadable; \
                 skipping the footer text",
                mode.id
            ),
        }
    }

    if let Some(overlay_path) = &mode.overlay {
        apply_overlay(&mut sheet, overlay_path)
            .with_context(|| format!("applying overlay {}", overlay_path.display()))?;
    }

    Ok(sheet)
}

/// Centre-crop `source` to the cell aspect ratio, then scale it to the cell.
fn fill_cell(source: &RgbImage, cell_width: u32, cell_height: u32) -> RgbImage {
    let source_ratio = f64::from(source.width()) / f64::from(source.height());
    let cell_ratio = f64::from(cell_width) / f64::from(cell_height);
    let (crop_width, crop_height) = if source_ratio > cell_ratio {
        // Source is wider: trim the sides.
        let width = (f64::from(source.height()) * cell_ratio).round() as u32;
        (width.clamp(1, source.width()), source.height())
    } else {
        // Source is taller: trim top and bottom.
        let height = (f64::from(source.width()) / cell_ratio).round() as u32;
        (source.width(), height.clamp(1, source.height()))
    };
    let x = (source.width() - crop_width) / 2;
    let y = (source.height() - crop_height) / 2;
    let cropped = image::imageops::crop_imm(source, x, y, crop_width, crop_height).to_image();
    image::imageops::resize(&cropped, cell_width, cell_height, RESIZE_FILTER)
}

fn apply_overlay(sheet: &mut RgbImage, path: &Path) -> Result<()> {
    let overlay = image::open(path)?.to_rgba8();
    let overlay: RgbaImage =
        if overlay.width() == sheet.width() && overlay.height() == sheet.height() {
            overlay
        } else {
            image::imageops::resize(&overlay, sheet.width(), sheet.height(), RESIZE_FILTER)
        };
    for (x, y, pixel) in overlay.enumerate_pixels() {
        let alpha = f32::from(pixel.0[3]) / 255.0;
        if alpha <= 0.0 {
            continue;
        }
        let base = sheet.get_pixel_mut(x, y);
        for channel in 0..3 {
            base.0[channel] = (f32::from(pixel.0[channel]) * alpha
                + f32::from(base.0[channel]) * (1.0 - alpha))
                .round() as u8;
        }
    }
    Ok(())
}

fn draw_footer(sheet: &mut RgbImage, mode: &Mode, font: &FontVec) -> Result<()> {
    let color = Rgb(parse_hex_color(&mode.footer_color)?);
    let band_top = sheet.height() - mode.footer_height;
    let max_width = sheet.width() as f32 * FOOTER_TEXT_WIDTH_RATIO;
    let mut scale = PxScale::from(mode.footer_height as f32 * FOOTER_TEXT_HEIGHT_RATIO);

    // Shrink until the line fits the sheet width.
    let mut width = text_width(font, scale, &mode.footer_text);
    while width > max_width && scale.x > 8.0 {
        let factor = (max_width / width).max(0.5);
        scale = PxScale::from(scale.x * factor);
        width = text_width(font, scale, &mode.footer_text);
    }

    let scaled = font.as_scaled(scale);
    let baseline_x = (sheet.width() as f32 - width) / 2.0;
    // Centre the ascent-to-descent box in the footer band.
    let text_height = scaled.ascent() - scaled.descent();
    let baseline_y =
        band_top as f32 + (mode.footer_height as f32 - text_height) / 2.0 + scaled.ascent();

    let mut caret = baseline_x;
    let mut previous = None;
    for character in mode.footer_text.chars() {
        let glyph_id = font.glyph_id(character);
        if let Some(previous) = previous {
            caret += scaled.kern(previous, glyph_id);
        }
        previous = Some(glyph_id);
        let glyph = glyph_id.with_scale_and_position(scale, ab_glyph::point(caret, baseline_y));
        caret += scaled.h_advance(glyph_id);
        let Some(outlined) = font.outline_glyph(glyph) else {
            continue;
        };
        let bounds = outlined.px_bounds();
        outlined.draw(|x, y, coverage| {
            if coverage <= 0.0 {
                return;
            }
            let px = bounds.min.x as i64 + i64::from(x);
            let py = bounds.min.y as i64 + i64::from(y);
            if px < 0 || py < 0 || px >= i64::from(sheet.width()) || py >= i64::from(sheet.height())
            {
                return;
            }
            let pixel = sheet.get_pixel_mut(px as u32, py as u32);
            let coverage = coverage.clamp(0.0, 1.0);
            for channel in 0..3 {
                pixel.0[channel] = (f32::from(color.0[channel]) * coverage
                    + f32::from(pixel.0[channel]) * (1.0 - coverage))
                    .round() as u8;
            }
        });
    }
    Ok(())
}

fn text_width(font: &FontVec, scale: PxScale, text: &str) -> f32 {
    let scaled = font.as_scaled(scale);
    let mut width = 0.0;
    let mut previous = None;
    for character in text.chars() {
        let glyph_id = font.glyph_id(character);
        if let Some(previous) = previous {
            width += scaled.kern(previous, glyph_id);
        }
        previous = Some(glyph_id);
        width += scaled.h_advance(glyph_id);
    }
    width
}

/// Downscale so the longest edge is at most `longest_edge`. Smaller images are
/// returned unchanged.
pub fn thumbnail(image: RgbImage, longest_edge: u32) -> RgbImage {
    let longest = image.width().max(image.height());
    if longest <= longest_edge {
        return image;
    }
    let scale = f64::from(longest_edge) / f64::from(longest);
    let width = ((f64::from(image.width()) * scale) as u32).max(1);
    let height = ((f64::from(image.height()) * scale) as u32).max(1);
    image::imageops::resize(&image, width, height, FilterType::Triangle)
}

/// Write an RGB image as JPEG.
pub fn save_jpeg(image: &RgbImage, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let file =
        std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let mut writer = std::io::BufWriter::new(file);
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut writer, JPEG_QUALITY);
    encoder
        .encode_image(image)
        .with_context(|| format!("encoding {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_mode(captures: u32, columns: u32) -> Mode {
        Mode {
            id: "test".into(),
            name: "Test".into(),
            description: String::new(),
            captures,
            columns,
            sheet: [600, 900],
            margin: 20,
            gap: 10,
            background: "#ffffff".into(),
            footer_height: 100,
            footer_text: String::new(),
            footer_color: "#000000".into(),
            overlay: None,
        }
    }

    #[test]
    fn sheet_matches_the_configured_size() {
        let mode = test_mode(4, 2);
        let captures = vec![RgbImage::from_pixel(320, 240, Rgb([10, 20, 30])); 4];
        let sheet = compose(&mode, &captures, None).unwrap();
        assert_eq!((sheet.width(), sheet.height()), (600, 900));
    }

    #[test]
    fn missing_captures_leave_the_background_showing() {
        let mode = test_mode(4, 2);
        let captures = vec![RgbImage::from_pixel(320, 240, Rgb([0, 0, 0]))];
        let sheet = compose(&mode, &captures, None).unwrap();
        // Bottom-right cell was never filled.
        let probe = sheet.get_pixel(sheet.width() - 30, sheet.height() - 130);
        assert_eq!(probe.0, [255, 255, 255]);
    }

    #[test]
    fn empty_captures_are_rejected() {
        assert!(compose(&test_mode(1, 1), &[], None).is_err());
    }

    #[test]
    fn oversized_margins_are_rejected() {
        let mut mode = test_mode(4, 2);
        mode.margin = 400;
        let captures = vec![RgbImage::from_pixel(32, 24, Rgb([0, 0, 0])); 4];
        assert!(compose(&mode, &captures, None).is_err());
    }

    #[test]
    fn cell_fill_crops_rather_than_letterboxes() {
        // A wide source into a square cell keeps full height, trimmed width.
        let source = RgbImage::from_pixel(400, 100, Rgb([7, 8, 9]));
        let cell = fill_cell(&source, 200, 200);
        assert_eq!((cell.width(), cell.height()), (200, 200));
        assert_eq!(cell.get_pixel(100, 100).0, [7, 8, 9]);
    }
}
