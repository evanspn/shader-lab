//! The terminal frame a shader is run over (`iChannel0`): a generic synthetic screen made with the
//! embedded font, or a PNG you provide. Also records which pixels are text, for the checks.

use crate::font::{GLYPH_H, GLYPH_W, glyph};

pub const BG: [u8; 3] = [0x1c, 0x1c, 0x24];
const FG: [u8; 3] = [0xe6, 0xe6, 0xee];
const GREEN: [u8; 3] = [0x7f, 0xdc, 0x4d];
const YELLOW: [u8; 3] = [0xe5, 0xc0, 0x7b];
const RED: [u8; 3] = [0xff, 0x8a, 0x8a];
const BLUE: [u8; 3] = [0x6e, 0xa8, 0xff];
const MAGENTA: [u8; 3] = [0xd7, 0x8b, 0xff];
const CYAN: [u8; 3] = [0x6e, 0xe7, 0xe7];
const DIM: [u8; 3] = [0x9a, 0xa0, 0xb4];
const SELECTION: [u8; 3] = [0x2d, 0x4a, 0x9a];

/// Coverage at or above this counts as a text pixel (the core of a stroke, not its soft edge).
const TEXT_COVERAGE: f32 = 0.85;
/// For a loaded image, pixels this much brighter than the background count as text.
const IMAGE_TEXT_LUMINANCE_ABOVE_BG: f32 = 0.45;

pub struct Frame {
    /// The terminal's background color: what a pixel with nothing drawn on it looks like.
    pub background: (u8, u8, u8),
    pub width: u32,
    pub height: u32,
    /// RGBA8, top row first. Alpha is always 255.
    pub rgba: Vec<u8>,
    /// One flag per pixel: is this pixel part of the text (or the cursor block)?
    pub text: Vec<bool>,
}

type Line = Vec<([u8; 3], &'static str)>;

/// Generic sample content: a build log, a directory listing, some code. Nothing personal.
fn sample_lines() -> Vec<Line> {
    vec![
        vec![(GREEN, "$ "), (FG, "cargo build --release")],
        vec![(GREEN, "   Compiling "), (FG, "demo v0.3.1 (/work/demo)")],
        vec![
            (YELLOW, "warning"),
            (FG, ": unused variable: "),
            (CYAN, "`count`"),
        ],
        vec![(BLUE, "  --> "), (FG, "src/main.rs:42:9")],
        vec![(DIM, "   |")],
        vec![(DIM, "42 |"), (FG, "     let count = items.len();")],
        vec![
            (DIM, "   |"),
            (YELLOW, "         ^^^^^ help: prefix with an underscore"),
        ],
        vec![
            (GREEN, "    Finished "),
            (FG, "release [optimized] in 4.21s"),
        ],
        vec![],
        vec![(GREEN, "$ "), (FG, "ls -la src/")],
        vec![
            (BLUE, "drwxr-xr-x"),
            (FG, "  4 dev  staff   128 Jan  1 12:00 "),
            (BLUE, "."),
        ],
        vec![(FG, "-rw-r--r--  1 dev  staff  2210 Jan  1 12:00 lib.rs")],
        vec![(FG, "-rw-r--r--  1 dev  staff   913 Jan  1 12:00 main.rs")],
        vec![
            (FG, "-rwxr-xr-x  1 dev  staff   387 Jan  1 12:00 "),
            (GREEN, "build.sh"),
        ],
        vec![],
        vec![(GREEN, "$ "), (FG, "cat src/lib.rs")],
        vec![
            (MAGENTA, "pub fn "),
            (BLUE, "average"),
            (FG, "(values: &[f64]) -> f64 {"),
        ],
        vec![
            (FG, "    "),
            (MAGENTA, "if "),
            (FG, "values.is_empty() { "),
            (MAGENTA, "return "),
            (CYAN, "0.0"),
            (FG, "; }"),
        ],
        vec![
            (FG, "    values.iter().sum::<f64>() / values.len() "),
            (MAGENTA, "as "),
            (FG, "f64"),
        ],
        vec![(FG, "}")],
        vec![],
        vec![(
            DIM,
            "// The quick brown fox jumps over the lazy dog. 0123456789",
        )],
        vec![(
            DIM,
            "// Sphinx of black quartz, judge my vow! {}[]()<>=+-*/%&|~^",
        )],
        vec![],
        vec![(GREEN, "$ "), (FG, "git status --short")],
        vec![(YELLOW, " M "), (FG, "src/lib.rs")],
        vec![(RED, " D "), (FG, "src/old.rs")],
        vec![(GREEN, "?? "), (FG, "examples/")],
        vec![],
        vec![(GREEN, "$ "), (FG, "cargo test")],
        vec![(FG, "running 12 tests")],
        vec![(FG, "test average_of_empty ... "), (GREEN, "ok")],
        vec![(FG, "test average_of_three ... "), (GREEN, "ok")],
        vec![
            (FG, "test result: "),
            (GREEN, "ok"),
            (FG, ". 12 passed; 0 failed"),
        ],
        vec![(GREEN, "$ ")],
    ]
}

fn smoothstep(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Soft coverage of glyph `g` at fractional glyph-pixel position (gx, gy): bilinear over the bitmap,
/// then a smoothstep, so strokes get smooth, anti-aliased edges at any scale.
fn coverage(g: &[u8; GLYPH_H], gx: f32, gy: f32) -> f32 {
    let bit = |x: i32, y: i32| -> f32 {
        if x < 0 || y < 0 || x >= GLYPH_W as i32 || y >= GLYPH_H as i32 {
            0.0
        } else {
            ((g[y as usize] >> (GLYPH_W as i32 - 1 - x)) & 1) as f32
        }
    };
    let (x0, y0) = (gx.floor(), gy.floor());
    let (fx, fy) = (gx - x0, gy - y0);
    let (x0, y0) = (x0 as i32, y0 as i32);
    let top = bit(x0, y0) * (1.0 - fx) + bit(x0 + 1, y0) * fx;
    let bottom = bit(x0, y0 + 1) * (1.0 - fx) + bit(x0 + 1, y0 + 1) * fx;
    smoothstep(0.3, 0.7, top * (1.0 - fy) + bottom * fy)
}

impl Frame {
    fn blank(width: u32, height: u32) -> Frame {
        let mut rgba = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..width * height {
            rgba.extend_from_slice(&[BG[0], BG[1], BG[2], 255]);
        }
        Frame {
            background: (BG[0], BG[1], BG[2]),
            width,
            height,
            rgba,
            text: vec![false; (width * height) as usize],
        }
    }

    fn fill_rect(&mut self, x: u32, y: u32, w: u32, h: u32, c: [u8; 3], mark_text: bool) {
        for yy in y..(y + h).min(self.height) {
            for xx in x..(x + w).min(self.width) {
                let i = (yy * self.width + xx) as usize;
                self.rgba[i * 4..i * 4 + 3].copy_from_slice(&c);
                self.text[i] = mark_text;
            }
        }
    }

    /// Draw `ch` with its top-left cell corner at (x, y), each font pixel `scale` pixels square.
    fn draw_glyph(&mut self, ch: char, x: i32, y: i32, scale: u32, color: [u8; 3]) {
        let g = glyph(ch);
        let s = scale as f32;
        for py in 0..(GLYPH_H as u32 * scale + scale) {
            for px in 0..(GLYPH_W as u32 * scale + scale) {
                let (ox, oy) = (x + px as i32, y + py as i32);
                if ox < 0 || oy < 0 || ox >= self.width as i32 || oy >= self.height as i32 {
                    continue;
                }
                // pixel centre in glyph-pixel coordinates (font pixel centres sit at integers + 0.5)
                let gx = (px as f32 + 0.5) / s - 0.5 - 0.0;
                let gy = (py as f32 + 0.5) / s - 0.5 - 0.0;
                let c = coverage(&g, gx - 0.0, gy);
                if c <= 0.0 {
                    continue;
                }
                let i = (oy as u32 * self.width + ox as u32) as usize;
                for (k, &target) in color.iter().enumerate().take(3) {
                    let old = self.rgba[i * 4 + k] as f32;
                    self.rgba[i * 4 + k] = (old + (target as f32 - old) * c).round() as u8;
                }
                if c >= TEXT_COVERAGE {
                    self.text[i] = true;
                }
            }
        }
    }

    /// The generic synthetic terminal: about 30 lines of colored text, a selection highlight and a cursor.
    pub fn sample(width: u32, height: u32) -> Frame {
        let mut f = Frame::blank(width, height);
        let scale = (height / 360).max(1);
        let (cw, ch) = ((GLYPH_W as u32 + 1) * scale, (GLYPH_H as u32 + 2) * scale);
        let (margin_x, margin_y) = (cw, ch / 2);
        let lines = sample_lines();
        let rows = ((height.saturating_sub(margin_y)) / ch) as usize;
        let shown: Vec<&Line> = lines.iter().take(rows).collect();
        // the selection highlight sits behind some lines (drawn first, so the text goes over it)
        if shown.len() > 18 {
            f.fill_rect(
                margin_x + 4 * cw,
                margin_y + 16 * ch,
                40 * cw,
                3 * ch,
                SELECTION,
                false,
            );
        }
        let last = shown.len().saturating_sub(1);
        for (row, line) in shown.iter().enumerate() {
            let mut col = 0u32;
            for (color, text) in line.iter() {
                for c in text.chars() {
                    let (x, y) = (
                        (margin_x + col * cw) as i32,
                        (margin_y + row as u32 * ch) as i32,
                    );
                    if row == last && col >= 2 {
                        break;
                    }
                    f.draw_glyph(c, x, y, scale, *color);
                    col += 1;
                }
            }
        }
        // the cursor: a solid light block after the last prompt
        f.fill_rect(
            margin_x + 2 * cw,
            margin_y + last as u32 * ch,
            cw,
            ch,
            FG,
            true,
        );
        f
    }

    /// A frame from a PNG file. The most common color is taken as the background; pixels much brighter count as text.
    #[cfg(feature = "render")]
    pub fn from_png(path: &std::path::Path) -> anyhow::Result<Frame> {
        use anyhow::Context;
        let img = image::open(path)
            .with_context(|| format!("reading {}", path.display()))?
            .to_rgba8();
        let (width, height) = img.dimensions();
        let mut rgba = img.into_raw();
        for p in rgba.chunks_mut(4) {
            p[3] = 255;
        }
        // the background is the most common color; text is whatever is much brighter than it
        let mut counts: std::collections::HashMap<[u8; 3], u32> = std::collections::HashMap::new();
        for p in rgba.chunks(4) {
            *counts.entry([p[0], p[1], p[2]]).or_default() += 1;
        }
        let bg = counts
            .into_iter()
            .max_by_key(|(_, n)| *n)
            .map(|(c, _)| c)
            .unwrap_or(BG);
        let bg_lum = luminance(&bg);
        let text = rgba
            .chunks(4)
            .map(|p| luminance(p) >= (bg_lum + IMAGE_TEXT_LUMINANCE_ABOVE_BG).min(0.95))
            .collect();
        Ok(Frame {
            background: (bg[0], bg[1], bg[2]),
            width,
            height,
            rgba,
            text,
        })
    }

    pub fn text_pixels(&self) -> usize {
        self.text.iter().filter(|t| **t).count()
    }
}

/// Rec. 601 luminance of an RGB(A) pixel, 0..1.
pub fn luminance(p: &[u8]) -> f32 {
    (0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32) / 255.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sample_frame_has_text_a_cursor_and_a_selection_and_nothing_personal() {
        for (w, h) in [(320, 180), (1280, 720), (1920, 1080)] {
            let f = Frame::sample(w, h);
            assert_eq!(f.rgba.len(), (w * h * 4) as usize);
            assert_eq!(f.text.len(), (w * h) as usize);
            assert!(f.rgba.chunks(4).all(|p| p[3] == 255));
            let text = f.text_pixels();
            assert!(
                text > (w * h / 100) as usize,
                "{w}x{h}: only {text} text pixels"
            );
            // it is a dark screen with bright text: the text core is bright, the background dark
            for (i, p) in f.rgba.chunks(4).enumerate() {
                if f.text[i] {
                    assert!(luminance(p) >= 0.5, "{w}x{h}: a text pixel is dim: {p:?}");
                }
            }
            assert!(
                f.rgba.chunks(4).any(|p| p[..3] == SELECTION),
                "{w}x{h}: no selection highlight"
            );
        }
        let all: String = sample_lines()
            .iter()
            .flat_map(|l| l.iter().map(|(_, t)| *t))
            .collect::<Vec<_>>()
            .join("\n");
        for personal in ["/Users/", "/home/", "@", "password", "token"] {
            assert!(!all.contains(personal), "{personal}");
        }
        assert!(sample_lines().len() >= 30);
    }

    #[test]
    fn text_edges_are_anti_aliased() {
        let f = Frame::sample(1280, 720);
        // some pixels are neither the background nor a fully covered color: soft edges
        let soft = f
            .rgba
            .chunks(4)
            .enumerate()
            .filter(|(i, p)| !f.text[*i] && p[..3] != BG && p[..3] != SELECTION)
            .count();
        assert!(soft > 500, "{soft}");
    }

    #[test]
    fn the_frame_is_deterministic() {
        assert_eq!(Frame::sample(640, 360).rgba, Frame::sample(640, 360).rgba);
    }
}
