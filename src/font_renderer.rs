use fontdue::{Font, FontSettings};
use std::collections::hash_map::DefaultHasher;
use std::env;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::UNIX_EPOCH;

use crate::error::LeanbarError;

const ATLAS_MAGIC: &[u8; 5] = b"LBAT1"; // leanbar atlas v1
const GLYPH_COUNT: usize = 19;

#[derive(Default)]
pub struct RasterizedGlyph {
    pub width: usize,
    pub height: usize,
    pub coverage: Vec<u8>,
}

pub struct GlyphCache {
    pub numbers: [RasterizedGlyph; 10],
    pub am: RasterizedGlyph,
    pub pm: RasterizedGlyph,
    pub slash: RasterizedGlyph,
    pub colon: RasterizedGlyph,
    pub space: RasterizedGlyph,
    pub percent: RasterizedGlyph,
    pub plus: RasterizedGlyph,
    pub minus: RasterizedGlyph,
    pub full: RasterizedGlyph,
    pub max_digit_width: usize,
    pub max_ampm_width: usize,
}

impl GlyphCache {
    pub fn load_or_build(font_path: &str, size: f32) -> Result<Self, LeanbarError> {
        let atlas_path = atlas_cache_path(font_path, size)?;
        if let Ok(cache) = Self::load_from_atlas(font_path, size, &atlas_path) {
            println!("[FontAtlas] cache hit: {}", atlas_path.display());
            return Ok(cache);
        }
        println!("[FontAtlas] cache miss: rebuilding");
        build_atlas_with_helper(font_path, size, &atlas_path)?;
        Self::load_from_atlas(font_path, size, &atlas_path)
    }

    fn from_font(font_path: &str, size: f32) -> Result<Self, LeanbarError> {
        let font = Font::from_bytes(fs::read(font_path)?, FontSettings::default())
            .map_err(|e| LeanbarError::Font(e.to_string()))?;
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

    fn from_vec(all: Vec<RasterizedGlyph>) -> Result<Self, LeanbarError> {
        if all.len() != GLYPH_COUNT {
            return Err(LeanbarError::Atlas(format!(
                "expected {} glyphs, got {}",
                GLYPH_COUNT,
                all.len()
            )));
        }
        let mut it = all.into_iter();
        let numbers: [RasterizedGlyph; 10] = std::array::from_fn(|_| it.next().unwrap());

        let am = it.next().unwrap();
        let pm = it.next().unwrap();
        let max_digit_width = numbers.iter().map(|g| g.width).max().unwrap_or(0);
        let max_ampm_width = am.width.max(pm.width);

        Ok(GlyphCache {
            numbers,
            am,
            pm,
            slash: it.next().unwrap(),
            colon: it.next().unwrap(),
            space: it.next().unwrap(),
            percent: it.next().unwrap(),
            plus: it.next().unwrap(),
            minus: it.next().unwrap(),
            full: it.next().unwrap(),
            max_digit_width,
            max_ampm_width,
        })
    }

    fn write_atlas(
        &self,
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
        for glyph in self.as_slice_ordered() {
            writer.write_all(&(glyph.width as u16).to_le_bytes())?;
            writer.write_all(&(glyph.height as u16).to_le_bytes())?;
            writer.write_all(&(glyph.coverage.len() as u32).to_le_bytes())?;
            writer.write_all(&glyph.coverage)?;
        }
        writer.flush()?;
        Ok(())
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

        let path_len = u32::from_le_bytes(take(&mut cursor, 4)?.try_into()?) as usize;
        if String::from_utf8(take(&mut cursor, path_len)?.to_vec())? != expected_path {
            return Err(LeanbarError::Atlas("path mismatch".into()));
        }

        let secs = u64::from_le_bytes(take(&mut cursor, 8)?.try_into()?);
        let nanos = u32::from_le_bytes(take(&mut cursor, 4)?.try_into()?);
        if font_mtime(expected_path)? != (secs, nanos) {
            return Err(LeanbarError::Atlas("mtime mismatch".into()));
        }

        if u32::from_le_bytes(take(&mut cursor, 4)?.try_into()?) != expected_size.to_bits() {
            return Err(LeanbarError::Atlas("size mismatch".into()));
        }

        let mut glyphs = Vec::with_capacity(GLYPH_COUNT);
        for _ in 0..GLYPH_COUNT {
            let width = u16::from_le_bytes(take(&mut cursor, 2)?.try_into()?) as usize;
            let height = u16::from_le_bytes(take(&mut cursor, 2)?.try_into()?) as usize;
            let cov_len = u32::from_le_bytes(take(&mut cursor, 4)?.try_into()?) as usize;
            glyphs.push(RasterizedGlyph {
                width,
                height,
                coverage: take(&mut cursor, cov_len)?.to_vec(),
            });
        }
        GlyphCache::from_vec(glyphs)
    }

    fn as_slice_ordered(&self) -> [&RasterizedGlyph; GLYPH_COUNT] {
        [
            &self.numbers[0],
            &self.numbers[1],
            &self.numbers[2],
            &self.numbers[3],
            &self.numbers[4],
            &self.numbers[5],
            &self.numbers[6],
            &self.numbers[7],
            &self.numbers[8],
            &self.numbers[9],
            &self.am,
            &self.pm,
            &self.slash,
            &self.colon,
            &self.space,
            &self.percent,
            &self.plus,
            &self.minus,
            &self.full,
        ]
    }
}

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

    GlyphCache::from_font(font_path, size)?.write_atlas(font_path, size, Path::new(atlas_path))?;
    Ok(true)
}

fn build_atlas_with_helper(
    font_path: &str,
    size: f32,
    atlas_path: &Path,
) -> Result<(), LeanbarError> {
    if let Some(parent) = atlas_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let exe = env::current_exe()?;
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

fn font_mtime(path: &str) -> Result<(u64, u32), LeanbarError> {
    let dur = fs::metadata(path)?
        .modified()?
        .duration_since(UNIX_EPOCH)
        .map_err(|e| LeanbarError::Atlas(format!("mtime before epoch: {}", e)))?;
    Ok((dur.as_secs(), dur.subsec_nanos()))
}

fn take<'a>(cursor: &mut &'a [u8], n: usize) -> Result<&'a [u8], LeanbarError> {
    if cursor.len() < n {
        return Err(LeanbarError::Atlas("unexpected end of file".into()));
    }
    let (head, tail) = cursor.split_at(n);
    *cursor = tail;
    Ok(head)
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
    let mut glyphs = Vec::new();
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
    let mut final_coverage = vec![0; total_width * total_height];

    for (pos_x, metrics, coverage) in glyphs {
        if coverage.is_empty() {
            continue;
        }
        let start_x = (pos_x.round() as i32 + metrics.xmin - min_x) as usize;
        let start_y = (max_y - (metrics.ymin + metrics.height as i32)) as usize;
        for y in 0..metrics.height {
            for x in 0..metrics.width {
                let dst_idx = (start_y + y) * total_width + (start_x + x);
                final_coverage[dst_idx] =
                    final_coverage[dst_idx].max(coverage[y * metrics.width + x]);
            }
        }
    }
    RasterizedGlyph {
        width: total_width,
        height: total_height,
        coverage: final_coverage,
    }
}
