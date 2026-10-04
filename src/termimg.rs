//! Putting a picture in a terminal: the kitty graphics protocol, sixel, and truecolor half-block characters.
//!
//! Everything here turns pixels into bytes or cells and has no terminal in it, so it is unit-tested directly.

use std::io::Write;

/// How the picture reaches the terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    Kitty,
    Sixel,
    HalfBlocks,
}

impl Protocol {
    pub fn name(self) -> &'static str {
        match self {
            Protocol::Kitty => "kitty graphics",
            Protocol::Sixel => "sixel",
            Protocol::HalfBlocks => "half-blocks",
        }
    }
}

/// Pick a protocol from the environment (`get` reads a variable). Returns the choice and, for a fallback, a note.
///
/// Detection is by environment, not by querying the terminal: kitty graphics for Ghostty, kitty and WezTerm; sixel for terminals
/// known to speak it (or `TERM` naming it); otherwise half-blocks, which works everywhere. Inside tmux the graphics protocols
/// need passthrough, so tmux gets half-blocks unless a protocol is forced with `--protocol`.
pub fn detect(get: &dyn Fn(&str) -> Option<String>) -> (Protocol, Option<String>) {
    let has = |k: &str| get(k).is_some_and(|v| !v.is_empty());
    let term = get("TERM").unwrap_or_default();
    let program = get("TERM_PROGRAM").unwrap_or_default();
    if has("TMUX") {
        return (
            Protocol::HalfBlocks,
            Some("inside tmux: using half-blocks (kitty graphics needs `set -g allow-passthrough on`; force it with --protocol kitty)".into()),
        );
    }
    let kitty = term == "xterm-kitty"
        || term == "xterm-ghostty"
        || has("KITTY_WINDOW_ID")
        || has("GHOSTTY_RESOURCES_DIR")
        || matches!(program.as_str(), "ghostty" | "WezTerm" | "kitty");
    if kitty {
        return (Protocol::Kitty, None);
    }
    let sixel = term.contains("sixel")
        || matches!(program.as_str(), "iTerm.app" | "mlterm" | "foot")
        || term == "foot"
        || term == "mlterm";
    if sixel {
        return (Protocol::Sixel, None);
    }
    (
        Protocol::HalfBlocks,
        Some("this terminal was not recognised as supporting graphics: using half-block characters (force one with --protocol kitty|sixel)".into()),
    )
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 {
            B64[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            B64[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// RGBA8 -> packed RGB8.
pub fn rgba_to_rgb(rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgba.len() / 4 * 3);
    for p in rgba.chunks_exact(4) {
        out.extend_from_slice(&p[..3]);
    }
    out
}

/// The kitty escape sequences that (re)place image `id` showing `rgb` (`w`x`h` pixels) over `cols`x`rows` cells at the cursor.
///
/// Reusing the same id replaces the picture in place (no flicker); `C=1` leaves the cursor alone; `q=2` silences replies;
/// the pixels are zlib-compressed (`o=z`) because a smooth shader compresses well and the terminal link is the bottleneck.
/// The placement id every frame uses. Placing the same image id with the same placement id REPLACES the old placement in place;
/// without it each frame makes a new placement (measured in Ghostty 1.3.1: about 1 frame in 8 stalls for 2+ frames, vs none with it).
pub const PLACEMENT: u32 = 1;

pub fn kitty_image(rgb: &[u8], w: u32, h: u32, cols: u16, rows: u16, id: u32, z: i32) -> Vec<u8> {
    use flate2::{Compression, write::ZlibEncoder};
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::fast());
    enc.write_all(rgb).expect("writing to a Vec cannot fail");
    let payload = base64(&enc.finish().expect("finishing a Vec cannot fail"));
    let mut out = Vec::new();
    let chunks: Vec<&[u8]> = payload.as_bytes().chunks(4096).collect();
    for (i, chunk) in chunks.iter().enumerate() {
        let more = u8::from(i + 1 < chunks.len());
        if i == 0 {
            write!(
                out,
                "\x1b_Ga=T,f=24,o=z,s={w},v={h},i={id},p={PLACEMENT},c={cols},r={rows},z={z},C=1,q=2,m={more};"
            )
            .unwrap();
        } else {
            write!(out, "\x1b_Gm={more};").unwrap();
        }
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\x1b\\");
    }
    out
}

/// Place image `id` from a raw RGB file the terminal reads and deletes itself (`t=t`): the escape sequence is ~100 bytes instead of
/// hundreds of kilobytes of base64 per frame, so the terminal's parser never stalls on the picture. Only for a terminal on this
/// machine. The path must be in a temp directory and contain `tty-graphics-protocol` (the protocol's rule).
pub fn kitty_file(path: &str, w: u32, h: u32, cols: u16, rows: u16, id: u32, z: i32) -> Vec<u8> {
    format!(
        "\x1b_Ga=T,f=24,t=t,s={w},v={h},i={id},p={PLACEMENT},c={cols},r={rows},z={z},C=1,q=2;{}\x1b\\",
        base64(path.as_bytes())
    )
    .into_bytes()
}

/// Synchronized update (DEC mode 2026): the terminal holds everything between these and shows it as one frame, so the cells and the
/// picture change together. Also hides the cursor so a blinking one cannot flash over the picture.
pub const SYNC_BEGIN: &[u8] = b"\x1b[?2026h\x1b[?25l";
pub const SYNC_END: &[u8] = b"\x1b[?2026l";

/// Where frame `slot` is written for `t=t` transfer (a few rotating files; the terminal deletes each after reading it).
pub fn temp_frame_path(pid: u32, slot: usize) -> String {
    // the system temp directory ($TMPDIR): Ghostty only accepts a temporary file inside its own idea of the temp dir, which on
    // macOS is NOT /tmp (measured: "temporary file not in temp dir" for /tmp, OK for $TMPDIR)
    std::env::temp_dir()
        .join(format!("tty-graphics-protocol-shaderlab-{pid}-{slot}.rgb"))
        .to_string_lossy()
        .into_owned()
}

/// Whether the terminal is on this machine (a file path means nothing over ssh or mosh).
pub fn is_local(get: &dyn Fn(&str) -> Option<String>) -> bool {
    !["SSH_CONNECTION", "SSH_TTY", "MOSH_CONNECTION"]
        .iter()
        .any(|k| get(k).is_some_and(|v| !v.is_empty()))
}

/// Remove image `id` (and free its data).
pub fn kitty_delete(id: u32) -> Vec<u8> {
    format!("\x1b_Ga=d,d=I,i={id},q=2\x1b\\").into_bytes()
}

/// A sixel image of `rgb` using a fixed 6x6x6 color cube: simple and fast, a little banded.
pub fn sixel(rgb: &[u8], w: u32, h: u32) -> Vec<u8> {
    let (w, h) = (w as usize, h as usize);
    let idx = |p: &[u8]| -> usize {
        let q = |v: u8| ((v as usize * 5 + 127) / 255).min(5);
        q(p[0]) * 36 + q(p[1]) * 6 + q(p[2])
    };
    let pix: Vec<usize> = rgb.chunks_exact(3).map(idx).collect();
    let mut out = Vec::new();
    // "q" starts sixel data; the raster attributes "1;1;w;h give the size (1:1 pixel aspect)
    write!(out, "\x1bPq\"1;1;{w};{h}").unwrap();
    let mut used = [false; 216];
    for &c in &pix {
        used[c] = true;
    }
    for (c, _) in used.iter().enumerate().filter(|(_, u)| **u) {
        let (r, g, b) = (c / 36, (c / 6) % 6, c % 6);
        write!(out, "#{c};2;{};{};{}", r * 20, g * 20, b * 20).unwrap();
    }
    for band in (0..h).step_by(6) {
        let rows = (h - band).min(6);
        let mut present = [false; 216];
        for y in band..band + rows {
            for x in 0..w {
                present[pix[y * w + x]] = true;
            }
        }
        let mut first = true;
        for (c, _) in present.iter().enumerate().filter(|(_, p)| **p) {
            if !first {
                out.push(b'$');
            }
            first = false;
            write!(out, "#{c}").unwrap();
            let mut x = 0;
            while x < w {
                let bits = |x: usize| -> u8 {
                    (0..rows).fold(0u8, |acc, r| {
                        if pix[(band + r) * w + x] == c {
                            acc | (1 << r)
                        } else {
                            acc
                        }
                    })
                };
                let b = bits(x);
                let mut run = 1;
                while x + run < w && bits(x + run) == b {
                    run += 1;
                }
                let ch = 63 + b;
                if run > 3 {
                    write!(out, "!{run}{}", ch as char).unwrap();
                } else {
                    for _ in 0..run {
                        out.push(ch);
                    }
                }
                x += run;
            }
        }
        out.push(b'-');
    }
    out.extend_from_slice(b"\x1b\\");
    out
}

/// The two pixels a half-block cell shows: (upper, lower) for cell column `cx`, row `cy` of an RGBA image `w` wide.
pub fn half_block_pair(rgba: &[u8], w: u32, h: u32, cx: u32, cy: u32) -> ([u8; 3], [u8; 3]) {
    let px = |x: u32, y: u32| -> [u8; 3] {
        if x >= w || y >= h {
            return [0, 0, 0];
        }
        let i = ((y * w + x) * 4) as usize;
        [rgba[i], rgba[i + 1], rgba[i + 2]]
    };
    (px(cx, cy * 2), px(cx, cy * 2 + 1))
}

/// Like [`half_block_pair`] for an image rendered `ss` times finer than the cell grid: each of the two pixels is the average of
/// an `ss` x `ss` box.
pub fn half_block_pair_avg(
    rgba: &[u8],
    w: u32,
    h: u32,
    cx: u32,
    cy: u32,
    ss: u32,
) -> ([u8; 3], [u8; 3]) {
    if ss <= 1 {
        return half_block_pair(rgba, w, h, cx, cy);
    }
    let avg = |py: u32| -> [u8; 3] {
        let mut acc = [0u32; 3];
        let mut n = 0;
        for y in py * ss..(py + 1) * ss {
            for x in cx * ss..(cx + 1) * ss {
                if x < w && y < h {
                    let i = ((y * w + x) * 4) as usize;
                    for c in 0..3 {
                        acc[c] += rgba[i + c] as u32;
                    }
                    n += 1;
                }
            }
        }
        let n = n.max(1);
        [(acc[0] / n) as u8, (acc[1] / n) as u8, (acc[2] / n) as u8]
    };
    (avg(cy * 2), avg(cy * 2 + 1))
}

/// The nearest xterm-256 color for terminals without truecolor.
pub fn nearest_256(rgb: [u8; 3]) -> u8 {
    let q = |v: u8| -> u8 {
        if v < 48 {
            0
        } else if v < 115 {
            1
        } else {
            ((v as u16 - 35) / 40) as u8
        }
    };
    let (r, g, b) = (q(rgb[0]), q(rgb[1]), q(rgb[2]));
    16 + 36 * r + 6 * g + b
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn base64_matches_the_standard_vectors() {
        for (i, o) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(i.as_bytes()), o);
        }
    }

    #[test]
    fn protocol_detection_follows_the_terminal_and_tmux() {
        assert_eq!(
            detect(&env(&[("TERM_PROGRAM", "ghostty")])).0,
            Protocol::Kitty
        );
        assert_eq!(
            detect(&env(&[("TERM", "xterm-ghostty")])).0,
            Protocol::Kitty
        );
        assert_eq!(detect(&env(&[("TERM", "xterm-kitty")])).0, Protocol::Kitty);
        assert_eq!(detect(&env(&[("KITTY_WINDOW_ID", "3")])).0, Protocol::Kitty);
        assert_eq!(detect(&env(&[("TERM_PROGRAM", "foot")])).0, Protocol::Sixel);
        let (p, note) = detect(&env(&[("TERM", "xterm-256color")]));
        assert_eq!(p, Protocol::HalfBlocks);
        assert!(note.unwrap().contains("half-block"));
        // tmux wins over a kitty-looking environment, and says why
        let (p, note) = detect(&env(&[
            ("TERM_PROGRAM", "ghostty"),
            ("TMUX", "/tmp/tmux-1/default,1,0"),
        ]));
        assert_eq!(p, Protocol::HalfBlocks);
        assert!(note.unwrap().contains("allow-passthrough"));
    }

    #[test]
    fn a_kitty_image_is_chunked_and_decodes_back_to_the_pixels() {
        use std::io::Read;
        let (w, h) = (64u32, 48u32);
        let mut seed = 12345u32;
        let rgb: Vec<u8> = (0..w * h * 3)
            .map(|_| {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                (seed >> 24) as u8
            })
            .collect();
        let out = String::from_utf8(kitty_image(&rgb, w, h, 20, 10, 7, -1)).unwrap();
        let seqs: Vec<&str> = out.split("\x1b\\").filter(|s| !s.is_empty()).collect();
        assert!(
            seqs.len() >= 2,
            "an image this size needs several chunks: {}",
            seqs.len()
        );
        let first = seqs[0];
        assert!(
            first.starts_with("\x1b_Ga=T,f=24,o=z,s=64,v=48,i=7,p=1,c=20,r=10,z=-1,C=1,q=2,m=1;"),
            "{}",
            &first[..70]
        );
        let mut payload = String::new();
        for (i, s) in seqs.iter().enumerate() {
            let (keys, data) = s.split_once(';').unwrap();
            assert!(data.len() <= 4096, "chunks stay within 4096");
            let last = i + 1 == seqs.len();
            assert!(keys.ends_with(if last { "m=0" } else { "m=1" }), "{keys}");
            if i > 0 {
                assert_eq!(keys, format!("\x1b_Gm={}", u8::from(!last)));
            }
            payload.push_str(data);
        }
        // decode base64 then inflate
        let mut bytes = Vec::new();
        let mut acc = 0u32;
        let mut bits = 0;
        for c in payload.bytes().filter(|c| *c != b'=') {
            acc = acc << 6 | B64.iter().position(|b| *b == c).unwrap() as u32;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                bytes.push((acc >> bits) as u8);
                acc &= (1 << bits) - 1;
            }
        }
        let mut back = Vec::new();
        flate2::read::ZlibDecoder::new(&bytes[..])
            .read_to_end(&mut back)
            .unwrap();
        assert_eq!(back, rgb);
    }

    #[test]
    fn a_file_transfer_is_tiny_and_names_a_protocol_temp_file() {
        let path = temp_frame_path(4242, 1);
        assert!(
            path.starts_with(std::env::temp_dir().to_str().unwrap())
                && path.contains("tty-graphics-protocol")
        );
        let seq = String::from_utf8(kitty_file(&path, 640, 360, 80, 30, 7, -1)).unwrap();
        assert!(
            seq.starts_with("\x1b_Ga=T,f=24,t=t,s=640,v=360,i=7,p=1,c=80,r=30,z=-1,C=1,q=2;"),
            "{seq}"
        );
        assert!(seq.len() < 200, "a path, not pixels: {} bytes", seq.len());
        let b64 = seq.split_once(';').unwrap().1.trim_end_matches("\x1b\\");
        assert_eq!(b64, base64(path.as_bytes()));
        assert!(SYNC_BEGIN.starts_with(b"\x1b[?2026h") && SYNC_END == b"\x1b[?2026l");
        let local = |k: &str| (k == "TERM").then(|| "xterm".to_string());
        let remote = |k: &str| (k == "SSH_CONNECTION").then(|| "1.2.3.4 5 6.7.8.9 22".to_string());
        assert!(is_local(&local) && !is_local(&remote));
    }

    #[test]
    fn kitty_delete_names_the_id() {
        assert_eq!(kitty_delete(9), b"\x1b_Ga=d,d=I,i=9,q=2\x1b\\");
    }

    #[test]
    fn sixel_has_the_header_a_palette_for_each_color_bands_and_the_terminator() {
        // 4x12: the top 6 rows red, the bottom 6 rows blue
        let mut rgb = Vec::new();
        for y in 0..12 {
            for _ in 0..4 {
                rgb.extend_from_slice(if y < 6 { &[255, 0, 0] } else { &[0, 0, 255] });
            }
        }
        let s = String::from_utf8(sixel(&rgb, 4, 12)).unwrap();
        assert!(s.starts_with("\x1bPq\"1;1;4;12"), "{s:?}");
        assert!(s.ends_with("\x1b\\"));
        assert_eq!(
            s.matches(";2;").count(),
            2,
            "a palette entry per color: {s:?}"
        );
        assert_eq!(s.matches('-').count(), 2, "two bands of 6 rows");
        assert!(
            s.contains("#180;2;100;0;0") && s.contains("#5;2;0;0;100"),
            "{s:?}"
        );
        // a full band of one color is six set bits -> '~' (63 + 63), run-length encoded
        assert!(s.contains("#180!4~") || s.contains("#180~~~~"), "{s:?}");
    }

    #[test]
    fn a_half_block_cell_shows_two_vertically_stacked_pixels() {
        // 2x4 image: cell (1,1) is x=1, rows 2 and 3
        let mut img = vec![0u8; 2 * 4 * 4];
        let set = |img: &mut Vec<u8>, x: usize, y: usize, c: [u8; 3]| {
            let i = (y * 2 + x) * 4;
            img[i..i + 3].copy_from_slice(&c);
        };
        set(&mut img, 1, 2, [10, 20, 30]);
        set(&mut img, 1, 3, [200, 100, 50]);
        assert_eq!(
            half_block_pair(&img, 2, 4, 1, 1),
            ([10, 20, 30], [200, 100, 50])
        );
        assert_eq!(
            half_block_pair(&img, 2, 4, 5, 5),
            ([0, 0, 0], [0, 0, 0]),
            "outside the image is black"
        );
    }

    #[test]
    fn an_oversampled_picture_is_averaged_down_per_cell_half() {
        // 4x4 image drawn at ss=2 for a 2x1-cell grid... one cell is 2 wide x 4 tall pixels: upper half rows 0-1, lower rows 2-3
        let mut img = vec![0u8; 2 * 4 * 4];
        for (y, v) in [(0usize, 100u8), (1, 200), (2, 40), (3, 60)] {
            for x in 0..2 {
                let i = (y * 2 + x) * 4;
                img[i..i + 3].copy_from_slice(&[v, v, v]);
            }
        }
        let (up, down) = half_block_pair_avg(&img, 2, 4, 0, 0, 2);
        assert_eq!(up, [150, 150, 150]);
        assert_eq!(down, [50, 50, 50]);
        assert_eq!(
            half_block_pair_avg(&img, 2, 4, 0, 0, 1),
            half_block_pair(&img, 2, 4, 0, 0)
        );
    }

    #[test]
    fn the_256_color_fallback_maps_black_white_and_primaries() {
        assert_eq!(nearest_256([0, 0, 0]), 16);
        assert_eq!(nearest_256([255, 255, 255]), 231);
        assert_eq!(nearest_256([255, 0, 0]), 196);
        assert_eq!(nearest_256([0, 0, 255]), 21);
    }
}
