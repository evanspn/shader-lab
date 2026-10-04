//! `shaderlab preview`: a live window that runs a shader in real time over the synthetic terminal frame.
//!
//! The logic that decides what a key does and when a file changed ([`PreviewState`], [`FileWatcher`]) is plain code with
//! unit tests; the window itself (winit + wgpu surface) is the `preview` feature.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::params::{self, Kind, Schema};

/// A key press, independent of the windowing library.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PKey {
    Char(char),
    Space,
    Left,
    Right,
    Up,
    Down,
    Escape,
    Enter,
    /// Shift + left / right: a bigger step
    BigLeft,
    BigRight,
}

/// What the window should do after a key.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    None,
    /// The shader text or the terminal frame changed: rebuild the pipeline.
    Rebuild(String),
    /// Something to print and show in the title.
    Say(String),
    SavePng,
    Record,
    Quit,
}

/// Everything the keys change. No GPU, no window.
#[derive(Debug)]
pub struct PreviewState {
    pub schema: Schema,
    /// Index into `schema.presets`; `None` = the defaults.
    pub preset: Option<usize>,
    /// Manual `name=value` overrides, applied on top of the preset.
    pub sets: Vec<String>,
    pub time: f32,
    pub speed: f32,
    pub paused: bool,
    pub show_terminal: bool,
    pub param_sel: usize,
}

impl PreviewState {
    pub fn new(
        schema: Schema,
        preset: Option<&str>,
        sets: Vec<String>,
    ) -> Result<PreviewState, String> {
        let preset_idx = match preset {
            None => None,
            Some(name) => Some(
                schema
                    .presets
                    .iter()
                    .position(|p| p.name == name)
                    .ok_or_else(|| format!("no preset '{name}'"))?,
            ),
        };
        let st = PreviewState {
            schema,
            preset: preset_idx,
            sets,
            time: 0.0,
            speed: 1.0,
            paused: false,
            show_terminal: true,
            param_sel: 0,
        };
        st.values()?; // reject a bad --set up front
        Ok(st)
    }

    pub fn preset_name(&self) -> Option<&str> {
        self.preset.map(|i| self.schema.presets[i].name.as_str())
    }

    pub fn values(&self) -> Result<BTreeMap<String, String>, String> {
        params::values_from_args(&self.schema, self.preset_name(), &self.sets)
    }

    /// `name=value` of parameter `i`, as the effective (canonical) value.
    pub fn describe_param(&self, i: usize) -> Option<String> {
        let p = self.schema.params.get(i)?;
        let values = self.values().ok()?;
        let v = params::resolve(&self.schema, &values).get(i)?.clone();
        Some(format!("{} = {v}   ({})", p.name, p.label))
    }

    pub fn advance(&mut self, dt: f32) {
        if !self.paused {
            self.time += dt * self.speed;
        }
    }

    fn set_param(&mut self, name: &str, value: String) {
        self.sets
            .retain(|s| s.split_once('=').is_none_or(|(k, _)| k != name));
        self.sets.push(format!("{name}={value}"));
    }

    /// Set parameter `idx` to `value` (a number or `#rrggbb`); a value the schema rejects changes nothing.
    pub fn set_value(&mut self, idx: usize, value: &str) -> bool {
        let Some(p) = self.schema.params.get(idx).cloned() else {
            return false;
        };
        let before = self.sets.clone();
        self.set_param(&p.name, value.to_string());
        if self.values().is_err() {
            self.sets = before;
            return false;
        }
        true
    }

    /// Set a number parameter from a position along its bar, 0.0..=1.0.
    pub fn set_fraction(&mut self, idx: usize, fraction: f64) -> bool {
        let Some(Kind::Float { min, max }) = self.schema.params.get(idx).map(|p| p.kind.clone())
        else {
            return false;
        };
        let v = min + (max - min) * fraction.clamp(0.0, 1.0);
        // two decimals of the range's own scale is plenty for a slider
        let step = ((max - min) / 200.0).max(1e-4);
        let v = (v / step).round() * step;
        self.set_value(idx, &params::format_number(v.clamp(min, max)))
    }

    pub fn pick_preset(&mut self, preset: Option<usize>) {
        if preset.is_none_or(|i| i < self.schema.presets.len()) {
            self.preset = preset;
            self.sets.clear();
        }
    }

    /// The effective value of every parameter, in order.
    pub fn effective(&self) -> Vec<String> {
        self.values()
            .map(|v| params::resolve(&self.schema, &v))
            .unwrap_or_default()
    }

    fn adjust(&mut self, dir: f32) -> Action {
        let Some(p) = self.schema.params.get(self.param_sel).cloned() else {
            return Action::Say("this shader has no parameters".into());
        };
        let Ok(values) = self.values() else {
            return Action::None;
        };
        let current = params::resolve(&self.schema, &values)[self.param_sel].clone();
        match p.kind {
            Kind::Float { min, max } => {
                let step = (max - min) / 20.0;
                let v = (current.parse::<f64>().unwrap_or(0.0) + step * dir as f64).clamp(min, max);
                self.set_param(&p.name, params::format_number(v));
            }
            Kind::Color => {
                let Some((r, g, b)) = params::hex_rgb(&current) else {
                    return Action::None;
                };
                let (h, s, v) = rgb_to_hsv(r, g, b);
                let (r, g, b) = hsv_to_rgb((h + 15.0 * dir).rem_euclid(360.0), s, v);
                self.set_param(&p.name, format!("#{r:02x}{g:02x}{b:02x}"));
            }
        }
        let msg = self.describe_param(self.param_sel).unwrap_or_default();
        Action::Rebuild(msg)
    }

    pub fn key(&mut self, k: PKey) -> Action {
        let n_params = self.schema.params.len();
        match k {
            PKey::Space => {
                self.paused = !self.paused;
                Action::Say(if self.paused {
                    "paused".into()
                } else {
                    "running".into()
                })
            }
            PKey::Char('[') => {
                self.speed = (self.speed / 1.25).max(0.05);
                Action::Say(format!("speed x{:.2}", self.speed))
            }
            PKey::Char(']') => {
                self.speed = (self.speed * 1.25).min(20.0);
                Action::Say(format!("speed x{:.2}", self.speed))
            }
            PKey::Char('r' | 'R') => {
                self.time = 0.0;
                Action::Say("time reset to 0".into())
            }
            PKey::Char('t' | 'T') => {
                self.show_terminal = !self.show_terminal;
                Action::Rebuild(if self.show_terminal {
                    "terminal frame on".into()
                } else {
                    "terminal frame off (black)".into()
                })
            }
            PKey::Char(c @ ('p' | 'P')) => {
                if self.schema.presets.is_empty() {
                    return Action::Say("this shader has no presets".into());
                }
                let n = self.schema.presets.len();
                // p goes forward through the presets (defaults, then each), P goes back
                let pos = self.preset.map_or(0, |i| i + 1);
                let pos = if c == 'p' {
                    (pos + 1) % (n + 1)
                } else {
                    (pos + n) % (n + 1)
                };
                self.preset = pos.checked_sub(1);
                self.sets.clear();
                Action::Rebuild(format!(
                    "preset: {}",
                    self.preset_name().unwrap_or("(defaults)")
                ))
            }
            PKey::Char(c @ '1'..='9') => {
                let i = c as usize - '1' as usize;
                if i < n_params {
                    self.param_sel = i;
                    Action::Say(self.describe_param(i).unwrap_or_default())
                } else {
                    Action::Say(format!("this shader has {n_params} parameter(s)"))
                }
            }
            PKey::Up | PKey::Down if n_params > 0 => {
                self.param_sel = if k == PKey::Down {
                    (self.param_sel + 1) % n_params
                } else {
                    (self.param_sel + n_params - 1) % n_params
                };
                Action::Say(self.describe_param(self.param_sel).unwrap_or_default())
            }
            PKey::Left => self.adjust(-1.0),
            PKey::Right => self.adjust(1.0),
            PKey::BigLeft => self.adjust(-5.0),
            PKey::BigRight => self.adjust(5.0),
            PKey::Char('o' | 'O') => {
                let Some(i) = self
                    .schema
                    .params
                    .iter()
                    .position(|p| p.name == params::OPACITY)
                else {
                    return Action::Say("this shader has no opacity parameter".into());
                };
                if self.sets.iter().any(|s| s == "opacity=0") {
                    self.sets.retain(|s| s != "opacity=0");
                    Action::Rebuild(format!(
                        "opacity back on ({})",
                        self.effective().get(i).cloned().unwrap_or_default()
                    ))
                } else {
                    self.set_param(params::OPACITY, "0".into());
                    Action::Rebuild("opacity 0: the effect is hidden (O brings it back)".into())
                }
            }
            PKey::Char('s' | 'S') => Action::SavePng,
            PKey::Char('v' | 'V') => Action::Record,
            PKey::Char('q' | 'Q') | PKey::Escape => Action::Quit,
            _ => Action::None,
        }
    }
}

pub fn rgb_to_hsv(r: u8, g: u8, b: u8) -> (f32, f32, f32) {
    let (r, g, b) = (r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    let h = if d == 0.0 {
        0.0
    } else if max == r {
        60.0 * ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    (h, if max == 0.0 { 0.0 } else { d / max }, max)
}

pub fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (u8, u8, u8) {
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0).rem_euclid(2.0) - 1.0).abs());
    let m = v - c;
    let (r, g, b) = match (h / 60.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let q = |f: f32| ((f + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    (q(r), q(g), q(b))
}

/// Notices when a file changes (modified time or size), by polling: no OS watcher, so it works everywhere and in tests.
pub struct FileWatcher {
    path: PathBuf,
    stamp: Option<(SystemTime, u64)>,
}

impl FileWatcher {
    pub fn new(path: &Path) -> FileWatcher {
        FileWatcher {
            path: path.to_path_buf(),
            stamp: Self::stamp_of(path),
        }
    }

    fn stamp_of(path: &Path) -> Option<(SystemTime, u64)> {
        let m = std::fs::metadata(path).ok()?;
        Some((m.modified().ok()?, m.len()))
    }

    /// True once per change (including the file disappearing or coming back).
    pub fn changed(&mut self) -> bool {
        let now = Self::stamp_of(&self.path);
        if now != self.stamp {
            self.stamp = now;
            true
        } else {
            false
        }
    }
}

#[cfg(feature = "preview")]
pub use window::{PreviewOptions, run};

#[cfg(feature = "preview")]
mod window {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result, anyhow, bail};
    use winit::application::ApplicationHandler;
    use winit::dpi::PhysicalSize;
    use winit::event::{ElementState, WindowEvent};
    use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
    use winit::keyboard::{Key, NamedKey};
    use winit::window::{Window, WindowId};

    use super::{Action, FileWatcher, PKey, PreviewState};
    use crate::frame::Frame;
    use crate::gpu::{self, Gpu, Origin, Prepared, WindowGpu};
    use crate::params;
    use crate::video;

    pub struct PreviewOptions {
        pub file: PathBuf,
        pub preset: Option<String>,
        pub sets: Vec<String>,
        pub size: (u32, u32),
        /// `sample` or the path of a PNG
        pub text: String,
        pub origin: Origin,
    }

    const BLIT: &str = r#"
@group(0) @binding(0) var src: texture_2d<f32>;
@vertex
fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    var p = array<vec2<f32>, 3>(vec2<f32>(-1.0, -1.0), vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0));
    return vec4<f32>(p[i], 0.0, 1.0);
}
@fragment
fn fs(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let c = textureLoad(src, vec2<i32>(pos.xy), 0);
    return vec4<f32>(clamp(c.rgb, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}
"#;

    struct Presenter {
        pipeline: wgpu::RenderPipeline,
        layout: wgpu::BindGroupLayout,
        bind: wgpu::BindGroup,
    }

    impl Presenter {
        fn new(g: &Gpu, format: wgpu::TextureFormat, prepared: &Prepared) -> Presenter {
            let d = g.device();
            let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("blit"),
                source: wgpu::ShaderSource::Wgsl(BLIT.into()),
            });
            let layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: None,
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                }],
            });
            let pl = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: None,
                bind_group_layouts: &[Some(&layout)],
                immediate_size: 0,
            });
            let pipeline = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: None,
                layout: Some(&pl),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some("fs"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            });
            let bind = Self::bind(g, &layout, prepared);
            Presenter {
                pipeline,
                layout,
                bind,
            }
        }

        fn bind(g: &Gpu, layout: &wgpu::BindGroupLayout, prepared: &Prepared) -> wgpu::BindGroup {
            g.device().create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&prepared.target_view()),
                }],
            })
        }

        /// Copy the shader's picture onto `view` (the window's texture).
        fn encode(&self, g: &Gpu, view: &wgpu::TextureView) -> wgpu::CommandEncoder {
            let mut enc = g
                .device()
                .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            {
                let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: None,
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, Some(&self.bind), &[]);
                pass.draw(0..3, 0..1);
            }
            enc
        }

        fn retarget(&mut self, g: &Gpu, prepared: &Prepared) {
            self.bind = Self::bind(g, &self.layout, prepared);
        }
    }

    struct Win {
        window: Arc<Window>,
        surface: wgpu::Surface<'static>,
        wg: WindowGpu,
        config: wgpu::SurfaceConfiguration,
        presenter: Presenter,
        prepared: Prepared,
    }

    struct App {
        opts: PreviewOptions,
        state: PreviewState,
        watcher: FileWatcher,
        src: String,
        terminal: Frame,
        win: Option<Win>,
        fatal: Option<anyhow::Error>,
        last: Instant,
        last_check: Instant,
        title_at: Instant,
        frames: u32,
        error: Option<String>,
        message: String,
        saved: u32,
        frame_no: i32,
    }

    impl App {
        fn frame(&self) -> Frame {
            if self.state.show_terminal {
                self.terminal.clone()
            } else {
                self.terminal.without_text()
            }
        }

        /// Compile with the current state. On failure the last good shader keeps running and the error is shown.
        fn rebuild(&mut self) {
            let frame = self.frame();
            let Some(w) = self.win.as_mut() else { return };
            let preset = self.state.preset_name().map(str::to_string);
            match gpu::prepare_shader(
                &w.wg.gpu,
                &self.src,
                preset.as_deref(),
                &self.state.sets,
                &frame,
                self.opts.origin,
            ) {
                Ok(p) => {
                    w.presenter.retarget(&w.wg.gpu, &p);
                    w.prepared = p;
                    if self.error.take().is_some() {
                        println!("shader OK again");
                    }
                }
                Err(e) => {
                    println!("shader error (the last good version keeps running):\n{e}");
                    self.error = Some(e.lines().next().unwrap_or("error").to_string());
                }
            }
        }

        fn reload_file(&mut self) {
            match std::fs::read_to_string(&self.opts.file) {
                Ok(src) => {
                    // keep the old text if the new one has a bad annotation, show the error
                    match params::parse_schema(params::strip_header(&src)) {
                        Ok(schema) => {
                            self.src = src;
                            // keep the preset and overrides that still exist
                            self.state.schema = schema;
                            if self
                                .state
                                .preset
                                .is_some_and(|i| i >= self.state.schema.presets.len())
                            {
                                self.state.preset = None;
                            }
                            self.state.param_sel = 0;
                            self.state.sets.retain(|s| {
                                s.split_once('=').is_some_and(|(k, _)| {
                                    self.state.schema.params.iter().any(|p| p.name == k)
                                })
                            });
                            println!("reloaded {}", self.opts.file.display());
                            self.rebuild();
                        }
                        Err(e) => {
                            println!("shader error (the last good version keeps running): {e}");
                            self.error = Some(e);
                        }
                    }
                }
                Err(e) => self.error = Some(format!("cannot read the file: {e}")),
            }
        }

        fn name(&self) -> String {
            self.opts
                .file
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "shader".into())
        }

        fn do_action(&mut self, a: Action, el: &ActiveEventLoop) {
            match a {
                Action::None => {}
                Action::Say(m) => {
                    println!("{m}");
                    self.message = m;
                }
                Action::Rebuild(m) => {
                    println!("{m}");
                    self.message = m;
                    self.rebuild();
                }
                Action::Quit => el.exit(),
                Action::SavePng => self.save_png(),
                Action::Record => self.record(),
            }
        }

        fn save_png(&mut self) {
            let Some(w) = self.win.as_ref() else { return };
            let mut buf = Vec::new();
            if let Err(e) = w.prepared.draw_rgba8(
                &w.wg.gpu,
                self.state.time,
                1.0 / 60.0,
                self.frame_no,
                &mut buf,
            ) {
                self.message = format!("could not save: {e}");
                return;
            }
            self.saved += 1;
            let (pw, ph) = w.prepared.size();
            let stem = crate::home::render_stem(
                &self.name(),
                self.state.preset_name(),
                (pw, ph),
                self.state.time,
            );
            let Ok(path) =
                crate::home::Home::from_env().out_path(crate::home::OutKind::Render, &stem, "png")
            else {
                self.message = "could not create the renders folder".into();
                return;
            };
            match image::save_buffer(&path, &buf, pw, ph, image::ColorType::Rgba8) {
                Ok(()) => self.message = format!("saved {}", path.display()),
                Err(e) => self.message = format!("could not save {}: {e}", path.display()),
            }
            println!("{}", self.message);
        }

        fn record(&mut self) {
            let Some(w) = self.win.as_ref() else { return };
            w.window.set_title("shaderlab preview: recording 5 s...");
            let ffmpeg = video::find_ffmpeg();
            let (format, _) = video::choose_format(None, None, ffmpeg.is_some())
                .unwrap_or((video::Format::Gif, None));
            self.saved += 1;
            let stem = crate::home::video_stem(&self.name(), self.state.preset_name(), 5.0);
            let Ok(out) = crate::home::Home::from_env().out_path(
                crate::home::OutKind::Video,
                &stem,
                format.extension(),
            ) else {
                self.message = "could not create the videos folder".into();
                return;
            };
            let report = video::render_video(
                &w.wg.gpu,
                &w.prepared,
                &video::VideoOptions {
                    fps: 24,
                    duration: 5.0,
                    start: self.state.time,
                    format,
                    out: out.clone(),
                    loop_seamless: false,
                    ffmpeg,
                },
            );
            self.message = match report {
                Ok(r) => format!("recorded {} ({} frames)", r.path.display(), r.frames),
                Err(e) => format!("recording failed: {e:#}"),
            };
            println!("{}", self.message);
            self.last = Instant::now();
        }

        fn redraw(&mut self) {
            let now = Instant::now();
            let dt = (now - self.last).as_secs_f32().min(0.1);
            self.last = now;
            self.state.advance(dt);
            if now - self.last_check > Duration::from_millis(200) {
                self.last_check = now;
                if self.watcher.changed() {
                    self.reload_file();
                }
            }
            self.frame_no = self.frame_no.wrapping_add(1);
            let Some(w) = self.win.as_mut() else { return };
            w.prepared
                .render_only(&w.wg.gpu, self.state.time, dt.max(1e-4), self.frame_no);
            match w.surface.get_current_texture() {
                wgpu::CurrentSurfaceTexture::Success(t)
                | wgpu::CurrentSurfaceTexture::Suboptimal(t) => {
                    let view = t
                        .texture
                        .create_view(&wgpu::TextureViewDescriptor::default());
                    let enc = w.presenter.encode(&w.wg.gpu, &view);
                    w.wg.gpu.queue().submit(Some(enc.finish()));
                    w.window.pre_present_notify();
                    w.wg.gpu.queue().present(t);
                }
                wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                    w.surface.configure(w.wg.gpu.device(), &w.config);
                }
                _ => {}
            }
            self.frames += 1;
            if now - self.title_at > Duration::from_millis(500) {
                let secs = (now - self.title_at).as_secs_f32();
                let fps = self.frames as f32 / secs;
                self.title_at = now;
                self.frames = 0;
                let st = &self.state;
                let mut title = format!(
                    "shaderlab {} | {}{} | t={:.1}s x{:.2}{} | {:.0} fps, {:.1} ms",
                    self.name(),
                    st.preset_name().unwrap_or("defaults"),
                    if st.show_terminal { "" } else { " | black" },
                    st.time,
                    st.speed,
                    if st.paused { " PAUSED" } else { "" },
                    fps,
                    1000.0 / fps.max(0.001),
                );
                if let Some(e) = &self.error {
                    title = format!("SHADER ERROR (last good keeps running): {e} | {title}");
                } else if !self.message.is_empty() {
                    title = format!("{} | {title}", self.message);
                }
                if let Some(w) = self.win.as_ref() {
                    w.window.set_title(&title);
                }
            }
        }
    }

    fn to_pkey(key: &Key) -> Option<PKey> {
        match key {
            Key::Named(NamedKey::Space) => Some(PKey::Space),
            Key::Named(NamedKey::ArrowLeft) => Some(PKey::Left),
            Key::Named(NamedKey::ArrowRight) => Some(PKey::Right),
            Key::Named(NamedKey::ArrowUp) => Some(PKey::Up),
            Key::Named(NamedKey::ArrowDown) => Some(PKey::Down),
            Key::Named(NamedKey::Escape) => Some(PKey::Escape),
            Key::Character(s) => s.chars().next().map(PKey::Char),
            _ => None,
        }
    }

    impl ApplicationHandler for App {
        fn resumed(&mut self, el: &ActiveEventLoop) {
            if self.win.is_some() {
                return;
            }
            let (w, h) = self.opts.size;
            let attrs = Window::default_attributes()
                .with_title("shaderlab preview")
                .with_inner_size(PhysicalSize::new(w, h))
                .with_resizable(false);
            let window = match el.create_window(attrs) {
                Ok(w) => Arc::new(w),
                Err(e) => {
                    self.fatal = Some(anyhow!("could not open a window: {e}"));
                    el.exit();
                    return;
                }
            };
            let size = window.inner_size();
            let (wg, surface) = match Gpu::for_window(window.clone()) {
                Ok(x) => x,
                Err(e) => {
                    self.fatal = Some(anyhow!("{e}"));
                    el.exit();
                    return;
                }
            };
            let caps = surface.get_capabilities(&wg.adapter);
            // the shader writes display-ready colors: use a non-sRGB format so nothing converts them again
            let format = caps
                .formats
                .iter()
                .copied()
                .find(|f| !f.is_srgb())
                .or(caps.formats.first().copied());
            let Some(format) = format else {
                self.fatal = Some(anyhow!("the window surface supports no pixel format"));
                el.exit();
                return;
            };
            let config = wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                format,
                width: size.width.max(1),
                height: size.height.max(1),
                present_mode: wgpu::PresentMode::Fifo,
                desired_maximum_frame_latency: 2,
                alpha_mode: caps
                    .alpha_modes
                    .first()
                    .copied()
                    .unwrap_or(wgpu::CompositeAlphaMode::Auto),
                color_space: wgpu::SurfaceColorSpace::Auto,
                view_formats: vec![],
            };
            surface.configure(wg.gpu.device(), &config);
            let frame = self.frame();
            let preset = self.state.preset_name().map(str::to_string);
            let prepared = match gpu::prepare_shader(
                &wg.gpu,
                &self.src,
                preset.as_deref(),
                &self.state.sets,
                &frame,
                self.opts.origin,
            ) {
                Ok(p) => p,
                Err(e) => {
                    self.fatal = Some(anyhow!("{e}"));
                    el.exit();
                    return;
                }
            };
            let presenter = Presenter::new(&wg.gpu, format, &prepared);
            println!(
                "preview on {} (space pause, [ ] speed, R reset, T terminal, P preset, 1-9 / arrows params, S save, V record, Q quit)",
                wg.gpu.adapter_name
            );
            window.request_redraw();
            self.win = Some(Win {
                window,
                surface,
                wg,
                config,
                presenter,
                prepared,
            });
            self.last = Instant::now();
        }

        fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
            match event {
                WindowEvent::CloseRequested => el.exit(),
                WindowEvent::KeyboardInput { event, .. }
                    if event.state == ElementState::Pressed =>
                {
                    if let Some(k) = to_pkey(&event.logical_key) {
                        let a = self.state.key(k);
                        self.do_action(a, el);
                    }
                }
                WindowEvent::RedrawRequested => {
                    self.redraw();
                    if let Some(w) = self.win.as_ref() {
                        w.window.request_redraw();
                    }
                }
                _ => {}
            }
        }
    }

    /// Used by the unit test that checks the blit without a window.
    #[cfg(test)]
    #[allow(clippy::items_after_test_module)]
    pub(super) mod test_support {
        use super::*;

        pub fn blit_matches_draw(g: &Gpu) {
            let frame = Frame::sample(128, 72);
            let src = "void mainImage(out vec4 c, in vec2 p) { vec4 t = texture(iChannel0, p / iResolution.xy); c = vec4(mix(t.rgb, vec3(p.x / iResolution.x, 0.25, p.y / iResolution.y), 0.5), 1.0); }\n";
            let prepared = gpu::prepare_shader(g, src, None, &[], &frame, Origin::TopLeft).unwrap();
            let format = wgpu::TextureFormat::Bgra8Unorm;
            let presenter = Presenter::new(g, format, &prepared);
            prepared.render_only(g, 1.0, 0.016, 1);
            let d = g.device();
            let target = d.create_texture(&wgpu::TextureDescriptor {
                label: None,
                size: wgpu::Extent3d {
                    width: 128,
                    height: 72,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let mut enc = presenter.encode(
                g,
                &target.create_view(&wgpu::TextureViewDescriptor::default()),
            );
            let padded = (128u32 * 4).next_multiple_of(256);
            let buf = d.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: (padded * 72) as u64,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            enc.copy_texture_to_buffer(
                target.as_image_copy(),
                wgpu::TexelCopyBufferInfo {
                    buffer: &buf,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(padded),
                        rows_per_image: Some(72),
                    },
                },
                wgpu::Extent3d {
                    width: 128,
                    height: 72,
                    depth_or_array_layers: 1,
                },
            );
            g.queue().submit(Some(enc.finish()));
            let slice = buf.slice(..);
            slice.map_async(wgpu::MapMode::Read, |_| {});
            d.poll(wgpu::PollType::wait_indefinitely()).unwrap();
            let data = slice.get_mapped_range().unwrap();
            let mut want = Vec::new();
            prepared.draw_rgba8(g, 1.0, 0.016, 1, &mut want).unwrap();
            let mut worst = 0i32;
            for y in 0..72usize {
                for x in 0..128usize {
                    let o = y * padded as usize + x * 4;
                    let (b, gr, r) = (data[o], data[o + 1], data[o + 2]);
                    let w = (y * 128 + x) * 4;
                    for (a, e) in [(r, want[w]), (gr, want[w + 1]), (b, want[w + 2])] {
                        worst = worst.max((a as i32 - e as i32).abs());
                    }
                }
            }
            assert!(
                worst <= 1,
                "the window copy differs from the shader's picture by up to {worst}/255"
            );
            assert!(
                want.chunks(4).any(|p| p[0] > 40),
                "the picture is not just black"
            );
        }
    }

    /// Open the live preview window. Returns an error (never panics) when there is no display or no GPU.
    pub fn run(opts: PreviewOptions) -> Result<()> {
        let src = std::fs::read_to_string(&opts.file)
            .with_context(|| format!("reading {}", opts.file.display()))?;
        let schema = params::parse_schema(params::strip_header(&src)).map_err(|e| anyhow!(e))?;
        let state = PreviewState::new(schema, opts.preset.as_deref(), opts.sets.clone())
            .map_err(|e| anyhow!(e))?;
        let terminal = if opts.text == "sample" {
            Frame::sample(opts.size.0, opts.size.1)
        } else {
            Frame::from_png(std::path::Path::new(&opts.text))?
        };
        // winit can abort or panic with no window server (ssh, CI): turn that into an error
        let event_loop = std::panic::catch_unwind(EventLoop::new)
            .map_err(|_| anyhow!("no display is available for a window (are you on a remote or headless session?)"))?
            .map_err(|e| anyhow!("could not start the window system: {e}"))?;
        event_loop.set_control_flow(ControlFlow::Poll);
        let mut app = App {
            watcher: FileWatcher::new(&opts.file),
            opts,
            state,
            src,
            terminal,
            win: None,
            fatal: None,
            last: Instant::now(),
            last_check: Instant::now(),
            title_at: Instant::now(),
            frames: 0,
            error: None,
            message: String::new(),
            saved: 0,
            frame_no: 0,
        };
        event_loop
            .run_app(&mut app)
            .map_err(|e| anyhow!("the window loop failed: {e}"))?;
        if let Some(e) = app.fatal {
            bail!("{e}");
        }
        Ok(())
    }
}

#[cfg(all(test, feature = "preview"))]
mod window_tests {
    use super::window::test_support::blit_matches_draw;

    #[test]
    fn the_presenter_puts_exactly_the_shaders_picture_on_a_window_format_texture() {
        match crate::gpu::Gpu::new() {
            Ok(g) => blit_matches_draw(&g),
            Err(e) => eprintln!("SKIPPED: {e}; nothing was verified by this test"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> PreviewState {
        let src = "// @float opacity 1.0 0.0 1.0 \"Opacity\"\n// @color tint #336699 \"Tint\"\n// @float speed 1.0 0.0 3.0 \"Speed\"\n// @preset warm tint=#ff8800 speed=2\n// @preset cool tint=#0088ff\nvoid mainImage(out vec4 c, in vec2 p) { c = vec4(P_tint, 1.0); }\n";
        PreviewState::new(params::parse_schema(src).unwrap(), None, vec![]).unwrap()
    }

    #[test]
    fn space_pauses_and_time_stops_and_r_resets() {
        let mut s = state();
        s.advance(1.0);
        assert!((s.time - 1.0).abs() < 1e-6);
        assert_eq!(s.key(PKey::Space), Action::Say("paused".into()));
        s.advance(5.0);
        assert!((s.time - 1.0).abs() < 1e-6, "paused time does not move");
        s.key(PKey::Space);
        s.advance(0.5);
        assert!((s.time - 1.5).abs() < 1e-6);
        s.key(PKey::Char('r'));
        assert_eq!(s.time, 0.0);
    }

    #[test]
    fn brackets_change_the_speed_within_limits() {
        let mut s = state();
        s.key(PKey::Char(']'));
        assert!((s.speed - 1.25).abs() < 1e-6);
        s.advance(1.0);
        assert!((s.time - 1.25).abs() < 1e-6);
        for _ in 0..60 {
            s.key(PKey::Char('['));
        }
        assert!(s.speed >= 0.05);
        for _ in 0..80 {
            s.key(PKey::Char(']'));
        }
        assert!(s.speed <= 20.0);
    }

    #[test]
    fn p_cycles_through_the_presets_and_back_to_the_defaults() {
        let mut s = state();
        assert_eq!(s.preset_name(), None);
        assert!(matches!(s.key(PKey::Char('p')), Action::Rebuild(m) if m.contains("warm")));
        assert_eq!(s.values().unwrap()["tint"], "#ff8800");
        s.key(PKey::Char('p'));
        assert_eq!(s.preset_name(), Some("cool"));
        s.key(PKey::Char('p'));
        assert_eq!(s.preset_name(), None);
        // P goes backwards
        s.key(PKey::Char('P'));
        assert_eq!(s.preset_name(), Some("cool"));
        s.key(PKey::Char('P'));
        assert_eq!(s.preset_name(), Some("warm"));
        s.key(PKey::Char('P'));
        assert_eq!(s.preset_name(), None);
        s.key(PKey::Char('p'));
        s.key(PKey::Char('p'));
        s.key(PKey::Char('p'));
        // a manual change is dropped when the preset changes
        s.sets.push("speed=3".into());
        s.key(PKey::Char('p'));
        assert!(s.sets.is_empty());
    }

    #[test]
    fn number_keys_pick_a_parameter_and_left_right_change_it_within_its_range() {
        let mut s = state();
        assert!(matches!(s.key(PKey::Char('3')), Action::Say(m) if m.starts_with("speed = 1")));
        assert_eq!(s.param_sel, 2);
        assert!(matches!(s.key(PKey::Char('9')), Action::Say(m) if m.contains("3 parameter")));
        // speed ranges 0..3: a step is 0.15
        let Action::Rebuild(m) = s.key(PKey::Right) else {
            panic!()
        };
        assert!(m.starts_with("speed = 1.15"), "{m}");
        for _ in 0..100 {
            s.key(PKey::Right);
        }
        assert_eq!(s.values().unwrap()["speed"], "3", "clamped at the maximum");
        for _ in 0..100 {
            s.key(PKey::Left);
        }
        assert_eq!(s.values().unwrap()["speed"], "0", "and at the minimum");
        // the override list never holds the same name twice
        assert_eq!(s.sets.iter().filter(|x| x.starts_with("speed=")).count(), 1);
    }

    #[test]
    fn arrows_walk_the_parameters_and_a_colour_changes_by_hue() {
        let mut s = state();
        assert_eq!(s.param_sel, 0);
        s.key(PKey::Down);
        assert_eq!(s.param_sel, 1);
        s.key(PKey::Up);
        s.key(PKey::Up);
        assert_eq!(s.param_sel, 2, "wraps");
        s.key(PKey::Char('2')); // tint #336699
        let eff = |s: &PreviewState| params::resolve(&s.schema, &s.values().unwrap())[1].clone();
        let before = eff(&s);
        s.key(PKey::Right);
        let after = eff(&s);
        assert_ne!(before, after);
        assert!(
            params::hex_rgb(&after).is_some(),
            "still a valid color: {after}"
        );
        let (_, sat0, val0) = {
            let (r, g, b) = params::hex_rgb(&before).unwrap();
            rgb_to_hsv(r, g, b)
        };
        let (_, sat1, val1) = {
            let (r, g, b) = params::hex_rgb(&after).unwrap();
            rgb_to_hsv(r, g, b)
        };
        assert!(
            (sat0 - sat1).abs() < 0.02 && (val0 - val1).abs() < 0.02,
            "only the hue moves"
        );
    }

    #[test]
    fn big_steps_o_toggles_opacity_and_mouse_setters_validate() {
        let mut s = state();
        s.key(PKey::Char('3')); // speed 0..3, a step is 0.15
        s.key(PKey::BigRight);
        assert_eq!(s.effective()[2], "1.75", "five steps");
        // O hides the effect and brings it back
        assert!(matches!(s.key(PKey::Char('o')), Action::Rebuild(m) if m.contains("hidden")));
        assert_eq!(s.effective()[0], "0");
        assert!(matches!(s.key(PKey::Char('O')), Action::Rebuild(m) if m.contains("back on")));
        assert_eq!(s.effective()[0], "1");
        // a bar click sets a fraction of the range; a bad value changes nothing
        assert!(s.set_fraction(2, 0.5));
        assert_eq!(s.effective()[2], "1.5");
        assert!(s.set_fraction(2, 5.0), "clamped to the end");
        assert_eq!(s.effective()[2], "3");
        assert!(!s.set_fraction(1, 0.5), "a color has no bar");
        assert!(!s.set_value(1, "nonsense"));
        assert!(s.set_value(1, "#112233"));
        assert_eq!(s.effective()[1], "#112233");
        s.pick_preset(Some(1));
        assert_eq!(s.preset_name(), Some("cool"));
        assert!(s.sets.is_empty());
    }

    #[test]
    fn t_s_v_q_map_to_actions() {
        let mut s = state();
        assert!(matches!(s.key(PKey::Char('t')), Action::Rebuild(_)));
        assert!(!s.show_terminal);
        assert_eq!(s.key(PKey::Char('s')), Action::SavePng);
        assert_eq!(s.key(PKey::Char('V')), Action::Record);
        assert_eq!(s.key(PKey::Char('q')), Action::Quit);
        assert_eq!(s.key(PKey::Escape), Action::Quit);
        assert_eq!(s.key(PKey::Char('x')), Action::None);
    }

    #[test]
    fn a_shader_without_parameters_says_so() {
        let schema =
            params::parse_schema("void mainImage(out vec4 c, in vec2 p) { c = vec4(1.0); }\n")
                .unwrap();
        let mut s = PreviewState::new(schema, None, vec![]).unwrap();
        assert!(matches!(s.key(PKey::Right), Action::Say(m) if m.contains("no parameters")));
        assert!(matches!(s.key(PKey::Char('p')), Action::Say(m) if m.contains("no presets")));
        assert!(matches!(s.key(PKey::Char('1')), Action::Say(m) if m.contains("0 parameter")));
    }

    #[test]
    fn a_bad_preset_or_override_is_rejected_up_front() {
        let schema = params::parse_schema(
            "// @float a 1 0 2 \"A\"\nvoid mainImage(out vec4 c, in vec2 p) {}\n",
        )
        .unwrap();
        assert!(
            PreviewState::new(schema.clone(), Some("nope"), vec![])
                .unwrap_err()
                .contains("no preset")
        );
        assert!(PreviewState::new(schema, None, vec!["a=9".into()]).is_err());
    }

    #[test]
    fn the_watcher_reports_each_change_once_and_a_deleted_file() {
        let td = tempfile::tempdir().unwrap();
        let f = td.path().join("s.glsl");
        std::fs::write(&f, "a").unwrap();
        let mut w = FileWatcher::new(&f);
        assert!(!w.changed(), "nothing changed yet");
        std::fs::write(&f, "a longer file").unwrap();
        assert!(w.changed(), "the size changed");
        assert!(!w.changed(), "reported once");
        // same size, new time (an editor saving the same length)
        let t =
            std::fs::metadata(&f).unwrap().modified().unwrap() + std::time::Duration::from_secs(5);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&f)
            .unwrap()
            .set_modified(t)
            .unwrap();
        assert!(w.changed(), "the modified time changed");
        std::fs::remove_file(&f).unwrap();
        assert!(w.changed(), "the file went away");
        assert!(!w.changed());
        std::fs::write(&f, "back").unwrap();
        assert!(w.changed(), "and came back");
    }

    #[test]
    fn hsv_round_trips() {
        for (r, g, b) in [
            (255, 0, 0),
            (51, 102, 153),
            (0, 0, 0),
            (255, 255, 255),
            (12, 200, 99),
        ] {
            let (h, s, v) = rgb_to_hsv(r, g, b);
            let (r2, g2, b2) = hsv_to_rgb(h, s, v);
            assert!(
                (r as i32 - r2 as i32).abs() <= 1
                    && (g as i32 - g2 as i32).abs() <= 1
                    && (b as i32 - b2 as i32).abs() <= 1
            );
        }
    }
}
