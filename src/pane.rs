//! `shaderlab pane`: the shader fills the WHOLE terminal pane, like a living background to leave running in a split.
//!
//! No side panel, no footer, no border: the picture, and (only on request) one line of stats. Smoothness is the point:
//!
//! * frames are drawn straight to RGBA8 and read back through a ring of staging buffers without ever waiting for the frame just
//!   submitted ([`crate::gpu::Streamer`]); a finished frame is written to a file the terminal reads (`t=t`) and placed in one
//!   synchronized update, replacing the same image and placement id in place;
//! * a fixed-step clock drops frames it cannot keep up with, and never answers a late frame with a burst;
//! * [`Quality`] watches the 95th-percentile frame cost and lowers the render scale, then the frame rate, when the pane cannot keep
//!   up, and raises them again when there is headroom;
//! * with `--pause-unfocused` nothing is rendered while the pane or window is not focused.

use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use ratatui::crossterm::event::{
    self, DisableFocusChange, EnableFocusChange, Event, KeyCode, KeyEventKind, KeyModifiers,
};
use ratatui::crossterm::execute;
use ratatui::layout::Rect;
use ratatui::widgets::Clear;

use crate::frame::Frame;
use crate::gpu::{self, Gpu, Origin, Prepared, Streamer};
use crate::params;
use crate::preview::{FileWatcher, PreviewState};
use crate::termimg::{self, Protocol};
use crate::tui::{HalfBlocks, Transfer};

/// The picture is never rendered larger than this (long side, and total pixels): the terminal scales it to the pane.
pub const MAX_SIDE: u32 = 2560;
pub const MAX_PIXELS: u32 = 3_000_000;
/// What `--scale auto` starts from: about a 1080p picture.
pub const AUTO_START_PIXELS: u32 = 2_100_000;
const KITTY_ID: u32 = 4243;
const RING: usize = 3;
const FILE_SLOTS: usize = 3;

/// `--scale`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ScaleMode {
    /// start near 1080p and let [`Quality`] adapt
    Auto,
    /// a fixed fraction of the pane's pixels
    Fixed(f32),
}

pub struct PaneOptions {
    pub file: PathBuf,
    pub preset: Option<String>,
    pub sets: Vec<String>,
    pub fps: u32,
    pub scale: ScaleMode,
    pub protocol: Option<Protocol>,
    pub transfer: Transfer,
    /// `none` (the default: background only) or `sample` (a sample terminal as iChannel0)
    pub text: String,
    pub origin: Origin,
    pub stats: bool,
    pub pause_unfocused: bool,
    /// seconds after which `iTime` starts again from 0 (keeps float precision in a pane left running for days); 0 = never
    pub time_wrap: f32,
    /// append one line per second (time, fps, p95, scale, dropped, bytes, rss) to this file
    pub log: Option<PathBuf>,
}

// ---- sizes -------------------------------------------------------------------------------------------

/// The size of the picture to render for a pane of `cols` x `rows` cells, with `scale` applied (1.0 = the pane's real pixels).
/// Never below 16 px, never above [`MAX_SIDE`] / [`MAX_PIXELS`], always even, and always the pane's aspect ratio.
pub fn render_size(cols: u16, rows: u16, cell_px: (f32, f32), scale: f32) -> (u32, u32) {
    let pw = cols.max(1) as f32 * cell_px.0.max(1.0);
    let ph = rows.max(1) as f32 * cell_px.1.max(1.0);
    let mut k = scale.clamp(0.05, 1.0);
    k = k.min(MAX_SIDE as f32 / pw.max(ph));
    k = k.min((MAX_PIXELS as f32 / (pw * ph)).sqrt());
    let w = ((pw * k).round() as u32).max(16) & !1;
    let h = ((ph * k).round() as u32).max(16) & !1;
    (w, h)
}

/// The scale `--scale auto` starts from: 1.0 for a pane up to about 1080p, smaller for a bigger one.
pub fn auto_start_scale(cols: u16, rows: u16, cell_px: (f32, f32)) -> f32 {
    let area = cols.max(1) as f32 * cell_px.0 * rows.max(1) as f32 * cell_px.1;
    (AUTO_START_PIXELS as f32 / area).sqrt().min(1.0)
}

// ---- adaptive quality -------------------------------------------------------------------------------

/// The render scales the controller steps through, best first.
pub const SCALES: [f32; 4] = [1.0, 0.75, 0.5, 0.35];

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Change {
    Scale(f32),
    Fps(u32),
}

/// Lowers the render scale, then the frame rate, when the 95th-percentile frame cost goes over the budget, and raises them again
/// (frame rate first) after a stretch of clear headroom. Pure: it is told the cost of each frame and the time, and answers with
/// at most one change; it never touches the GPU.
#[derive(Debug)]
pub struct Quality {
    adaptive: bool,
    /// index into [`SCALES`] the controller has settled on, relative to `base_scale`
    scale_i: usize,
    base_scale: f32,
    fps_levels: Vec<u32>,
    fps_i: usize,
    window: VecDeque<f32>,
    last_change: f64,
    good_since: Option<f64>,
}

const WINDOW: usize = 90;
const MIN_SAMPLES: usize = 30;

impl Quality {
    pub fn new(requested_fps: u32, base_scale: f32, adaptive: bool) -> Quality {
        let f = requested_fps.clamp(1, 120);
        let mut fps_levels = vec![f];
        for k in [0.8, 0.6, 0.5] {
            let v = ((f as f32 * k).round() as u32).max(10);
            if v < *fps_levels.last().expect("never empty") {
                fps_levels.push(v);
            }
        }
        Quality {
            adaptive,
            scale_i: 0,
            base_scale,
            fps_levels,
            fps_i: 0,
            window: VecDeque::new(),
            last_change: f64::NEG_INFINITY,
            good_since: None,
        }
    }

    pub fn scale(&self) -> f32 {
        self.base_scale * SCALES[self.scale_i]
    }

    pub fn fps(&self) -> u32 {
        self.fps_levels[self.fps_i]
    }

    pub fn budget_ms(&self) -> f32 {
        1000.0 / self.fps() as f32
    }

    /// The 95th percentile of the recent frame costs, in ms (0 with no samples).
    pub fn p95(&self) -> f32 {
        if self.window.is_empty() {
            return 0.0;
        }
        let mut v: Vec<f32> = self.window.iter().copied().collect();
        v.sort_by(|a, b| a.partial_cmp(b).expect("no NaN costs"));
        v[((v.len() - 1) as f32 * 0.95).round() as usize]
    }

    fn reset(&mut self, now: f64) {
        self.window.clear();
        self.last_change = now;
        self.good_since = None;
    }

    /// Record the cost of one frame (ms from starting it to having it on screen's way) at time `now` (seconds).
    pub fn record(&mut self, cost_ms: f32, now: f64) -> Option<Change> {
        if !cost_ms.is_finite() {
            return None;
        }
        self.window.push_back(cost_ms);
        while self.window.len() > WINDOW {
            self.window.pop_front();
        }
        if !self.adaptive || self.window.len() < MIN_SAMPLES {
            return None;
        }
        let p95 = self.p95();
        let budget = self.budget_ms();
        // over budget: a step down, at most every 3 s
        if p95 > 0.85 * budget && now - self.last_change >= 3.0 {
            if self.scale_i + 1 < SCALES.len() {
                self.scale_i += 1;
                self.reset(now);
                return Some(Change::Scale(self.scale()));
            }
            if self.fps_i + 1 < self.fps_levels.len() {
                self.fps_i += 1;
                self.reset(now);
                return Some(Change::Fps(self.fps()));
            }
            return None;
        }
        // clear headroom for 10 s: a step back up (the frame rate first), at most every 10 s. The cost grows with the pixels, so
        // the next scale up must still fit: cost x (pixel ratio) well under the budget.
        let (up_scale, up_fps) = (self.scale_i > 0, self.fps_i > 0);
        if !(up_scale || up_fps) {
            return None;
        }
        let ratio = if up_fps {
            self.budget_ms() / (1000.0 / self.fps_levels[self.fps_i - 1] as f32)
        } else {
            let (a, b) = (SCALES[self.scale_i - 1], SCALES[self.scale_i]);
            (a / b) * (a / b)
        };
        let fits = if up_fps {
            p95 < 0.55 * (1000.0 / self.fps_levels[self.fps_i - 1] as f32)
        } else {
            p95 * ratio < 0.6 * budget
        };
        if fits {
            let since = *self.good_since.get_or_insert(now);
            if now - since >= 10.0 && now - self.last_change >= 10.0 {
                if up_fps {
                    self.fps_i -= 1;
                    self.reset(now);
                    return Some(Change::Fps(self.fps()));
                }
                self.scale_i -= 1;
                self.reset(now);
                return Some(Change::Scale(self.scale()));
            }
        } else {
            self.good_since = None;
        }
        None
    }
}

// ---- the terminal sequences --------------------------------------------------------------------------

/// Where frame `slot` is written: `t=t` files, a small ring reused (the terminal deletes each as it reads it).
fn frame_path(slot: usize) -> String {
    termimg::temp_frame_path(std::process::id(), slot)
}

/// The kitty sequence that puts a raw RGBA file (`f=32`) over the whole pane: image id and placement id fixed, below the text.
pub fn kitty_pane_file(path: &str, w: u32, h: u32, cols: u16, rows: u16) -> Vec<u8> {
    format!(
        "\x1b_Ga=T,f=32,t=t,s={w},v={h},i={KITTY_ID},p={},c={cols},r={rows},z=-1,C=1,q=2;{}\x1b\\",
        termimg::PLACEMENT,
        termimg::base64(path.as_bytes())
    )
    .into_bytes()
}

/// Remove frame files left by a pane that was killed (older than two minutes, so a pane running in another split is left alone).
pub fn clean_stale_files() -> usize {
    let mut removed = 0;
    let Ok(dir) = std::fs::read_dir(std::env::temp_dir()) else {
        return 0;
    };
    for e in dir.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !(name.starts_with("tty-graphics-protocol-shaderlab-") && name.ends_with(".rgb")) {
            continue;
        }
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > Duration::from_secs(120));
        if old && std::fs::remove_file(e.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

// ---- running -----------------------------------------------------------------------------------------

struct Pane {
    gpu: Gpu,
    state: PreviewState,
    src: String,
    opts: PaneOptions,
    proto: Protocol,
    use_file: bool,
    quality: Quality,
    watcher: FileWatcher,
    prepared: Option<Prepared>,
    streamer: Option<Streamer>,
    /// submit times of the frames in flight, oldest first
    submitted: VecDeque<Instant>,
    cells: (u16, u16),
    cell_px: (f32, f32),
    size: (u32, u32),
    frame_no: i32,
    slot: usize,
    shown: bool,
    // counters
    emitted: u64,
    attempted: bool,
    last_note: Instant,
    dropped: u64,
    bytes: usize,
    quit: bool,
    focused: bool,
    help_until: Option<Instant>,
    save_next: bool,
    message: Option<(String, Instant)>,
    fps_meter: (Instant, u64, f32),
    error: Option<String>,
    /// for the non-kitty fallback: the last frame as RGBA8
    last_rgba: Vec<u8>,
    t0: Instant,
    last_log: Instant,
    last_rss: u64,
}

impl Pane {
    fn frame(&self, size: (u32, u32)) -> Frame {
        if self.opts.text == "sample" {
            Frame::sample(size.0, size.1)
        } else {
            Frame::sample(size.0, size.1).without_text()
        }
    }

    /// (Re)build the pipeline and the readback ring for `size`. On failure the previous pipeline keeps running.
    fn rebuild(&mut self, size: (u32, u32)) {
        let frame = self.frame(size);
        let preset = self.state.preset_name().map(str::to_string);
        match gpu::prepare_shader(
            &self.gpu,
            &self.src,
            preset.as_deref(),
            &self.state.sets,
            &frame,
            self.opts.origin,
        ) {
            Ok(p) => {
                self.size = (frame.width, frame.height);
                if self.proto == Protocol::Kitty {
                    let st = p.streamer(&self.gpu, RING);
                    self.bytes = st.frame_bytes();
                    self.streamer = Some(st);
                }
                self.prepared = Some(p);
                self.submitted.clear();
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
    }

    fn target_size(&self) -> (u32, u32) {
        match self.proto {
            Protocol::Kitty => render_size(
                self.cells.0,
                self.cells.1,
                self.cell_px,
                self.quality.scale(),
            ),
            // the fallbacks send a small picture: as many pixels as the protocol can carry
            p => {
                let r = Rect::new(0, 0, self.cells.0, self.cells.1);
                crate::tui::pixel_size(p, r, self.cell_px, false)
            }
        }
    }

    fn name(&self) -> String {
        self.opts
            .file
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "shader".into())
    }

    fn say(&mut self, m: impl Into<String>) {
        self.message = Some((m.into(), Instant::now() + Duration::from_secs(3)));
    }

    fn change_preset(&mut self, dir: i32) {
        let n = self.state.schema.presets.len();
        if n == 0 {
            self.say("this shader has no presets");
            return;
        }
        // None (defaults) sits between the last and the first preset
        let cur = self.state.preset.map(|i| i as i32).unwrap_or(-1);
        let next = (cur + dir).rem_euclid(n as i32 + 1) - 1;
        self.state
            .pick_preset(if next < 0 { None } else { Some(next as usize) });
        let name = self.state.preset_name().unwrap_or("defaults").to_string();
        self.say(format!("preset: {name}"));
        let size = self.size;
        self.rebuild(size);
    }

    fn on_event(&mut self, ev: Event) {
        match ev {
            Event::Key(k) if k.kind == KeyEventKind::Press => match k.code {
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.quit = true
                }
                KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => self.quit = true,
                KeyCode::Char('p') | KeyCode::Char(' ') => {
                    self.state.paused = !self.state.paused;
                    let m = if self.state.paused {
                        "paused"
                    } else {
                        "running"
                    };
                    self.say(m);
                }
                KeyCode::Char('n') => self.change_preset(1),
                KeyCode::Char('N') => self.change_preset(-1),
                KeyCode::Char('s') | KeyCode::Char('S') => self.save_next = true,
                KeyCode::Char('?') => {
                    self.help_until = Some(Instant::now() + Duration::from_secs(3))
                }
                _ => {}
            },
            Event::FocusGained => self.focused = true,
            Event::FocusLost => self.focused = false,
            _ => {}
        }
    }

    fn save_png(&mut self, rgba: Vec<u8>) {
        let (w, h) = self.size;
        let home = crate::home::Home::from_env();
        let stem = crate::home::render_stem(
            &self.name(),
            self.state.preset_name(),
            (w, h),
            self.state.time,
        );
        match home.out_path(crate::home::OutKind::Render, &stem, "png") {
            Ok(path) => match image::save_buffer(&path, &rgba, w, h, image::ColorType::Rgba8) {
                Ok(()) => self.say(format!("saved {}", path.display())),
                Err(e) => self.say(format!("could not save: {e}")),
            },
            Err(e) => self.say(format!("could not save: {e}")),
        }
    }

    /// Write one finished kitty frame to the terminal, in one synchronized update.
    fn emit_kitty(&mut self, out: &mut impl Write) -> std::io::Result<f32> {
        let (w, h) = self.size;
        let want_png = std::mem::take(&mut self.save_next);
        let t0 = Instant::now();
        let path = frame_path(self.slot);
        self.slot = (self.slot + 1) % FILE_SLOTS;
        let use_file = self.use_file;
        let (mut copy, mut seq) = (Vec::new(), Vec::new());
        let Some(streamer) = self.streamer.as_mut() else {
            return Ok(0.0);
        };
        let r = streamer.take(|rows| -> std::io::Result<()> {
            if use_file {
                let mut f = std::fs::File::create(&path)?;
                let mut buf = std::io::BufWriter::with_capacity(1 << 20, &mut f);
                for row in rows {
                    buf.write_all(row)?;
                    if want_png {
                        copy.extend_from_slice(row);
                    }
                }
                buf.flush()?;
            } else {
                let mut raw = Vec::with_capacity((w * h * 4) as usize);
                for row in rows {
                    raw.extend_from_slice(row);
                }
                seq = raw;
            }
            Ok(())
        });
        let Some(res) = r else {
            return Ok(0.0);
        };
        res?;
        // the frames in flight now are those the streamer still holds; the rest were taken or dropped
        let newest = loop {
            let pending = self.streamer.as_ref().map_or(0, Streamer::pending);
            if self.submitted.len() > pending {
                let t = self.submitted.pop_front();
                if self.submitted.len() == pending {
                    break t;
                }
            } else {
                break None;
            }
        };
        let gpu_ms = newest.map_or(0.0, |t| {
            t0.saturating_duration_since(t).as_secs_f32() * 1000.0
        });
        let (cols, rows) = self.cells;
        out.write_all(termimg::SYNC_BEGIN)?;
        out.write_all(b"\x1b7\x1b[1;1H")?;
        if use_file {
            out.write_all(&kitty_pane_file(&path, w, h, cols, rows))?;
        } else {
            // base64 over the pty (a remote terminal): zlib-compressed RGB
            let rgb = termimg::rgba_to_rgb(&seq);
            out.write_all(&termimg::kitty_image(&rgb, w, h, cols, rows, KITTY_ID, -1))?;
        }
        out.write_all(b"\x1b8")?;
        self.draw_overlay(out)?;
        out.write_all(termimg::SYNC_END)?;
        out.flush()?;
        self.shown = true;
        self.emitted += 1;
        if want_png {
            self.save_png(copy);
        }
        let write_ms = t0.elapsed().as_secs_f32() * 1000.0;
        // the cost of a frame: the GPU and the wait for it, plus getting it to the terminal
        Ok(gpu_ms + write_ms)
    }

    /// The optional stats line and the `?` overlay, drawn with plain escape sequences over the picture (the image is below the text).
    fn draw_overlay(&mut self, out: &mut impl Write) -> std::io::Result<()> {
        let now = Instant::now();
        let mut lines: Vec<String> = Vec::new();
        if let Some((m, until)) = &self.message {
            if now < *until {
                lines.push(m.clone());
            } else {
                self.message = None;
            }
        }
        if self.help_until.is_some_and(|u| now < u) {
            lines.push("q quit   p pause   n/N preset   s save PNG   ? help".into());
        }
        if self.opts.stats {
            lines.push(self.stats_line());
        }
        if let Some(e) = &self.error {
            lines.push(format!("shader error: {}", e.lines().next().unwrap_or("")));
        }
        if lines.is_empty() {
            return Ok(());
        }
        out.write_all(b"\x1b7")?;
        for (i, l) in lines.iter().enumerate() {
            let text: String = l
                .chars()
                .take(self.cells.0.saturating_sub(2) as usize)
                .collect();
            // white on a dark chip so it reads over any picture
            write!(out, "\x1b[{};1H\x1b[0;97;40m {text} \x1b[0m", i + 1)?;
        }
        out.write_all(b"\x1b8")
    }

    fn stats_line(&self) -> String {
        format!(
            "{}  {}x{}  scale {:.2}  {:.0}/{} fps  p95 {:.1} ms  dropped {}  {} KB/frame",
            self.name(),
            self.size.0,
            self.size.1,
            self.quality.scale(),
            self.fps_meter.2,
            self.quality.fps(),
            self.quality.p95(),
            self.dropped,
            self.bytes / 1024
        )
    }
}

fn rss_kb() -> u64 {
    std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
        .unwrap_or(0)
}

/// Run the pane until the user quits. The terminal is restored on every exit path, including a panic.
pub fn run(opts: PaneOptions) -> Result<()> {
    let src = std::fs::read_to_string(&opts.file)
        .with_context(|| format!("reading {}", opts.file.display()))?;
    let schema = params::parse_schema(params::strip_header(&src)).map_err(|e| anyhow!(e))?;
    let state = PreviewState::new(schema, opts.preset.as_deref(), opts.sets.clone())
        .map_err(|e| anyhow!(e))?;
    let gpu = Gpu::new()?;
    let (proto, _note) = match opts.protocol {
        Some(p) => (p, None),
        None => termimg::detect(&|k| std::env::var(k).ok()),
    };
    let local = termimg::is_local(&|k| std::env::var(k).ok());
    let use_file = proto == Protocol::Kitty
        && cfg!(unix)
        && match opts.transfer {
            Transfer::File => true,
            Transfer::Direct => false,
            Transfer::Auto => local,
        };
    clean_stale_files();
    let fps = opts.fps.clamp(1, 60);
    let mut terminal = ratatui::init();
    let area = terminal.size()?;
    let cell_px = cell_pixels();
    let base = match opts.scale {
        ScaleMode::Auto => auto_start_scale(area.width, area.height, cell_px),
        ScaleMode::Fixed(s) => s,
    };
    let adaptive = matches!(opts.scale, ScaleMode::Auto);
    let mut pane = Pane {
        watcher: FileWatcher::new(&opts.file),
        gpu,
        state,
        src,
        proto,
        use_file,
        quality: Quality::new(fps, base, adaptive),
        prepared: None,
        streamer: None,
        submitted: VecDeque::new(),
        cells: (area.width, area.height),
        cell_px,
        size: (0, 0),
        frame_no: 0,
        slot: 0,
        shown: false,
        emitted: 0,
        attempted: false,
        last_note: Instant::now(),
        dropped: 0,
        bytes: 0,
        quit: false,
        focused: true,
        help_until: None,
        save_next: false,
        message: None,
        fps_meter: (Instant::now(), 0, 0.0),
        error: None,
        last_rgba: Vec::new(),
        t0: Instant::now(),
        last_log: Instant::now(),
        last_rss: 0,
        opts,
    };
    let _ = execute!(std::io::stdout(), EnableFocusChange);
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore(proto);
        previous(info);
    }));
    let result = event_loop(&mut terminal, &mut pane);
    let _ = restore(proto);
    ratatui::restore();
    for slot in 0..FILE_SLOTS {
        let _ = std::fs::remove_file(frame_path(slot));
    }
    result
}

fn restore(proto: Protocol) -> std::io::Result<()> {
    let mut out = std::io::stdout();
    if proto == Protocol::Kitty {
        out.write_all(&termimg::kitty_delete(KITTY_ID))?;
    }
    execute!(out, DisableFocusChange, ratatui::crossterm::cursor::Show)
}

fn cell_pixels() -> (f32, f32) {
    match ratatui::crossterm::terminal::window_size() {
        Ok(ws) if ws.width > 0 && ws.height > 0 && ws.columns > 0 && ws.rows > 0 => (
            ws.width as f32 / ws.columns as f32,
            ws.height as f32 / ws.rows as f32,
        ),
        _ => (8.0, 16.0),
    }
}

fn event_loop(terminal: &mut ratatui::DefaultTerminal, pane: &mut Pane) -> Result<()> {
    let mut interval = Duration::from_secs_f64(1.0 / pane.quality.fps() as f64);
    let mut next_frame = Instant::now();
    let mut last = Instant::now();
    let mut last_check = Instant::now();
    // the picture is drawn under the (empty) text layer; start from a clean screen with the cursor hidden
    terminal.clear()?;
    terminal.backend_mut().write_all(termimg::SYNC_BEGIN)?;
    terminal.backend_mut().write_all(termimg::SYNC_END)?;
    while !pane.quit {
        // wait for input, but never longer than the next frame, and wake early to collect a finished frame
        let now = Instant::now();
        let until = next_frame.saturating_duration_since(now);
        let wait = if pane.streamer.as_ref().is_some_and(|s| s.pending() > 0) {
            until.min(Duration::from_millis(1))
        } else if !pane.focused && pane.opts.pause_unfocused {
            Duration::from_millis(250)
        } else {
            until
        };
        if event::poll(wait)? {
            while event::poll(Duration::ZERO)? {
                let ev = event::read()?;
                if let Event::Resize(..) = ev {
                    continue;
                }
                pane.on_event(ev);
            }
        }
        if pane.quit {
            break;
        }
        let mut out = std::io::stdout().lock();
        // size changes (a resize, a font change)
        let area = terminal.size()?;
        if (area.width, area.height) != pane.cells || !pane.attempted {
            pane.attempted = true;
            pane.cells = (area.width.max(1), area.height.max(1));
            pane.cell_px = cell_pixels();
            // no flash: the old picture stays until the first frame of the new size replaces it in place
            terminal.clear()?;
            pane.shown = false;
            let size = pane.target_size();
            pane.rebuild(size);
        }
        if last_check.elapsed() > Duration::from_millis(250) {
            last_check = Instant::now();
            if pane.watcher.changed() {
                match std::fs::read_to_string(&pane.opts.file) {
                    Ok(src) => {
                        pane.src = src;
                        let size = pane.size;
                        pane.rebuild(size);
                    }
                    Err(e) => pane.error = Some(e.to_string()),
                }
            }
            // a different quality level means a different size
            let want = pane.target_size();
            if want != pane.size && pane.prepared.is_some() {
                pane.rebuild(want);
            }
        }
        // collect a finished frame
        if pane.proto == Protocol::Kitty {
            if let Some(s) = pane.streamer.as_ref() {
                s.poll(&pane.gpu);
            }
            if pane.streamer.as_ref().is_some_and(|s| s.pending() > 0) {
                let cost = pane.emit_kitty(&mut out)?;
                if cost > 0.0 {
                    let now_s = pane.t0.elapsed().as_secs_f64();
                    pane.fps_meter.1 += 1;
                    match pane.quality.record(cost, now_s) {
                        Some(Change::Scale(_)) => {}
                        Some(Change::Fps(f)) => {
                            interval = Duration::from_secs_f64(1.0 / f as f64);
                            pane.say(format!("{f} fps"));
                        }
                        None => {}
                    }
                }
            }
        }
        // with no picture to carry them, the message and the error are drawn on their own twice a second
        if pane.prepared.is_none() && pane.last_note.elapsed() > Duration::from_millis(500) {
            pane.last_note = Instant::now();
            out.write_all(termimg::SYNC_BEGIN)?;
            pane.draw_overlay(&mut out)?;
            out.write_all(termimg::SYNC_END)?;
            out.flush()?;
        }
        // the next tick
        let now = Instant::now();
        if now < next_frame {
            continue;
        }
        let behind = now - next_frame;
        if behind >= interval {
            pane.dropped += (behind.as_secs_f64() / interval.as_secs_f64()) as u64;
            next_frame = now;
        }
        next_frame += interval;
        let running = pane.focused || !pane.opts.pause_unfocused;
        let dt = (now - last).as_secs_f32().min(0.1);
        last = now;
        if !running {
            continue;
        }
        pane.state.advance(dt);
        let mut clock = pane.state.time;
        if pane.opts.time_wrap > 0.0 && clock > pane.opts.time_wrap {
            pane.state.time -= pane.opts.time_wrap * (clock / pane.opts.time_wrap).floor();
            clock = pane.state.time;
        }
        let Some(p) = pane.prepared.as_ref() else {
            continue;
        };
        pane.frame_no = pane.frame_no.wrapping_add(1);
        match pane.proto {
            Protocol::Kitty => {
                if let Some(s) = pane.streamer.as_mut() {
                    if s.submit(p, &pane.gpu, clock, dt.max(1e-4), pane.frame_no) {
                        pane.submitted.push_back(Instant::now());
                    } else {
                        pane.dropped += 1;
                    }
                }
            }
            other => {
                // the fallbacks: a small picture, drawn and sent in one go
                let mut rgba = std::mem::take(&mut pane.last_rgba);
                let t0 = Instant::now();
                if p.draw_rgba8(&pane.gpu, clock, dt.max(1e-4), pane.frame_no, &mut rgba)
                    .is_ok()
                {
                    let (w, h) = pane.size;
                    present_fallback(terminal, other, &rgba, w, h)?;
                    pane.emitted += 1;
                    pane.fps_meter.1 += 1;
                    let cost = t0.elapsed().as_secs_f32() * 1000.0;
                    let now_s = pane.t0.elapsed().as_secs_f64();
                    if let Some(Change::Fps(f)) = pane.quality.record(cost, now_s) {
                        interval = Duration::from_secs_f64(1.0 / f as f64);
                    }
                }
                pane.last_rgba = rgba;
            }
        }
        // the frame meter and the log
        if pane.fps_meter.0.elapsed() >= Duration::from_millis(1000) {
            let secs = pane.fps_meter.0.elapsed().as_secs_f32();
            pane.fps_meter.2 = pane.fps_meter.1 as f32 / secs;
            pane.fps_meter = (Instant::now(), 0, pane.fps_meter.2);
            log_line(pane);
        }
    }
    Ok(())
}

fn present_fallback(
    terminal: &mut ratatui::DefaultTerminal,
    proto: Protocol,
    rgba: &[u8],
    w: u32,
    h: u32,
) -> Result<()> {
    match proto {
        Protocol::Sixel => {
            let rgb = termimg::rgba_to_rgb(rgba);
            let out = terminal.backend_mut();
            out.write_all(termimg::SYNC_BEGIN)?;
            out.write_all(b"\x1b7\x1b[1;1H")?;
            out.write_all(&termimg::sixel(&rgb, w, h))?;
            out.write_all(b"\x1b8")?;
            out.write_all(termimg::SYNC_END)?;
            out.flush()?;
        }
        _ => {
            terminal.draw(|f| {
                let area = f.area();
                f.render_widget(Clear, area);
                f.render_widget(
                    HalfBlocks {
                        rgba,
                        width: w,
                        height: h,
                        truecolor: true,
                    },
                    area,
                );
            })?;
        }
    }
    Ok(())
}

fn log_line(pane: &mut Pane) {
    let Some(path) = pane.opts.log.clone() else {
        return;
    };
    if pane.last_log.elapsed() < Duration::from_secs(1) {
        return;
    }
    pane.last_log = Instant::now();
    if pane.t0.elapsed().as_secs().is_multiple_of(10) || pane.last_rss == 0 {
        pane.last_rss = rss_kb();
    }
    let line = format!(
        "{:.0},{:.1},{:.2},{:.2},{},{},{},{}\n",
        pane.t0.elapsed().as_secs_f32(),
        pane.fps_meter.2,
        pane.quality.p95(),
        pane.quality.scale(),
        pane.quality.fps(),
        pane.dropped,
        pane.bytes,
        pane.last_rss
    );
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = f.write_all(line.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_render_size_keeps_the_pane_shape_and_stays_bounded() {
        // a 120x40 pane of 18x36 px cells is 2160x1440
        let (w, h) = render_size(120, 40, (18.0, 36.0), 1.0);
        assert!(w * h <= MAX_PIXELS, "{w}x{h}");
        assert!(w.max(h) <= MAX_SIDE);
        let pane = 2160.0 / 1440.0;
        assert!(((w as f32 / h as f32) - pane).abs() < 0.02, "{w}x{h}");
        assert_eq!((w % 2, h % 2), (0, 0));
        // half the scale is about a quarter of the pixels
        let (w2, h2) = render_size(120, 40, (18.0, 36.0), 0.5);
        assert!(((w2 * h2) as f32) < 0.3 * (w * h) as f32);
        // wide, tall and tiny panes
        for (c, r) in [(300u16, 10u16), (10, 100), (1, 1), (0, 0), (500, 200)] {
            let (w, h) = render_size(c, r, (8.0, 16.0), 1.0);
            assert!(
                w >= 16 && h >= 16 && w * h <= MAX_PIXELS && w.max(h) <= MAX_SIDE,
                "{c}x{r} -> {w}x{h}"
            );
        }
        // cells with no known pixel size never divide by zero
        assert!(render_size(80, 24, (0.0, 0.0), 1.0).0 >= 16);
    }

    #[test]
    fn auto_starts_at_about_1080p_for_a_big_pane_and_full_for_a_small_one() {
        let big = auto_start_scale(300, 90, (10.0, 21.0)); // 3000x1890
        assert!(big < 0.9 && big > 0.5, "{big}");
        assert_eq!(auto_start_scale(60, 20, (8.0, 16.0)), 1.0);
    }

    fn feed(q: &mut Quality, ms: f32, from: f64, secs: f64, per_sec: u32) -> Vec<(f64, Change)> {
        let mut changes = Vec::new();
        let n = (secs * per_sec as f64) as u32;
        for i in 0..n {
            let now = from + i as f64 / per_sec as f64;
            if let Some(c) = q.record(ms, now) {
                changes.push((now, c));
            }
        }
        changes
    }

    #[test]
    fn a_pane_that_cannot_keep_up_steps_the_scale_down_then_the_frame_rate() {
        let mut q = Quality::new(30, 1.0, true);
        // 30 fps = 33 ms budget; frames cost 60 ms
        let ch = feed(&mut q, 60.0, 0.0, 60.0, 30);
        let scales: Vec<f32> = ch
            .iter()
            .filter_map(|(_, c)| {
                if let Change::Scale(s) = c {
                    Some(*s)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(scales, vec![0.75, 0.5, 0.35], "{ch:?}");
        let fps: Vec<u32> = ch
            .iter()
            .filter_map(|(_, c)| {
                if let Change::Fps(f) = c {
                    Some(*f)
                } else {
                    None
                }
            })
            .collect();
        assert!(!fps.is_empty() && fps[0] < 30, "{ch:?}");
        // never more often than every 3 s
        for w in ch.windows(2) {
            assert!(w[1].0 - w[0].0 >= 3.0, "{ch:?}");
        }
    }

    #[test]
    fn a_pane_with_headroom_stays_put_and_one_that_recovers_climbs_back() {
        let mut q = Quality::new(30, 1.0, true);
        assert!(feed(&mut q, 5.0, 0.0, 60.0, 30).is_empty());
        assert_eq!((q.scale(), q.fps()), (1.0, 30));
        // overloaded for a while, then fine
        let down = feed(&mut q, 60.0, 100.0, 12.0, 30);
        assert!(!down.is_empty());
        let after_down = q.scale();
        assert!(after_down < 1.0);
        let up = feed(&mut q, 3.0, 120.0, 60.0, 30);
        assert!(!up.is_empty(), "never climbed back");
        assert!(q.scale() > after_down || q.fps() == 30);
        // and it climbs one step at a time, no faster than every 10 s
        for w in up.windows(2) {
            assert!(w[1].0 - w[0].0 >= 10.0, "{up:?}");
        }
    }

    #[test]
    fn a_fixed_scale_never_adapts() {
        let mut q = Quality::new(30, 0.5, false);
        assert!(feed(&mut q, 200.0, 0.0, 60.0, 30).is_empty());
        assert_eq!(q.scale(), 0.5);
        assert!(q.p95() > 100.0);
    }

    #[test]
    fn a_single_slow_frame_does_not_change_anything() {
        let mut q = Quality::new(30, 1.0, true);
        feed(&mut q, 8.0, 0.0, 10.0, 30);
        assert!(q.record(400.0, 10.0).is_none());
        assert_eq!(q.scale(), 1.0);
        assert!(q.p95() < 20.0);
    }

    #[test]
    fn the_kitty_sequence_names_one_image_and_one_placement_over_the_whole_pane() {
        let s = String::from_utf8(kitty_pane_file(
            "/tmp/x/tty-graphics-protocol-shaderlab-1-0.rgb",
            640,
            360,
            100,
            30,
        ))
        .unwrap();
        assert!(
            s.starts_with("\x1b_Ga=T,f=32,t=t,s=640,v=360,i=4243,p=1,c=100,r=30,z=-1,C=1,q=2;"),
            "{s}"
        );
        assert!(s.ends_with("\x1b\\"));
        assert_eq!(s.matches("\x1b_G").count(), 1);
    }

    #[test]
    fn the_fps_ladder_goes_down_from_the_request_and_never_below_ten() {
        let q = Quality::new(30, 1.0, true);
        assert_eq!(q.fps_levels, vec![30, 24, 18, 15]);
        let q = Quality::new(12, 1.0, true);
        assert!(q.fps_levels.iter().all(|&f| f >= 10), "{:?}", q.fps_levels);
        assert_eq!(Quality::new(1, 1.0, true).fps_levels, vec![1]);
    }
}
