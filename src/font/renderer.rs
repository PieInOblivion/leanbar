use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use std::{env, fs};

use crate::error::LeanbarError;

pub const ATLAS_MAGIC: &[u8; 5] = b"LBAT1"; // leanbar atlas v1
pub const GLYPH_COUNT: usize = 19;

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
        super::builder::build_atlas_with_helper(font_path, size, &atlas_path)?;
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

        let path_len = u32::from_le_bytes(take_array(&mut cursor)?) as usize;
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

        let numbers: [RasterizedGlyph; 10] = [
            read_glyph(&mut cursor)?,
            read_glyph(&mut cursor)?,
            read_glyph(&mut cursor)?,
            read_glyph(&mut cursor)?,
            read_glyph(&mut cursor)?,
            read_glyph(&mut cursor)?,
            read_glyph(&mut cursor)?,
            read_glyph(&mut cursor)?,
            read_glyph(&mut cursor)?,
            read_glyph(&mut cursor)?,
        ];
        let am = read_glyph(&mut cursor)?;
        let pm = read_glyph(&mut cursor)?;
        let slash = read_glyph(&mut cursor)?;
        let colon = read_glyph(&mut cursor)?;
        let space = read_glyph(&mut cursor)?;
        let percent = read_glyph(&mut cursor)?;
        let plus = read_glyph(&mut cursor)?;
        let minus = read_glyph(&mut cursor)?;
        let full = read_glyph(&mut cursor)?;

        let max_digit_width = numbers.iter().map(|g| g.width).max().unwrap_or(0);
        let max_ampm_width = am.width.max(pm.width);

        Ok(GlyphCache {
            numbers,
            am,
            pm,
            slash,
            colon,
            space,
            percent,
            plus,
            minus,
            full,
            max_digit_width,
            max_ampm_width,
        })
    }

    pub fn as_slice_ordered(&self) -> [&RasterizedGlyph; GLYPH_COUNT] {
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

fn take<'a>(cursor: &mut &'a [u8], n: usize) -> Result<&'a [u8], LeanbarError> {
    if cursor.len() < n {
        return Err(LeanbarError::Atlas("unexpected end of file".into()));
    }
    let (head, tail) = cursor.split_at(n);
    *cursor = tail;
    Ok(head)
}

fn take_array<const N: usize>(cursor: &mut &[u8]) -> Result<[u8; N], LeanbarError> {
    if cursor.len() < N {
        return Err(LeanbarError::Atlas("unexpected end of file".into()));
    }
    let (head, tail) = cursor.split_at(N);
    *cursor = tail;
    Ok(head.try_into().unwrap())
}

fn read_glyph(cursor: &mut &[u8]) -> Result<RasterizedGlyph, LeanbarError> {
    let width = u16::from_le_bytes(take_array(cursor)?) as usize;
    let height = u16::from_le_bytes(take_array(cursor)?) as usize;
    let cov_len = u32::from_le_bytes(take_array(cursor)?) as usize;
    Ok(RasterizedGlyph {
        width,
        height,
        coverage: take(cursor, cov_len)?.to_vec(),
    })
}
