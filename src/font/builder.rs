use fontdue::{Font, FontSettings};
use std::fs;
use std::io::{BufWriter, Write};
use std::path::Path;

use crate::error::LeanbarError;
use crate::font::FontAtlas;
use crate::font::atlas::{ATLAS_MAGIC, GlyphMetrics, font_mtime};

#[derive(Default)]
struct RasterizedGlyph {
    width: usize,
    height: usize,
    coverage: Box<[u8]>,
}

pub fn run_builder_mode(args: &mut impl Iterator<Item = String>) -> Result<(), LeanbarError> {
    let font_path = args
        .next()
        .ok_or_else(|| LeanbarError::Atlas("missing font path".into()))?;
    let size_str = args
        .next()
        .ok_or_else(|| LeanbarError::Atlas("missing size".into()))?;
    let size: f32 = size_str.parse()?;
    let atlas_path = args
        .next()
        .ok_or_else(|| LeanbarError::Atlas("missing atlas path".into()))?;

    let cache = from_font(&font_path, size)?;
    write_atlas(&cache, &font_path, size, Path::new(&atlas_path))?;
    Ok(())
}

fn from_font(font_path: &str, size: f32) -> Result<FontAtlas, LeanbarError> {
    let font = Font::from_bytes(fs::read(font_path)?, FontSettings::default())
        .map_err(|e| LeanbarError::Atlas(e.to_string()))?;

    let raw_numbers: [RasterizedGlyph; 10] =
        std::array::from_fn(|i| rasterize_char(&font, (b'0' + i as u8) as char, size));
    let raw_am = rasterize_string(&font, "AM", size);
    let raw_pm = rasterize_string(&font, "PM", size);
    let raw_slash = rasterize_char(&font, '/', size);
    let raw_colon = rasterize_char(&font, ':', size);
    let raw_space = rasterize_char(&font, ' ', size);
    let raw_percent = rasterize_char(&font, '%', size);
    let raw_plus = rasterize_char(&font, '+', size);
    let raw_minus = rasterize_char(&font, '-', size);
    let raw_full = rasterize_string(&font, "Full", size);

    let raw_glyphs = [
        &raw_numbers[0],
        &raw_numbers[1],
        &raw_numbers[2],
        &raw_numbers[3],
        &raw_numbers[4],
        &raw_numbers[5],
        &raw_numbers[6],
        &raw_numbers[7],
        &raw_numbers[8],
        &raw_numbers[9],
        &raw_am,
        &raw_pm,
        &raw_slash,
        &raw_colon,
        &raw_space,
        &raw_percent,
        &raw_plus,
        &raw_minus,
        &raw_full,
    ];

    let mut buffer_vec = Vec::new();

    let glyphs = std::array::from_fn(|i| {
        buffer_vec.extend_from_slice(&raw_glyphs[i].coverage);
        GlyphMetrics {
            offset: buffer_vec.len(),
            len: raw_glyphs[i].coverage.len(),
            width: raw_glyphs[i].width,
            height: raw_glyphs[i].height,
        }
    });

    let digit_widths = std::array::from_fn(|i| glyphs[i].width);
    let max_digit_width = digit_widths.iter().copied().max().unwrap_or(0);
    let max_ampm_width = glyphs[10].width.max(glyphs[11].width);

    let slash_width = glyphs[12].width;
    let colon_width = glyphs[13].width;
    let space_width = glyphs[14].width;

    let date_slot_max_width = (max_digit_width * 6) + (slash_width * 2) + 10;
    let clock_slot_max_width =
        (max_digit_width * 4) + colon_width + space_width + max_ampm_width + 10;

    Ok(FontAtlas {
        buffer: buffer_vec.into_boxed_slice(),
        glyphs,
        digit_widths,
        date_slot_max_width,
        clock_slot_max_width,
    })
}

fn write_atlas(
    cache: &FontAtlas,
    font_path: &str,
    size: f32,
    target_path: &Path,
) -> Result<(), LeanbarError> {
    if let Some(parent) = target_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut writer = BufWriter::new(fs::File::create(target_path)?);

    writer.write_all(ATLAS_MAGIC)?;
    writer.write_all(&font_path.len().to_le_bytes())?;
    writer.write_all(font_path.as_bytes())?;

    let (secs, nanos) = font_mtime(font_path)?;
    writer.write_all(&secs.to_le_bytes())?;
    writer.write_all(&nanos.to_le_bytes())?;
    writer.write_all(&size.to_bits().to_le_bytes())?;

    writer.write_all(&cache.date_slot_max_width.to_le_bytes())?;
    writer.write_all(&cache.clock_slot_max_width.to_le_bytes())?;

    writer.write_all(&cache.buffer.len().to_le_bytes())?;

    for g in &cache.glyphs {
        writer.write_all(&g.offset.to_le_bytes())?;
        writer.write_all(&g.len.to_le_bytes())?;
        writer.write_all(&g.width.to_le_bytes())?;
        writer.write_all(&g.height.to_le_bytes())?;
    }

    writer.write_all(&cache.buffer)?;
    writer.flush()?;
    Ok(())
}

fn rasterize_char(font: &Font, c: char, size: f32) -> RasterizedGlyph {
    let (metrics, coverage) = font.rasterize(c, size);
    RasterizedGlyph {
        width: metrics.width,
        height: metrics.height,
        coverage: coverage.into_boxed_slice(),
    }
}

fn rasterize_string(font: &Font, s: &str, size: f32) -> RasterizedGlyph {
    let mut glyphs = Vec::with_capacity(s.len());
    let mut current_x: f32 = 0.0;

    let mut min_x = i32::MAX;
    let mut max_x = i32::MIN;
    let mut min_y = i32::MAX;
    let mut max_y = i32::MIN;

    for c in s.chars() {
        let (metrics, coverage) = font.rasterize(c, size);
        if !coverage.is_empty() {
            let glyph_x = current_x.round() as i32 + metrics.xmin;
            min_x = min_x.min(glyph_x);
            max_x = max_x.max(glyph_x + metrics.width as i32);
            min_y = min_y.min(metrics.ymin);
            max_y = max_y.max(metrics.ymin + metrics.height as i32);
        }
        glyphs.push((current_x, metrics, coverage));
        current_x += metrics.advance_width;
    }
    if glyphs.is_empty() || min_x == i32::MAX {
        return RasterizedGlyph::default();
    }

    let total_width = (max_x - min_x) as usize;
    let total_height = (max_y - min_y) as usize;
    let mut final_coverage = vec![0u8; total_width * total_height].into_boxed_slice();

    for (pos_x, metrics, coverage) in glyphs {
        if coverage.is_empty() || metrics.width == 0 {
            continue;
        }
        let start_x = (pos_x.round() as i32 + metrics.xmin - min_x) as usize;
        let start_y = (max_y - (metrics.ymin + metrics.height as i32)) as usize;

        for (y, src_row) in coverage.chunks_exact(metrics.width).enumerate() {
            let dst_offset = (start_y + y) * total_width + start_x;
            if dst_offset + metrics.width <= final_coverage.len() {
                let dst_row = &mut final_coverage[dst_offset..dst_offset + metrics.width];
                for (dst, &src) in dst_row.iter_mut().zip(src_row) {
                    *dst = (*dst).max(src);
                }
            }
        }
    }

    RasterizedGlyph {
        width: total_width,
        height: total_height,
        coverage: final_coverage,
    }
}
