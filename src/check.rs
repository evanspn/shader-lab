//! Objective checks for a shader, run on the GPU over a synthetic (or provided) terminal frame.
//!
//! * **compiles**: the shader (with every parameter variant) parses, validates and is accepted by the GPU;
//! * **finite**: no NaN or infinity in the output, at the defaults, every preset and extreme values;
//! * **not blank**: the output is not an all-black or an all-white frame;
//! * **text preserved**: pixels that are text in the input come out unchanged (within an epsilon), and the edge energy of
//!   the text region (how sharp it is) does not drop: a blur or a wash fails;
//! * **animation**: a shader that uses `iTime` changes from one second to the next, one that does not is static, and the
//!   same time always gives the same picture;
//! * **motion**: the declared `@motion` matches the direction measured on screen;
//! * **speed**: the time per 1080p frame stays inside a budget.

use std::path::PathBuf;

use crate::frame::{Frame, luminance};
use crate::gpu::{self, Gpu, Origin};
use crate::params::{self, Motion, RenderContext};

#[derive(Clone, Debug)]
pub struct CheckOptions {
    pub origin: Origin,
    /// Size of the frame used for the text and animation checks.
    pub size: (u32, u32),
    /// The most a text pixel may change, in 0..255 levels.
    pub epsilon: i32,
    /// The least edge energy the text region may keep (1.0 = exactly as sharp as the input).
    pub sharpness_min: f32,
    /// Milliseconds per 1080p frame.
    pub budget_ms: f64,
    /// Checks to skip: `text`, `motion`, `animation`, `perf`.
    pub skip: Vec<String>,
    /// A PNG to use as the terminal frame instead of the built-in sample.
    pub frame: Option<PathBuf>,
    pub perf_frames: u32,
}

impl Default for CheckOptions {
    fn default() -> Self {
        CheckOptions {
            origin: Origin::TopLeft,
            size: (640, 360),
            epsilon: 2,
            sharpness_min: 0.85,
            budget_ms: 16.7,
            skip: Vec::new(),
            frame: None,
            perf_frames: 20,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Finding {
    pub check: String,
    pub passed: bool,
    pub detail: String,
}

#[derive(Clone, Debug)]
pub struct Report {
    pub shader: String,
    pub findings: Vec<Finding>,
    pub frame_ms: Option<f64>,
    pub variants: usize,
}

impl Report {
    pub fn passed(&self) -> bool {
        self.findings.iter().all(|f| f.passed)
    }

    pub fn render(&self) -> String {
        let mut out = format!(
            "{} {}\n",
            if self.passed() { "PASS" } else { "FAIL" },
            self.shader
        );
        for f in &self.findings {
            out.push_str(&format!(
                "  {} {:<10} {}\n",
                if f.passed { "ok  " } else { "FAIL" },
                f.check,
                f.detail
            ));
        }
        out
    }
}

fn skip(opts: &CheckOptions, name: &str) -> bool {
    opts.skip.iter().any(|s| s == name)
}

/// Edge energy of the luminance over the pixels in `region`: the sum of absolute horizontal and vertical differences.
fn edge_energy(rgba: &[u8], w: usize, h: usize, region: &[bool]) -> f64 {
    let lum = |x: usize, y: usize| luminance(&rgba[(y * w + x) * 4..(y * w + x) * 4 + 4]) as f64;
    let mut e = 0.0;
    for y in 0..h.saturating_sub(1) {
        for x in 0..w.saturating_sub(1) {
            if region[y * w + x] {
                e += (lum(x + 1, y) - lum(x, y)).abs() + (lum(x, y + 1) - lum(x, y)).abs();
            }
        }
    }
    e
}

/// Pixels within `n` pixels (Chebyshev) of `mask`, the mask included.
pub fn dilate(mask: &[bool], w: usize, h: usize, n: i32) -> Vec<bool> {
    let mut out = mask.to_vec();
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            if mask[y as usize * w + x as usize] {
                continue;
            }
            'o: for dy in -n..=n {
                for dx in -n..=n {
                    let (xx, yy) = (x + dx, y + dy);
                    if xx >= 0
                        && yy >= 0
                        && xx < w as i32
                        && yy < h as i32
                        && mask[yy as usize * w + xx as usize]
                    {
                        out[y as usize * w + x as usize] = true;
                        break 'o;
                    }
                }
            }
        }
    }
    out
}

/// The dominant (dx, dy) shift, in pixels, of effect layer `b` relative to `a` (rows grow downward, columns rightward),
/// searching `range` pixels along the judged axis and a few pixels along the other.
pub fn dominant_shift(
    a: &[f32],
    b: &[f32],
    w: usize,
    h: usize,
    vertical: bool,
    range: i32,
) -> (i32, i32, f32, f32) {
    let side = 8;
    let corr = |dx: i32, dy: i32| -> f32 {
        let mut sum = 0.0f64;
        let mut n = 0usize;
        for y in 0..h as i32 {
            let yy = y + dy;
            if yy < 0 || yy >= h as i32 {
                continue;
            }
            for x in 0..w as i32 {
                let xx = x + dx;
                if xx < 0 || xx >= w as i32 {
                    continue;
                }
                let v = a[y as usize * w + x as usize];
                if v > 0.0 {
                    sum += (v * b[yy as usize * w + xx as usize]) as f64;
                }
                n += 1;
            }
        }
        (sum / n.max(1) as f64) as f32
    };
    let (mut best, mut best_c) = ((0, 0), corr(0, 0));
    let (rx, ry) = if vertical {
        (side, range)
    } else {
        (range, side)
    };
    for dy in -ry..=ry {
        for dx in -rx..=rx {
            let c = corr(dx, dy);
            if c > best_c {
                best_c = c;
                best = (dx, dy);
            }
        }
    }
    (best.0, best.1, best_c, corr(-best.0, -best.1))
}

fn to_levels(px: &[f32]) -> Vec<u8> {
    gpu::to_rgba8(px)
}

/// The light a shader added over a plain background, per pixel (never negative).
fn effect_layer(out: &[u8], input: &[u8]) -> Vec<f32> {
    out.chunks(4)
        .zip(input.chunks(4))
        .map(|(o, i)| (luminance(o) - luminance(i)).max(0.0))
        .collect()
}

fn plain_frame(bg: (u8, u8, u8), w: u32, h: u32) -> Frame {
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for _ in 0..w * h {
        rgba.extend_from_slice(&[bg.0, bg.1, bg.2, 255]);
    }
    Frame {
        background: bg,
        width: w,
        height: h,
        rgba,
        text: vec![false; (w * h) as usize],
    }
}

/// Run all the checks on `src`. With `gpu == None` only the CPU compile check is done.
pub fn check(gpu: Option<&Gpu>, name: &str, src: &str, opts: &CheckOptions) -> Report {
    let mut r = Report {
        shader: name.to_string(),
        findings: Vec::new(),
        frame_ms: None,
        variants: 0,
    };
    let mut add = |check: &str, passed: bool, detail: String| {
        r.findings.push(Finding {
            check: check.into(),
            passed,
            detail,
        })
    };

    let body = params::strip_header(src);
    let schema = match params::parse_schema(body) {
        Ok(s) => s,
        Err(e) => {
            add("annotations", false, e);
            params::Schema::default()
        }
    };
    let frame = match &opts.frame {
        Some(p) => match Frame::from_png(p) {
            Ok(f) => f,
            Err(e) => {
                add("frame", false, format!("{e:#}"));
                return r;
            }
        },
        None => Frame::sample(opts.size.0, opts.size.1),
    };
    let ctx = RenderContext {
        background: frame.background,
        ..RenderContext::default()
    };
    let variants = params::check_variants(&schema);
    r.variants = variants.len();
    let mut add = |check: &str, passed: bool, detail: String| {
        r.findings.push(Finding {
            check: check.into(),
            passed,
            detail,
        })
    };

    // ---- compiles (every variant), on the CPU first
    let mut compiled = Vec::new();
    for (label, values) in &variants {
        let text = params::render_ctx(src, values, &ctx).unwrap_or_else(|_| src.to_string());
        match gpu::compile_check(&text) {
            Ok(()) => compiled.push((label.clone(), text)),
            Err(e) => {
                add("compiles", false, format!("[{label}] {e}"));
                return r;
            }
        }
    }
    add(
        "compiles",
        true,
        format!("{} parameter variants parse and validate", compiled.len()),
    );
    let Some(gpu) = gpu else {
        return r;
    };

    // ---- on the GPU: finite, not blank, text preserved
    let text_px = frame.text.iter().filter(|t| **t).count();
    let region = dilate(&frame.text, frame.width as usize, frame.height as usize, 2);
    let (w, h) = (frame.width as usize, frame.height as usize);
    let before_energy = edge_energy(&frame.rgba, w, h, &region);
    let (mut nonfinite, mut blank, mut washed) = (Vec::new(), Vec::new(), Vec::new());
    let (mut text_bad, mut sharp_bad): (Vec<String>, Vec<String>) = (Vec::new(), Vec::new());
    let mut worst_ratio = f64::MAX;
    let mut worst_change = 0;
    for (label, text) in &compiled {
        let prepared = match gpu.prepare(text, &frame, opts.origin) {
            Ok(p) => p,
            Err(e) => {
                add("compiles", false, format!("[{label}] {e}"));
                return r;
            }
        };
        for t in [0.7f32, 3.3] {
            let px = match prepared.draw(gpu, t) {
                Ok(px) => px,
                Err(e) => {
                    add("render", false, e);
                    return r;
                }
            };
            if px.iter().any(|v| !v.is_finite()) {
                nonfinite.push(format!("{label} @t={t}"));
                continue;
            }
            let rgba = to_levels(&px);
            let mean = rgba.chunks(4).map(luminance).sum::<f32>() / (w * h) as f32;
            let max = rgba
                .chunks(4)
                .map(|p| p[0].max(p[1]).max(p[2]))
                .max()
                .unwrap_or(0);
            if max < 3 {
                blank.push(format!("{label} @t={t}: all black"));
            }
            if mean > 0.97 {
                washed.push(format!("{label} @t={t}: all white"));
            }
            if skip(opts, "text") || text_px == 0 {
                continue;
            }
            let change = frame
                .text
                .iter()
                .enumerate()
                .filter(|(_, t)| **t)
                .map(|(i, _)| {
                    (0..3)
                        .map(|k| (rgba[i * 4 + k] as i32 - frame.rgba[i * 4 + k] as i32).abs())
                        .max()
                        .unwrap_or(0)
                })
                .max()
                .unwrap_or(0);
            worst_change = worst_change.max(change);
            if change > opts.epsilon {
                text_bad.push(format!(
                    "{label} @t={t}: text pixels changed by up to {change}/255"
                ));
            }
            if before_energy > 0.0 {
                let ratio = edge_energy(&rgba, w, h, &region) / before_energy;
                worst_ratio = worst_ratio.min(ratio);
                if ratio < opts.sharpness_min as f64 {
                    sharp_bad.push(format!(
                        "{label} @t={t}: text edge energy {:.0}% of the input",
                        ratio * 100.0
                    ));
                }
            }
        }
    }
    add(
        "finite",
        nonfinite.is_empty(),
        if nonfinite.is_empty() {
            "no NaN or infinity at any parameter set".into()
        } else {
            format!("NaN/Inf in: {}", nonfinite.join("; "))
        },
    );
    let blank_all: Vec<String> = blank.into_iter().chain(washed).collect();
    add(
        "not blank",
        blank_all.is_empty(),
        if blank_all.is_empty() {
            "never an all-black or all-white frame".into()
        } else {
            blank_all.join("; ")
        },
    );
    if !skip(opts, "text") {
        if text_px == 0 {
            add("text", true, "skipped: the frame has no text".into());
        } else {
            let mut bad = text_bad;
            bad.truncate(3);
            add(
                "text",
                bad.is_empty() && sharp_bad.is_empty(),
                if bad.is_empty() && sharp_bad.is_empty() {
                    format!(
                        "{text_px} text pixels unchanged (worst change {worst_change}/255); edge energy kept at {:.0}% or more",
                        (worst_ratio.min(9.99)) * 100.0
                    )
                } else {
                    sharp_bad.truncate(3);
                    bad.extend(sharp_bad);
                    bad.join("; ")
                },
            );
        }
    }

    // ---- animation (at the defaults)
    if !skip(opts, "animation") {
        let (_, default_text) = &compiled[0];
        if let Ok(p) = gpu.prepare(default_text, &frame, opts.origin)
            && let (Ok(a), Ok(b), Ok(a2)) = (p.draw(gpu, 2.0), p.draw(gpu, 3.0), p.draw(gpu, 2.0))
        {
            let (qa, qb, qa2) = (to_levels(&a), to_levels(&b), to_levels(&a2));
            let moved = qa.iter().zip(&qb).filter(|(x, y)| x != y).count();
            let uses_time = uses_itime(src);
            let same_time_ok = qa == qa2;
            let (ok, detail) = match (uses_time, moved > 0, same_time_ok) {
                (_, _, false) => (false, "the same time gave two different pictures (not deterministic)".to_string()),
                (true, true, true) => (true, format!("uses iTime: {moved} pixels changed in one second; same time, same picture")),
                (true, false, true) => (false, "uses iTime but the picture did not change between t=2s and t=3s at the default parameters".to_string()),
                (false, false, true) => (true, "static: does not use iTime and never changes".to_string()),
                (false, true, true) => (false, "does not use iTime yet the picture changed with time".to_string()),
            };
            add("animation", ok, detail);
        }
    }

    // ---- declared motion against measured motion
    if !skip(opts, "motion") {
        match schema.motion {
            Some(m @ (Motion::Down | Motion::Up | Motion::Left | Motion::Right)) => {
                let (vertical, want) = match m {
                    Motion::Down => (true, 1),
                    Motion::Up => (true, -1),
                    Motion::Left => (false, -1),
                    _ => (false, 1),
                };
                let bg = plain_frame(frame.background, 320, 180);
                let (_, default_text) = &compiled[0];
                let mut seen = Vec::new();
                let mut ok = false;
                if let Ok(p) = gpu.prepare(default_text, &bg, opts.origin) {
                    'dt: for (dt, range) in [(0.05f32, 12), (0.25, 30), (0.8, 60)] {
                        let (Ok(a), Ok(b)) = (p.draw(gpu, 7.0), p.draw(gpu, 7.0 + dt)) else {
                            continue;
                        };
                        let (a, b) = (
                            effect_layer(&to_levels(&a), &bg.rgba),
                            effect_layer(&to_levels(&b), &bg.rgba),
                        );
                        if !a.iter().any(|v| *v > 0.02) {
                            continue;
                        }
                        let (dx, dy, peak, opposite) =
                            dominant_shift(&a, &b, 320, 180, vertical, range);
                        let s = if vertical { dy } else { dx };
                        seen.push(format!("dt {dt}s: dx {dx:+}, dy {dy:+}"));
                        if s != 0 && s.signum() == want && peak > opposite * 1.2 {
                            ok = true;
                            break 'dt;
                        }
                    }
                }
                add(
                    "motion",
                    ok,
                    if ok {
                        format!(
                            "declared {} and measured {} (rows grow downward, columns rightward)",
                            m.name(),
                            seen.last().cloned().unwrap_or_default()
                        )
                    } else if seen.is_empty() {
                        format!(
                            "declared {} but nothing was drawn to measure on a plain background",
                            m.name()
                        )
                    } else {
                        format!("declared {} but measured: {}", m.name(), seen.join("; "))
                    },
                );
            }
            Some(_) => add(
                "motion",
                true,
                "declared none/radial: nothing to measure".into(),
            ),
            None => add(
                "motion",
                true,
                "no @motion declared (add one so the direction can be checked)".into(),
            ),
        }
    }

    // ---- speed at 1080p
    if !skip(opts, "perf") {
        let big = Frame::sample(1920, 1080);
        let (_, default_text) = &compiled[0];
        let big_ctx = RenderContext {
            background: big.background,
            ..RenderContext::default()
        };
        let text = params::render_ctx(src, &Default::default(), &big_ctx)
            .unwrap_or_else(|_| default_text.clone());
        match gpu
            .prepare(&text, &big, opts.origin)
            .and_then(|p| p.bench(gpu, opts.perf_frames))
        {
            Ok(ms) => {
                r.frame_ms = Some(ms);
                add(
                    "speed",
                    ms <= opts.budget_ms,
                    format!(
                        "{ms:.2} ms per 1080p frame (budget {:.1} ms) on {}",
                        opts.budget_ms, gpu.adapter_name
                    ),
                );
            }
            Err(e) => add("speed", false, e),
        }
    }
    r
}

/// Does the shader code (not its comments) read `iTime`?
pub fn uses_itime(src: &str) -> bool {
    let mut code = String::new();
    let mut rest = src;
    while let Some(i) = rest.find('/') {
        code.push_str(&rest[..i]);
        let tail = &rest[i..];
        if tail.starts_with("//") {
            rest = tail.find('\n').map_or("", |n| &tail[n..]);
        } else if let Some(block) = tail.strip_prefix("/*") {
            rest = block.find("*/").map_or("", |n| &block[n + 2..]);
        } else {
            code.push('/');
            rest = &tail[1..];
        }
    }
    code.push_str(rest);
    code.contains("iTime")
}

#[cfg(test)]
mod itime_tests {
    use super::uses_itime;

    #[test]
    fn comments_do_not_count_as_using_time() {
        assert!(!uses_itime("// static (no iTime)\nvoid mainImage() {}"));
        assert!(!uses_itime("/* iTime */ void f() {} // iTime"));
        assert!(uses_itime("// c\nfloat t = iTime * 2.0; // x"));
        assert!(uses_itime("float a = 1.0 / 2.0; float t = iTime;"));
    }
}
