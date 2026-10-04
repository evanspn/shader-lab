//! `shaderlab preview`: the shader running live INSIDE the terminal.
//!
//! The picture is rendered on the GPU at the size of the pane and shown with the best protocol the terminal speaks: kitty
//! graphics (Ghostty, kitty, WezTerm), sixel, or truecolor half-block characters. A Ratatui panel beside it shows the
//! shader's parameters and presets; keys and the mouse change them and the shader hot-reloads when the file is saved.

use std::cell::RefCell;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use ratatui::Frame as UiFrame;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget};

use crate::frame::Frame;
use crate::gpu::{self, Gpu, Origin, Prepared};
use crate::params::{self, Kind};
use crate::preview::{Action, FileWatcher, PKey, PreviewState};
use crate::termimg::{self, Protocol};
use crate::video;

pub struct TuiOptions {
    pub file: PathBuf,
    pub preset: Option<String>,
    pub sets: Vec<String>,
    /// `None` = detect
    pub protocol: Option<Protocol>,
    /// `sample` or the path of a PNG
    pub text: String,
    pub origin: Origin,
    pub fps: u32,
}

const KITTY_ID: u32 = 4242;
/// The largest picture sent over the terminal link, per protocol.
const KITTY_MAX: (u32, u32) = (960, 540);
const SIXEL_MAX: (u32, u32) = (640, 360);
const PANEL_WIDTH: u16 = 38;
/// Half-block pictures are rendered this many times finer than the cell grid.
pub const SS: u32 = 4;

// ---- layout ------------------------------------------------------------------------------------

/// Where everything goes for a terminal of `area`. Pure, so it is testable and never panics on tiny sizes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout2 {
    /// The bordered box around the picture.
    pub pane: Rect,
    /// The picture's cells (inside the border).
    pub image: Rect,
    pub panel: Rect,
    pub footer: Rect,
}

pub fn layout(area: Rect) -> Layout2 {
    let footer_h = if area.height >= 8 { 2 } else { 0 };
    let [body, footer] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(footer_h)]).areas(area);
    let (pane, panel) = if body.width >= PANEL_WIDTH + 24 {
        let [a, b] =
            Layout::horizontal([Constraint::Min(10), Constraint::Length(PANEL_WIDTH)]).areas(body);
        (a, b)
    } else {
        // too narrow for a side panel: the picture only
        (body, Rect::new(body.x, body.y, 0, 0))
    };
    let image = if pane.width >= 3 && pane.height >= 3 {
        Block::bordered().inner(pane)
    } else {
        Rect::new(pane.x, pane.y, 0, 0)
    };
    Layout2 {
        pane,
        image,
        panel,
        footer,
    }
}

/// The pixel size to render for `image` cells: exact cells x 2 for half-blocks, the real cell size for graphics (capped).
pub fn pixel_size(proto: Protocol, image: Rect, cell_px: (f32, f32)) -> (u32, u32) {
    let (cols, rows) = (image.width.max(1) as f32, image.height.max(1) as f32);
    match proto {
        // rendered 4x finer than the cell grid and averaged down: the sample terminal's text stays readable
        Protocol::HalfBlocks => (cols as u32 * SS, rows as u32 * 2 * SS),
        Protocol::Kitty | Protocol::Sixel => {
            let (w, h) = (cols * cell_px.0, rows * cell_px.1);
            let max = if proto == Protocol::Kitty {
                KITTY_MAX
            } else {
                SIXEL_MAX
            };
            let k = (max.0 as f32 / w).min(max.1 as f32 / h).min(1.0);
            (
                ((w * k).round() as u32).max(16) & !1,
                ((h * k).round() as u32).max(16) & !1,
            )
        }
    }
}

// ---- the picture widget (half-blocks) -------------------------------------------------------------

/// Draws RGBA pixels as upper-half-block characters: the cell's foreground is the upper pixel, its background the lower.
pub struct HalfBlocks<'a> {
    pub rgba: &'a [u8],
    pub width: u32,
    pub height: u32,
    pub truecolor: bool,
}

impl Widget for HalfBlocks<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let color = |c: [u8; 3]| {
            if self.truecolor {
                Color::Rgb(c[0], c[1], c[2])
            } else {
                Color::Indexed(termimg::nearest_256(c))
            }
        };
        // an image rendered SS times finer than the cells is averaged down; anything else maps 1:1
        let ss =
            if self.width >= area.width as u32 * SS && self.height >= area.height as u32 * 2 * SS {
                SS
            } else {
                1
            };
        for cy in 0..area.height {
            for cx in 0..area.width {
                let (up, down) = termimg::half_block_pair_avg(
                    self.rgba,
                    self.width,
                    self.height,
                    cx as u32,
                    cy as u32,
                    ss,
                );
                if let Some(cell) = buf.cell_mut((area.x + cx, area.y + cy)) {
                    cell.set_char('\u{2580}')
                        .set_fg(color(up))
                        .set_bg(color(down));
                }
            }
        }
    }
}

// ---- the color picker ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Picker {
    pub idx: usize,
    pub original: String,
    pub h: f32,
    pub s: f32,
    pub v: f32,
    /// 0 = hue, 1 = saturation, 2 = brightness
    pub focus: usize,
    drag: Option<usize>,
}

impl Picker {
    pub fn new(idx: usize, hex: &str) -> Picker {
        let (r, g, b) = params::hex_rgb(hex).unwrap_or((255, 255, 255));
        let (h, s, v) = crate::preview::rgb_to_hsv(r, g, b);
        Picker {
            idx,
            original: hex.to_string(),
            h,
            s,
            v,
            focus: 0,
            drag: None,
        }
    }

    pub fn hex(&self) -> String {
        let (r, g, b) = crate::preview::hsv_to_rgb(self.h, self.s, self.v);
        format!("#{r:02x}{g:02x}{b:02x}")
    }

    fn set(&mut self, bar: usize, fraction: f32) {
        let f = fraction.clamp(0.0, 1.0);
        match bar {
            0 => self.h = (f * 360.0).min(359.99),
            1 => self.s = f,
            _ => self.v = f,
        }
    }

    fn get(&self, bar: usize) -> f32 {
        match bar {
            0 => self.h / 360.0,
            1 => self.s,
            _ => self.v,
        }
    }
}

// ---- the app --------------------------------------------------------------------------------------

#[derive(Default)]
struct Hits {
    /// (the whole row, the bar or swatch part, parameter index)
    params: Vec<(Rect, Rect, usize)>,
    presets: Vec<(Rect, Option<usize>)>,
    /// the picker's three bars
    picker_bars: Vec<Rect>,
}

struct App {
    opts: TuiOptions,
    gpu: Gpu,
    state: PreviewState,
    watcher: FileWatcher,
    src: String,
    proto: Protocol,
    note: Option<String>,
    truecolor: bool,
    prepared: Option<Prepared>,
    prep_size: (u32, u32),
    prep_terminal: bool,
    image: Vec<u8>,
    dirty: bool,
    error: Option<String>,
    message: String,
    picker: Option<Picker>,
    drag_param: Option<usize>,
    hits: RefCell<Hits>,
    cell_px: (f32, f32),
    quit: bool,
    saved: u32,
    frame_no: i32,
    fps: f32,
    ms: f32,
    meter: (Instant, u32),
    kitty_shown: bool,
}

fn style_dim() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn accent() -> Style {
    Style::default().fg(Color::Cyan)
}

fn rgb_of(hex: &str) -> Color {
    params::hex_rgb(hex).map_or(Color::White, |(r, g, b)| Color::Rgb(r, g, b))
}

impl App {
    fn name(&self) -> String {
        self.opts
            .file
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "shader".into())
    }

    fn terminal_frame(&self, size: (u32, u32)) -> Result<Frame> {
        let base = if self.opts.text == "sample" {
            Frame::sample(size.0, size.1)
        } else {
            Frame::from_png(Path::new(&self.opts.text))?
        };
        Ok(if self.state.show_terminal {
            base
        } else {
            base.without_text()
        })
    }

    /// (Re)compile at the wanted size. On failure the last good picture keeps running and the error is shown.
    fn rebuild(&mut self, size: (u32, u32)) {
        let frame = match self.terminal_frame(size) {
            Ok(f) => f,
            Err(e) => {
                self.error = Some(format!("{e:#}"));
                return;
            }
        };
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
                self.prep_size = (frame.width, frame.height);
                self.prep_terminal = self.state.show_terminal;
                self.prepared = Some(p);
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
    }

    fn reload_file(&mut self) {
        match std::fs::read_to_string(&self.opts.file) {
            Ok(src) => match params::parse_schema(params::strip_header(&src)) {
                Ok(schema) => {
                    self.src = src;
                    self.state.schema = schema;
                    if self
                        .state
                        .preset
                        .is_some_and(|i| i >= self.state.schema.presets.len())
                    {
                        self.state.preset = None;
                    }
                    let known: Vec<String> = self
                        .state
                        .schema
                        .params
                        .iter()
                        .map(|p| p.name.clone())
                        .collect();
                    self.state.sets.retain(|s| {
                        s.split_once('=')
                            .is_some_and(|(k, _)| known.iter().any(|n| n == k))
                    });
                    self.state.param_sel = self.state.param_sel.min(known.len().saturating_sub(1));
                    self.message = "reloaded".into();
                    self.dirty = true;
                }
                Err(e) => self.error = Some(e),
            },
            Err(e) => self.error = Some(format!("cannot read the file: {e}")),
        }
    }

    fn apply_action(&mut self, a: Action) {
        match a {
            Action::None => {}
            Action::Say(m) => self.message = m,
            Action::Rebuild(m) => {
                self.message = m;
                self.dirty = true;
            }
            Action::Quit => self.quit = true,
            Action::SavePng => self.save_png(),
            Action::Record => self.record(),
        }
    }

    fn save_png(&mut self) {
        let Some(p) = self.prepared.as_ref() else {
            return;
        };
        let mut buf = Vec::new();
        if let Err(e) = p.draw_rgba8(
            &self.gpu,
            self.state.time,
            1.0 / 30.0,
            self.frame_no,
            &mut buf,
        ) {
            self.message = format!("could not save: {e}");
            return;
        }
        self.saved += 1;
        let path = PathBuf::from(format!("{}-preview-{}.png", self.name(), self.saved));
        let (w, h) = p.size();
        self.message = match image::save_buffer(&path, &buf, w, h, image::ColorType::Rgba8) {
            Ok(()) => format!("saved {}", path.display()),
            Err(e) => format!("could not save {}: {e}", path.display()),
        };
    }

    fn record(&mut self) {
        let Some(p) = self.prepared.as_ref() else {
            return;
        };
        let ffmpeg = video::find_ffmpeg();
        let (format, _) = video::choose_format(None, None, ffmpeg.is_some())
            .unwrap_or((video::Format::Gif, None));
        self.saved += 1;
        let out = PathBuf::from(format!(
            "{}-preview-{}.{}",
            self.name(),
            self.saved,
            format.extension()
        ));
        let r = video::render_video(
            &self.gpu,
            p,
            &video::VideoOptions {
                fps: 24,
                duration: 5.0,
                start: self.state.time,
                format,
                out,
                loop_seamless: false,
                ffmpeg,
            },
        );
        self.message = match r {
            Ok(r) => format!("recorded {} ({} frames)", r.path.display(), r.frames),
            Err(e) => format!("recording failed: {e:#}"),
        };
    }

    fn open_picker(&mut self, idx: usize) {
        if let (Some(p), Some(v)) = (
            self.state.schema.params.get(idx),
            self.state.effective().get(idx),
        ) && matches!(p.kind, Kind::Color)
        {
            self.state.param_sel = idx;
            self.picker = Some(Picker::new(idx, v));
        }
    }

    fn picker_changed(&mut self) {
        if let Some(p) = &self.picker {
            let hex = p.hex();
            if self.state.set_value(p.idx, &hex) {
                self.dirty = true;
            }
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        if let Some(p) = self.picker.as_mut() {
            let step = if shift { 0.1 } else { 0.02 };
            match key.code {
                KeyCode::Enter => {
                    self.picker = None;
                    self.message = "color set".into();
                }
                KeyCode::Esc => {
                    let (idx, orig) = (p.idx, p.original.clone());
                    self.picker = None;
                    self.state.set_value(idx, &orig);
                    self.dirty = true;
                    self.message = "color unchanged".into();
                }
                KeyCode::Up | KeyCode::BackTab => p.focus = (p.focus + 2) % 3,
                KeyCode::Down | KeyCode::Tab => p.focus = (p.focus + 1) % 3,
                KeyCode::Left | KeyCode::Right => {
                    let dir = if key.code == KeyCode::Left { -1.0 } else { 1.0 };
                    let f = p.focus;
                    let v = p.get(f) + dir * step;
                    if f == 0 {
                        p.h = (p.h + dir * step * 360.0).rem_euclid(360.0);
                    } else {
                        p.set(f, v);
                    }
                    self.picker_changed();
                }
                _ => {}
            }
            return;
        }
        let pk = match key.code {
            KeyCode::Char(' ') => Some(PKey::Space),
            KeyCode::Char(c) => Some(PKey::Char(c)),
            KeyCode::Left if shift => Some(PKey::BigLeft),
            KeyCode::Right if shift => Some(PKey::BigRight),
            KeyCode::Left => Some(PKey::Left),
            KeyCode::Right => Some(PKey::Right),
            KeyCode::Up => Some(PKey::Up),
            KeyCode::Down => Some(PKey::Down),
            KeyCode::Esc => Some(PKey::Escape),
            KeyCode::Enter => Some(PKey::Enter),
            _ => None,
        };
        match pk {
            Some(PKey::Enter) => self.open_picker(self.state.param_sel),
            Some(k) => {
                let a = self.state.key(k);
                self.apply_action(a);
            }
            None => {}
        }
    }

    fn on_mouse(&mut self, m: MouseEvent) {
        let (x, y) = (m.column, m.row);
        let inside = |r: Rect| {
            r.width > 0 && x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
        };
        let frac = |r: Rect| ((x.saturating_sub(r.x)) as f32 + 0.5) / r.width.max(1) as f32;
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if self.picker.is_some() {
                    let bars = self.hits.borrow().picker_bars.clone();
                    if let Some(i) = bars.iter().position(|r| inside(*r)) {
                        let f = frac(bars[i]);
                        if let Some(p) = self.picker.as_mut() {
                            p.focus = i;
                            p.drag = Some(i);
                            p.set(i, f);
                        }
                        self.picker_changed();
                    }
                    return;
                }
                let (params_hit, preset_hit) = {
                    let h = self.hits.borrow();
                    (
                        h.params.iter().find(|(row, _, _)| inside(*row)).copied(),
                        h.presets.iter().find(|(r, _)| inside(*r)).map(|(_, p)| *p),
                    )
                };
                if let Some((_, bar, idx)) = params_hit {
                    self.state.param_sel = idx;
                    match self.state.schema.params[idx].kind {
                        Kind::Float { .. } if inside(bar) => {
                            self.drag_param = Some(idx);
                            if self.state.set_fraction(idx, frac(bar) as f64) {
                                self.dirty = true;
                            }
                        }
                        Kind::Color if inside(bar) => self.open_picker(idx),
                        _ => {}
                    }
                } else if let Some(p) = preset_hit {
                    self.state.pick_preset(p);
                    self.message = format!(
                        "preset: {}",
                        self.state.preset_name().unwrap_or("(defaults)")
                    );
                    self.dirty = true;
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(i) = self.picker.as_ref().and_then(|p| p.drag) {
                    let r = self.hits.borrow().picker_bars.get(i).copied();
                    if let (Some(r), Some(p)) = (r, self.picker.as_mut()) {
                        p.set(i, frac(r));
                    }
                    self.picker_changed();
                } else if let Some(idx) = self.drag_param {
                    let bar = self
                        .hits
                        .borrow()
                        .params
                        .iter()
                        .find(|(_, _, i)| *i == idx)
                        .map(|(_, b, _)| *b);
                    if let Some(bar) = bar
                        && self.state.set_fraction(idx, frac(bar) as f64)
                    {
                        self.dirty = true;
                    }
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.drag_param = None;
                if let Some(p) = self.picker.as_mut() {
                    p.drag = None;
                }
            }
            _ => {}
        }
    }

    // ---- drawing ----------------------------------------------------------------------------

    fn draw(&self, f: &mut UiFrame) {
        let lay = layout(f.area());
        self.draw_pane(f, &lay);
        self.draw_panel(f, &lay);
        self.draw_footer(f, &lay);
        if let Some(p) = &self.picker {
            self.draw_picker(f, p);
        }
    }

    fn draw_pane(&self, f: &mut UiFrame, lay: &Layout2) {
        if lay.pane.width == 0 || lay.pane.height == 0 {
            return;
        }
        let block = Block::bordered()
            .title(format!(" {} ", self.name()))
            .border_style(style_dim());
        f.render_widget(block, lay.pane);
        if lay.image.width == 0 || lay.image.height == 0 {
            return;
        }
        match self.proto {
            Protocol::HalfBlocks => {
                let (w, h) = self.prep_size;
                if !self.image.is_empty() && self.image.len() == (w * h * 4) as usize {
                    f.render_widget(
                        HalfBlocks {
                            rgba: &self.image,
                            width: w,
                            height: h,
                            truecolor: self.truecolor,
                        },
                        lay.image,
                    );
                } else {
                    f.render_widget(Clear, lay.image);
                }
            }
            // the picture itself is written after the cells; the cells under it stay blank
            _ => f.render_widget(Clear, lay.image),
        }
        if self.error.is_some() && self.prepared.is_none() {
            f.render_widget(
                Paragraph::new("the shader does not compile: see the panel")
                    .style(Style::default().fg(Color::Red)),
                lay.image,
            );
        }
    }

    fn draw_panel(&self, f: &mut UiFrame, lay: &Layout2) {
        let mut hits = self.hits.borrow_mut();
        hits.params.clear();
        hits.presets.clear();
        if lay.panel.width < 8 || lay.panel.height < 3 {
            return;
        }
        let block = Block::bordered()
            .title(" shaderlab ")
            .border_style(style_dim());
        let inner = block.inner(lay.panel);
        f.render_widget(block, lay.panel);
        let st = &self.state;
        let mut lines: Vec<Line> = vec![
            Line::from(vec![Span::styled(
                self.name(),
                accent().add_modifier(Modifier::BOLD),
            )]),
            Line::from(format!(
                "preset  {}",
                st.preset_name().unwrap_or("(defaults)")
            )),
            Line::from(format!(
                "t {:.1}s  x{:.2}{}{}",
                st.time,
                st.speed,
                if st.paused { "  PAUSED" } else { "" },
                if st.show_terminal { "" } else { "  black" }
            )),
            Line::from(format!(
                "{:.0} fps  {:.1} ms  {}x{}",
                self.fps, self.ms, self.prep_size.0, self.prep_size.1
            )),
            Line::styled(self.proto.name().to_string(), style_dim()),
        ];
        if let Some(e) = &self.error {
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                "SHADER ERROR (last good keeps running)",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ));
            for l in e.lines().take(4) {
                lines.push(Line::styled(l.to_string(), Style::default().fg(Color::Red)));
            }
        }
        lines.push(Line::raw(""));
        lines.push(Line::styled("parameters", style_dim()));
        let header_rows = lines.len() as u16;
        let effective = st.effective();
        let max_rows = inner.height.saturating_sub(header_rows + 2) as usize;
        let n = st.schema.params.len();
        // keep the selected row visible
        let first = if n <= max_rows.max(1) {
            0
        } else {
            st.param_sel
                .saturating_sub(max_rows.saturating_sub(1))
                .min(n - max_rows.max(1))
        };
        let mut y = inner.y + header_rows;
        let bottom = inner.y + inner.height;
        f.render_widget(Paragraph::new(lines), inner);
        for i in first..n.min(first + max_rows) {
            if y >= bottom {
                break;
            }
            let p = &st.schema.params[i];
            let val = effective.get(i).cloned().unwrap_or_default();
            let sel = i == st.param_sel;
            let row = Rect::new(inner.x, y, inner.width, 1);
            let label_w = 12.min(inner.width as usize / 2);
            let label: String = p.label.chars().take(label_w).collect();
            let mut spans = vec![Span::styled(
                format!(
                    "{}{:<w$}",
                    if sel { "\u{25b8}" } else { " " },
                    label,
                    w = label_w
                ),
                if sel {
                    accent().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                },
            )];
            let used = 1 + label_w as u16 + 1;
            let bar = match p.kind {
                Kind::Float { min, max } => {
                    let width = inner.width.saturating_sub(used + 7).clamp(4, 14);
                    let frac = ((val.parse::<f64>().unwrap_or(min) - min) / (max - min).max(1e-9))
                        .clamp(0.0, 1.0);
                    let filled = (frac * width as f64).round() as usize;
                    spans.push(Span::raw(format!(" {:>5} ", short(&val))));
                    spans.push(Span::styled("\u{2588}".repeat(filled), accent()));
                    spans.push(Span::styled(
                        "\u{2591}".repeat(width as usize - filled),
                        style_dim(),
                    ));
                    Rect::new(row.x + used + 7, y, width, 1)
                }
                Kind::Color => {
                    spans.push(Span::raw(" "));
                    spans.push(Span::styled(
                        "\u{2588}\u{2588}",
                        Style::default().fg(rgb_of(&val)),
                    ));
                    spans.push(Span::raw(format!(" {val}")));
                    Rect::new(row.x + used, y, 2, 1)
                }
            };
            let line = Line::from(spans);
            let style = if sel {
                Style::default().bg(Color::Rgb(30, 34, 44))
            } else {
                Style::default()
            };
            f.render_widget(Paragraph::new(line).style(style), row);
            hits.params.push((row, bar.intersection(row), i));
            y += 1;
        }
        if st.schema.presets.is_empty() || y + 2 > bottom {
            self.draw_message(f, inner);
            return;
        }
        f.render_widget(
            Paragraph::new(Line::styled("presets", style_dim())),
            Rect::new(inner.x, y, inner.width, 1),
        );
        y += 1;
        let mut items: Vec<(Option<usize>, String)> = vec![(None, "(defaults)".into())];
        items.extend(
            st.schema
                .presets
                .iter()
                .enumerate()
                .map(|(i, p)| (Some(i), p.name.clone())),
        );
        for (id, name) in items {
            if y >= bottom.saturating_sub(1) {
                break;
            }
            let on = st.preset == id;
            let row = Rect::new(inner.x, y, inner.width, 1);
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::raw(if on { "(*) " } else { "( ) " }),
                    Span::styled(name, if on { accent() } else { Style::default() }),
                ])),
                row,
            );
            hits.presets.push((row, id));
            y += 1;
        }
        self.draw_message(f, inner);
    }

    fn draw_message(&self, f: &mut UiFrame, inner: Rect) {
        if self.message.is_empty() || inner.height < 2 {
            return;
        }
        let row = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
        f.render_widget(
            Paragraph::new(self.message.clone()).style(Style::default().fg(Color::Yellow)),
            row,
        );
    }

    fn draw_footer(&self, f: &mut UiFrame, lay: &Layout2) {
        if lay.footer.height == 0 {
            return;
        }
        let mut lines = vec![Line::styled(
            "space pause  [ ] speed  R reset  T frame  p/P preset  O opacity  \u{2191}\u{2193} param  \u{2190}\u{2192} change  Enter color  S save  V rec  Q quit",
            style_dim(),
        )];
        if let Some(n) = &self.note {
            lines.push(Line::styled(
                format!("note: {n}"),
                Style::default().fg(Color::Yellow),
            ));
        }
        f.render_widget(Paragraph::new(lines), lay.footer);
    }

    fn draw_picker(&self, f: &mut UiFrame, p: &Picker) {
        let area = f.area();
        let (w, h) = (46.min(area.width), 9.min(area.height));
        if w < 20 || h < 7 {
            return;
        }
        let r = Rect::new(
            area.x + (area.width - w) / 2,
            area.y + (area.height - h) / 2,
            w,
            h,
        );
        f.render_widget(Clear, r);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" color ")
            .border_style(accent());
        let inner = block.inner(r);
        f.render_widget(block, r);
        let label_w = 4;
        let bar_w = inner.width.saturating_sub(label_w + 1);
        let mut hits = self.hits.borrow_mut();
        hits.picker_bars.clear();
        for (i, name) in ["hue", "sat", "val"].iter().enumerate() {
            let y = inner.y + i as u16 * 2;
            let bar = Rect::new(inner.x + label_w, y, bar_w, 1);
            let focus = p.focus == i;
            f.render_widget(
                Paragraph::new(Span::styled(
                    format!("{name:<4}"),
                    if focus {
                        accent().add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    },
                )),
                Rect::new(inner.x, y, label_w, 1),
            );
            let buf = f.buffer_mut();
            let marker = (p.get(i) * bar_w.saturating_sub(1) as f32).round() as u16;
            for x in 0..bar_w {
                let t = x as f32 / bar_w.max(2).saturating_sub(1) as f32;
                let (cr, cg, cb) = match i {
                    0 => crate::preview::hsv_to_rgb(t * 359.0, p.s.max(0.35), p.v.max(0.5)),
                    1 => crate::preview::hsv_to_rgb(p.h, t, p.v.max(0.3)),
                    _ => crate::preview::hsv_to_rgb(p.h, p.s, t),
                };
                if let Some(c) = buf.cell_mut((bar.x + x, bar.y)) {
                    c.set_char(if x == marker { '\u{2502}' } else { ' ' })
                        .set_bg(Color::Rgb(cr, cg, cb))
                        .set_fg(Color::White);
                }
            }
            hits.picker_bars.push(bar);
        }
        let hex = p.hex();
        let line = Line::from(vec![
            Span::styled(
                "\u{2588}\u{2588}\u{2588}",
                Style::default().fg(rgb_of(&hex)),
            ),
            Span::raw(format!("  {hex}   Enter accept  Esc cancel")),
        ]);
        f.render_widget(
            Paragraph::new(line),
            Rect::new(inner.x, inner.y + 6, inner.width, 1),
        );
    }

    // ---- the picture over the link -----------------------------------------------------------------

    /// Write the picture for kitty/sixel at the pane (outside Ratatui's buffer, so nothing flickers).
    fn emit_image(&mut self, out: &mut impl Write, lay: &Layout2) -> std::io::Result<()> {
        if self.proto == Protocol::HalfBlocks {
            return Ok(());
        }
        let hidden = self.picker.is_some()
            || lay.image.width == 0
            || lay.image.height == 0
            || self.image.is_empty();
        if hidden {
            if self.kitty_shown && self.proto == Protocol::Kitty {
                out.write_all(&termimg::kitty_delete(KITTY_ID))?;
                self.kitty_shown = false;
            }
            return Ok(());
        }
        let (w, h) = self.prep_size;
        if self.image.len() != (w * h * 4) as usize {
            return Ok(());
        }
        let rgb = termimg::rgba_to_rgb(&self.image);
        // move to the pane's first cell (1-based), draw, and leave the cursor where the next Ratatui frame expects it
        write!(out, "\x1b7\x1b[{};{}H", lay.image.y + 1, lay.image.x + 1)?;
        match self.proto {
            Protocol::Kitty => {
                out.write_all(&termimg::kitty_image(
                    &rgb,
                    w,
                    h,
                    lay.image.width,
                    lay.image.height,
                    KITTY_ID,
                ))?;
                self.kitty_shown = true;
            }
            _ => out.write_all(&termimg::sixel(&rgb, w, h))?,
        }
        out.write_all(b"\x1b8")?;
        out.flush()
    }
}

fn short(v: &str) -> String {
    v.chars().take(5).collect()
}

// ---- running ----------------------------------------------------------------------------------------

fn cell_pixels() -> (f32, f32) {
    match ratatui::crossterm::terminal::window_size() {
        Ok(ws) if ws.width > 0 && ws.height > 0 && ws.columns > 0 && ws.rows > 0 => (
            ws.width as f32 / ws.columns as f32,
            ws.height as f32 / ws.rows as f32,
        ),
        _ => (8.0, 16.0),
    }
}

fn truecolor_supported() -> bool {
    std::env::var("COLORTERM").is_ok_and(|v| v == "truecolor" || v == "24bit")
        || matches!(
            std::env::var("TERM_PROGRAM").as_deref(),
            Ok("ghostty" | "WezTerm" | "iTerm.app" | "vscode")
        )
}

/// Run the terminal UI until the user quits. The terminal is restored on every exit path, including a panic.
pub fn run(opts: TuiOptions) -> Result<()> {
    let src = std::fs::read_to_string(&opts.file)
        .with_context(|| format!("reading {}", opts.file.display()))?;
    let schema = params::parse_schema(params::strip_header(&src)).map_err(|e| anyhow!(e))?;
    let state = PreviewState::new(schema, opts.preset.as_deref(), opts.sets.clone())
        .map_err(|e| anyhow!(e))?;
    let gpu = Gpu::new()?;
    let (proto, note) = match opts.protocol {
        Some(p) => (p, None),
        None => termimg::detect(&|k| std::env::var(k).ok()),
    };
    let mut app = App {
        watcher: FileWatcher::new(&opts.file),
        gpu,
        state,
        src,
        proto,
        note,
        truecolor: truecolor_supported(),
        prepared: None,
        prep_size: (0, 0),
        prep_terminal: true,
        image: Vec::new(),
        dirty: true,
        error: None,
        message: String::new(),
        picker: None,
        drag_param: None,
        hits: RefCell::new(Hits::default()),
        cell_px: (8.0, 16.0),
        quit: false,
        saved: 0,
        frame_no: 0,
        fps: 0.0,
        ms: 0.0,
        meter: (Instant::now(), 0),
        kitty_shown: false,
        opts,
    };
    // ratatui::init() enters raw mode and the alternate screen and installs a panic hook that restores them
    let mut terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableMouseCapture);
    let previous = std::panic::take_hook();
    let proto_for_hook = app.proto;
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_extras(proto_for_hook);
        previous(info);
    }));
    let result = event_loop(&mut terminal, &mut app);
    let _ = restore_extras(app.proto);
    ratatui::restore();
    result
}

fn restore_extras(proto: Protocol) -> std::io::Result<()> {
    let mut out = std::io::stdout();
    if proto == Protocol::Kitty {
        out.write_all(&termimg::kitty_delete(KITTY_ID))?;
    }
    execute!(out, DisableMouseCapture, ratatui::crossterm::cursor::Show)
}

fn event_loop(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
    let interval = Duration::from_secs_f32(1.0 / app.opts.fps.clamp(1, 60) as f32);
    let mut last = Instant::now();
    let mut last_check = Instant::now();
    let mut last_size = (0u16, 0u16);
    let mut next_frame = Instant::now();
    while !app.quit {
        // input: wait for the next frame time, so the work of drawing is part of the interval, not added to it
        let mut wait = next_frame.saturating_duration_since(Instant::now());
        next_frame = Instant::now().max(next_frame) + interval;
        while event::poll(wait)? {
            match event::read()? {
                Event::Key(k) if k.kind == KeyEventKind::Press => app.on_key(k),
                Event::Mouse(m) => app.on_mouse(m),
                _ => {}
            }
            wait = Duration::ZERO;
        }
        let area = terminal.size()?;
        let rect = Rect::new(0, 0, area.width, area.height);
        if (area.width, area.height) != last_size {
            last_size = (area.width, area.height);
            app.cell_px = cell_pixels();
            terminal.clear()?;
        }
        let lay = layout(rect);
        let now = Instant::now();
        if now - last_check > Duration::from_millis(200) {
            last_check = now;
            if app.watcher.changed() {
                app.reload_file();
            }
        }
        // the size to render at; a --text PNG fixes it
        let want = if app.opts.text == "sample" {
            pixel_size(app.proto, lay.image, app.cell_px)
        } else {
            app.prep_size.max((16, 16))
        };
        if lay.image.width > 0
            && lay.image.height > 0
            && (app.dirty
                || app.prepared.is_none()
                || want != app.prep_size
                || app.prep_terminal != app.state.show_terminal)
        {
            app.dirty = false;
            let size = if app.opts.text == "sample" {
                want
            } else {
                (0, 0)
            };
            let size = if size == (0, 0) {
                app.terminal_frame((16, 16))
                    .map(|f| (f.width, f.height))
                    .unwrap_or((16, 16))
            } else {
                size
            };
            app.rebuild(size);
        }
        let dt = (now - last).as_secs_f32().min(0.1);
        last = now;
        app.state.advance(dt);
        if lay.image.width > 0
            && lay.image.height > 0
            && let Some(p) = app.prepared.as_ref()
        {
            app.frame_no = app.frame_no.wrapping_add(1);
            let t0 = Instant::now();
            if p.draw_rgba8(
                &app.gpu,
                app.state.time,
                dt.max(1e-4),
                app.frame_no,
                &mut app.image,
            )
            .is_ok()
            {
                app.ms = app.ms * 0.9 + t0.elapsed().as_secs_f32() * 1000.0 * 0.1;
            }
        }
        terminal.draw(|f| app.draw(f))?;
        app.emit_image(terminal.backend_mut(), &lay)?;
        app.meter.1 += 1;
        if app.meter.0.elapsed() >= Duration::from_millis(500) {
            app.fps = app.meter.1 as f32 / app.meter.0.elapsed().as_secs_f32();
            app.meter = (Instant::now(), 0);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn gpu() -> Option<Gpu> {
        match Gpu::new() {
            Ok(g) => Some(g),
            Err(e) => {
                eprintln!("SKIPPED: {e}; nothing was verified by this test");
                None
            }
        }
    }

    fn app_for(g: Gpu, src: &str, proto: Protocol) -> App {
        let schema = params::parse_schema(src).unwrap();
        let opts = TuiOptions {
            file: PathBuf::from("demo.glsl"),
            preset: None,
            sets: vec![],
            protocol: Some(proto),
            text: "sample".into(),
            origin: Origin::TopLeft,
            fps: 30,
        };
        App {
            watcher: FileWatcher::new(Path::new("/nonexistent")),
            gpu: g,
            state: PreviewState::new(schema, None, vec![]).unwrap(),
            src: src.to_string(),
            proto,
            note: None,
            truecolor: true,
            prepared: None,
            prep_size: (0, 0),
            prep_terminal: true,
            image: Vec::new(),
            dirty: true,
            error: None,
            message: String::new(),
            picker: None,
            drag_param: None,
            hits: RefCell::new(Hits::default()),
            cell_px: (8.0, 16.0),
            quit: false,
            saved: 0,
            frame_no: 0,
            fps: 30.0,
            ms: 1.2,
            meter: (Instant::now(), 0),
            kitty_shown: false,
            opts,
        }
    }

    const DEMO: &str = "// @float opacity 1.0 0.0 1.0 \"Opacity\"\n// @color glow #33aaff \"Glow color\"\n// @float speed 1.0 0.0 3.0 \"Speed\"\n// @preset warm glow=#ff8800 speed=2\n// @preset cool glow=#0088ff\nvoid mainImage(out vec4 c, in vec2 p) { vec4 t = texture(iChannel0, p / iResolution.xy); c = vec4(mix(t.rgb, P_glow * (0.5 + 0.5 * sin(iTime * P_speed + p.x * 0.02)), 0.3 * P_opacity), 1.0); }\n";

    fn render(app: &mut App, w: u16, h: u16) -> Buffer {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        // render a frame the way the loop does
        let lay = layout(Rect::new(0, 0, w, h));
        if lay.image.width > 0 && lay.image.height > 0 {
            let size = pixel_size(app.proto, lay.image, app.cell_px);
            app.rebuild(size);
            if let Some(p) = app.prepared.as_ref() {
                p.draw_rgba8(&app.gpu, 3.0, 0.033, 1, &mut app.image)
                    .unwrap();
            }
        }
        t.draw(|f| app.draw(f)).unwrap();
        t.backend().buffer().clone()
    }

    fn text_of(buf: &Buffer) -> String {
        let w = buf.area.width as usize;
        buf.content()
            .chunks(w.max(1))
            .map(|r| r.iter().map(|c| c.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_layout_gives_the_picture_most_of_the_screen_and_survives_any_size() {
        let l = layout(Rect::new(0, 0, 120, 40));
        assert_eq!(l.panel.width, PANEL_WIDTH);
        assert_eq!(l.footer.height, 2);
        assert!(l.image.width > 70 && l.image.height > 30, "{l:?}");
        assert_eq!(l.pane.width + l.panel.width, 120);
        // narrow: no panel, still a picture
        let l = layout(Rect::new(0, 0, 50, 20));
        assert_eq!(l.panel.width, 0);
        assert!(l.image.width > 40);
        for (w, h) in [
            (0, 0),
            (1, 1),
            (2, 2),
            (10, 3),
            (3, 30),
            (500, 1),
            (80, 7),
            (65, 9),
        ] {
            let l = layout(Rect::new(0, 0, w, h));
            let area = Rect::new(0, 0, w, h);
            for r in [l.pane, l.image, l.panel, l.footer] {
                assert!(
                    r.width == 0 || r.height == 0 || area.contains((r.x, r.y).into()),
                    "{w}x{h}: {r:?}"
                );
                assert!(
                    r.x + r.width <= w && r.y + r.height <= h,
                    "{w}x{h}: {r:?} overflows"
                );
            }
        }
    }

    #[test]
    fn the_render_size_is_the_cells_for_half_blocks_and_capped_for_graphics() {
        let img = Rect::new(0, 0, 100, 30);
        assert_eq!(
            pixel_size(Protocol::HalfBlocks, img, (8.0, 16.0)),
            (100 * SS, 60 * SS)
        );
        let (w, h) = pixel_size(Protocol::Kitty, img, (8.0, 16.0));
        assert!(w <= 960 && h <= 540 && w % 2 == 0 && h % 2 == 0, "{w}x{h}");
        // aspect is kept: 800x480 fits
        assert_eq!(pixel_size(Protocol::Kitty, img, (8.0, 16.0)), (800, 480));
        let (w, h) = pixel_size(Protocol::Kitty, Rect::new(0, 0, 300, 100), (10.0, 20.0));
        assert!(w <= 960 && h <= 540);
        assert!((w as f32 / h as f32 - 1.5).abs() < 0.05, "aspect {w}x{h}");
        assert!(pixel_size(Protocol::Sixel, img, (8.0, 16.0)).0 <= 640);
        assert!(pixel_size(Protocol::HalfBlocks, Rect::new(0, 0, 0, 0), (8.0, 16.0)).0 >= 1);
    }

    #[test]
    fn the_layout_shows_the_name_parameters_presets_and_keys_with_the_selection_marked() {
        let Some(g) = gpu() else { return };
        let mut app = app_for(g, DEMO, Protocol::HalfBlocks);
        app.state.param_sel = 1;
        let buf = render(&mut app, 120, 36);
        let t = text_of(&buf);
        for want in [
            "demo",
            "preset  (defaults)",
            "parameters",
            "Opacity",
            "Glow color",
            "#33aaff",
            "Speed",
            "presets",
            "(*) (defaults)",
            "( ) warm",
            "( ) cool",
            "space pause",
            "Q quit",
        ] {
            assert!(t.contains(want), "missing '{want}' in:\n{t}");
        }
        assert!(
            t.contains("\u{25b8}Glow color"),
            "the selected row is marked:\n{t}"
        );
        assert!(t.contains("\u{2588}"), "a bar or swatch is drawn");
        // the picture is half-block characters with colors
        let hb = buf
            .content()
            .iter()
            .filter(|c| c.symbol() == "\u{2580}")
            .count();
        assert!(hb > 1000, "{hb} half-block cells");
        // hit areas exist for rows, bars and presets
        let h = app.hits.borrow();
        assert_eq!(h.params.len(), 3);
        assert_eq!(h.presets.len(), 3);
    }

    #[test]
    fn a_shader_error_is_shown_in_the_panel_and_the_last_good_picture_is_kept() {
        let Some(g) = gpu() else { return };
        let mut app = app_for(g, DEMO, Protocol::HalfBlocks);
        let _ = render(&mut app, 120, 36);
        assert!(app.prepared.is_some());
        app.src = "void mainImage(out vec4 c, in vec2 p) { c = no_such_fn(p); }\n".into();
        let buf = render(&mut app, 120, 36);
        let t = text_of(&buf);
        assert!(t.contains("SHADER ERROR"), "{t}");
        assert!(t.contains("no_such_fn"), "{t}");
        assert!(app.prepared.is_some(), "the last good shader keeps running");
    }

    #[test]
    fn nothing_panics_on_tiny_terminals() {
        let Some(g) = gpu() else { return };
        let mut app = app_for(g, DEMO, Protocol::HalfBlocks);
        for (w, h) in [
            (1, 1),
            (2, 2),
            (5, 3),
            (10, 3),
            (20, 5),
            (30, 8),
            (62, 9),
            (64, 40),
        ] {
            let _ = render(&mut app, w, h);
        }
        app.picker = Some(Picker::new(1, "#33aaff"));
        for (w, h) in [(1, 1), (10, 5), (30, 8), (46, 9), (120, 36)] {
            let _ = render(&mut app, w, h);
        }
    }

    #[test]
    fn keys_drive_the_state_and_the_picker_edits_the_color_live() {
        let Some(g) = gpu() else { return };
        let mut app = app_for(g, DEMO, Protocol::HalfBlocks);
        let key = |app: &mut App, c: KeyCode, m: KeyModifiers| app.on_key(KeyEvent::new(c, m));
        key(&mut app, KeyCode::Char(' '), KeyModifiers::NONE);
        assert!(app.state.paused);
        key(&mut app, KeyCode::Char(']'), KeyModifiers::NONE);
        assert!(app.state.speed > 1.0);
        key(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.state.param_sel, 1);
        // Enter on a color opens the picker; moving it changes the color; Esc restores it
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.picker.is_some());
        let before = app.state.effective()[1].clone();
        key(&mut app, KeyCode::Right, KeyModifiers::NONE);
        let during = app.state.effective()[1].clone();
        assert_ne!(before, during, "live while moving");
        key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.picker.is_none());
        assert_eq!(app.state.effective()[1], before, "Esc cancels");
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        key(&mut app, KeyCode::Right, KeyModifiers::SHIFT);
        let moved = app.state.effective()[1].clone();
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.state.effective()[1], moved, "Enter keeps it");
        // a number: shift makes a bigger step
        key(&mut app, KeyCode::Down, KeyModifiers::NONE);
        key(&mut app, KeyCode::Right, KeyModifiers::NONE);
        let small = app.state.effective()[2].parse::<f64>().unwrap();
        key(&mut app, KeyCode::Right, KeyModifiers::SHIFT);
        let big = app.state.effective()[2].parse::<f64>().unwrap();
        assert!(big - small > 0.5, "{small} -> {big}");
        key(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(app.quit);
    }

    #[test]
    fn the_mouse_sets_a_bar_picks_a_preset_and_opens_the_picker_on_a_swatch() {
        let Some(g) = gpu() else { return };
        let mut app = app_for(g, DEMO, Protocol::HalfBlocks);
        let _ = render(&mut app, 120, 36);
        let mouse = |app: &mut App, kind, x: u16, y: u16| {
            app.on_mouse(MouseEvent {
                kind,
                column: x,
                row: y,
                modifiers: KeyModifiers::NONE,
            })
        };
        let (speed_row, speed_bar) = {
            let h = app.hits.borrow();
            let (row, bar, _) = h.params[2];
            (row, bar)
        };
        assert!(speed_bar.width >= 4);
        // click the far right of the speed bar: the maximum
        mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            speed_bar.x + speed_bar.width - 1,
            speed_row.y,
        );
        assert!(app.state.effective()[2].parse::<f64>().unwrap() > 2.6);
        // drag back to the left
        mouse(
            &mut app,
            MouseEventKind::Drag(MouseButton::Left),
            speed_bar.x,
            speed_row.y,
        );
        assert!(app.state.effective()[2].parse::<f64>().unwrap() < 0.4);
        mouse(
            &mut app,
            MouseEventKind::Up(MouseButton::Left),
            speed_bar.x,
            speed_row.y,
        );
        assert!(app.drag_param.is_none());
        // click a preset
        let warm = app.hits.borrow().presets[1];
        mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            warm.0.x + 5,
            warm.0.y,
        );
        assert_eq!(app.state.preset_name(), Some("warm"));
        // click the color swatch
        let swatch = app.hits.borrow().params[1].1;
        mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            swatch.x,
            swatch.y,
        );
        assert!(app.picker.is_some());
        // click the hue bar of the picker
        let _ = render(&mut app, 120, 36);
        let hue = app.hits.borrow().picker_bars[0];
        let before = app.state.effective()[1].clone();
        mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            hue.x + hue.width / 2,
            hue.y,
        );
        mouse(
            &mut app,
            MouseEventKind::Up(MouseButton::Left),
            hue.x + hue.width / 2,
            hue.y,
        );
        assert_ne!(app.state.effective()[1], before);
    }

    #[test]
    fn the_tui_frame_can_be_dumped_as_a_picture_to_look_at() {
        // set SHADERLAB_DUMP_TUI=/some/path.png to write what the half-block TUI looks like
        let Some(g) = gpu() else { return };
        let Some(out) = std::env::var_os("SHADERLAB_DUMP_TUI") else {
            return;
        };
        let src = std::fs::read_to_string(
            std::env::var("SHADERLAB_DUMP_SHADER")
                .unwrap_or_else(|_| "examples/shaders/rain-down.glsl".into()),
        )
        .unwrap();
        let mut app = app_for(g, &src, Protocol::HalfBlocks);
        let buf = render(&mut app, 120, 36);
        // each cell is 8x16 px: a half-block cell paints its upper half with fg and lower with bg; other cells are text on bg
        let (cw, ch) = (8u32, 16u32);
        let (w, h) = (buf.area.width as u32 * cw, buf.area.height as u32 * ch);
        let mut img = image::RgbaImage::new(w, h);
        let rgb = |c: Color, default: [u8; 3]| match c {
            Color::Rgb(r, g, b) => [r, g, b],
            Color::Indexed(i) => [i, i, i],
            _ => default,
        };
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                let cell = &buf[(x, y)];
                let (up, down) = if cell.symbol() == "\u{2580}" {
                    (rgb(cell.fg, [255, 255, 255]), rgb(cell.bg, [0, 0, 0]))
                } else {
                    let bg = rgb(cell.bg, [12, 12, 16]);
                    let fg = if cell.symbol() == " " {
                        bg
                    } else {
                        rgb(cell.fg, [200, 200, 200])
                    };
                    (if cell.symbol() == " " { bg } else { fg }, bg)
                };
                for py in 0..ch {
                    let c = if py < ch / 2 { up } else { down };
                    for px in 0..cw {
                        img.put_pixel(
                            x as u32 * cw + px,
                            y as u32 * ch + py,
                            image::Rgba([c[0], c[1], c[2], 255]),
                        );
                    }
                }
            }
        }
        img.save(out).unwrap();
    }
}
