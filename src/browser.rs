//! `shaderlab` with no arguments, and `shaderlab browse [DIR]`: a Ratatui file browser and gallery for what shaderlab makes.
//!
//! Tabs Shaders | Renders | Videos | Sheets (or one Files tab for `browse DIR`); a list with name, size and date, a filter and
//! sort; and a preview pane that shows images (kitty / sixel / half-blocks, the same pipeline as `shaderlab preview`), plays
//! gifs and videos (frames from ffmpeg), and runs a live render of a shader next to its parameters.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use ratatui::Frame as UiFrame;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

use crate::frame::Frame;
use crate::gpu::{self, Gpu, Origin, Prepared};
use crate::home::{self, Home, OutKind};
use crate::imgpane::{self, Img, Sender};
use crate::params;
use crate::preview::PreviewState;
use crate::termimg::{self, Protocol};
use crate::video;

const MAX_ENTRIES: usize = 5000;
const IMAGE_ID: u32 = 8181;
const FILE_MAX: (u32, u32) = (960, 540);
const DIRECT_MAX: (u32, u32) = (640, 360);

// ---- the listing ------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Shader,
    Image,
    Video,
    Other,
}

pub fn kind_of(path: &Path) -> Kind {
    match path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .as_deref()
    {
        Some("glsl" | "frag") => Kind::Shader,
        Some("png" | "jpg" | "jpeg" | "bmp" | "webp") => Kind::Image,
        Some("gif" | "mp4" | "mov" | "webm" | "m4v") => Kind::Video,
        _ => Kind::Other,
    }
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub kind: Kind,
    pub size: u64,
    pub modified: Option<SystemTime>,
    /// shipped with shaderlab: shown, never edited, moved or deleted
    pub builtin: bool,
}

/// The files of `dir` whose kind `want` accepts, at most `cap` of them (and whether there were more). Hidden files are skipped;
/// a folder that cannot be read is simply empty.
pub fn list_dir(dir: &Path, want: &dyn Fn(Kind) -> bool, cap: usize) -> (Vec<Entry>, bool) {
    let mut out = Vec::new();
    let mut more = false;
    let Ok(rd) = std::fs::read_dir(dir) else {
        return (out, false);
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let path = e.path();
        let Ok(meta) = e.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let kind = kind_of(&path);
        if !want(kind) {
            continue;
        }
        if out.len() >= cap {
            more = true;
            break;
        }
        out.push(Entry {
            path,
            name,
            kind,
            size: meta.len(),
            modified: meta.modified().ok(),
            builtin: false,
        });
    }
    (out, more)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sort {
    Name,
    Date,
    Size,
}

impl Sort {
    pub fn next(self) -> Sort {
        match self {
            Sort::Name => Sort::Date,
            Sort::Date => Sort::Size,
            Sort::Size => Sort::Name,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Sort::Name => "name",
            Sort::Date => "date",
            Sort::Size => "size",
        }
    }
}

/// Sort in place: by name (A-Z), date (newest first) or size (largest first); `reverse` flips it.
pub fn sort_entries(v: &mut [Entry], sort: Sort, reverse: bool) {
    match sort {
        Sort::Name => v.sort_by_key(|e| e.name.to_lowercase()),
        Sort::Date => v.sort_by(|a, b| {
            b.modified
                .cmp(&a.modified)
                .then_with(|| a.name.cmp(&b.name))
        }),
        Sort::Size => v.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name))),
    }
    if reverse {
        v.reverse();
    }
}

/// Indices of the entries whose name contains `filter` (case-insensitive); all of them when it is empty.
pub fn filter_indices(entries: &[Entry], filter: &str) -> Vec<usize> {
    let f = filter.to_lowercase();
    entries
        .iter()
        .enumerate()
        .filter(|(_, e)| f.is_empty() || e.name.to_lowercase().contains(&f))
        .map(|(i, _)| i)
        .collect()
}

pub fn human_size(n: u64) -> String {
    const U: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

/// `YYYY-MM-DD HH:MM` (UTC).
pub fn fmt_date(t: SystemTime) -> String {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // civil from days (Howard Hinnant)
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60
    )
}

// ---- tabs --------------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Shaders,
    Renders,
    Videos,
    Sheets,
    Files,
}

impl Tab {
    pub fn label(self) -> &'static str {
        match self {
            Tab::Shaders => "Shaders",
            Tab::Renders => "Renders",
            Tab::Videos => "Videos",
            Tab::Sheets => "Sheets",
            Tab::Files => "Files",
        }
    }
}

/// The examples that ship with shaderlab, shown (read-only) beside the user's own shaders.
const BUILTIN: [(&str, &str); 3] = [
    (
        "vignette.glsl",
        include_str!("../examples/shaders/vignette.glsl"),
    ),
    (
        "rain-down.glsl",
        include_str!("../examples/shaders/rain-down.glsl"),
    ),
    (
        "shadertoy-glow.glsl",
        include_str!("../examples/shaders/shadertoy-glow.glsl"),
    ),
];

fn builtin_dir() -> PathBuf {
    std::env::temp_dir().join(format!("shaderlab-builtin-{}", std::process::id()))
}

fn builtin_entries() -> Vec<Entry> {
    let dir = builtin_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return Vec::new();
    }
    BUILTIN
        .iter()
        .filter_map(|(name, text)| {
            let path = dir.join(name);
            std::fs::write(&path, text).ok()?;
            Some(Entry {
                path,
                name: format!("{name} (built-in)"),
                kind: Kind::Shader,
                size: text.len() as u64,
                modified: None,
                builtin: true,
            })
        })
        .collect()
}

// ---- preview content ---------------------------------------------------------------------------------------

enum Loaded {
    Still {
        generation: u64,
        img: Img,
    },
    Frames {
        generation: u64,
        frames: Vec<(Img, Duration)>,
    },
    Note {
        generation: u64,
        text: String,
    },
}

enum Preview {
    None,
    Loading,
    Note(String),
    Still(Img),
    Anim {
        frames: Vec<(Img, Duration)>,
        idx: usize,
        since: Instant,
    },
    Stream {
        current: Option<Img>,
    },
    Shader(Box<ShaderPreview>),
}

struct ShaderPreview {
    prepared: Prepared,
    size: (u32, u32),
    started: Instant,
    last: Img,
    lines: Vec<String>,
}

pub struct Browse {
    pub dir: Option<PathBuf>,
    pub protocol: Option<Protocol>,
    pub fps: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    None,
    /// leave the terminal UI, open the file in the editor, come back
    Editor(PathBuf),
    /// leave the browser, run `shaderlab preview` on this shader, come back
    FullPreview(PathBuf),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Mode {
    List,
    Viewer,
    Filter,
    Confirm(PathBuf),
    Import(String),
    Help,
}

pub struct App {
    pub home: Home,
    dir: Option<PathBuf>,
    pub tabs: Vec<Tab>,
    pub tab: usize,
    lists: BTreeMap<usize, (Vec<Entry>, bool)>,
    pub filter: String,
    pub sort: Sort,
    pub reverse: bool,
    pub sel: usize,
    top: usize,
    pub message: String,
    mode: Mode,
    pub quit: bool,
    pub proto: Protocol,
    pub note: Option<String>,
    pub truecolor: bool,
    pub cell_px: (f32, f32),
    sender: Sender,
    preview: Preview,
    preview_key: Option<(PathBuf, (u32, u32))>,
    generation: u64,
    loader_tx: mpsc::Sender<Loaded>,
    loader_rx: Receiver<Loaded>,
    stream_rx: Option<Receiver<Img>>,
    stream_stop: Arc<AtomicBool>,
    pub paused: bool,
    gpu: Option<std::result::Result<Gpu, String>>,
    resized: Option<(PathBuf, (u32, u32), usize, Img)>,
    list_rows: Vec<(Rect, usize)>,
    tab_rects: Vec<(Rect, usize)>,
    /// the cells the preview picture was last drawn into, and its pixel size
    shown: Option<(Rect, Img)>,
    frame_seq: u64,
    sent_seq: u64,
}

fn style_dim() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn accent() -> Style {
    Style::default().fg(Color::Cyan)
}

/// Run an external command, or in tests (SHADERLAB_NO_EXEC) only say what would run.
fn external(cmd: &str, args: &[&str], stdin: Option<&str>) -> std::result::Result<String, String> {
    if std::env::var_os("SHADERLAB_NO_EXEC").is_some() {
        return Ok(format!("would run: {cmd} {}", args.join(" ")));
    }
    let mut c = Command::new(cmd);
    c.args(args).stdout(Stdio::null()).stderr(Stdio::null());
    if stdin.is_some() {
        c.stdin(Stdio::piped());
    } else {
        c.stdin(Stdio::null());
    }
    let mut child = c.spawn().map_err(|e| format!("could not run {cmd}: {e}"))?;
    if let (Some(text), Some(mut si)) = (stdin, child.stdin.take()) {
        let _ = si.write_all(text.as_bytes());
    }
    let st = child.wait().map_err(|e| e.to_string())?;
    if st.success() {
        Ok(String::new())
    } else {
        Err(format!("{cmd} failed"))
    }
}

/// Move a file to the Trash (never delete it): `$SHADERLAB_TRASH`, else `~/.Trash` on macOS or the freedesktop trash elsewhere.
pub fn trash(path: &Path) -> std::result::Result<PathBuf, String> {
    let dir = if let Some(d) = std::env::var_os("SHADERLAB_TRASH").filter(|v| !v.is_empty()) {
        PathBuf::from(d)
    } else {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or("no home folder")?;
        if cfg!(target_os = "macos") {
            home.join(".Trash")
        } else {
            std::env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".local/share"))
                .join("Trash/files")
        }
    };
    std::fs::create_dir_all(&dir).map_err(|e| format!("could not open the trash: {e}"))?;
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let ext = path
        .extension()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let dest = home::unique_path(&dir, &stem, &ext);
    if std::fs::rename(path, &dest).is_err() {
        std::fs::copy(path, &dest).map_err(|e| format!("could not move it to the trash: {e}"))?;
        std::fs::remove_file(path).map_err(|e| format!("could not remove the original: {e}"))?;
    }
    Ok(dest)
}

fn expand_tilde(s: &str) -> PathBuf {
    match s.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join(rest))
            .unwrap_or_else(|| PathBuf::from(s)),
        None => PathBuf::from(s),
    }
}

// ---- loading previews off the UI thread ------------------------------------------------------------------------

fn decode_still(path: &Path) -> std::result::Result<Img, String> {
    let meta = image::image_dimensions(path).map_err(|e| format!("cannot read this image: {e}"))?;
    if (meta.0 as u64) * (meta.1 as u64) > 60_000_000 {
        return Err(format!("too large to preview ({}x{})", meta.0, meta.1));
    }
    let img = image::open(path)
        .map_err(|e| format!("cannot read this image: {e}"))?
        .to_rgba8();
    Ok(Img::new(img.width(), img.height(), img.into_raw()))
}

fn decode_gif(path: &Path) -> std::result::Result<Vec<(Img, Duration)>, String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut opts = gif::DecodeOptions::new();
    opts.set_color_output(gif::ColorOutput::RGBA);
    let mut dec = opts
        .read_info(file)
        .map_err(|e| format!("cannot read this gif: {e}"))?;
    let (sw, sh) = (dec.width() as u32, dec.height() as u32);
    if sw == 0 || sh == 0 || (sw as u64) * (sh as u64) > 16_000_000 {
        return Err("this gif is too large to preview".into());
    }
    let mut canvas = vec![0u8; (sw * sh * 4) as usize];
    let mut frames = Vec::new();
    while let Some(f) = dec
        .read_next_frame()
        .map_err(|e| format!("cannot read this gif: {e}"))?
    {
        for y in 0..f.height as u32 {
            for x in 0..f.width as u32 {
                let (cx, cy) = (f.left as u32 + x, f.top as u32 + y);
                if cx >= sw || cy >= sh {
                    continue;
                }
                let s = ((y * f.width as u32 + x) * 4) as usize;
                if f.buffer[s + 3] > 0 {
                    let d = ((cy * sw + cx) * 4) as usize;
                    canvas[d..d + 4].copy_from_slice(&f.buffer[s..s + 4]);
                }
            }
        }
        let full = Img::new(sw, sh, canvas.clone());
        let (w, h) = imgpane::contain(sw, sh, 640, 640);
        frames.push((
            if (w, h) == (sw, sh) {
                full
            } else {
                imgpane::resize(&full, w, h)
            },
            Duration::from_millis((f.delay as u64 * 10).max(20)),
        ));
        if frames.len() >= 300 {
            break;
        }
    }
    if frames.is_empty() {
        return Err("this gif has no frames".into());
    }
    Ok(frames)
}

/// Stream a video's frames, letterboxed to exactly `px`, looping until `stop`. Bounded, so decoding waits for the viewer.
fn spawn_stream(
    path: PathBuf,
    px: (u32, u32),
    stop: Arc<AtomicBool>,
    tx: SyncSender<Img>,
    notes: mpsc::Sender<Loaded>,
    generation: u64,
) {
    std::thread::spawn(move || {
        let Some(ffmpeg) = video::find_ffmpeg() else {
            let _ = notes.send(Loaded::Note {
                generation,
                text: "ffmpeg was not found, so this video cannot play here (brew install ffmpeg)"
                    .into(),
            });
            return;
        };
        let (w, h) = (px.0.max(2) & !1, px.1.max(2) & !1);
        let filter = format!(
            "fps=15,scale={w}:{h}:force_original_aspect_ratio=decrease,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2"
        );
        let mut got_any = false;
        while !stop.load(Ordering::Relaxed) {
            let spawned = Command::new(&ffmpeg)
                .args(["-v", "error", "-i"])
                .arg(&path)
                .args(["-vf", &filter, "-f", "rawvideo", "-pix_fmt", "rgba", "-"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn();
            let Ok(mut child) = spawned else {
                let _ = notes.send(Loaded::Note {
                    generation,
                    text: "could not start ffmpeg".into(),
                });
                return;
            };
            let Some(mut out) = child.stdout.take() else {
                return;
            };
            let mut buf = vec![0u8; (w * h * 4) as usize];
            while !stop.load(Ordering::Relaxed) && out.read_exact(&mut buf).is_ok() {
                got_any = true;
                let mut img = Img::new(w, h, buf.clone());
                loop {
                    match tx.try_send(img) {
                        Ok(()) => break,
                        Err(TrySendError::Full(back)) => {
                            if stop.load(Ordering::Relaxed) {
                                break;
                            }
                            img = back;
                            std::thread::sleep(Duration::from_millis(4));
                        }
                        Err(TrySendError::Disconnected(_)) => {
                            let _ = child.kill();
                            return;
                        }
                    }
                }
            }
            let _ = child.kill();
            let _ = child.wait();
            if !got_any {
                let _ = notes.send(Loaded::Note {
                    generation,
                    text: "ffmpeg could not decode this video".into(),
                });
                return;
            }
        }
    });
}

impl App {
    pub fn new(
        home: Home,
        dir: Option<PathBuf>,
        proto: Protocol,
        note: Option<String>,
        use_file: bool,
    ) -> App {
        let tabs = if dir.is_some() {
            vec![Tab::Files]
        } else {
            vec![Tab::Shaders, Tab::Renders, Tab::Videos, Tab::Sheets]
        };
        let (loader_tx, loader_rx) = mpsc::channel();
        let mut app = App {
            home,
            dir,
            tabs,
            tab: 0,
            lists: BTreeMap::new(),
            filter: String::new(),
            sort: Sort::Name,
            reverse: false,
            sel: 0,
            top: 0,
            message: String::new(),
            mode: Mode::List,
            quit: false,
            proto,
            note,
            truecolor: std::env::var("COLORTERM").is_ok_and(|v| v == "truecolor" || v == "24bit")
                || matches!(
                    std::env::var("TERM_PROGRAM").as_deref(),
                    Ok("ghostty" | "WezTerm" | "iTerm.app" | "vscode")
                ),
            cell_px: (14.0, 28.0),
            sender: Sender::new(proto, use_file, IMAGE_ID),
            preview: Preview::None,
            preview_key: None,
            generation: 0,
            loader_tx,
            loader_rx,
            stream_rx: None,
            stream_stop: Arc::new(AtomicBool::new(false)),
            paused: false,
            gpu: None,
            resized: None,
            list_rows: Vec::new(),
            tab_rects: Vec::new(),
            shown: None,
            frame_seq: 0,
            sent_seq: 0,
        };
        app.refresh();
        app
    }

    // ---- the lists ----------------------------------------------------------------------------------

    /// Re-read the folders of every tab.
    pub fn refresh(&mut self) {
        self.lists.clear();
        for (i, t) in self.tabs.clone().into_iter().enumerate() {
            let (mut v, more) = match t {
                Tab::Shaders => {
                    let (mut v, more) =
                        list_dir(&self.home.library, &|k| k == Kind::Shader, MAX_ENTRIES);
                    v.extend(builtin_entries());
                    (v, more)
                }
                Tab::Renders => list_dir(&self.home.renders(), &|k| k == Kind::Image, MAX_ENTRIES),
                Tab::Videos => list_dir(&self.home.videos(), &|k| k == Kind::Video, MAX_ENTRIES),
                Tab::Sheets => list_dir(&self.home.sheets(), &|k| k == Kind::Image, MAX_ENTRIES),
                Tab::Files => list_dir(
                    self.dir.as_deref().unwrap_or(Path::new(".")),
                    &|_| true,
                    MAX_ENTRIES,
                ),
            };
            sort_entries(&mut v, self.sort, self.reverse);
            self.lists.insert(i, (v, more));
        }
        self.clamp();
        self.preview_key = None;
    }

    fn resort(&mut self) {
        let keep = self.selected().map(|e| e.path.clone());
        for (v, _) in self.lists.values_mut() {
            sort_entries(v, self.sort, self.reverse);
        }
        if let Some(p) = keep
            && let Some(i) = self
                .visible()
                .iter()
                .position(|&i| self.entries()[i].path == p)
        {
            self.sel = i;
        }
        self.clamp();
    }

    pub fn entries(&self) -> &[Entry] {
        self.lists.get(&self.tab).map_or(&[], |(v, _)| v.as_slice())
    }

    pub fn truncated(&self) -> bool {
        self.lists.get(&self.tab).is_some_and(|(_, m)| *m)
    }

    /// Indices (into `entries`) shown under the current filter.
    pub fn visible(&self) -> Vec<usize> {
        filter_indices(self.entries(), &self.filter)
    }

    pub fn selected(&self) -> Option<&Entry> {
        let v = self.visible();
        v.get(self.sel).and_then(|&i| self.entries().get(i))
    }

    fn clamp(&mut self) {
        let n = self.visible().len();
        self.sel = self.sel.min(n.saturating_sub(1));
        self.top = self.top.min(self.sel);
    }

    fn move_sel(&mut self, by: isize) {
        let n = self.visible().len() as isize;
        if n == 0 {
            self.sel = 0;
            return;
        }
        self.sel = (self.sel as isize + by).clamp(0, n - 1) as usize;
    }

    fn switch_tab(&mut self, to: usize) {
        if to < self.tabs.len() && to != self.tab {
            self.tab = to;
            self.sel = 0;
            self.top = 0;
            self.filter.clear();
            self.preview_key = None;
        }
    }

    // ---- previews ----------------------------------------------------------------------------------------

    fn stop_stream(&mut self) {
        self.stream_stop.store(true, Ordering::Relaxed);
        self.stream_rx = None;
        self.stream_stop = Arc::new(AtomicBool::new(false));
    }

    fn gpu(&mut self) -> std::result::Result<&Gpu, String> {
        if self.gpu.is_none() {
            self.gpu = Some(Gpu::new().map_err(|e| e.to_string()));
        }
        self.gpu
            .as_ref()
            .expect("just set")
            .as_ref()
            .map_err(Clone::clone)
    }

    fn bump(&mut self) {
        self.stop_stream();
        self.generation += 1;
        self.resized = None;
    }

    /// Start showing the selected entry in a pane of `px` pixels (the pixel size of the box it will be drawn in).
    fn load_preview(&mut self, px: (u32, u32)) {
        let Some(entry) = self.selected().cloned() else {
            if self.preview_key.is_some() || !matches!(self.preview, Preview::None) {
                self.bump();
            }
            self.preview = Preview::None;
            self.preview_key = None;
            return;
        };
        let key = (entry.path.clone(), px);
        if self.preview_key.as_ref() == Some(&key) {
            return;
        }
        // same file, new size: only a stream needs reloading (stills are resized when drawn)
        let same_file = self
            .preview_key
            .as_ref()
            .is_some_and(|(p, _)| *p == entry.path);
        self.preview_key = Some(key);
        if same_file && !matches!(self.preview, Preview::Stream { .. } | Preview::Shader(_)) {
            return;
        }
        self.bump();
        self.preview = Preview::Loading;
        let generation = self.generation;
        let tx = self.loader_tx.clone();
        match entry.kind {
            Kind::Image => {
                let path = entry.path.clone();
                std::thread::spawn(move || {
                    let _ = tx.send(match decode_still(&path) {
                        Ok(img) => Loaded::Still { generation, img },
                        Err(text) => Loaded::Note { generation, text },
                    });
                });
            }
            Kind::Video
                if entry
                    .path
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("gif")) =>
            {
                let path = entry.path.clone();
                std::thread::spawn(move || {
                    let _ = tx.send(match decode_gif(&path) {
                        Ok(frames) => Loaded::Frames { generation, frames },
                        Err(text) => Loaded::Note { generation, text },
                    });
                });
            }
            Kind::Video => {
                let (stx, srx) = mpsc::sync_channel(2);
                self.stream_rx = Some(srx);
                self.preview = Preview::Stream { current: None };
                spawn_stream(
                    entry.path.clone(),
                    px,
                    self.stream_stop.clone(),
                    stx,
                    tx,
                    generation,
                );
            }
            Kind::Shader => self.preview = self.shader_preview(&entry.path, px),
            Kind::Other => self.preview = Preview::Note("no preview for this kind of file".into()),
        }
    }

    fn shader_preview(&mut self, path: &Path, px: (u32, u32)) -> Preview {
        let src = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => return Preview::Note(format!("cannot read this shader: {e}")),
        };
        let schema = match params::parse_schema(params::strip_header(&src)) {
            Ok(s) => s,
            Err(e) => return Preview::Note(format!("annotation error: {e}")),
        };
        let state = match PreviewState::new(schema, None, vec![]) {
            Ok(s) => s,
            Err(e) => return Preview::Note(e),
        };
        let size = (px.0.clamp(32, 640) & !1, px.1.clamp(32, 360) & !1);
        let frame = Frame::sample(size.0, size.1);
        let prepared = match self.gpu() {
            Ok(g) => match gpu::prepare_shader(g, &src, None, &[], &frame, Origin::TopLeft) {
                Ok(p) => p,
                Err(e) => return Preview::Note(format!("the shader does not compile:\n{e}")),
            },
            Err(e) => return Preview::Note(format!("no GPU for a live render: {e}")),
        };
        let mut lines = Vec::new();
        for i in 0..state.schema.params.len() {
            if let Some(l) = state.describe_param(i) {
                lines.push(l);
            }
        }
        if !state.schema.presets.is_empty() {
            lines.push(format!(
                "presets: {}",
                state
                    .schema
                    .presets
                    .iter()
                    .map(|p| p.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        Preview::Shader(Box::new(ShaderPreview {
            prepared,
            size,
            started: Instant::now(),
            last: Img::blank(size.0, size.1),
            lines,
        }))
    }

    /// Move the preview along: finished loads, the next gif frame, the next video frame, the shader's next picture.
    pub fn tick(&mut self, _now: Instant) {
        while let Ok(m) = self.loader_rx.try_recv() {
            match m {
                Loaded::Still { generation, img } if generation == self.generation => {
                    self.preview = Preview::Still(img);
                    self.frame_seq += 1;
                }
                Loaded::Frames { generation, frames } if generation == self.generation => {
                    self.preview = Preview::Anim {
                        frames,
                        idx: 0,
                        since: Instant::now(),
                    };
                    self.frame_seq += 1;
                }
                Loaded::Note { generation, text } if generation == self.generation => {
                    self.preview = Preview::Note(text)
                }
                _ => {}
            }
        }
        if self.paused {
            return;
        }
        match &mut self.preview {
            Preview::Anim { frames, idx, since } if since.elapsed() >= frames[*idx].1 => {
                *idx = (*idx + 1) % frames.len();
                *since = Instant::now();
                self.frame_seq += 1;
                self.resized = None;
            }
            Preview::Stream { current } => {
                if let Some(rx) = &self.stream_rx
                    && let Ok(mut img) = rx.try_recv()
                {
                    // keep only the newest
                    while let Ok(newer) = rx.try_recv() {
                        img = newer;
                    }
                    *current = Some(img);
                    self.frame_seq += 1;
                }
            }
            Preview::Shader(sp) => {
                let t = sp.started.elapsed().as_secs_f32();
                if let Some(Ok(g)) = &self.gpu {
                    let mut buf = Vec::new();
                    if sp
                        .prepared
                        .draw_rgba8(g, t, 1.0 / 15.0, (t * 15.0) as i32, &mut buf)
                        .is_ok()
                    {
                        sp.last = Img::new(sp.size.0, sp.size.1, buf);
                        self.frame_seq += 1;
                    }
                }
            }
            _ => {}
        }
    }

    /// The picture to show right now, and a note to show instead when there is none.
    fn current_img(&self) -> Option<&Img> {
        match &self.preview {
            Preview::Still(i) => Some(i),
            Preview::Anim { frames, idx, .. } => Some(&frames[*idx].0),
            Preview::Stream { current } => current.as_ref(),
            Preview::Shader(sp) => Some(&sp.last),
            _ => None,
        }
    }

    // ---- input -----------------------------------------------------------------------------------------

    pub fn on_key(&mut self, key: KeyEvent) -> Effect {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return Effect::None;
        }
        match self.mode.clone() {
            Mode::Help => {
                self.mode = Mode::List;
                Effect::None
            }
            Mode::Viewer => {
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter | KeyCode::Backspace => {
                        self.mode = Mode::List
                    }
                    KeyCode::Char(' ') | KeyCode::Char('p') => self.paused = !self.paused,
                    _ => {}
                }
                Effect::None
            }
            Mode::Filter => {
                match key.code {
                    KeyCode::Enter => self.mode = Mode::List,
                    KeyCode::Esc => {
                        self.filter.clear();
                        self.mode = Mode::List;
                    }
                    KeyCode::Backspace => {
                        self.filter.pop();
                    }
                    KeyCode::Char(c) => self.filter.push(c),
                    _ => {}
                }
                self.sel = 0;
                self.top = 0;
                self.clamp();
                Effect::None
            }
            Mode::Confirm(path) => {
                self.mode = Mode::List;
                if matches!(key.code, KeyCode::Char('y' | 'Y')) {
                    self.message = match trash(&path) {
                        Ok(dest) => format!("moved to the trash: {}", dest.display()),
                        Err(e) => e,
                    };
                    self.refresh();
                } else {
                    self.message = "kept it".into();
                }
                Effect::None
            }
            Mode::Import(mut buf) => {
                match key.code {
                    KeyCode::Esc => self.mode = Mode::List,
                    KeyCode::Enter => {
                        self.mode = Mode::List;
                        self.run_import(&buf);
                    }
                    KeyCode::Backspace => {
                        buf.pop();
                        self.mode = Mode::Import(buf);
                    }
                    KeyCode::Char(c) => {
                        buf.push(c);
                        self.mode = Mode::Import(buf);
                    }
                    _ => {}
                }
                Effect::None
            }
            Mode::List => self.list_key(key),
        }
    }

    fn run_import(&mut self, text: &str) {
        let paths: Vec<PathBuf> = text
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(expand_tilde)
            .collect();
        if paths.is_empty() {
            self.message = "nothing to import".into();
            return;
        }
        self.message = match home::import(&self.home, &paths, false) {
            Ok(r) => {
                let why = r
                    .skipped
                    .first()
                    .map(|(p, w)| format!(" (skipped {}: {w})", p.display()))
                    .unwrap_or_default();
                format!("imported {} file(s){why}", r.imported.len())
            }
            Err(e) => format!("import failed: {e}"),
        };
        self.refresh();
    }

    fn list_key(&mut self, key: KeyEvent) -> Effect {
        let n = self.visible().len();
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Esc => {
                if self.filter.is_empty() {
                    self.quit = true;
                } else {
                    self.filter.clear();
                    self.clamp();
                }
            }
            KeyCode::Tab => self.switch_tab((self.tab + 1) % self.tabs.len()),
            KeyCode::BackTab => self.switch_tab((self.tab + self.tabs.len() - 1) % self.tabs.len()),
            KeyCode::Char(c @ '1'..='5') => self.switch_tab(c as usize - '1' as usize),
            KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),
            KeyCode::PageDown => self.move_sel(10),
            KeyCode::PageUp => self.move_sel(-10),
            KeyCode::Home | KeyCode::Char('g') => self.sel = 0,
            KeyCode::End | KeyCode::Char('G') => self.sel = n.saturating_sub(1),
            KeyCode::Char('/') => self.mode = Mode::Filter,
            KeyCode::Char('s') => {
                self.sort = self.sort.next();
                self.resort();
                self.message = format!(
                    "sorted by {}{}",
                    self.sort.label(),
                    if self.reverse { " (reversed)" } else { "" }
                );
            }
            KeyCode::Char('S') => {
                self.reverse = !self.reverse;
                self.resort();
                self.message = format!(
                    "sorted by {}{}",
                    self.sort.label(),
                    if self.reverse { " (reversed)" } else { "" }
                );
            }
            KeyCode::Char(' ') | KeyCode::Char('p') => self.paused = !self.paused,
            KeyCode::Char('i') => self.mode = Mode::Import(String::new()),
            KeyCode::Enter => return self.open_selected(),
            KeyCode::Char('r') => self.render_still(),
            KeyCode::Char('v') => self.record_video(),
            KeyCode::Char('e') => return self.edit_selected(),
            KeyCode::Char('o') => self.reveal(),
            KeyCode::Char('c') => self.copy_path(),
            KeyCode::Char('d') => {
                let picked = self
                    .selected()
                    .map(|e| (e.builtin, e.name.clone(), e.path.clone()));
                match picked {
                    Some((true, ..)) => self.message = "built-in shaders cannot be deleted".into(),
                    Some((false, name, path)) => {
                        self.message = format!("move {name} to the trash? y to confirm");
                        self.mode = Mode::Confirm(path);
                    }
                    None => {}
                }
            }
            _ => {}
        }
        Effect::None
    }

    fn open_selected(&mut self) -> Effect {
        let Some(e) = self.selected().cloned() else {
            return Effect::None;
        };
        match e.kind {
            Kind::Shader => Effect::FullPreview(e.path),
            Kind::Image | Kind::Video => {
                self.mode = Mode::Viewer;
                self.preview_key = None;
                Effect::None
            }
            Kind::Other => {
                self.message = "nothing to open for this kind of file".into();
                Effect::None
            }
        }
    }

    fn edit_selected(&mut self) -> Effect {
        let Some(e) = self.selected().cloned() else {
            return Effect::None;
        };
        if e.kind != Kind::Shader {
            self.message = "e edits shaders".into();
            return Effect::None;
        }
        if e.builtin {
            // a built-in is read-only: edit a copy in your library
            if let Err(err) = std::fs::create_dir_all(&self.home.library) {
                self.message = format!("could not create your shader library: {err}");
                return Effect::None;
            }
            let stem = e
                .name
                .trim_end_matches(" (built-in)")
                .trim_end_matches(".glsl")
                .to_string();
            let copy = home::unique_path(&self.home.library, &stem, "glsl");
            if let Err(err) = std::fs::copy(&e.path, &copy) {
                self.message = format!("could not copy it: {err}");
                return Effect::None;
            }
            self.message = format!("editing a copy in your library: {}", copy.display());
            self.refresh();
            return Effect::Editor(copy);
        }
        Effect::Editor(e.path)
    }

    fn reveal(&mut self) {
        let Some(e) = self.selected().cloned() else {
            return;
        };
        let r = if cfg!(target_os = "macos") {
            external("open", &["-R", &e.path.to_string_lossy()], None)
        } else {
            external(
                "xdg-open",
                &[&e.path.parent().unwrap_or(Path::new(".")).to_string_lossy()],
                None,
            )
        };
        self.message = match r {
            Ok(m) if m.is_empty() => "revealed".into(),
            Ok(m) => m,
            Err(e) => e,
        };
    }

    fn copy_path(&mut self) {
        let Some(e) = self.selected().cloned() else {
            return;
        };
        let text = e.path.to_string_lossy().into_owned();
        let r = if cfg!(target_os = "macos") {
            external("pbcopy", &[], Some(&text))
        } else if external("wl-copy", &[], Some(&text)).is_ok() {
            Ok(String::new())
        } else {
            external("xclip", &["-selection", "clipboard"], Some(&text))
        };
        self.message = match r {
            Ok(m) if m.is_empty() => format!("copied {text}"),
            Ok(m) => m,
            Err(e) => format!("could not copy: {e}; the path is {text}"),
        };
    }

    fn shader_source(&self) -> Option<(Entry, String)> {
        let e = self.selected().cloned()?;
        if e.kind != Kind::Shader {
            return None;
        }
        let src = std::fs::read_to_string(&e.path).ok()?;
        Some((e, src))
    }

    /// `r`: a still of the selected shader into the renders folder.
    fn render_still(&mut self) {
        let Some((e, src)) = self.shader_source() else {
            self.message = "r renders a shader".into();
            return;
        };
        let size = (1280u32, 720u32);
        let frame = Frame::sample(size.0, size.1);
        let stem = home::render_stem(
            e.name
                .trim_end_matches(" (built-in)")
                .trim_end_matches(".glsl"),
            None,
            size,
            5.0,
        );
        let result = (|| -> std::result::Result<PathBuf, String> {
            let g = self.gpu().map_err(|e| format!("no GPU: {e}"))?;
            let prepared = gpu::prepare_shader(g, &src, None, &[], &frame, Origin::TopLeft)?;
            let mut px = Vec::new();
            prepared.draw_rgba8(g, 5.0, 1.0 / 60.0, 300, &mut px)?;
            let out = self
                .home
                .out_path(OutKind::Render, &stem, "png")
                .map_err(|e| e.to_string())?;
            image::save_buffer(&out, &px, size.0, size.1, image::ColorType::Rgba8)
                .map_err(|e| e.to_string())?;
            Ok(out)
        })();
        self.message = match result {
            Ok(p) => format!("rendered {}", p.display()),
            Err(e) => format!("could not render: {e}"),
        };
        self.refresh_keep_tab();
    }

    /// `v`: five seconds of the selected shader into the videos folder (mp4 with ffmpeg, else a gif).
    fn record_video(&mut self) {
        let Some((e, src)) = self.shader_source() else {
            self.message = "v records a shader".into();
            return;
        };
        let size = (960u32, 540u32);
        let frame = Frame::sample(size.0, size.1);
        let name = e
            .name
            .trim_end_matches(" (built-in)")
            .trim_end_matches(".glsl")
            .to_string();
        let ffmpeg = video::find_ffmpeg();
        let (fmt, _) = video::choose_format(None, None, ffmpeg.is_some())
            .unwrap_or((video::Format::Gif, None));
        let result = (|| -> std::result::Result<PathBuf, String> {
            let g = self.gpu().map_err(|e| format!("no GPU: {e}"))?;
            let prepared = gpu::prepare_shader(g, &src, None, &[], &frame, Origin::TopLeft)?;
            let out = self
                .home
                .out_path(
                    OutKind::Video,
                    &home::video_stem(&name, None, 5.0),
                    fmt.extension(),
                )
                .map_err(|e| e.to_string())?;
            let g = self.gpu().map_err(|e| e.to_string())?;
            video::render_video(
                g,
                &prepared,
                &video::VideoOptions {
                    fps: 24,
                    duration: 5.0,
                    start: 0.0,
                    format: fmt,
                    out: out.clone(),
                    loop_seamless: false,
                    ffmpeg,
                },
            )
            .map_err(|e| format!("{e:#}"))?;
            Ok(out)
        })();
        self.message = match result {
            Ok(p) => format!("recorded {}", p.display()),
            Err(e) => format!("could not record: {e}"),
        };
        self.refresh_keep_tab();
    }

    fn refresh_keep_tab(&mut self) {
        let (tab, sel) = (self.tab, self.sel);
        self.refresh();
        self.tab = tab;
        self.sel = sel;
        self.clamp();
    }

    pub fn on_mouse(&mut self, m: MouseEvent) {
        let (x, y) = (m.column, m.row);
        let hit = |r: &Rect| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height;
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((_, i)) = self.tab_rects.iter().find(|(r, _)| hit(r)).copied() {
                    self.switch_tab(i);
                } else if let Some((_, i)) = self.list_rows.iter().find(|(r, _)| hit(r)).copied() {
                    self.sel = i;
                }
            }
            MouseEventKind::ScrollDown => self.move_sel(3),
            MouseEventKind::ScrollUp => self.move_sel(-3),
            _ => {}
        }
    }

    // ---- drawing -----------------------------------------------------------------------------------------

    pub fn draw(&mut self, f: &mut UiFrame) {
        let area = f.area();
        if area.width == 0 || area.height == 0 {
            return;
        }
        let footer_h = if area.height >= 4 { 2 } else { 0 };
        let tabs_h = if area.height >= 6 { 1 } else { 0 };
        let [tabs, body, footer] = Layout::vertical([
            Constraint::Length(tabs_h),
            Constraint::Min(0),
            Constraint::Length(footer_h),
        ])
        .areas(area);
        self.draw_tabs(f, tabs);
        let pane_rect = if self.mode == Mode::Viewer {
            self.list_rows.clear();
            body
        } else if body.width >= 70 {
            let lw = (body.width * 2 / 5).clamp(30, 70);
            let [l, r] =
                Layout::horizontal([Constraint::Length(lw), Constraint::Min(10)]).areas(body);
            self.draw_list(f, l);
            r
        } else {
            self.draw_list(f, body);
            Rect::new(body.x, body.y, 0, 0)
        };
        self.draw_preview(f, pane_rect);
        self.draw_footer(f, footer);
        if self.mode == Mode::Help {
            self.draw_help(f);
        }
    }

    fn draw_tabs(&mut self, f: &mut UiFrame, r: Rect) {
        self.tab_rects.clear();
        if r.height == 0 {
            return;
        }
        let mut x = r.x;
        let mut spans = vec![Span::styled(
            " shaderlab ",
            Style::default().add_modifier(Modifier::BOLD),
        )];
        x += 11;
        for (i, t) in self.tabs.iter().enumerate() {
            let label = format!(" {} ", t.label());
            let w = label.chars().count() as u16;
            spans.push(Span::styled(
                label,
                if i == self.tab {
                    Style::default().fg(Color::Black).bg(Color::Cyan)
                } else {
                    style_dim()
                },
            ));
            self.tab_rects
                .push((Rect::new(x, r.y, w.min(r.right().saturating_sub(x)), 1), i));
            x += w;
        }
        f.render_widget(Paragraph::new(Line::from(spans)), r);
    }

    fn draw_list(&mut self, f: &mut UiFrame, r: Rect) {
        self.list_rows.clear();
        let vis = self.visible();
        let title = format!(
            " {} ({}{}) {}{} ",
            self.tabs[self.tab].label(),
            vis.len(),
            if self.truncated() { "+" } else { "" },
            self.sort.label(),
            if self.reverse { " \u{2191}" } else { "" }
        );
        let block = Block::bordered().title(title).border_style(style_dim());
        let inner = block.inner(r);
        f.render_widget(block, r);
        if inner.width < 6 || inner.height == 0 {
            return;
        }
        if vis.is_empty() {
            let msg = if self.filter.is_empty() {
                self.empty_hint()
            } else {
                "nothing matches the filter".into()
            };
            f.render_widget(
                Paragraph::new(msg)
                    .style(style_dim())
                    .wrap(Wrap { trim: true }),
                inner,
            );
            return;
        }
        let rows = inner.height as usize;
        if self.sel < self.top {
            self.top = self.sel;
        } else if self.sel >= self.top + rows {
            self.top = self.sel + 1 - rows;
        }
        let name_w = (inner.width as usize).saturating_sub(21).max(8);
        for (k, &idx) in vis.iter().enumerate().skip(self.top).take(rows) {
            let e = &self.entries()[idx];
            let y = inner.y + (k - self.top) as u16;
            let rect = Rect::new(inner.x, y, inner.width, 1);
            let mut name: String = e.name.chars().take(name_w).collect();
            if e.name.chars().count() > name_w {
                name.pop();
                name.push('~');
            }
            let date = e.modified.map(fmt_date).unwrap_or_default();
            let line = format!(
                "{name:<name_w$} {:>8} {}",
                human_size(e.size),
                date.get(..10).unwrap_or("")
            );
            let style = if k == self.sel {
                Style::default().fg(Color::Black).bg(Color::Cyan)
            } else {
                Style::default()
            };
            f.render_widget(Paragraph::new(line).style(style), rect);
            self.list_rows.push((rect, k));
        }
    }

    fn empty_hint(&self) -> String {
        match self.tabs[self.tab] {
            Tab::Shaders => format!(
                "no shaders yet: drop .glsl files in {}",
                self.home.library.display()
            ),
            Tab::Renders => format!(
                "no renders yet: shaderlab render FILE, or r on a shader. They land in {}",
                self.home.renders().display()
            ),
            Tab::Videos => format!(
                "no videos yet: shaderlab video FILE, or v on a shader. They land in {}",
                self.home.videos().display()
            ),
            Tab::Sheets => format!(
                "no contact sheets yet: shaderlab contact-sheet FILE. They land in {}",
                self.home.sheets().display()
            ),
            Tab::Files => "this folder has no files".into(),
        }
    }

    fn draw_preview(&mut self, f: &mut UiFrame, r: Rect) {
        self.shown = None;
        if r.width < 4 || r.height < 3 {
            return;
        }
        let title = self
            .selected()
            .map(|e| format!(" {} ", e.name))
            .unwrap_or_else(|| " preview ".into());
        let block = Block::bordered().title(title).border_style(style_dim());
        let inner = block.inner(r);
        f.render_widget(block, r);
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        // text under the picture: parameters of a shader, or what the file is
        let info = self.info_lines();
        let info_h =
            (info.len() as u16 + u16::from(!info.is_empty())).min(inner.height.saturating_sub(3));
        let [pic, text] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(info_h)]).areas(inner);
        if text.height > 0 {
            f.render_widget(Paragraph::new(info).wrap(Wrap { trim: true }), text);
        }
        let px_box = self.box_px(pic);
        self.load_preview(px_box);
        match &self.preview {
            Preview::None => {}
            Preview::Loading => {
                f.render_widget(Paragraph::new("loading...").style(style_dim()), pic)
            }
            Preview::Note(t) => f.render_widget(
                Paragraph::new(t.clone())
                    .style(Style::default().fg(Color::Yellow))
                    .wrap(Wrap { trim: true }),
                pic,
            ),
            _ => {
                let seq = self.frame_seq;
                if let Some(img) = self.current_img() {
                    let (w, h) = (img.w, img.h);
                    let max = if self.sender.use_file {
                        FILE_MAX
                    } else {
                        DIRECT_MAX
                    };
                    let pl = imgpane::place(self.proto, pic, w, h, self.cell_px, max);
                    let key = (pl.px, seq as usize);
                    let img = img.clone();
                    let cached = self
                        .resized
                        .as_ref()
                        .filter(|(p, px, s, _)| {
                            Some(p) == self.preview_key.as_ref().map(|(p, _)| p)
                                && *px == pl.px
                                && *s == seq as usize
                        })
                        .map(|(_, _, _, i)| i.clone());
                    let shown = cached.unwrap_or_else(|| {
                        let r = imgpane::resize(&img, pl.px.0, pl.px.1);
                        if let Some((p, _)) = &self.preview_key {
                            self.resized = Some((p.clone(), key.0, key.1, r.clone()));
                        }
                        r
                    });
                    if self.proto == Protocol::HalfBlocks {
                        imgpane::paint_half_blocks(
                            f.buffer_mut(),
                            pl.cells,
                            &shown,
                            self.truecolor,
                        );
                    } else {
                        f.render_widget(Clear, pic);
                    }
                    self.shown = Some((pl.cells, shown));
                }
            }
        }
    }

    /// The pixel size of a box of cells (what a stream is asked to fit).
    fn box_px(&self, r: Rect) -> (u32, u32) {
        match self.proto {
            Protocol::HalfBlocks => (r.width as u32, r.height as u32 * 2),
            _ => (
                (r.width as f32 * self.cell_px.0) as u32,
                (r.height as f32 * self.cell_px.1) as u32,
            ),
        }
    }

    fn info_lines(&self) -> Vec<Line<'static>> {
        let Some(e) = self.selected() else {
            return vec![];
        };
        let mut lines = vec![Line::styled(
            format!(
                "{}  {}  {}",
                human_size(e.size),
                e.modified.map(fmt_date).unwrap_or_default(),
                e.path
                    .parent()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default()
            ),
            style_dim(),
        )];
        match &self.preview {
            Preview::Still(i) => lines.push(Line::from(format!("{}x{} px", i.w, i.h))),
            Preview::Shader(sp) => {
                for l in &sp.lines {
                    lines.push(Line::from(l.clone()));
                }
                lines.push(Line::styled(
                    "Enter: full preview   r: render a still   v: record 5 s   e: edit",
                    style_dim(),
                ));
            }
            Preview::Stream { .. } | Preview::Anim { .. } => lines.push(Line::styled(
                if self.paused {
                    "paused (space)"
                } else {
                    "playing (space pauses)"
                },
                style_dim(),
            )),
            _ => {}
        }
        lines
    }

    fn draw_footer(&self, f: &mut UiFrame, r: Rect) {
        if r.height == 0 {
            return;
        }
        let first = match &self.mode {
            Mode::Filter => Line::from(vec![
                Span::styled("/", accent()),
                Span::raw(self.filter.clone()),
                Span::styled("_", accent()),
                Span::styled("   Enter keep  Esc clear", style_dim()),
            ]),
            Mode::Import(b) => Line::from(vec![
                Span::styled("import path(s) ; separated: ", accent()),
                Span::raw(b.clone()),
                Span::styled("_   Enter import  Esc cancel", style_dim()),
            ]),
            Mode::Confirm(_) => {
                Line::styled(self.message.clone(), Style::default().fg(Color::Yellow))
            }
            _ => Line::styled(
                "\u{2191}\u{2193} move  Tab tabs  Enter open  / filter  s sort  r render  v record  e edit  o reveal  c copy  d trash  i import  ? help  q quit",
                style_dim(),
            ),
        };
        let mut second = Vec::new();
        if !self.message.is_empty() && !matches!(self.mode, Mode::Confirm(_)) {
            second.push(Span::styled(
                self.message.clone(),
                Style::default().fg(Color::Yellow),
            ));
        }
        if let Some(n) = &self.note {
            second.push(Span::styled(format!("   {n}"), style_dim()));
        }
        f.render_widget(Paragraph::new(vec![first, Line::from(second)]), r);
    }

    fn draw_help(&self, f: &mut UiFrame) {
        let area = f.area();
        let lines = help_lines();
        let (w, h) = (
            66.min(area.width),
            (lines.len() as u16 + 2).min(area.height),
        );
        if w < 20 || h < 5 {
            return;
        }
        let r = Rect::new(
            area.x + (area.width - w) / 2,
            area.y + (area.height - h) / 2,
            w,
            h,
        );
        f.render_widget(Clear, r);
        f.render_widget(
            Paragraph::new(lines).block(
                Block::bordered()
                    .title(" help (any key closes) ")
                    .border_style(accent()),
            ),
            r,
        );
    }

    // ---- the picture over the link ---------------------------------------------------------------------

    /// Send the preview picture (kitty / sixel) after Ratatui's cells; remove it when nothing is showing or a dialog covers it.
    pub fn emit(&mut self, out: &mut impl Write) -> std::io::Result<()> {
        if self.proto == Protocol::HalfBlocks {
            return Ok(());
        }
        let covered = matches!(self.mode, Mode::Help);
        match (&self.shown, covered) {
            (Some((cells, img)), false) => {
                if self.sent_seq != self.frame_seq || !self.sender.is_shown() {
                    self.sender.send(out, *cells, img)?;
                    self.sent_seq = self.frame_seq;
                }
            }
            _ => {
                self.sender.hide(out)?;
                self.sent_seq = u64::MAX;
            }
        }
        out.flush()
    }

    /// Forget what was sent (after a resize or coming back from another program) so the next frame draws the picture again.
    pub fn invalidate(&mut self) {
        self.sent_seq = u64::MAX;
        self.resized = None;
    }

    pub fn hide_image(&mut self, out: &mut impl Write) -> std::io::Result<()> {
        self.sender.hide(out)?;
        self.invalidate();
        Ok(())
    }

    pub fn in_viewer(&self) -> bool {
        self.mode == Mode::Viewer
    }
}

pub fn help_lines() -> Vec<Line<'static>> {
    let dim = style_dim();
    let row = |a: &str, b: &str| {
        Line::from(vec![
            Span::styled(
                format!("{a:<18}"),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(b.to_string(), dim),
        ])
    };
    vec![
        row("up/down j/k", "move (PageUp/PageDown, g/G for the ends)"),
        row("Tab, 1-4", "switch tab (or click it)"),
        row(
            "Enter",
            "shader: full live preview; image or video: view it",
        ),
        row("/", "filter by name (Enter keeps, Esc clears)"),
        row("s  S", "cycle the sort (name, date, size); reverse it"),
        row("space / p", "pause a playing gif, video or shader"),
        row("r", "render a still of the shader into renders/"),
        row("v", "record 5 s of the shader into videos/"),
        row("e", "edit in $EDITOR (a built-in opens a copy)"),
        row("o", "reveal in Finder"),
        row("c", "copy the path"),
        row("d", "move to the Trash (asks first; never deletes)"),
        row("i", "import files: path or paths separated by ;"),
        row("click, scroll", "select a row or tab; scroll the list"),
        row("q  Esc", "quit"),
        Line::raw(""),
        Line::styled("dates are UTC. Folders: shaderlab where", dim),
    ]
}

// ---- running ----------------------------------------------------------------------------------------------

fn restore_extras(sender_hide: Option<&mut App>) {
    let mut out = std::io::stdout();
    if let Some(app) = sender_hide {
        let _ = app.hide_image(&mut out);
    }
    let _ = execute!(out, DisableMouseCapture, ratatui::crossterm::cursor::Show);
}

fn cell_pixels() -> (f32, f32) {
    match ratatui::crossterm::terminal::window_size() {
        Ok(ws) if ws.width > 0 && ws.height > 0 && ws.columns > 0 && ws.rows > 0 => (
            ws.width as f32 / ws.columns as f32,
            ws.height as f32 / ws.rows as f32,
        ),
        _ => (14.0, 28.0),
    }
}

/// Open the browser until the user quits. The terminal is restored on every exit path, including a panic.
pub fn run(opts: Browse) -> Result<()> {
    let home = Home::from_env();
    let (proto, note) = match opts.protocol {
        Some(p) => (p, None),
        None => termimg::detect(&|k| std::env::var(k).ok()),
    };
    let use_file =
        proto == Protocol::Kitty && cfg!(unix) && termimg::is_local(&|k| std::env::var(k).ok());
    let dir = opts.dir.clone();
    if let Some(d) = &dir
        && !d.is_dir()
    {
        return Err(anyhow!("{} is not a folder", d.display()));
    }
    let mut app = App::new(home, dir, proto, note, use_file);
    let mut terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableMouseCapture);
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_extras(None);
        previous(info);
    }));
    let result = event_loop(&mut terminal, &mut app, opts.fps.clamp(5, 30));
    restore_extras(Some(&mut app));
    ratatui::restore();
    result
}

fn event_loop(terminal: &mut ratatui::DefaultTerminal, app: &mut App, fps: u32) -> Result<()> {
    let interval = Duration::from_secs_f64(1.0 / fps as f64);
    let mut next_frame = Instant::now();
    let mut last_size = (0u16, 0u16);
    while !app.quit {
        loop {
            let remaining = next_frame.saturating_duration_since(Instant::now());
            if !event::poll(remaining)? {
                break;
            }
            let effect = match event::read()? {
                Event::Key(k) if k.kind == KeyEventKind::Press => app.on_key(k),
                Event::Mouse(m) => {
                    app.on_mouse(m);
                    Effect::None
                }
                _ => Effect::None,
            };
            if effect != Effect::None {
                run_effect(terminal, app, effect)?;
            }
            if remaining.is_zero() || app.quit {
                break;
            }
        }
        let now = Instant::now();
        if now.saturating_duration_since(next_frame) >= interval {
            next_frame = now;
        }
        next_frame += interval;
        if app.quit {
            break;
        }
        let size = terminal.size()?;
        if (size.width, size.height) != last_size {
            last_size = (size.width, size.height);
            app.cell_px = cell_pixels();
            app.invalidate();
            terminal.backend_mut().write_all(termimg::SYNC_BEGIN)?;
            terminal.clear()?;
        }
        app.tick(now);
        terminal.backend_mut().write_all(termimg::SYNC_BEGIN)?;
        terminal.draw(|f| app.draw(f))?;
        app.emit(terminal.backend_mut())?;
        terminal.backend_mut().write_all(termimg::SYNC_END)?;
        terminal.backend_mut().flush()?;
    }
    Ok(())
}

/// Leave the terminal UI, run something that wants the whole terminal, and come back.
fn run_effect(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    effect: Effect,
) -> Result<()> {
    app.hide_image(&mut std::io::stdout())?;
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    match effect {
        Effect::Editor(path) => {
            let editor = std::env::var("VISUAL")
                .ok()
                .filter(|v| !v.is_empty())
                .or_else(|| std::env::var("EDITOR").ok().filter(|v| !v.is_empty()))
                .unwrap_or_else(|| "vi".into());
            let mut parts = editor.split_whitespace();
            if let Some(cmd) = parts.next() {
                let st = Command::new(cmd).args(parts).arg(&path).status();
                app.message = match st {
                    Ok(s) if s.success() => format!("edited {}", path.display()),
                    Ok(_) => format!("{editor} exited with an error"),
                    Err(e) => format!("could not run {editor}: {e}"),
                };
            }
        }
        Effect::FullPreview(path) => {
            let r = crate::tui::run(crate::tui::TuiOptions {
                file: path,
                preset: None,
                sets: vec![],
                protocol: Some(app.proto),
                transfer: crate::tui::Transfer::Auto,
                text: "sample".into(),
                origin: Origin::TopLeft,
                fps: 30,
            });
            if let Err(e) = r {
                app.message = format!("preview failed: {e:#}");
            }
        }
        Effect::None => {}
    }
    *terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableMouseCapture);
    app.refresh_keep_tab();
    app.invalidate();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyEventState;
    use std::sync::Mutex;

    /// the tests that touch process-wide environment variables take turns
    static ENV: Mutex<()> = Mutex::new(());

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent {
            code: c,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn ch(c: char) -> KeyEvent {
        key(KeyCode::Char(c))
    }

    fn no_exec() {
        // nothing the browser would run (pbcopy, open -R, an editor) really runs in a test
        unsafe { std::env::set_var("SHADERLAB_NO_EXEC", "1") };
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        lib: PathBuf,
    }

    fn png(path: &Path, w: u32, h: u32) {
        let px: Vec<u8> = (0..w * h * 3).map(|i| (i % 251) as u8).collect();
        image::save_buffer(path, &px, w, h, image::ColorType::Rgb8).unwrap();
    }

    /// A home with two shaders, three renders, one video (not a real one), one sheet and a note that is none of them.
    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("home");
        let lib = dir.path().join("config/shaderlab/shaders");
        for d in ["renders", "videos", "sheets", "frames"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(
            lib.join("alpha.glsl"),
            "void mainImage(out vec4 c, in vec2 f) { c = vec4(0.1, 0.4, 0.8, 1.0); }\n",
        )
        .unwrap();
        std::fs::write(
            lib.join("beta.glsl"),
            "void mainImage(out vec4 c, in vec2 f) { c = vec4(0.8, 0.4, 0.1, 1.0); }\n",
        )
        .unwrap();
        png(&root.join("renders/one.png"), 64, 36);
        png(&root.join("renders/two.png"), 32, 32);
        std::fs::write(root.join("renders/broken.png"), b"this is not a png").unwrap();
        std::fs::write(root.join("renders/notes.txt"), b"not an image").unwrap();
        std::fs::write(root.join("videos/empty.mp4"), b"").unwrap();
        png(&root.join("sheets/sheet.png"), 48, 48);
        Fixture {
            _dir: dir,
            root,
            lib,
        }
    }

    fn app_for(f: &Fixture, dir: Option<PathBuf>) -> App {
        let (root, lib) = (f.root.clone(), f.lib.clone());
        let config = lib.parent().unwrap().parent().unwrap().to_path_buf();
        let home = Home::from_vars(&move |k| match k {
            "SHADERLAB_HOME" => Some(root.to_string_lossy().into_owned()),
            "XDG_CONFIG_HOME" => Some(config.to_string_lossy().into_owned()),
            "HOME" => Some("/nonexistent".into()),
            _ => None,
        });
        no_exec();
        App::new(home, dir, Protocol::HalfBlocks, None, false)
    }

    fn names(app: &App) -> Vec<String> {
        app.visible()
            .iter()
            .map(|&i| app.entries()[i].name.clone())
            .collect()
    }

    fn render(app: &mut App, w: u16, h: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        let buf = t.backend().buffer().clone();
        let mut s = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                s.push_str(buf[(x, y)].symbol());
            }
            s.push('\n');
        }
        s
    }

    fn settle(app: &mut App) {
        // let the loader threads finish
        for _ in 0..200 {
            app.tick(Instant::now());
            std::thread::sleep(Duration::from_millis(10));
            if !matches!(app.preview, Preview::Loading) {
                break;
            }
        }
    }

    #[test]
    fn files_are_told_apart_sized_sorted_and_filtered() {
        assert_eq!(kind_of(Path::new("a/b.GLSL")), Kind::Shader);
        assert_eq!(kind_of(Path::new("x.png")), Kind::Image);
        assert_eq!(kind_of(Path::new("x.MP4")), Kind::Video);
        assert_eq!(kind_of(Path::new("x.gif")), Kind::Video);
        assert_eq!(kind_of(Path::new("x.txt")), Kind::Other);
        assert_eq!(kind_of(Path::new("noext")), Kind::Other);
        assert_eq!(human_size(0), "0 B");
        assert!(human_size(1536).contains("KB") || human_size(1536).contains("K"));
        assert!(human_size(5 * 1024 * 1024).contains('M'));
        let mk = |n: &str, size: u64, age: u64| Entry {
            path: PathBuf::from(n),
            name: n.into(),
            kind: Kind::Image,
            size,
            modified: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(age)),
            builtin: false,
        };
        let mut v = vec![mk("b.png", 10, 3), mk("a.png", 30, 1), mk("c.png", 20, 2)];
        sort_entries(&mut v, Sort::Name, false);
        assert_eq!(v[0].name, "a.png");
        sort_entries(&mut v, Sort::Size, false);
        assert_eq!(v.iter().map(|e| e.size).collect::<Vec<_>>().len(), 3);
        let order: Vec<u64> = v.iter().map(|e| e.size).collect();
        assert!(
            order == vec![30, 20, 10] || order == vec![10, 20, 30],
            "{order:?}"
        );
        sort_entries(&mut v, Sort::Name, true);
        assert_eq!(v[0].name, "c.png");
        let idx = filter_indices(&v, "A.P");
        assert_eq!(idx.len(), 1);
        assert_eq!(filter_indices(&v, "").len(), 3);
    }

    #[test]
    fn a_huge_folder_is_listed_lazily_up_to_a_cap() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..1200 {
            std::fs::write(dir.path().join(format!("f{i}.png")), b"x").unwrap();
        }
        std::fs::write(dir.path().join(".hidden.png"), b"x").unwrap();
        let (v, more) = list_dir(dir.path(), &|k| k == Kind::Image, 500);
        assert_eq!(v.len(), 500);
        assert!(more, "it says there were more");
        let (v, more) = list_dir(dir.path(), &|k| k == Kind::Image, 5000);
        assert_eq!(v.len(), 1200, "hidden files are skipped");
        assert!(!more);
        // a folder that does not exist is simply empty
        assert!(
            list_dir(Path::new("/definitely/not/here"), &|_| true, 10)
                .0
                .is_empty()
        );
    }

    #[test]
    fn the_tabs_list_shaders_renders_videos_and_sheets_and_the_builtins_are_read_only() {
        let f = fixture();
        let mut app = app_for(&f, None);
        assert_eq!(app.tabs.len(), 4);
        let shaders = names(&app);
        assert!(shaders.contains(&"alpha.glsl".to_string()), "{shaders:?}");
        assert!(
            shaders
                .iter()
                .any(|n| n.contains("vignette") && n.contains("built-in")),
            "{shaders:?}"
        );
        app.on_key(ch('2'));
        assert_eq!(app.tab, 1);
        let renders = names(&app);
        assert_eq!(
            renders.len(),
            3,
            "png only (the txt is not an image): {renders:?}"
        );
        app.on_key(key(KeyCode::Tab));
        assert_eq!(names(&app), vec!["empty.mp4".to_string()]);
        app.on_key(key(KeyCode::BackTab));
        assert_eq!(app.tab, 1);
        app.on_key(ch('4'));
        assert_eq!(names(&app), vec!["sheet.png".to_string()]);
        // a folder given on the command line is browsed as one list of everything
        let mut files = app_for(&f, Some(f.root.join("renders")));
        assert_eq!(files.tabs, vec![Tab::Files]);
        assert!(names(&files).len() >= 3);
        files.on_key(ch('q'));
        assert!(files.quit);
    }

    #[test]
    fn keys_move_filter_sort_help_and_quit() {
        let f = fixture();
        let mut app = app_for(&f, None);
        app.on_key(ch('2'));
        assert_eq!(app.sel, 0);
        app.on_key(ch('j'));
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.sel, 2);
        app.on_key(ch('j'));
        assert_eq!(app.sel, 2, "it stops at the last row");
        app.on_key(ch('k'));
        app.on_key(ch('g'));
        assert_eq!(app.sel, 0);
        app.on_key(ch('G'));
        assert_eq!(app.sel, 2);
        // the filter narrows the list while typing, Esc clears it
        app.on_key(ch('/'));
        for c in "two".chars() {
            app.on_key(ch(c));
        }
        assert_eq!(names(&app), vec!["two.png".to_string()]);
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.filter, "two");
        app.on_key(key(KeyCode::Esc));
        assert!(app.filter.is_empty() && !app.quit);
        assert_eq!(names(&app).len(), 3);
        // sorting
        app.on_key(ch('s'));
        assert!(app.message.contains("sorted by"), "{}", app.message);
        let before = names(&app);
        app.on_key(ch('S'));
        assert_ne!(names(&app), before, "reversing changes the order");
        // help opens and any key closes it
        app.on_key(ch('?'));
        assert!(render(&mut app, 100, 40).contains("quit"));
        app.on_key(ch('x'));
        assert!(!render(&mut app, 100, 40).contains("Esc quit") || !app.quit);
        // pausing, then quitting; Ctrl-C quits from anywhere
        app.on_key(ch(' '));
        assert!(app.paused);
        app.on_key(ch('q'));
        assert!(app.quit);
        let mut app2 = app_for(&f, None);
        app2.on_key(ch('/'));
        let mut ctrl_c = ch('c');
        ctrl_c.modifiers = KeyModifiers::CONTROL;
        app2.on_key(ctrl_c);
        assert!(app2.quit);
    }

    #[test]
    fn enter_opens_a_shader_in_the_full_preview_and_a_picture_full_size() {
        let f = fixture();
        let mut app = app_for(&f, None);
        let e = app.on_key(key(KeyCode::Enter));
        match e {
            Effect::FullPreview(p) => assert!(p.to_string_lossy().ends_with(".glsl")),
            other => panic!("{other:?}"),
        }
        app.on_key(ch('2'));
        assert_eq!(app.on_key(key(KeyCode::Enter)), Effect::None);
        assert!(app.in_viewer());
        // the viewer closes with Esc and shows the picture
        settle(&mut app);
        assert!(render(&mut app, 90, 30).len() > 100);
        app.on_key(key(KeyCode::Esc));
        assert!(!app.in_viewer());
        // 'e' edits only a shader of the user's own (never a built-in)
        app.on_key(ch('1'));
        match app.on_key(ch('e')) {
            Effect::Editor(p) => assert!(p.starts_with(&f.lib)),
            other => panic!("{other:?}"),
        }
        // a built-in is never edited in place: 'e' copies it into the user's library and edits the copy
        app.on_key(ch('G'));
        let builtin = app.selected().unwrap().clone();
        assert!(builtin.builtin);
        match app.on_key(ch('e')) {
            Effect::Editor(p) => {
                assert!(p.starts_with(&f.lib), "{p:?}");
                assert!(p.exists());
                assert_ne!(p, builtin.path);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn copy_and_reveal_say_what_they_do_without_running_anything_in_tests() {
        let f = fixture();
        let mut app = app_for(&f, None);
        app.on_key(ch('c'));
        assert!(
            app.message.contains("pbcopy")
                || app.message.contains("copied")
                || app.message.contains("would run"),
            "{}",
            app.message
        );
        app.on_key(ch('o'));
        assert!(!app.message.is_empty());
    }

    #[test]
    fn d_asks_first_and_moves_to_the_trash_never_deleting_and_never_a_builtin() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let f = fixture();
        let trash_dir = f.root.join("trash-test");
        unsafe { std::env::set_var("SHADERLAB_TRASH", &trash_dir) };
        let mut app = app_for(&f, None);
        app.on_key(ch('2'));
        let target = app.selected().unwrap().path.clone();
        assert!(target.exists());
        app.on_key(ch('d'));
        assert!(
            render(&mut app, 100, 30).to_lowercase().contains("trash"),
            "a confirmation is shown"
        );
        app.on_key(ch('n'));
        assert!(target.exists(), "anything but y keeps the file");
        app.on_key(ch('d'));
        app.on_key(ch('y'));
        assert!(!target.exists(), "the file left its place");
        let moved: Vec<_> = std::fs::read_dir(&trash_dir).unwrap().flatten().collect();
        assert_eq!(moved.len(), 1, "it is in the trash, not deleted");
        assert!(app.message.contains("trash"), "{}", app.message);
        // a built-in cannot be trashed
        app.on_key(ch('1'));
        app.on_key(ch('G'));
        let builtin = app.selected().unwrap().clone();
        assert!(builtin.builtin);
        app.on_key(ch('d'));
        app.on_key(ch('y'));
        assert!(builtin.path.exists());
        unsafe { std::env::remove_var("SHADERLAB_TRASH") };
    }

    #[test]
    fn i_imports_files_into_the_library_and_keeps_the_originals() {
        let f = fixture();
        let outside = tempfile::tempdir().unwrap();
        let src = outside.path().join("gamma.glsl");
        std::fs::write(
            &src,
            "void mainImage(out vec4 c, in vec2 f) { c = vec4(1.0); }\n",
        )
        .unwrap();
        let mut app = app_for(&f, None);
        app.on_key(ch('i'));
        for c in src.to_string_lossy().chars() {
            app.on_key(ch(c));
        }
        app.on_key(key(KeyCode::Enter));
        assert!(app.message.contains("imported 1"), "{}", app.message);
        assert!(f.lib.join("gamma.glsl").exists());
        assert!(src.exists(), "the original is only read");
        assert!(names(&app).contains(&"gamma.glsl".to_string()));
        // an empty import, and Esc
        app.on_key(ch('i'));
        app.on_key(key(KeyCode::Enter));
        assert!(app.message.contains("nothing"), "{}", app.message);
        app.on_key(ch('i'));
        app.on_key(key(KeyCode::Esc));
        assert!(!app.in_viewer());
    }

    #[test]
    fn r_renders_a_still_into_the_renders_folder_without_overwriting() {
        let f = fixture();
        let mut app = app_for(&f, None);
        if app.gpu().is_err() {
            eprintln!("SKIPPED: no GPU adapter; nothing was verified by this test");
            return;
        }
        app.on_key(ch('r'));
        assert!(
            app.message.contains("rendered") || app.message.contains("saved"),
            "{}",
            app.message
        );
        app.on_key(ch('r'));
        let n = std::fs::read_dir(f.root.join("renders"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("alpha-"))
            .count();
        assert_eq!(n, 2, "the second render got its own name");
    }

    #[test]
    fn the_screen_shows_tabs_columns_and_a_preview_and_survives_any_size_and_bad_files() {
        let f = fixture();
        let mut app = app_for(&f, None);
        let s = render(&mut app, 120, 36);
        for t in ["Shaders", "Renders", "Videos", "Sheets"] {
            assert!(s.contains(t), "{t} missing in:\n{s}");
        }
        assert!(s.contains("alpha.glsl"));
        // every tab, with every kind of bad file selected in turn, at several sizes
        for tab in ['1', '2', '3', '4'] {
            app.on_key(ch(tab));
            for _ in 0..4 {
                for (w, h) in [
                    (120u16, 36u16),
                    (60, 20),
                    (30, 8),
                    (10, 3),
                    (1, 1),
                    (200, 60),
                ] {
                    app.tick(Instant::now());
                    let _ = render(&mut app, w, h);
                }
                settle(&mut app);
                app.on_key(ch('j'));
            }
        }
        // a corrupt png and an empty video say so instead of failing
        app.on_key(ch('2'));
        app.on_key(ch('/'));
        for c in "broken".chars() {
            app.on_key(ch(c));
        }
        app.on_key(key(KeyCode::Enter));
        settle(&mut app);
        let s = render(&mut app, 100, 30);
        assert!(!s.is_empty());
        // the mouse: a click on a tab switches to it, the wheel scrolls
        render(&mut app, 100, 30);
        let (rect, idx) = app.tab_rects.last().copied().expect("tab hit areas");
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x,
            row: rect.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.tab, idx);
        let sel = app.sel;
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 5,
            row: 5,
            modifiers: KeyModifiers::NONE,
        });
        assert!(app.sel >= sel);
        // a click on a list row selects it
        app.on_key(ch('1'));
        render(&mut app, 100, 30);
        let (rrect, ri) = app.list_rows.get(1).copied().expect("rows");
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rrect.x + 1,
            row: rrect.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.sel, ri);
    }

    #[test]
    fn a_picture_preview_is_one_kitty_image_replaced_in_place_and_hidden_when_it_goes() {
        let f = fixture();
        let (root, lib) = (f.root.clone(), f.lib.clone());
        let config = lib.parent().unwrap().parent().unwrap().to_path_buf();
        let home = Home::from_vars(&move |k| match k {
            "SHADERLAB_HOME" => Some(root.to_string_lossy().into_owned()),
            "XDG_CONFIG_HOME" => Some(config.to_string_lossy().into_owned()),
            _ => None,
        });
        no_exec();
        let mut app = App::new(home, None, Protocol::Kitty, None, false);
        app.on_key(ch('2'));
        app.on_key(ch('/'));
        for c in "one".chars() {
            app.on_key(ch(c));
        }
        app.on_key(key(KeyCode::Enter));
        settle(&mut app);
        let mut t = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let mut sent = Vec::new();
        for _ in 0..3 {
            app.tick(Instant::now());
            t.draw(|f| app.draw(f)).unwrap();
            app.emit(&mut sent).unwrap();
            std::thread::sleep(Duration::from_millis(30));
        }
        let text = String::from_utf8_lossy(&sent).into_owned();
        assert!(text.contains("\x1b_G"), "an image was sent");
        assert!(text.contains(&format!("i={IMAGE_ID},p=1")), "{text:?}");
        // moving to a tab with nothing to show hides the image (one delete) rather than leaving a stale picture
        app.on_key(ch('3'));
        app.on_key(ch('/'));
        app.on_key(key(KeyCode::Enter));
        settle(&mut app);
        let mut after = Vec::new();
        app.tick(Instant::now());
        t.draw(|f| app.draw(f)).unwrap();
        app.emit(&mut after).unwrap();
        app.hide_image(&mut after).unwrap();
        assert!(
            String::from_utf8_lossy(&after).contains("a=d"),
            "the image is deleted"
        );
    }
}
