//! Showing a picture in a terminal pane: the sizing maths (pure) and the kitty / sixel / half-block drawing.

use std::io::Write;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;

use crate::termimg::{self, Protocol};

/// An RGBA picture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Img {
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
}

impl Img {
    pub fn new(w: u32, h: u32, rgba: Vec<u8>) -> Img {
        Img { w, h, rgba }
    }

    pub fn blank(w: u32, h: u32) -> Img {
        Img {
            w,
            h,
            rgba: vec![0; (w * h * 4) as usize],
        }
    }
}

/// Largest picture that fits `max` while keeping the aspect ratio.
pub fn contain(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if w == 0 || h == 0 || max_w == 0 || max_h == 0 {
        return (1, 1);
    }
    let k = (max_w as f64 / w as f64).min(max_h as f64 / h as f64);
    (
        ((w as f64 * k).round() as u32).max(1),
        ((h as f64 * k).round() as u32).max(1),
    )
}

/// Resample to `w` x `h`: a box filter going down, nearest going up (pixel art stays crisp).
pub fn resize(src: &Img, w: u32, h: u32) -> Img {
    if src.w == w && src.h == h {
        return src.clone();
    }
    let (w, h) = (w.max(1), h.max(1));
    let mut out = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        let y0 = (y as u64 * src.h as u64 / h as u64) as u32;
        let y1 = (((y as u64 + 1) * src.h as u64).div_ceil(h as u64) as u32).clamp(y0 + 1, src.h);
        for x in 0..w {
            let x0 = (x as u64 * src.w as u64 / w as u64) as u32;
            let x1 =
                (((x as u64 + 1) * src.w as u64).div_ceil(w as u64) as u32).clamp(x0 + 1, src.w);
            let mut acc = [0u32; 4];
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let i = ((sy * src.w + sx) * 4) as usize;
                    for (c, a) in acc.iter_mut().enumerate() {
                        *a += src.rgba[i + c] as u32;
                    }
                }
            }
            let n = ((y1 - y0) * (x1 - x0)).max(1);
            let o = ((y * w + x) * 4) as usize;
            for c in 0..4 {
                out[o + c] = (acc[c] / n) as u8;
            }
        }
    }
    Img { w, h, rgba: out }
}

/// Where a picture goes in a pane and at what pixel size to send it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    /// the cells the picture covers (centered in the pane)
    pub cells: Rect,
    /// the pixel size to send
    pub px: (u32, u32),
}

/// Fit a `w` x `h` picture into `area` for `proto`. `cell_px` is the size of a cell in pixels; `max_px` caps what is sent.
pub fn place(
    proto: Protocol,
    area: Rect,
    w: u32,
    h: u32,
    cell_px: (f32, f32),
    max_px: (u32, u32),
) -> Placement {
    if area.width == 0 || area.height == 0 {
        return Placement {
            cells: Rect::new(area.x, area.y, 0, 0),
            px: (1, 1),
        };
    }
    let (cw, ch) = match proto {
        Protocol::HalfBlocks => (1.0, 2.0),
        _ => (cell_px.0.max(1.0), cell_px.1.max(1.0)),
    };
    let box_w = (area.width as f32 * cw) as u32;
    let box_h = (area.height as f32 * ch) as u32;
    let (mut pw, mut ph) = contain(w, h, box_w, box_h);
    if proto != Protocol::HalfBlocks {
        // never send more pixels than the cap; the terminal scales the picture to its cells
        let (cap_w, cap_h) = max_px;
        if pw > cap_w || ph > cap_h {
            let (a, b) = contain(pw, ph, cap_w, cap_h);
            pw = a;
            ph = b;
        }
    }
    // the picture as shown covers this many cells (it was fitted to the box, then possibly capped, so scale back up)
    let shown_w =
        (box_w as f64).min(w as f64 * (box_w as f64 / w as f64).min(box_h as f64 / h as f64));
    let shown_h =
        (box_h as f64).min(h as f64 * (box_w as f64 / w as f64).min(box_h as f64 / h as f64));
    let cols = ((shown_w / cw as f64).round() as u16).clamp(1, area.width);
    let rows = ((shown_h / ch as f64).round() as u16).clamp(1, area.height);
    let x = area.x + (area.width - cols) / 2;
    let y = area.y + (area.height - rows) / 2;
    Placement {
        cells: Rect::new(x, y, cols, rows),
        px: (pw, ph),
    }
}

/// Paint `img` (already resized to `place(...).px`) as half-block characters into `placement.cells`.
pub fn paint_half_blocks(buf: &mut Buffer, cells: Rect, img: &Img, truecolor: bool) {
    let color = |c: [u8; 3]| {
        if truecolor {
            Color::Rgb(c[0], c[1], c[2])
        } else {
            Color::Indexed(termimg::nearest_256(c))
        }
    };
    for cy in 0..cells.height {
        for cx in 0..cells.width {
            let (up, down) =
                termimg::half_block_pair(&img.rgba, img.w, img.h, cx as u32, cy as u32);
            if let Some(cell) = buf.cell_mut((cells.x + cx, cells.y + cy)) {
                cell.set_char('\u{2580}')
                    .set_fg(color(up))
                    .set_bg(color(down));
            }
        }
    }
}

/// Sends a picture over kitty graphics or sixel, outside Ratatui's buffer.
pub struct Sender {
    pub proto: Protocol,
    pub use_file: bool,
    pub id: u32,
    shown: bool,
    slot: usize,
}

impl Sender {
    pub fn new(proto: Protocol, use_file: bool, id: u32) -> Sender {
        Sender {
            proto,
            use_file,
            id,
            shown: false,
            slot: 0,
        }
    }

    /// Draw `img` at `cells` (kitty replaces the same image id and placement in place; sixel just paints).
    pub fn send(&mut self, out: &mut impl Write, cells: Rect, img: &Img) -> std::io::Result<()> {
        if self.proto == Protocol::HalfBlocks || cells.width == 0 || cells.height == 0 {
            return Ok(());
        }
        let rgb = termimg::rgba_to_rgb(&img.rgba);
        write!(out, "\x1b7\x1b[{};{}H", cells.y + 1, cells.x + 1)?;
        match self.proto {
            Protocol::Kitty => {
                let seq = if self.use_file {
                    self.slot = (self.slot + 1) % 3;
                    let path = termimg::temp_frame_path(
                        std::process::id(),
                        self.slot + 10 * (self.id as usize % 10),
                    );
                    std::fs::write(&path, &rgb)?;
                    termimg::kitty_file(&path, img.w, img.h, cells.width, cells.height, self.id, -1)
                } else {
                    termimg::kitty_image(&rgb, img.w, img.h, cells.width, cells.height, self.id, -1)
                };
                out.write_all(&seq)?;
                self.shown = true;
            }
            _ => out.write_all(&termimg::sixel(&rgb, img.w, img.h))?,
        }
        out.write_all(b"\x1b8")
    }

    /// Remove the picture (kitty): when the pane shows something else or the program ends.
    pub fn hide(&mut self, out: &mut impl Write) -> std::io::Result<()> {
        if self.shown && self.proto == Protocol::Kitty {
            out.write_all(&termimg::kitty_delete(self.id))?;
            self.shown = false;
        }
        Ok(())
    }

    pub fn is_shown(&self) -> bool {
        self.shown
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        for slot in 0..3 {
            let _ = std::fs::remove_file(termimg::temp_frame_path(
                std::process::id(),
                slot + 10 * (self.id as usize % 10),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, c: [u8; 4]) -> Img {
        Img::new(w, h, (0..w * h).flat_map(|_| c).collect())
    }

    #[test]
    fn contain_keeps_the_aspect_ratio() {
        assert_eq!(contain(1280, 720, 640, 640), (640, 360));
        assert_eq!(contain(100, 200, 400, 100), (50, 100));
        assert_eq!(contain(10, 10, 100, 50), (50, 50), "it scales up too");
        assert_eq!(contain(0, 5, 10, 10), (1, 1));
    }

    #[test]
    fn resize_averages_down_and_repeats_up() {
        let mut src = solid(4, 4, [0, 0, 0, 255]);
        for p in src.rgba.chunks_mut(4).take(8) {
            p[..3].copy_from_slice(&[200, 100, 50]);
        }
        let small = resize(&src, 2, 2);
        assert_eq!(
            &small.rgba[..4],
            &[200, 100, 50, 255],
            "top row is all coloured"
        );
        assert_eq!(&small.rgba[8..12], &[0, 0, 0, 255], "bottom row is black");
        let big = resize(&solid(2, 2, [9, 8, 7, 255]), 6, 4);
        assert_eq!((big.w, big.h), (6, 4));
        assert!(big.rgba.chunks(4).all(|p| p == [9, 8, 7, 255]));
        assert_eq!(resize(&src, 4, 4), src);
    }

    #[test]
    fn a_picture_is_centred_in_the_pane_with_the_right_cells_and_pixels() {
        let area = Rect::new(10, 5, 60, 20);
        // half-blocks: one pixel per column, two per row; a 2:1 picture in a 60x40 pixel box
        let p = place(
            Protocol::HalfBlocks,
            area,
            400,
            200,
            (14.0, 28.0),
            (960, 540),
        );
        assert_eq!(p.px, (60, 30));
        assert_eq!(
            p.cells,
            Rect::new(10, 7, 60, 15),
            "half the height used, centred"
        );
        // kitty: real cell size, capped
        let p = place(Protocol::Kitty, area, 1920, 1080, (10.0, 20.0), (960, 540));
        assert!(p.px.0 <= 960 && p.px.1 <= 540);
        assert!(p.cells.width <= area.width && p.cells.height <= area.height);
        assert!(p.cells.x >= area.x && p.cells.y >= area.y);
        // the covered cells keep the picture's shape: 16:9 on 10x20 px cells
        let ratio = (p.cells.width as f32 * 10.0) / (p.cells.height as f32 * 20.0);
        assert!((ratio - 16.0 / 9.0).abs() < 0.2, "{ratio}");
        // empty pane
        assert_eq!(
            place(
                Protocol::Kitty,
                Rect::new(0, 0, 0, 5),
                10,
                10,
                (10.0, 20.0),
                (960, 540)
            )
            .cells
            .width,
            0
        );
    }

    #[test]
    fn half_blocks_paint_two_pixels_per_cell() {
        let mut img = solid(2, 4, [0, 0, 0, 255]);
        img.rgba[((2 * 2) * 4)..((2 * 2) * 4 + 3)].copy_from_slice(&[10, 20, 30]); // pixel (0,2)
        img.rgba[((3 * 2) * 4)..((3 * 2) * 4 + 3)].copy_from_slice(&[200, 100, 50]); // pixel (0,3)
        let mut buf = Buffer::empty(Rect::new(0, 0, 2, 2));
        paint_half_blocks(&mut buf, Rect::new(0, 0, 2, 2), &img, true);
        let c = &buf[(0, 1)];
        assert_eq!(
            (c.symbol(), c.fg, c.bg),
            ("\u{2580}", Color::Rgb(10, 20, 30), Color::Rgb(200, 100, 50))
        );
        paint_half_blocks(&mut buf, Rect::new(0, 0, 2, 2), &img, false);
        assert!(matches!(buf[(0, 1)].fg, Color::Indexed(_)));
    }

    #[test]
    fn the_sender_replaces_one_image_and_hides_it_when_asked() {
        let img = solid(8, 8, [1, 2, 3, 255]);
        let mut s = Sender::new(Protocol::Kitty, false, 9001);
        let mut out = Vec::new();
        s.send(&mut out, Rect::new(2, 3, 4, 2), &img).unwrap();
        let t = String::from_utf8_lossy(&out).to_string();
        assert!(t.starts_with("\x1b7\x1b[4;3H") && t.ends_with("\x1b8"));
        assert!(t.contains("i=9001,p=1,") && t.contains("c=4,r=2") && t.contains("z=-1"));
        assert!(s.is_shown());
        let mut out2 = Vec::new();
        s.hide(&mut out2).unwrap();
        assert_eq!(out2, termimg::kitty_delete(9001));
        assert!(!s.is_shown());
        s.hide(&mut out2).unwrap();
        assert_eq!(
            out2.len(),
            termimg::kitty_delete(9001).len(),
            "hiding twice sends nothing more"
        );
        // half-blocks never write escapes
        let mut h = Sender::new(Protocol::HalfBlocks, false, 9002);
        let mut o = Vec::new();
        h.send(&mut o, Rect::new(0, 0, 4, 2), &img).unwrap();
        assert!(o.is_empty());
    }
}
