//! Contact sheets: a grid of rendered frames with a small label on each.

use crate::font::{GLYPH_H, GLYPH_W, glyph};

/// Draw `text` (5x7 font, `scale` pixels per font pixel) onto an RGBA buffer at (x, y), white on a dark bar.
fn label(rgba: &mut [u8], stride: u32, x: u32, y: u32, text: &str, scale: u32) {
    let bar_w = (text.chars().count() as u32 * (GLYPH_W as u32 + 1) * scale + 4 * scale)
        .min(stride.saturating_sub(x));
    let bar_h = (GLYPH_H as u32 + 3) * scale;
    for yy in y..y + bar_h {
        for xx in x..x + bar_w {
            let i = ((yy * stride + xx) * 4) as usize;
            if i + 3 < rgba.len() {
                for k in 0..3 {
                    rgba[i + k] = (rgba[i + k] as u32 * 2 / 5) as u8;
                }
            }
        }
    }
    for (n, c) in text.chars().enumerate() {
        let g = glyph(c);
        for (gy, row) in g.iter().enumerate() {
            for gx in 0..GLYPH_W {
                if (row >> (GLYPH_W - 1 - gx)) & 1 == 0 {
                    continue;
                }
                for sy in 0..scale {
                    for sx in 0..scale {
                        let px = x
                            + 2 * scale
                            + (n as u32 * (GLYPH_W as u32 + 1) + gx as u32) * scale
                            + sx;
                        let py = y + (gy as u32 + 1) * scale + sy;
                        let i = ((py * stride + px) * 4) as usize;
                        if px < stride && i + 3 < rgba.len() {
                            rgba[i..i + 3].copy_from_slice(&[255, 255, 255]);
                        }
                    }
                }
            }
        }
    }
}

/// Lay `cells` (each `cell_w` x `cell_h` RGBA, row-major) out in `cols` columns with a 4 px gutter and a label on each.
/// Returns (width, height, RGBA).
pub fn contact_sheet(
    cells: &[(String, Vec<u8>)],
    cell_w: u32,
    cell_h: u32,
    cols: u32,
) -> (u32, u32, Vec<u8>) {
    let cols = cols.max(1);
    let rows = (cells.len() as u32).div_ceil(cols).max(1);
    let gutter = 4;
    let (w, h) = (
        cols * cell_w + (cols - 1) * gutter,
        rows * cell_h + (rows - 1) * gutter,
    );
    let mut out = vec![60u8; (w * h * 4) as usize];
    for px in out.chunks_mut(4) {
        px[3] = 255;
    }
    for (i, (name, rgba)) in cells.iter().enumerate() {
        let (cx, cy) = (
            i as u32 % cols * (cell_w + gutter),
            i as u32 / cols * (cell_h + gutter),
        );
        for y in 0..cell_h {
            let src = (y * cell_w * 4) as usize;
            let dst = (((cy + y) * w + cx) * 4) as usize;
            out[dst..dst + (cell_w * 4) as usize]
                .copy_from_slice(&rgba[src..src + (cell_w * 4) as usize]);
        }
        label(&mut out, w, cx, cy, name, (cell_h / 180).max(1));
    }
    (w, h, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sheet_has_the_right_size_and_labels_change_the_pixels() {
        let cell = |v: u8| vec![v; 160 * 90 * 4];
        let cells = vec![
            ("t=0s".to_string(), cell(100)),
            ("t=2s".to_string(), cell(100)),
            ("t=5s".to_string(), cell(100)),
        ];
        let (w, h, px) = contact_sheet(&cells, 160, 90, 2);
        assert_eq!((w, h), (2 * 160 + 4, 2 * 90 + 4));
        assert_eq!(px.len(), (w * h * 4) as usize);
        assert!(
            px.chunks(4).any(|p| p[..3] == [255, 255, 255]),
            "the label is drawn"
        );
        // the unlabelled interior is untouched
        let i = ((60 * w + 80) * 4) as usize;
        assert_eq!(px[i], 100);
    }
}
