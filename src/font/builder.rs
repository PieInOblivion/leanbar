use fontdue::{Font, FontSettings};
use std::fs;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::process::Command;

use super::renderer::{ATLAS_MAGIC, GlyphCache, RasterizedGlyph, font_mtime};
use crate::error::LeanbarError;

pub fn maybe_run_builder_mode(args: &[String]) -> Result<bool, LeanbarError> {
    if args.get(1).map(String::as_str) != Some("--build-font-atlas") {
        return Ok(false);
    }
    let font_path = args
        .get(2)
        .ok_or_else(|| LeanbarError::Atlas("missing font path".into()))?;
    let size: f32 = args
        .get(3)
        .ok_or_else(|| LeanbarError::Atlas("missing size".into()))?
        .parse()?;
    let atlas_path = args
        .get(4)
        .ok_or_else(|| LeanbarError::Atlas("missing atlas path".into()))?;

    let cache = from_font(font_path, size)?;
    write_atlas(&cache, font_path, size, Path::new(atlas_path))?;
    Ok(true)
}

pub fn build_atlas_with_helper(
    font_path: &str,
    size: f32,
    atlas_path: &Path,
) -> Result<(), LeanbarError> {
    if let Some(parent) = atlas_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let exe = std::env::current_exe()?;
    let status = Command::new(exe)
        .arg("--build-font-atlas")
        .arg(font_path)
        .arg(format!("{size:.2}"))
        .arg(atlas_path)
        .status()?;

    if !status.success() {
        return Err(LeanbarError::Atlas(
            "font atlas helper process failed".into(),
        ));
    }

    Ok(())
}

fn from_font(font_path: &str, size: f32) -> Result<GlyphCache, LeanbarError> {
    let font = Font::from_bytes(fs::read(font_path)?, FontSettings::default())
        .map_err(|e| LeanbarError::Atlas(e.to_string()))?;
    let numbers: [RasterizedGlyph; 10] =
        std::array::from_fn(|i| rasterize_char(&font, (b'0' + i as u8) as char, size));

    let am = rasterize_string(&font, "AM", size);
    let pm = rasterize_string(&font, "PM", size);
    let max_digit_width = numbers.iter().map(|g| g.width).max().unwrap_or(0);
    let max_ampm_width = am.width.max(pm.width);

    Ok(GlyphCache {
        numbers,
        am,
        pm,
        slash: rasterize_char(&font, '/', size),
        colon: rasterize_char(&font, ':', size),
        space: rasterize_char(&font, ' ', size),
        percent: rasterize_char(&font, '%', size),
        plus: rasterize_char(&font, '+', size),
        minus: rasterize_char(&font, '-', size),
        full: rasterize_string(&font, "Full", size),
        max_digit_width,
        max_ampm_width,
    })
}

fn write_atlas(
    cache: &GlyphCache,
    font_path: &str,
    size: f32,
    target_path: &Path,
) -> Result<(), LeanbarError> {
    if let Some(parent) = target_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut writer = BufWriter::new(fs::File::create(target_path)?);
    writer.write_all(ATLAS_MAGIC)?;
    writer.write_all(&(font_path.len() as u32).to_le_bytes())?;
    writer.write_all(font_path.as_bytes())?;
    let (secs, nanos) = font_mtime(font_path)?;
    writer.write_all(&secs.to_le_bytes())?;
    writer.write_all(&nanos.to_le_bytes())?;
    writer.write_all(&size.to_bits().to_le_bytes())?;
    for glyph in cache.as_slice_ordered() {
        writer.write_all(&(glyph.width as u16).to_le_bytes())?;
        writer.write_all(&(glyph.height as u16).to_le_bytes())?;
        writer.write_all(&(glyph.coverage.len() as u32).to_le_bytes())?;
        writer.write_all(&glyph.coverage)?;
    }
    writer.flush()?;
    Ok(())
}

fn rasterize_char(font: &Font, c: char, size: f32) -> RasterizedGlyph {
    let (metrics, coverage) = font.rasterize(c, size);
    RasterizedGlyph {
        width: metrics.width,
        height: metrics.height,
        coverage,
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
    let mut final_coverage = vec![0u8; total_width * total_height];

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
