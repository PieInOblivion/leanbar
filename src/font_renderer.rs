use fontdue::{Font, FontSettings};
use std::fs;

#[derive(Default)]
pub struct RasterizedGlyph {
    pub width: usize,
    pub height: usize,
    pub coverage: Vec<u8>,
}

pub struct GlyphCache {
    pub numbers: [RasterizedGlyph; 10], // 0-9
    pub am: RasterizedGlyph,
    pub pm: RasterizedGlyph,
    pub slash: RasterizedGlyph,         // '/'
    pub colon: RasterizedGlyph,         // ':'
    pub space: RasterizedGlyph,         // ' '
    pub time_icon: RasterizedGlyph,     // ''
    pub calendar_icon: RasterizedGlyph, // ''
}

impl GlyphCache {
    pub fn new(font_path: &str, size: f32) -> Result<Self, Box<dyn std::error::Error>> {
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
        let time_icon = rasterize_char(&font, '', size);
        let calendar_icon = rasterize_char(&font, '', size);

        Ok(GlyphCache {
            numbers,
            am,
            pm,
            slash,
            colon,
            space,
            time_icon,
            calendar_icon,
        })
    }
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
