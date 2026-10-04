//! `shaderlab regress`: regression checks that protect every shader fix and optimization from being quietly undone.
//!
//! For each shader (and each preset, and each `scene` lock) it runs:
//!
//! * **golden**: the picture at a fixed time over the synthetic terminal frame, compared with a committed golden PNG (mean and
//!   worst-block difference); `--update` rewrites the goldens and says what changed, so a change of look is deliberate;
//! * **orient**: the same at 16:9, 4:3, 1:1, 9:16 and 3:1, so a flipped, stretched or rotated scene shows up;
//! * **text**: text pixels of the synthetic frame come out unchanged;
//! * **coverage** and **flat**: how much of the frame the shader draws, and the largest block of one flat colour, against the
//!   accepted values stored next to the golden (a flat patch where there was a gradient is a regression);
//! * **temporal**: over a stretch of animation, no frame differs from its neighbours far more than the usual motion, the average
//!   brightness never steps (a cross-fade that lurches), and nothing jumps at the usual time-wrap moments;
//! * **perf**: p50 / p95 milliseconds per 1080p frame against `perf-baseline.json`, only on the machine it was recorded on.
//!
//! Goldens hold only the shader's output over the SYNTHETIC frame (never anything personal) and stay tiny.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::check::dilate;
use crate::frame::{Frame, luminance};
use crate::gpu::{self, Gpu, Origin, Prepared};
use crate::params::{self, Kind};

/// The check classes, as `--only` names them.
pub const CLASSES: [&str; 6] = ["golden", "orient", "text", "coverage", "temporal", "perf"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Pass,
    Fail,
    Skip,
    Updated,
}

#[derive(Clone, Debug)]
pub struct Row {
    pub shader: String,
    pub variant: String,
    pub check: String,
    pub status: Status,
    pub detail: String,
}

#[derive(Clone, Debug)]
pub struct Options {
    /// where the goldens live: `DIR/<shader>/<variant>.png`
    pub golden_dir: PathBuf,
    /// `perf-baseline.json`
    pub baseline: PathBuf,
    pub update: bool,
    /// empty = every class
    pub only: Vec<String>,
    /// the quick subset: no perf, short temporal run, no seam probe
    pub fast: bool,
    pub origin: Origin,
    /// override of the machine id (tests)
    pub machine: Option<String>,
    /// the length of the first temporal window in seconds (default: 8 fast, 125 for the defaults of a full run, 40 for scene locks)
    pub start_secs: Option<f32>,
}

impl Options {
    pub fn for_repo(root: &Path) -> Options {
        Options {
            golden_dir: root.join("tests/golden"),
            baseline: root.join("perf-baseline.json"),
            update: false,
            only: Vec::new(),
            fast: false,
            origin: Origin::TopLeft,
            machine: None,
            start_secs: None,
        }
    }

    fn wants(&self, class: &str) -> bool {
        self.only.is_empty() || self.only.iter().any(|c| c == class)
    }
}

// ---- tolerances ---------------------------------------------------------------------------------------

/// Mean absolute difference over all RGB values, in 0..255 levels.
const GOLDEN_MEAN: f32 = 0.8;
/// The largest mean difference in any 16x16 block.
const GOLDEN_BLOCK: f32 = 6.0;
/// ... but never looser than this fraction of what the shader adds to the terminal frame (its own effect), nor tighter than the floors.
const GOLDEN_REL_MEAN: f32 = 0.05;
const GOLDEN_REL_BLOCK: f32 = 0.10;
const GOLDEN_MIN_MEAN: f32 = 0.1;
const GOLDEN_MIN_BLOCK: f32 = 0.5;
/// The most a text pixel may change.
const TEXT_EPS: i32 = 2;
const COVERAGE_DELTA: f32 = 0.08;
const FLAT_DELTA: f32 = 0.02;
/// A frame whose change from the previous one exceeds `SPIKE_K` x the median change + `SPIKE_FLOOR` levels is a pop.
const SPIKE_K: f32 = 8.0;
const SPIKE_FLOOR: f32 = 0.15;
/// ... plus this fraction of what the shader adds to the frame (its effect energy), so strong shaders keep their old allowance.
const SPIKE_REL: f32 = 0.10;
/// the time-wrap probes are short and quiet: a jump there must beat only this many times the window's usual change
const SEAM_K: f32 = 3.0;
/// in a long run a pop must also touch at least this fraction of the picture (a single falling streak does not)
const POP_MIN_AREA: f32 = 0.15;
const SEAM_REL: f32 = 0.02;
const LUMA_REL: f32 = 0.08;
const LUMA_MIN: f32 = 0.8;
/// a brightness step must beat this many times the usual flutter of the average brightness
const LURCH_K: f32 = 10.0;
/// The most the average brightness may change between two frames (0..255) before it is a lurch.
const PERF_P50_RATIO: f64 = 1.25;
const PERF_P95_RATIO: f64 = 1.6;
const PERF_P50_FLOOR_MS: f64 = 0.04;
const PERF_P95_FLOOR_MS: f64 = 0.15;

const GOLDEN_SIZE: (u32, u32) = (192, 108);
const GOLDEN_TIME: f32 = 7.0;
/// further moments the text is checked at
const TEXT_TIMES: [f32; 6] = [0.5, 2.0, 13.0, 29.0, 41.0, 67.0];
/// (label, width, height) for the orientation goldens: all 60 px tall.
const ASPECTS: [(&str, u32, u32); 5] = [
    ("16x9", 106, 60),
    ("4x3", 80, 60),
    ("1x1", 60, 60),
    ("9x16", 34, 60),
    ("3x1", 180, 60),
];

// ---- variants ----------------------------------------------------------------------------------------

/// A named way of running a shader: the defaults, a preset, or one value of a `scene` selector.
#[derive(Clone, Debug)]
pub struct Variant {
    pub label: String,
    pub preset: Option<String>,
    pub sets: Vec<String>,
}

pub fn variants_of(src: &str) -> Vec<Variant> {
    let mut v = vec![Variant {
        label: "default".into(),
        preset: None,
        sets: vec![],
    }];
    let Ok(schema) = params::parse_schema(params::strip_header(src)) else {
        return v;
    };
    for p in &schema.presets {
        v.push(Variant {
            label: format!("preset-{}", p.name),
            preset: Some(p.name.clone()),
            sets: vec![],
        });
    }
    // a `scene` selector: every value is its own look
    if let Some(sc) = schema.params.iter().find(|p| p.name == "scene")
        && let Kind::Float { min, max } = sc.kind
        && max - min <= 8.0
    {
        let (lo, hi) = (min.ceil() as i32, max.floor() as i32);
        for n in lo.max(1)..=hi {
            v.push(Variant {
                label: format!("scene-{n}"),
                preset: None,
                sets: vec![format!("scene={n}")],
            });
        }
    }
    v
}

// ---- image helpers ------------------------------------------------------------------------------------

fn prepare(
    gpu: &Gpu,
    src: &str,
    var: &Variant,
    frame: &Frame,
    origin: Origin,
) -> Result<Prepared, String> {
    gpu::prepare_shader(gpu, src, var.preset.as_deref(), &var.sets, frame, origin)
}

fn render_at(
    gpu: &Gpu,
    src: &str,
    var: &Variant,
    size: (u32, u32),
    t: f32,
    origin: Origin,
) -> Result<(Frame, Vec<u8>), String> {
    let frame = Frame::sample(size.0, size.1);
    let p = prepare(gpu, src, var, &frame, origin)?;
    let mut out = Vec::new();
    p.draw_rgba8(gpu, t, 1.0 / 60.0, (t * 60.0) as i32, &mut out)?;
    Ok((frame, out))
}

fn save_png(path: &Path, rgba: &[u8], w: u32, h: u32) -> Result<(), String> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    image::save_buffer(path, rgba, w, h, image::ColorType::Rgba8).map_err(|e| e.to_string())
}

fn load_png(path: &Path) -> Result<(Vec<u8>, u32, u32), String> {
    let img = image::open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?
        .to_rgba8();
    let (w, h) = img.dimensions();
    Ok((img.into_raw(), w, h))
}

/// (mean absolute difference over RGB, the largest 16x16-block mean difference), in 0..255 levels.
pub fn image_diff(a: &[u8], b: &[u8], w: u32, h: u32) -> (f32, f32) {
    let (w, h) = (w as usize, h as usize);
    let mut total = 0.0f64;
    let (bw, bh) = (w.div_ceil(16), h.div_ceil(16));
    let mut blocks = vec![(0.0f64, 0usize); bw * bh];
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 4;
            let d: f64 = (0..3)
                .map(|c| (a[i + c] as f64 - b[i + c] as f64).abs())
                .sum::<f64>()
                / 3.0;
            total += d;
            let blk = &mut blocks[(y / 16) * bw + x / 16];
            blk.0 += d;
            blk.1 += 1;
        }
    }
    let worst = blocks
        .iter()
        .filter(|b| b.1 > 0)
        .map(|b| b.0 / b.1 as f64)
        .fold(0.0, f64::max);
    ((total / (w * h) as f64) as f32, worst as f32)
}

/// The largest group of touching pixels with exactly one colour, as a fraction of the frame.
pub fn largest_flat_fraction(rgba: &[u8], w: u32, h: u32) -> f32 {
    let (w, h) = (w as usize, h as usize);
    let px = |x: usize, y: usize| {
        [
            rgba[(y * w + x) * 4],
            rgba[(y * w + x) * 4 + 1],
            rgba[(y * w + x) * 4 + 2],
        ]
    };
    let mut seen = vec![false; w * h];
    let mut best = 0usize;
    let mut stack = Vec::new();
    for sy in 0..h {
        for sx in 0..w {
            if seen[sy * w + sx] {
                continue;
            }
            let c = px(sx, sy);
            seen[sy * w + sx] = true;
            stack.push((sx, sy));
            let mut n = 0;
            while let Some((x, y)) = stack.pop() {
                n += 1;
                let mut visit = |nx: usize, ny: usize| {
                    if !seen[ny * w + nx] && px(nx, ny) == c {
                        seen[ny * w + nx] = true;
                        stack.push((nx, ny));
                    }
                };
                if x > 0 {
                    visit(x - 1, y);
                }
                if x + 1 < w {
                    visit(x + 1, y);
                }
                if y > 0 {
                    visit(x, y - 1);
                }
                if y + 1 < h {
                    visit(x, y + 1);
                }
            }
            best = best.max(n);
        }
    }
    best as f32 / (w * h) as f32
}

/// The fraction of pixels the shader changed from the input by more than 3 levels.
pub fn coverage_fraction(frame: &Frame, out: &[u8]) -> f32 {
    let n = frame.rgba.len() / 4;
    let changed = (0..n)
        .filter(|i| {
            (0..3).any(|c| (frame.rgba[i * 4 + c] as i32 - out[i * 4 + c] as i32).abs() > 3)
        })
        .count();
    changed as f32 / n as f32
}

// ---- accepted values stored beside each golden --------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Meta {
    coverage: f32,
    flat: f32,
}

fn meta_path(dir: &Path, variant: &str) -> PathBuf {
    dir.join(format!("{variant}.meta"))
}

fn read_meta(path: &Path) -> Option<Meta> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut m = Meta::default();
    for line in text.lines() {
        let (k, v) = line.split_once('=')?;
        let v: f32 = v.trim().parse().ok()?;
        match k.trim() {
            "coverage" => m.coverage = v,
            "flat" => m.flat = v,
            _ => {}
        }
    }
    Some(m)
}

fn write_meta(path: &Path, m: Meta) -> std::io::Result<()> {
    std::fs::write(
        path,
        format!("coverage={:.4}\nflat={:.4}\n", m.coverage, m.flat),
    )
}

// ---- the perf baseline ---------------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct Baseline {
    pub machine: String,
    /// key -> (p50 ms, p95 ms, the reference shader's p50 measured in the same run)
    pub entries: BTreeMap<String, (f64, f64, f64)>,
}

impl Baseline {
    pub fn to_json(&self) -> String {
        let mut s = format!(
            "{{\n  \"machine\": \"{}\",\n  \"entries\": {{\n",
            self.machine.replace('"', "'")
        );
        let n = self.entries.len();
        for (i, (k, (p50, p95, rf))) in self.entries.iter().enumerate() {
            s.push_str(&format!(
                "    \"{k}\": {{\"p50_ms\": {p50:.3}, \"p95_ms\": {p95:.3}, \"ref_ms\": {rf:.3}}}{}\n",
                if i + 1 < n { "," } else { "" }
            ));
        }
        s.push_str("  }\n}\n");
        s
    }

    pub fn parse(text: &str) -> Option<Baseline> {
        let mut b = Baseline::default();
        for line in text.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("\"machine\":") {
                b.machine = rest
                    .trim()
                    .trim_end_matches(',')
                    .trim_matches('"')
                    .to_string();
            } else if line.contains("\"p50_ms\"") {
                let key = line.split('"').nth(1)?.to_string();
                let num = |name: &str| -> Option<f64> {
                    let i = line.find(name)? + name.len();
                    let rest = line[i..].trim_start_matches(['"', ':', ' ']);
                    let end = rest.find([',', '}'])?;
                    rest[..end].trim().parse().ok()
                };
                b.entries.insert(
                    key,
                    (num("p50_ms")?, num("p95_ms")?, num("ref_ms").unwrap_or(0.0)),
                );
            }
        }
        if b.machine.is_empty() { None } else { Some(b) }
    }
}

/// Which machine a baseline was recorded on: the GPU and the operating system.
pub fn machine_id(gpu: &Gpu) -> String {
    let os = std::process::Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()
        .map(|o| format!("macOS {}", String::from_utf8_lossy(&o.stdout).trim()))
        .unwrap_or_else(|| std::env::consts::OS.to_string());
    format!("{} / {os}", gpu.adapter_name)
}

fn percentile(sorted: &[f64], q: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * q).round() as usize]
}

/// A fixed shader measured alongside every shader, in the same process: when the machine is busy the reference slows down too, so
/// a shader's time RELATIVE to it still tells whether the shader itself got slower.
const REFERENCE: &str = "void mainImage(out vec4 c, in vec2 f) { vec2 p = f / iResolution.xy; vec3 a = vec3(0.0); for (int i = 0; i < 24; i++) { a += 0.02 * sin(p.xyx * float(i) + iTime + a); } c = vec4(a * 0.5 + 0.5, 1.0); }";

#[derive(Clone, Copy, Debug)]
pub struct Perf {
    pub p50: f64,
    pub p95: f64,
    /// the reference shader's p50, measured in the same call
    pub reference: f64,
}

const PERF_BATCHES: u32 = 21;
const PERF_PER_BATCH: u32 = 8;

fn batch_stats(t: &mut [f64]) -> (f64, f64) {
    t.sort_by(|a, b| a.partial_cmp(b).expect("no NaN times"));
    (percentile(t, 0.5), percentile(t, 0.95))
}

/// p50 and p95 of the GPU time per frame at `size`, in batches of back-to-back frames (the cost of waiting is spread over each batch),
/// from `t0` seconds of shader time, with the reference shader's p50 beside it.
pub fn perf_of(
    gpu: &Gpu,
    src: &str,
    var: &Variant,
    origin: Origin,
    size: (u32, u32),
    t0: f32,
) -> Result<Perf, String> {
    let frame = Frame::sample(size.0, size.1);
    let p = prepare(gpu, src, var, &frame, origin)?;
    let mut t = p.frame_times_batched(gpu, PERF_BATCHES, PERF_PER_BATCH, 3, t0)?;
    let (p50, p95) = batch_stats(&mut t);
    let r = gpu::prepare_shader(gpu, REFERENCE, None, &[], &frame, origin)?;
    let mut rt = r.frame_times_batched(gpu, PERF_BATCHES, PERF_PER_BATCH, 3, 0.0)?;
    let (reference, _) = batch_stats(&mut rt);
    Ok(Perf {
        p50,
        p95,
        reference,
    })
}

/// Is `p` within tolerance of the baseline entry `(b50, b95, bref)`? Slower by more than 25% (plus a small floor that grows with the
/// shader's own time) in absolute terms AND relative to the reference shader is a regression; slower in absolute terms only is a
/// busy machine.
pub fn perf_within(p: &Perf, b: (f64, f64, f64)) -> bool {
    let (b50, b95, bref) = b;
    let abs50 = p.p50 <= b50 * PERF_P50_RATIO + PERF_P50_FLOOR_MS + 0.05 * b50;
    let abs95 = p.p95 <= b95 * PERF_P95_RATIO + PERF_P95_FLOOR_MS + 0.05 * b95;
    let rel50 = bref > 0.0
        && p.reference > 0.0
        && p.p50 / p.reference <= (b50 / bref) * PERF_P50_RATIO + 0.02;
    (abs50 && abs95) || rel50
}

// ---- the checks ---------------------------------------------------------------------------------------

fn row(shader: &str, variant: &str, check: &str, status: Status, detail: impl Into<String>) -> Row {
    Row {
        shader: shader.into(),
        variant: variant.into(),
        check: check.into(),
        status,
        detail: detail.into(),
    }
}

/// Compare `rgba` (w x h) with the golden at `path`; with `update`, write it.
#[allow(clippy::too_many_arguments)]
fn golden_row(
    shader: &str,
    variant: &str,
    check: &str,
    path: &Path,
    rgba: &[u8],
    input: &[u8],
    (w, h): (u32, u32),
    update: bool,
) -> Row {
    if update {
        let before = load_png(path)
            .ok()
            .filter(|(_, bw, bh)| (*bw, *bh) == (w, h));
        let note = match before {
            Some((old, _, _)) => {
                let (mean, block) = image_diff(&old, rgba, w, h);
                if mean < 0.05 && block < 0.5 {
                    "unchanged".to_string()
                } else {
                    format!("changed: mean {mean:.2}, worst block {block:.1}")
                }
            }
            None => "new".to_string(),
        };
        return match save_png(path, rgba, w, h) {
            Ok(()) => row(shader, variant, check, Status::Updated, note),
            Err(e) => row(
                shader,
                variant,
                check,
                Status::Fail,
                format!("could not write the golden: {e}"),
            ),
        };
    }
    match load_png(path) {
        Err(_) => row(
            shader,
            variant,
            check,
            Status::Fail,
            format!(
                "no golden at {} (run with --update to create it)",
                path.display()
            ),
        ),
        Ok((gold, gw, gh)) => {
            if (gw, gh) != (w, h) {
                return row(
                    shader,
                    variant,
                    check,
                    Status::Fail,
                    format!("golden is {gw}x{gh}, render is {w}x{h}"),
                );
            }
            let (mean, block) = image_diff(&gold, rgba, w, h);
            // the tolerance is relative to what the shader itself adds to the picture: a faint effect (a few levels over the
            // terminal) must not hide a +10% change behind a fixed allowance meant for strong ones
            let (effect_mean, effect_block) = image_diff(&gold, input, w, h);
            let tol_mean = (GOLDEN_REL_MEAN * effect_mean).clamp(GOLDEN_MIN_MEAN, GOLDEN_MEAN);
            let tol_block = (GOLDEN_REL_BLOCK * effect_block).clamp(GOLDEN_MIN_BLOCK, GOLDEN_BLOCK);
            if mean <= tol_mean && block <= tol_block {
                row(
                    shader,
                    variant,
                    check,
                    Status::Pass,
                    format!(
                        "mean {mean:.2} (max {tol_mean:.2}), worst block {block:.1} (max {tol_block:.1})"
                    ),
                )
            } else {
                row(
                    shader,
                    variant,
                    check,
                    Status::Fail,
                    format!(
                        "differs from the golden: mean {mean:.2} (max {tol_mean:.2} = {:.0}% of its effect {effect_mean:.2}), worst block {block:.1} (max {tol_block:.1})",
                        GOLDEN_REL_MEAN * 100.0
                    ),
                )
            }
        }
    }
}

/// The gap between neighbouring 32-bit floats near `t`, in seconds.
pub fn f32_spacing(t: f32) -> f32 {
    2.0f32.powi(t.max(1.0).log2().floor() as i32 - 23)
}

/// Moments a long-running shader might wrap its clock or position (modulo restarts at 100 and 1800 s, hours, days, float limits).
pub const WRAP_PROBES: [f32; 18] = [
    100.0,
    200.0,
    300.0,
    600.0,
    900.0,
    1800.0,
    2400.0,
    3600.0,
    7200.0,
    14400.0,
    21600.0,
    43200.0,
    65536.0,
    86400.0,
    100_000.0,
    131_072.0,
    604_800.0,
    1_000_000.0,
];

/// The animation checks over `secs` of shader time at 24 fps, 160x90: pops, brightness lurches, and (full run) the time-wrap seams.
pub fn temporal_rows(
    gpu: &Gpu,
    name: &str,
    src: &str,
    var: &Variant,
    origin: Origin,
    secs: f32,
    seams: bool,
) -> Vec<Row> {
    let (w, h) = (160u32, 90u32);
    let frame = Frame::sample(w, h);
    let p = match prepare(gpu, src, var, &frame, origin) {
        Ok(p) => p,
        Err(e) => return vec![row(name, &var.label, "temporal", Status::Fail, e)],
    };
    let mut windows: Vec<(String, f32, f32)> = vec![("start".into(), 5.0, secs)];
    if seams {
        // the usual moments a long-running shader wraps its clock or its position
        for t in WRAP_PROBES {
            windows.push((format!("t={t}"), t - 2.0, 4.0));
        }
    }
    let mut out = Vec::new();
    let mut reference_median = 0.0f32;
    let mut reference_step = 0.0f32;
    let mut reference_area = 0.0f32;
    // what the shader adds to the terminal frame: the pop floor and the brightness step are relative to it, so a faint effect's
    // clock jump or flash is not hidden below a fixed allowance
    let mut effect = 0.0f32;
    for (label, start, len) in windows {
        // 32-bit iTime cannot tell one 24 fps frame from the next once its spacing exceeds half a frame: that is the clock, not the shader
        if label != "start" && f32_spacing(start) * 2.0 > 1.0 / 24.0 {
            out.push(row(
                name,
                &var.label,
                "seam",
                Status::Skip,
                format!("{label}: a 32-bit iTime has steps of {:.3} s here, coarser than a frame: not probed", f32_spacing(start)),
            ));
            continue;
        }
        let n = (len * 24.0) as usize;
        let mut prev: Option<Vec<u8>> = None;
        let (mut diffs, mut means, mut areas) = (Vec::new(), Vec::new(), Vec::new());
        let mut effects: Vec<f32> = Vec::new();
        let mut buf = Vec::new();
        for i in 0..n {
            let t = start + i as f32 / 24.0;
            if p.draw_rgba8(gpu, t, 1.0 / 24.0, i as i32, &mut buf)
                .is_err()
            {
                out.push(row(
                    name,
                    &var.label,
                    "temporal",
                    Status::Fail,
                    "the GPU failed to draw a frame",
                ));
                return out;
            }
            let mean = buf.chunks(4).map(luminance).sum::<f32>() / (w * h) as f32 * 255.0;
            means.push(mean);
            if label == "start" {
                effects.push(
                    buf.iter()
                        .zip(&frame.rgba)
                        .map(|(a, b)| (*a as f32 - *b as f32).abs())
                        .sum::<f32>()
                        / buf.len() as f32,
                );
            }
            if let Some(pv) = &prev {
                let d = pv
                    .iter()
                    .zip(&buf)
                    .map(|(a, b)| (*a as f32 - *b as f32).abs())
                    .sum::<f32>()
                    / buf.len() as f32;
                diffs.push(d);
                // how much of the picture moved: a rain drop changes a few pixels a lot, a camera jump changes most of them
                areas.push(
                    pv.iter()
                        .zip(&buf)
                        .filter(|(a, b)| (**a as i32 - **b as i32).abs() >= 2)
                        .count() as f32
                        / buf.len() as f32,
                );
            }
            prev = Some(buf.clone());
        }
        let mut sorted = diffs.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
        let median = sorted[sorted.len() / 2];
        // a pop is a frame far from ITS OWN neighbourhood (a playlist moves faster in some scenes than in others), so each change is
        // judged against the median of the changes within a second either side of it
        let local_median = |v: &[f32], i: usize| {
            let (lo, hi) = (i.saturating_sub(24), (i + 25).min(v.len()));
            let mut w: Vec<f32> = v[lo..hi].to_vec();
            w.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
            w[w.len() / 2]
        };
        let mut sorted_areas = areas.clone();
        sorted_areas.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
        let median_area = sorted_areas[sorted_areas.len() / 2];
        if label == "start" {
            effects.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
            effect = effects[effects.len() / 2];
        }
        let floor = (SPIKE_REL * effect).max(SPIKE_FLOOR);
        let luma_step = (LUMA_REL * effect).clamp(LUMA_MIN, 4.0);
        let mut worst = 0.0f32;
        let mut pop = false;
        for (i, d) in diffs.iter().enumerate() {
            worst = worst.max(*d);
            // a pop is big in size AND wide in extent (sparse sparkle or rain is not a pop)
            if *d > SPIKE_K * local_median(&diffs, i).max(0.5 * median) + floor
                && *d > 0.25 * effect
                && areas[i]
                    > (2.0 * local_median(&areas, i).max(0.5 * median_area) + 0.03)
                        .max(POP_MIN_AREA)
            {
                pop = true;
            }
        }
        // a lurch is a step in the average brightness far beyond the usual flutter (rain, sparkles): judged against the median step
        let steps: Vec<f32> = means.windows(2).map(|w2| (w2[1] - w2[0]).abs()).collect();
        let mut sorted_steps = steps.clone();
        sorted_steps.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
        let median_step = sorted_steps[sorted_steps.len() / 2];
        let lurch = steps.iter().cloned().fold(0.0f32, f32::max);
        let lurch_at =
            start + (steps.iter().position(|v| *v == lurch).unwrap_or(0) + 1) as f32 / 24.0;
        if label == "start" {
            reference_step = median_step;
        }
        let lurch_limit = LURCH_K * median_step.max(reference_step) + luma_step;
        if label == "start" {
            reference_median = median;
            reference_area = median_area;
        }
        // around a wrap the reference is the ordinary motion of the start of the run
        if label != "start" {
            let widest = areas.iter().cloned().fold(0.0f32, f32::max);
            // (short windows where nothing should happen: tighter than the long run, judged against the window's own motion)
            pop = worst
                > SEAM_K * median.max(0.5 * reference_median)
                    + (SEAM_REL * effect).max(SPIKE_FLOOR)
                && widest > 2.0 * reference_area.max(median_area) + 0.03;
        }
        let lurched = lurch > lurch_limit;
        let status = if pop || lurched {
            Status::Fail
        } else {
            Status::Pass
        };
        let mut detail = format!(
            "{label}: median change {median:.2}, worst {worst:.2}, largest brightness step {lurch:.2} (at t={lurch_at:.1} s)"
        );
        if pop {
            detail.push_str(" - a frame pops");
        }
        if lurched {
            detail.push_str(" - the brightness lurches");
        }
        out.push(row(
            name,
            &var.label,
            if label == "start" { "temporal" } else { "seam" },
            status,
            detail,
        ));
    }
    out
}

/// Run every wanted check on one shader. Needs the GPU.
pub fn run_shader(
    gpu: &Gpu,
    name: &str,
    src: &str,
    opts: &Options,
    baseline: &mut Baseline,
    machine: &str,
) -> Vec<Row> {
    let mut rows = Vec::new();
    let stem = name.trim_end_matches(".glsl");
    let gdir = opts.golden_dir.join(stem);
    let baseline_machine_ok = baseline.machine.is_empty() || baseline.machine == machine;
    for var in variants_of(src) {
        let need_main = opts.wants("golden") || opts.wants("text") || opts.wants("coverage");
        if need_main {
            match render_at(gpu, src, &var, GOLDEN_SIZE, GOLDEN_TIME, opts.origin) {
                Err(e) => rows.push(row(stem, &var.label, "golden", Status::Fail, e)),
                Ok((frame, out)) => {
                    if opts.wants("golden") {
                        let path = gdir.join(format!("{}.png", var.label));
                        rows.push(golden_row(
                            stem,
                            &var.label,
                            "golden",
                            &path,
                            &out,
                            &frame.rgba,
                            GOLDEN_SIZE,
                            opts.update,
                        ));
                    }
                    if opts.wants("text") && src.contains("// regress: text-ok") {
                        rows.push(row(
                            stem,
                            &var.label,
                            "text",
                            Status::Skip,
                            "marked `// regress: text-ok`: it draws over text by design",
                        ));
                    } else if opts.wants("text") {
                        // at several moments: a guard that fails only when a drop or a ribbon happens to cross the text still shows
                        let changed = |f: &Frame, o: &[u8]| {
                            (0..f.text.len())
                                .filter(|&i| {
                                    f.text[i]
                                        && (0..3).any(|c| {
                                            (f.rgba[i * 4 + c] as i32 - o[i * 4 + c] as i32).abs()
                                                > TEXT_EPS
                                        })
                                })
                                .count()
                        };
                        let mut bad = changed(&frame, &out);
                        for t in TEXT_TIMES {
                            if let Ok((f2, o2)) =
                                render_at(gpu, src, &var, GOLDEN_SIZE, t, opts.origin)
                            {
                                bad += changed(&f2, &o2);
                            }
                        }
                        let _ = dilate;
                        rows.push(if bad == 0 {
                            row(
                                stem,
                                &var.label,
                                "text",
                                Status::Pass,
                                format!(
                                    "{} text pixels unchanged at {} moments",
                                    frame.text_pixels(),
                                    TEXT_TIMES.len() + 1
                                ),
                            )
                        } else {
                            row(
                                stem,
                                &var.label,
                                "text",
                                Status::Fail,
                                format!("{bad} text pixels changed"),
                            )
                        });
                    }
                    if opts.wants("coverage") {
                        let now = Meta {
                            coverage: coverage_fraction(&frame, &out),
                            flat: largest_flat_fraction(&out, GOLDEN_SIZE.0, GOLDEN_SIZE.1),
                        };
                        let mp = meta_path(&gdir, &var.label);
                        if opts.update {
                            let _ = std::fs::create_dir_all(&gdir);
                            let _ = write_meta(&mp, now);
                            rows.push(row(
                                stem,
                                &var.label,
                                "coverage",
                                Status::Updated,
                                format!(
                                    "coverage {:.2}, largest flat block {:.3}",
                                    now.coverage, now.flat
                                ),
                            ));
                        } else if let Some(m) = read_meta(&mp) {
                            let cov_ok = (now.coverage - m.coverage).abs() <= COVERAGE_DELTA;
                            let flat_ok = now.flat <= m.flat + FLAT_DELTA;
                            let status = if cov_ok && flat_ok {
                                Status::Pass
                            } else {
                                Status::Fail
                            };
                            let mut d = format!(
                                "coverage {:.2} (accepted {:.2}), largest flat block {:.3} (accepted {:.3})",
                                now.coverage, m.coverage, now.flat, m.flat
                            );
                            if !cov_ok {
                                d.push_str(" - coverage changed");
                            }
                            if !flat_ok {
                                d.push_str(" - a flat block appeared");
                            }
                            rows.push(row(stem, &var.label, "coverage", status, d));
                        } else {
                            rows.push(row(
                                stem,
                                &var.label,
                                "coverage",
                                Status::Fail,
                                "no accepted values (run with --update)",
                            ));
                        }
                    }
                }
            }
        }
        if opts.wants("orient") && (var.label == "default" || var.label.starts_with("scene-")) {
            for (label, w, h) in ASPECTS {
                let check = format!("orient {label}");
                match render_at(gpu, src, &var, (w, h), GOLDEN_TIME, opts.origin) {
                    Err(e) => rows.push(row(stem, &var.label, &check, Status::Fail, e)),
                    Ok((aframe, out)) => {
                        let path = gdir.join(format!("{}@{label}.png", var.label));
                        rows.push(golden_row(
                            stem,
                            &var.label,
                            &check,
                            &path,
                            &out,
                            &aframe.rgba,
                            (w, h),
                            opts.update,
                        ));
                    }
                }
            }
        }
        if opts.wants("temporal") && (var.label == "default" || var.label.starts_with("scene-")) {
            let secs = if let Some(s) = opts.start_secs {
                s
            } else if opts.fast {
                8.0
            } else if var.label == "default" {
                125.0
            } else {
                40.0
            };
            rows.extend(temporal_rows(
                gpu,
                stem,
                src,
                &var,
                opts.origin,
                secs,
                !opts.fast,
            ));
        }
        if opts.wants("perf") && !opts.fast {
            let mut jobs: Vec<(String, (u32, u32), f32)> = Vec::new();
            if var.label == "default" || var.label.starts_with("scene-") {
                jobs.push((var.label.clone(), (1920, 1080), 0.0));
            }
            if var.label == "default" {
                jobs.push((var.label.clone(), (2560, 1440), 0.0));
                // a playlist's cross-fade renders two scenes at once: time the middle of one
                if let Ok(schema) = params::parse_schema(params::strip_header(src)) {
                    let num = |name: &str| {
                        schema
                            .params
                            .iter()
                            .find(|p| p.name == name)
                            .and_then(|p| p.default.parse::<f32>().ok())
                    };
                    if let (Some(period), Some(fade)) = (num("scene_period"), num("fade")) {
                        jobs.push(("crossfade".into(), (1920, 1080), period - fade / 2.0));
                    }
                }
            }
            for (label, size, t0) in jobs {
                let v = if label == "crossfade" {
                    Variant {
                        label: "default".into(),
                        preset: None,
                        sets: vec![],
                    }
                } else {
                    var.clone()
                };
                let key = format!("{stem}|{label}|{}x{}", size.0, size.1);
                rows.push(perf_row(
                    gpu,
                    stem,
                    src,
                    &v,
                    &label,
                    &key,
                    size,
                    t0,
                    opts,
                    baseline,
                    machine,
                    baseline_machine_ok,
                ));
            }
        }
    }
    rows
}

#[allow(clippy::too_many_arguments)]
fn perf_row(
    gpu: &Gpu,
    stem: &str,
    src: &str,
    var: &Variant,
    label: &str,
    key: &str,
    size: (u32, u32),
    t0: f32,
    opts: &Options,
    baseline: &mut Baseline,
    machine: &str,
    baseline_machine_ok: bool,
) -> Row {
    let first = match perf_of(gpu, src, var, opts.origin, size, t0) {
        Ok(p) => p,
        Err(e) => return row(stem, label, "perf", Status::Fail, e),
    };
    if opts.update {
        baseline
            .entries
            .insert(key.to_string(), (first.p50, first.p95, first.reference));
        return row(
            stem,
            label,
            "perf",
            Status::Updated,
            format!(
                "{}x{}: p50 {:.3} ms, p95 {:.3} ms (reference {:.3} ms)",
                size.0, size.1, first.p50, first.p95, first.reference
            ),
        );
    }
    if !baseline_machine_ok {
        return row(
            stem,
            label,
            "perf",
            Status::Skip,
            format!(
                "baseline is for {:?}, this machine is {machine:?}: not compared (p50 {:.3} ms)",
                baseline.machine, first.p50
            ),
        );
    }
    let Some(&b) = baseline.entries.get(key) else {
        return row(
            stem,
            label,
            "perf",
            Status::Fail,
            "no baseline entry (run with --update)",
        );
    };
    // a reading over the line is measured twice more, interleaved with the rest of the run, before it counts
    let mut best = first;
    let mut ok = perf_within(&first, b);
    for _ in 0..2 {
        if ok {
            break;
        }
        if let Ok(q) = perf_of(gpu, src, var, opts.origin, size, t0) {
            ok = perf_within(&q, b);
            if q.p50 < best.p50 {
                best = q;
            }
        }
    }
    row(
        stem,
        label,
        "perf",
        if ok { Status::Pass } else { Status::Fail },
        format!(
            "{}x{}: p50 {:.3} ms (baseline {:.3}), p95 {:.3} ms (baseline {:.3}), x{:.2} of the reference (baseline x{:.2}){}",
            size.0,
            size.1,
            best.p50,
            b.0,
            best.p95,
            b.1,
            best.p50 / best.reference.max(1e-9),
            b.0 / b.2.max(1e-9),
            if ok {
                ""
            } else {
                " - slower than the baseline"
            }
        ),
    )
}

/// Shaders under `paths` (a .glsl file, or a folder of them).
pub fn find_shaders(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut v = Vec::new();
    for p in paths {
        if p.is_dir() {
            if let Ok(rd) = std::fs::read_dir(p) {
                let mut files: Vec<PathBuf> = rd
                    .flatten()
                    .map(|e| e.path())
                    .filter(|f| f.extension().is_some_and(|x| x == "glsl"))
                    .collect();
                files.sort();
                v.extend(files);
            }
        } else {
            v.push(p.clone());
        }
    }
    v
}

/// The repository root above `path` (a folder with `.git`, `Cargo.toml` or `tests/golden`), else the current folder.
pub fn repo_root(path: &Path) -> PathBuf {
    let start = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let mut dir = if start.is_dir() {
        start.clone()
    } else {
        start
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or(start.clone())
    };
    loop {
        if dir.join(".git").exists() || dir.join("Cargo.toml").exists() {
            return dir;
        }
        if !dir.pop() {
            return PathBuf::from(".");
        }
    }
}

/// Run everything for `shaders`; returns the rows. Writes the baseline when updating.
pub fn run(gpu: &Gpu, shaders: &[PathBuf], opts: &Options) -> Vec<Row> {
    let machine = opts.machine.clone().unwrap_or_else(|| machine_id(gpu));
    let mut baseline = std::fs::read_to_string(&opts.baseline)
        .ok()
        .and_then(|t| Baseline::parse(&t))
        .unwrap_or_default();
    if opts.update {
        // a new baseline is for this machine; entries recorded elsewhere would not be comparable
        if baseline.machine != machine {
            baseline = Baseline {
                machine: machine.clone(),
                entries: BTreeMap::new(),
            };
        }
    }
    let mut rows = Vec::new();
    for path in shaders {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        match std::fs::read_to_string(path) {
            Ok(src) if src.contains("// regress: skip") => rows.push(row(
                &name,
                "-",
                "fixture",
                Status::Skip,
                "marked `// regress: skip`: a deliberately broken fixture",
            )),
            Ok(src) => rows.extend(run_shader(gpu, &name, &src, opts, &mut baseline, &machine)),
            Err(e) => rows.push(row(
                &name,
                "-",
                "read",
                Status::Fail,
                format!("{}: {e}", path.display()),
            )),
        }
    }
    if opts.update && opts.wants("perf") && !opts.fast {
        let _ = std::fs::write(&opts.baseline, baseline.to_json());
    }
    rows
}

/// A compact table plus the details of everything that did not pass. Returns (text, any failure).
pub fn render(rows: &[Row]) -> (String, bool) {
    let mut out = String::new();
    let mut shaders: Vec<&str> = rows.iter().map(|r| r.shader.as_str()).collect();
    shaders.dedup();
    let mut seen = std::collections::BTreeSet::new();
    shaders.retain(|s| seen.insert(*s));
    let mark = |s: Status| match s {
        Status::Pass => "ok",
        Status::Fail => "FAIL",
        Status::Skip => "skip",
        Status::Updated => "upd",
    };
    out.push_str(&format!(
        "{:<22} {:>5} {:>6} {:>6} {:>6} {:>6}  {:>6} {:>5}\n",
        "shader", "vars", "golden", "orient", "text", "cover", "motion", "perf"
    ));
    for s in &shaders {
        let mine: Vec<&Row> = rows.iter().filter(|r| r.shader == *s).collect();
        let vars: std::collections::BTreeSet<&str> =
            mine.iter().map(|r| r.variant.as_str()).collect();
        let cell = |pred: &dyn Fn(&Row) -> bool| -> String {
            let sel: Vec<&&Row> = mine.iter().filter(|r| pred(r)).collect();
            if sel.is_empty() {
                return "-".into();
            }
            let worst = if sel.iter().any(|r| r.status == Status::Fail) {
                Status::Fail
            } else if sel.iter().all(|r| r.status == Status::Skip) {
                Status::Skip
            } else if sel.iter().any(|r| r.status == Status::Updated) {
                Status::Updated
            } else {
                Status::Pass
            };
            mark(worst).into()
        };
        out.push_str(&format!(
            "{:<22} {:>5} {:>6} {:>6} {:>6} {:>6}  {:>6} {:>5}\n",
            s,
            vars.len(),
            cell(&|r| r.check == "golden"),
            cell(&|r| r.check.starts_with("orient")),
            cell(&|r| r.check == "text"),
            cell(&|r| r.check == "coverage"),
            cell(&|r| r.check == "temporal" || r.check == "seam"),
            cell(&|r| r.check == "perf"),
        ));
    }
    let failed = rows.iter().any(|r| r.status == Status::Fail);
    let bad: Vec<&Row> = rows
        .iter()
        .filter(|r| matches!(r.status, Status::Fail))
        .collect();
    if !bad.is_empty() {
        out.push('\n');
        for r in &bad {
            out.push_str(&format!(
                "FAIL {} [{}] {}: {}\n",
                r.shader, r.variant, r.check, r.detail
            ));
        }
    }
    let upd: Vec<&Row> = rows
        .iter()
        .filter(|r| r.status == Status::Updated && !r.detail.starts_with("unchanged"))
        .collect();
    if !upd.is_empty() {
        out.push('\n');
        for r in upd {
            out.push_str(&format!(
                "updated {} [{}] {}: {}\n",
                r.shader, r.variant, r.check, r.detail
            ));
        }
    }
    for r in rows.iter().filter(|r| r.status == Status::Skip) {
        out.push_str(&format!(
            "skipped {} [{}] {}: {}\n",
            r.shader, r.variant, r.check, r.detail
        ));
    }
    let n = |s: Status| rows.iter().filter(|r| r.status == s).count();
    out.push_str(&format!(
        "\n{} checks: {} ok, {} FAILED, {} skipped, {} updated\n",
        rows.len(),
        n(Status::Pass),
        n(Status::Fail),
        n(Status::Skip),
        n(Status::Updated)
    ));
    (out, failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_baseline_round_trips_through_its_json() {
        let mut b = Baseline {
            machine: "Apple M3 Max / macOS 26.0".into(),
            entries: BTreeMap::new(),
        };
        b.entries
            .insert("a|default|1920x1080".into(), (1.25, 1.5, 0.5));
        b.entries
            .insert("b|scene-3|1920x1080".into(), (3.0, 4.125, 0.75));
        let back = Baseline::parse(&b.to_json()).expect("parses");
        assert_eq!(back.machine, b.machine);
        assert_eq!(back.entries, b.entries);
        assert!(Baseline::parse("not json").is_none());
    }

    #[test]
    fn image_diff_sees_a_small_change_and_a_local_one() {
        let (w, h) = (64u32, 64u32);
        let a = vec![100u8; (w * h * 4) as usize];
        assert_eq!(image_diff(&a, &a, w, h), (0.0, 0.0));
        // everything 1 level brighter: a tiny mean, a tiny worst block
        let b: Vec<u8> = a.iter().map(|v| v + 1).collect();
        let (m, blk) = image_diff(&a, &b, w, h);
        assert!(m < 1.1 && blk < 1.1);
        // one 16x16 block 60 levels brighter: small mean, big worst block
        let mut c = a.clone();
        for y in 0..16 {
            for x in 0..16 {
                for ch in 0..3 {
                    c[((y * w + x) * 4 + ch) as usize] += 60;
                }
            }
        }
        let (m, blk) = image_diff(&a, &c, w, h);
        assert!(m < 5.0 && blk > 50.0, "{m} {blk}");
    }

    #[test]
    fn the_largest_flat_block_is_found() {
        let (w, h) = (20u32, 10u32);
        let mut img = vec![0u8; (w * h * 4) as usize];
        // a gradient: no two neighbours the same
        for i in 0..(w * h) as usize {
            img[i * 4] = (i * 3 % 250) as u8;
            img[i * 4 + 1] = (i * 7 % 250) as u8;
        }
        assert!(largest_flat_fraction(&img, w, h) < 0.05);
        // a 10x5 flat patch
        for y in 0..5 {
            for x in 0..10 {
                let i = ((y * w + x) * 4) as usize;
                img[i..i + 3].copy_from_slice(&[9, 9, 9]);
            }
        }
        assert!((largest_flat_fraction(&img, w, h) - 0.25).abs() < 0.01);
    }

    #[test]
    fn variants_cover_the_defaults_presets_and_scene_locks() {
        let src = "// @float scene 0 0 3 \"Scene\"\n// @preset calm scene=1\nvoid mainImage(out vec4 c, in vec2 f) { c = vec4(1.0); }\n";
        let labels: Vec<String> = variants_of(src).into_iter().map(|v| v.label).collect();
        assert_eq!(
            labels,
            vec!["default", "preset-calm", "scene-1", "scene-2", "scene-3"]
        );
        // a shader without annotations is just the defaults
        assert_eq!(
            variants_of("void mainImage(out vec4 c, in vec2 f) { c = vec4(1.0); }").len(),
            1
        );
    }

    #[test]
    fn the_table_marks_failures_and_lists_their_details() {
        let rows = vec![
            row("a", "default", "golden", Status::Pass, "ok"),
            row(
                "a",
                "default",
                "perf",
                Status::Fail,
                "p50 9 ms (baseline 1)",
            ),
            row("b", "default", "golden", Status::Skip, "other machine"),
        ];
        let (text, failed) = render(&rows);
        assert!(failed);
        assert!(text.contains("FAIL a [default] perf: p50 9 ms"), "{text}");
        assert!(text.contains("skipped b"), "{text}");
        let (_, failed) = render(&rows[..1]);
        assert!(!failed);
    }
}
