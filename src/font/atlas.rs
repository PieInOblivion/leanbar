use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::UNIX_EPOCH;
use std::{env, fs};

use crate::error::LeanbarError;

pub const ATLAS_MAGIC: &[u8; 5] = b"LBAT2"; // leanbar atlas v3
pub const GLYPH_COUNT: usize = 19;

pub enum GlyphId {
    _0,
    _1,
    _2,
    _3,
    _4,
    _5,
    _6,
    _7,
    _8,
    _9,
    Am,
    Pm,
    Slash,
    Colon,
    Space,
    Percent,
    Plus,
    Minus,
    Full,
}

#[derive(Clone, Copy, Default)]
pub struct GlyphMetrics {
    pub offset: usize,
    pub len: usize,
    pub width: usize,
    pub height: usize,
}

pub struct FontAtlas {
    /// ONE single heap allocation containing all 19 concatenated coverage bitmaps.
    pub buffer: Box<[u8]>,
    /// Metrics array indexed by GlyphId: [0..9, AM, PM, /, :, space, %, +, -, Full].
    pub glyphs: [GlyphMetrics; GLYPH_COUNT],
    /// Fast lookup array for digit pixel widths (0..=9).
    pub digit_widths: [usize; 10],
    /// Precalculated max slot widths loaded directly from the binary file header.
    pub date_slot_max_width: usize,
    pub clock_slot_max_width: usize,
}

impl FontAtlas {
    pub fn load_or_build(font_path: &str, size: f32) -> Result<Self, LeanbarError> {
        let atlas_path = atlas_cache_path(font_path, size)?;

        if let Ok(cache) = Self::load_from_atlas(font_path, size, &atlas_path) {
            println!("[FontAtlas] cache hit: {}", atlas_path.display());
            return Ok(cache);
        }

        println!("[FontAtlas] cache miss: rebuilding");

        if let Some(parent) = atlas_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let exe = std::env::current_exe()?;
        let status = Command::new(exe)
            .arg("--build-font-atlas")
            .arg(font_path)
            .arg(format!("{size:.2}"))
            .arg(&atlas_path)
            .status()?;

        if !status.success() {
            return Err(LeanbarError::Atlas(
                "font atlas helper process failed".into(),
            ));
        }

        Self::load_from_atlas(font_path, size, &atlas_path)
    }

    fn load_from_atlas(
        expected_path: &str,
        expected_size: f32,
        atlas_path: &Path,
    ) -> Result<Self, LeanbarError> {
        let bytes = fs::read(atlas_path)?;
        let mut cursor = bytes.as_slice();

        if take(&mut cursor, ATLAS_MAGIC.len())? != ATLAS_MAGIC {
            return Err(LeanbarError::Atlas("invalid atlas magic".into()));
        }

        let path_len = take_usize(&mut cursor)?;
        if take(&mut cursor, path_len)? != expected_path.as_bytes() {
            return Err(LeanbarError::Atlas("path mismatch".into()));
        }

        let secs = u64::from_le_bytes(take_array(&mut cursor)?);
        let nanos = u32::from_le_bytes(take_array(&mut cursor)?);
        if font_mtime(expected_path)? != (secs, nanos) {
            return Err(LeanbarError::Atlas("mtime mismatch".into()));
        }

        if u32::from_le_bytes(take_array(&mut cursor)?) != expected_size.to_bits() {
            return Err(LeanbarError::Atlas("size mismatch".into()));
        }

        let date_slot_max_width = take_usize(&mut cursor)?;
        let clock_slot_max_width = take_usize(&mut cursor)?;

        let payload_size = take_usize(&mut cursor)?;

        let mut glyphs = [GlyphMetrics::default(); GLYPH_COUNT];
        for glyph in &mut glyphs {
            *glyph = GlyphMetrics {
                offset: take_usize(&mut cursor)?,
                len: take_usize(&mut cursor)?,
                width: take_usize(&mut cursor)?,
                height: take_usize(&mut cursor)?,
            };
        }

        // ONE single heap allocation for all coverage bitmaps
        let buffer = take(&mut cursor, payload_size)?.into();

        let digit_widths = std::array::from_fn(|i| glyphs[i].width);

        Ok(FontAtlas {
            buffer,
            glyphs,
            digit_widths,
            date_slot_max_width,
            clock_slot_max_width,
        })
    }

    pub fn get_metrics(&self, id: GlyphId) -> &GlyphMetrics {
        &self.glyphs[id as usize]
    }

    pub fn coverage(&self, glyph: &GlyphMetrics) -> &[u8] {
        &self.buffer[glyph.offset..glyph.offset + glyph.len]
    }

    /// Decompose a number into digits once on the stack (up to 4 digits).
    pub fn format_num(mut num: usize, pad: usize) -> ([u8; 4], usize) {
        let mut digits = [0u8; 4];
        let mut len = 0;
        loop {
            digits[len] = (num % 10) as u8;
            len += 1;
            num /= 10;
            if (num == 0 && len >= pad) || len == digits.len() {
                break;
            }
        }
        (digits, len)
    }

    /// Measure pixel width of formatted digits using the precalculated lookup array.
    pub fn measure_formatted_digits(&self, digits: &[u8], len: usize, spacing: usize) -> usize {
        if len == 0 {
            return 0;
        }
        let mut width = len.saturating_sub(1) * spacing;
        for &d in &digits[..len] {
            width += self.digit_widths[d as usize];
        }
        width
    }

    /// Blit a single glyph directly into a raw RGBA/ARGB u32 pixel slice.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_glyph(
        &self,
        pixels: &mut [u32],
        stride: usize,
        height: usize,
        x: usize,
        y: usize,
        glyph: &GlyphMetrics,
        color: u32,
    ) {
        let mask = self.coverage(glyph);
        let gw = glyph.width;
        let gh = glyph.height;

        if mask.is_empty() || x >= stride || y >= height {
            return;
        }

        let max_gy = gh.min(height - y);
        let max_gx = gw.min(stride - x);

        let color_a = (color >> 24) & 0xFF;
        let color_r = (color >> 16) & 0xFF;
        let color_g = (color >> 8) & 0xFF;
        let color_b = color & 0xFF;

        for gy in 0..max_gy {
            let row_start = (y + gy) * stride + x;
            let mask_start = gy * gw;

            let pixel_row = &mut pixels[row_start..row_start + max_gx];
            let mask_row = &mask[mask_start..mask_start + max_gx];

            for (px_out, &alpha_u8) in pixel_row.iter_mut().zip(mask_row) {
                let a1 = alpha_u8 as u32 + 1;
                let r = (color_r * a1) >> 8;
                let g = (color_g * a1) >> 8;
                let b = (color_b * a1) >> 8;
                let a = (color_a * a1) >> 8;
                *px_out = (a << 24) | (r << 16) | (g << 8) | b;
            }
        }
    }

    /// Utility to draw a pre-formatted digit array directly using digit indices.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_formatted_digits(
        &self,
        pixels: &mut [u32],
        stride: usize,
        height: usize,
        x: &mut usize,
        digits: &[u8],
        len: usize,
        color: u32,
        spacing: usize,
    ) {
        for i in (0..len).rev() {
            let digit = digits[i] as usize;
            let g = &self.glyphs[digit];
            let y = (height.saturating_sub(g.height)) / 2;
            self.draw_glyph(pixels, stride, height, *x, y, g, color);
            *x += g.width + if i > 0 { spacing } else { 0 };
        }
    }
}

fn atlas_cache_path(font_path: &str, size: f32) -> Result<PathBuf, LeanbarError> {
    let cache_root = env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|_| env::var("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .map_err(|_| LeanbarError::NoHome)?;

    let mut hasher = DefaultHasher::new();
    font_path.hash(&mut hasher);
    let name = Path::new(font_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("font");
    Ok(cache_root.join("leanbar").join(format!(
        "font_atlas_{}_{}_{:02}.bin",
        name,
        hasher.finish(),
        (size * 10.0).round() as u32
    )))
}

pub fn font_mtime(path: &str) -> Result<(u64, u32), LeanbarError> {
    let dur = fs::metadata(path)?
        .modified()?
        .duration_since(UNIX_EPOCH)
        .map_err(|e| LeanbarError::Atlas(format!("mtime before epoch: {}", e)))?;
    Ok((dur.as_secs(), dur.subsec_nanos()))
}

pub fn take<'a>(cursor: &mut &'a [u8], n: usize) -> Result<&'a [u8], LeanbarError> {
    if cursor.len() < n {
        return Err(LeanbarError::Atlas("unexpected end of file".into()));
    }
    let (head, tail) = cursor.split_at(n);
    *cursor = tail;
    Ok(head)
}

pub fn take_array<const N: usize>(cursor: &mut &[u8]) -> Result<[u8; N], LeanbarError> {
    if cursor.len() < N {
        return Err(LeanbarError::Atlas("unexpected end of file".into()));
    }
    let (head, tail) = cursor.split_at(N);
    *cursor = tail;
    Ok(head.try_into().unwrap())
}

pub fn take_usize(cursor: &mut &[u8]) -> Result<usize, LeanbarError> {
    Ok(usize::from_le_bytes(take_array(cursor)?))
}
