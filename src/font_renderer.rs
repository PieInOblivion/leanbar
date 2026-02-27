use fontdue::{Font, FontSettings};
use std::collections::hash_map::DefaultHasher;
use std::env;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::UNIX_EPOCH;

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
}

impl GlyphCache {
    pub fn load_or_build(font_path: &str, size: f32) -> Result<Self, Box<dyn std::error::Error>> {
        let atlas_path = atlas_cache_path(font_path, size)?;

        if let Ok(cache) = Self::load_from_atlas(font_path, size, &atlas_path) {
            println!("[FontAtlas] cache hit: {}", atlas_path.display());
            return Ok(cache);
        }

        println!(
            "[FontAtlas] cache miss: {}, rebuilding",
            atlas_path.display()
        );
        build_atlas_with_helper(font_path, size, &atlas_path)?;
        let cache = Self::load_from_atlas(font_path, size, &atlas_path)?;
        println!("[FontAtlas] cache ready: {}", atlas_path.display());
        Ok(cache)
    }

    fn from_font(font_path: &str, size: f32) -> Result<Self, Box<dyn std::error::Error>> {
        let font_data = fs::read(font_path)?;
        let font =
            Font::from_bytes(font_data, FontSettings::default()).map_err(|e| e.to_string())?;

        let mut numbers: [RasterizedGlyph; 10] = Default::default();
        for (i, c) in ('0'..='9').enumerate() {
            numbers[i] = rasterize_char(&font, c, size);
        }

        let am = rasterize_string(&font, "AM", size);
        let pm = rasterize_string(&font, "PM", size);
        let slash = rasterize_char(&font, '/', size);
        let colon = rasterize_char(&font, ':', size);
        let space = rasterize_char(&font, ' ', size);
        let percent = rasterize_char(&font, '%', size);
        let plus = rasterize_char(&font, '+', size);
        let minus = rasterize_char(&font, '-', size);
        let full = rasterize_string(&font, "Full", size);

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
        })
    }

    fn from_vec(mut all: Vec<RasterizedGlyph>) -> Result<Self, Box<dyn std::error::Error>> {
        if all.len() != GLYPH_COUNT {
            return Err(format!(
                "invalid glyph count: expected {}, got {}",
                GLYPH_COUNT,
                all.len()
            )
            .into());
        }

        let full = all.pop().ok_or("missing full")?;
        let minus = all.pop().ok_or("missing minus")?;
        let plus = all.pop().ok_or("missing plus")?;
        let percent = all.pop().ok_or("missing percent")?;
        let space = all.pop().ok_or("missing space")?;
        let colon = all.pop().ok_or("missing colon")?;
        let slash = all.pop().ok_or("missing slash")?;
        let pm = all.pop().ok_or("missing pm")?;
        let am = all.pop().ok_or("missing am")?;

        let numbers_vec = all;
        let numbers: [RasterizedGlyph; 10] = numbers_vec
            .try_into()
            .map_err(|_| "invalid number glyph count")?;

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
        })
    }

    fn write_atlas(
        &self,
        font_path: &str,
        size: f32,
        target_path: &Path,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mtime = font_mtime(font_path)?;

        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut file = fs::File::create(target_path)?;
        file.write_all(ATLAS_MAGIC)?;

        write_u32(&mut file, font_path.len() as u32)?;
        file.write_all(font_path.as_bytes())?;

        write_u64(&mut file, mtime.0)?;
        write_u32(&mut file, mtime.1)?;
        write_u32(&mut file, size.to_bits())?;

        for glyph in self.as_slice_ordered() {
            write_u16(&mut file, glyph.width as u16)?;
            write_u16(&mut file, glyph.height as u16)?;
            write_u32(&mut file, glyph.coverage.len() as u32)?;
            file.write_all(&glyph.coverage)?;
        }

        Ok(())
    }

    fn load_from_atlas(
        expected_font_path: &str,
        expected_size: f32,
        atlas_path: &Path,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let mut file = fs::File::open(atlas_path)?;

        let mut magic = [0u8; 5];
        file.read_exact(&mut magic)?;
        if &magic != ATLAS_MAGIC {
            return Err("invalid atlas magic".into());
        }

        let path_len = read_u32(&mut file)? as usize;
        let mut path_bytes = vec![0u8; path_len];
        file.read_exact(&mut path_bytes)?;
        let atlas_font_path = String::from_utf8(path_bytes)?;

        let atlas_mtime_sec = read_u64(&mut file)?;
        let atlas_mtime_nsec = read_u32(&mut file)?;
        let atlas_size_bits = read_u32(&mut file)?;

        if atlas_font_path != expected_font_path {
            return Err("atlas font path mismatch".into());
        }

        let current_mtime = font_mtime(expected_font_path)?;
        if current_mtime != (atlas_mtime_sec, atlas_mtime_nsec) {
            return Err("atlas font timestamp mismatch".into());
        }

        if atlas_size_bits != expected_size.to_bits() {
            return Err("atlas font size mismatch".into());
        }

        let mut glyphs = Vec::with_capacity(GLYPH_COUNT);
        for _ in 0..GLYPH_COUNT {
            let width = read_u16(&mut file)? as usize;
            let height = read_u16(&mut file)? as usize;
            let cov_len = read_u32(&mut file)? as usize;
            let mut coverage = vec![0u8; cov_len];
            file.read_exact(&mut coverage)?;
            glyphs.push(RasterizedGlyph {
                width,
                height,
                coverage,
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

pub fn maybe_run_builder_mode(args: &[String]) -> Result<bool, Box<dyn std::error::Error>> {
    if args.get(1).map(String::as_str) != Some("--build-font-atlas") {
        return Ok(false);
    }

    let font_path = args.get(2).ok_or("missing font path")?;
    let size: f32 = args.get(3).ok_or("missing size")?.parse()?;
    let atlas_path = args.get(4).ok_or("missing atlas path")?;

    let glyph_cache = GlyphCache::from_font(font_path, size)?;
    glyph_cache.write_atlas(font_path, size, Path::new(atlas_path))?;

    Ok(true)
}

fn build_atlas_with_helper(
    font_path: &str,
    size: f32,
    atlas_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
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
        return Err("font atlas helper process failed".into());
    }

    Ok(())
}

fn atlas_cache_path(font_path: &str, size: f32) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let cache_root = if let Ok(path) = env::var("XDG_CACHE_HOME") {
        PathBuf::from(path)
    } else {
        let home = env::var("HOME")?;
        PathBuf::from(home).join(".cache")
    };

    let font_name = Path::new(font_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("font")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>();

    let mut hasher = DefaultHasher::new();
    font_path.hash(&mut hasher);
    let path_hash = hasher.finish();
    let size_tag = format!("{:02}", (size * 10.0).round() as u32);

    Ok(cache_root.join("leanbar").join(format!(
        "font_atlas_{}_{}_{}.bin",
        font_name, path_hash, size_tag
    )))
}

fn font_mtime(font_path: &str) -> Result<(u64, u32), Box<dyn std::error::Error>> {
    let meta = fs::metadata(font_path)?;
    let modified = meta.modified()?;
    let dur = modified.duration_since(UNIX_EPOCH)?;
    Ok((dur.as_secs(), dur.subsec_nanos()))
}

fn write_u16<W: Write>(w: &mut W, value: u16) -> Result<(), Box<dyn std::error::Error>> {
    w.write_all(&value.to_le_bytes())?;
    Ok(())
}

fn write_u32<W: Write>(w: &mut W, value: u32) -> Result<(), Box<dyn std::error::Error>> {
    w.write_all(&value.to_le_bytes())?;
    Ok(())
}

fn write_u64<W: Write>(w: &mut W, value: u64) -> Result<(), Box<dyn std::error::Error>> {
    w.write_all(&value.to_le_bytes())?;
    Ok(())
}

fn read_u16<R: Read>(r: &mut R) -> Result<u16, Box<dyn std::error::Error>> {
    let mut bytes = [0u8; 2];
    r.read_exact(&mut bytes)?;
    Ok(u16::from_le_bytes(bytes))
}

fn read_u32<R: Read>(r: &mut R) -> Result<u32, Box<dyn std::error::Error>> {
    let mut bytes = [0u8; 4];
    r.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64<R: Read>(r: &mut R) -> Result<u64, Box<dyn std::error::Error>> {
    let mut bytes = [0u8; 8];
    r.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
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
    let mut total_width = 0;
    let mut max_height = 0;
    let mut glyphs = Vec::new();

    for c in s.chars() {
        let (metrics, coverage) = font.rasterize(c, size);
        glyphs.push((metrics, coverage));
        total_width += metrics.width;
        if metrics.height > max_height {
            max_height = metrics.height;
        }
    }

    let mut final_coverage = vec![0; total_width * max_height];
    let mut current_x = 0;

    for (metrics, coverage) in glyphs {
        for y in 0..metrics.height {
            for x in 0..metrics.width {
                let src_idx = y * metrics.width + x;
                let dst_idx = y * total_width + current_x + x;
                final_coverage[dst_idx] = coverage[src_idx];
            }
        }
        current_x += metrics.width;
    }

    RasterizedGlyph {
        width: total_width,
        height: max_height,
        coverage: final_coverage,
    }
}
