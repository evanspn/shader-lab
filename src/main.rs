use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};

use shaderlab::check::{CheckOptions, check};
use shaderlab::frame::Frame;
use shaderlab::gpu::{self, Gpu, GpuError, Origin};
use shaderlab::params::{self, RenderContext};
use shaderlab::sheet::contact_sheet;
use shaderlab::video;

#[derive(Clone, Copy, ValueEnum)]
enum OriginArg {
    /// Ghostty: origin top-left, y grows downward
    TopLeft,
    /// Shadertoy: origin bottom-left, y grows upward
    BottomLeft,
}

impl From<OriginArg> for Origin {
    fn from(o: OriginArg) -> Origin {
        match o {
            OriginArg::TopLeft => Origin::TopLeft,
            OriginArg::BottomLeft => Origin::BottomLeft,
        }
    }
}

const ORIENTATION: &str = "ORIENTATION: Ghostty's fragCoord has its origin at the TOP-left (y grows DOWN); Shadertoy's is the BOTTOM-left (y grows UP). \
Pass --origin to say which convention a shader was written for (default: top-left, Ghostty). Images (PNG) have row 0 at the top.";

#[derive(Parser)]
#[command(name = "shaderlab", version, about = "Render, preview and check Ghostty/Shadertoy-style terminal shaders headlessly", after_help = ORIENTATION)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Render one frame of a shader over a terminal frame to a PNG
    Render {
        file: PathBuf,
        /// A named preset from the shader's `@preset` lines
        #[arg(long)]
        preset: Option<String>,
        /// Override a parameter: --set name=value (repeatable)
        #[arg(long = "set")]
        sets: Vec<String>,
        /// The time in seconds (iTime)
        #[arg(long, default_value_t = 5.0)]
        time: f32,
        /// Output size, WIDTHxHEIGHT
        #[arg(long, default_value = "1280x720")]
        size: String,
        /// `sample` (a synthetic terminal) or the path of a PNG to use as the terminal frame
        #[arg(long, default_value = "sample")]
        text: String,
        #[arg(long, value_enum, default_value_t = OriginArg::TopLeft)]
        origin: OriginArg,
        /// Where to write the PNG (default: ./NAME.png)
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// A grid of frames: one row per preset, one column per time
    ContactSheet {
        file: PathBuf,
        /// Comma-separated seconds
        #[arg(long, default_value = "0,2,5,9")]
        times: String,
        /// `all`, or comma-separated preset names (default: just the defaults)
        #[arg(long)]
        presets: Option<String>,
        /// Size of each cell, WIDTHxHEIGHT
        #[arg(long, default_value = "480x270")]
        size: String,
        #[arg(long, default_value = "sample")]
        text: String,
        #[arg(long, value_enum, default_value_t = OriginArg::TopLeft)]
        origin: OriginArg,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Render many frames in one GPU session and encode them: mp4 (needs ffmpeg), a GIF, or a folder of PNGs
    Video {
        file: PathBuf,
        #[arg(long)]
        preset: Option<String>,
        #[arg(long = "set")]
        sets: Vec<String>,
        #[arg(long, default_value = "1280x720")]
        size: String,
        /// Frames per second (a GIF is capped at 20)
        #[arg(long, default_value_t = 24)]
        fps: u32,
        /// Seconds of video
        #[arg(long, default_value_t = 10.0)]
        duration: f32,
        /// iTime of the first frame, in seconds
        #[arg(long, default_value_t = 0.0)]
        start: f32,
        #[arg(long, default_value = "sample")]
        text: String,
        #[arg(long, value_enum, default_value_t = OriginArg::TopLeft)]
        origin: OriginArg,
        /// mp4, gif or frames (default: from the output name, else mp4 when ffmpeg is installed, else gif)
        #[arg(long, value_enum)]
        format: Option<FormatArg>,
        /// Cross-fade the last second into the first so the clip loops without a jump
        #[arg(long)]
        loop_seamless: bool,
        /// Output file (default ./NAME.mp4 or .gif); for --format frames, a folder
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Live preview INSIDE the terminal (kitty graphics, sixel or half-blocks), with hot reload; `--window` opens a separate window.
    /// Keys: space pause, [ ] speed, R reset, T terminal on/off, P/p presets, O opacity, arrows pick/change (shift: more),
    /// Enter color picker, S save PNG, V record 5 s, Q quit. The mouse sets bars, presets and the picker.
    Preview {
        file: PathBuf,
        #[arg(long)]
        preset: Option<String>,
        #[arg(long = "set")]
        sets: Vec<String>,
        /// Window size (with --window)
        #[arg(long, default_value = "1280x720")]
        size: String,
        #[arg(long, default_value = "sample")]
        text: String,
        #[arg(long, value_enum, default_value_t = OriginArg::TopLeft)]
        origin: OriginArg,
        /// How the picture reaches the terminal (default: detected)
        #[arg(long, value_enum, default_value_t = ProtocolArg::Auto)]
        protocol: ProtocolArg,
        /// Frames per second in the terminal (capped at 60)
        #[arg(long, default_value_t = 30)]
        fps: u32,
        /// Open a separate window instead of drawing in the terminal
        #[arg(long)]
        window: bool,
    },
    /// Objective checks (compiles, text preserved, animates, declared motion, speed); exits non-zero on a failure
    Check {
        /// Shader files, or folders (every .glsl directly inside is checked)
        paths: Vec<PathBuf>,
        #[arg(long, value_enum, default_value_t = OriginArg::TopLeft)]
        origin: OriginArg,
        /// The most a text pixel may change, in 0..255 levels
        #[arg(long, default_value_t = 2)]
        epsilon: i32,
        /// The least text edge energy to keep (1.0 = as sharp as the input)
        #[arg(long, default_value_t = 0.85)]
        sharpness: f32,
        /// Milliseconds per 1080p frame
        #[arg(long, default_value_t = 16.7)]
        budget_ms: f64,
        /// Skip checks: text, motion, animation, perf (comma-separated)
        #[arg(long, value_delimiter = ',')]
        skip: Vec<String>,
        /// Frame size for the text and animation checks, WIDTHxHEIGHT
        #[arg(long, default_value = "640x360")]
        size: String,
        /// A PNG to use as the terminal frame instead of the built-in sample
        #[arg(long)]
        text: Option<PathBuf>,
        /// Only compile-check on the CPU (no GPU needed)
        #[arg(long)]
        cpu_only: bool,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum ProtocolArg {
    Auto,
    Kitty,
    Sixel,
    Halfblocks,
}

#[cfg(feature = "tui")]
impl From<ProtocolArg> for Option<shaderlab::termimg::Protocol> {
    fn from(p: ProtocolArg) -> Self {
        use shaderlab::termimg::Protocol;
        match p {
            ProtocolArg::Auto => None,
            ProtocolArg::Kitty => Some(Protocol::Kitty),
            ProtocolArg::Sixel => Some(Protocol::Sixel),
            ProtocolArg::Halfblocks => Some(Protocol::HalfBlocks),
        }
    }
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum FormatArg {
    Mp4,
    Gif,
    Frames,
}

fn parse_size(s: &str) -> Result<(u32, u32)> {
    let (w, h) = s
        .split_once(['x', 'X'])
        .with_context(|| format!("size must look like 1280x720, got '{s}'"))?;
    let (w, h): (u32, u32) = (w.trim().parse()?, h.trim().parse()?);
    if !(16..=8192).contains(&w) || !(16..=8192).contains(&h) {
        bail!("size must be between 16 and 8192 pixels each way");
    }
    Ok((w, h))
}

fn frame_for(text: &str, size: (u32, u32)) -> Result<Frame> {
    if text == "sample" {
        Ok(Frame::sample(size.0, size.1))
    } else {
        Frame::from_png(Path::new(text))
    }
}

fn open_gpu() -> Result<Gpu, GpuError> {
    Gpu::new()
}

fn save_png(path: &Path, w: u32, h: u32, rgba: &[u8]) -> Result<()> {
    image::save_buffer(path, rgba, w, h, image::ColorType::Rgba8)
        .with_context(|| format!("writing {}", path.display()))
}

fn read_shader(file: &Path) -> Result<String> {
    std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))
}

/// The real parameter header, then the GPU pipeline for `src` over `frame`.
fn prepare_shader(
    gpu: &Gpu,
    src: &str,
    preset: Option<&str>,
    sets: &[String],
    frame: &Frame,
    origin: Origin,
) -> Result<gpu::Prepared> {
    gpu::prepare_shader(gpu, src, preset, sets, frame, origin).map_err(anyhow::Error::msg)
}

/// Render one frame: the real parameter header, then the GPU.
fn render_frame(
    gpu: &Gpu,
    src: &str,
    preset: Option<&str>,
    sets: &[String],
    frame: &Frame,
    time: f32,
    origin: Origin,
) -> Result<Vec<u8>> {
    let schema = params::parse_schema(params::strip_header(src)).map_err(anyhow::Error::msg)?;
    let values = params::values_from_args(&schema, preset, sets).map_err(anyhow::Error::msg)?;
    let ctx = RenderContext {
        background: frame.background,
        ..RenderContext::default()
    };
    let text = params::render_ctx(src, &values, &ctx).map_err(anyhow::Error::msg)?;
    let prepared = gpu
        .prepare(&text, frame, origin)
        .map_err(anyhow::Error::msg)?;
    Ok(gpu::to_rgba8(
        &prepared.draw(gpu, time).map_err(anyhow::Error::msg)?,
    ))
}

fn run() -> Result<ExitCode> {
    match Cli::parse().command {
        Cmd::Render {
            file,
            preset,
            sets,
            time,
            size,
            text,
            origin,
            out,
        } => {
            let src = read_shader(&file)?;
            let frame = frame_for(&text, parse_size(&size)?)?;
            let gpu = open_gpu()?;
            let origin: Origin = origin.into();
            let px = render_frame(&gpu, &src, preset.as_deref(), &sets, &frame, time, origin)?;
            let stem = file
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "shader".into());
            let out = out.unwrap_or_else(|| PathBuf::from(format!("{stem}.png")));
            save_png(&out, frame.width, frame.height, &px)?;
            println!(
                "wrote {} ({}x{}, t={time}s, origin {}, {})",
                out.display(),
                frame.width,
                frame.height,
                origin.name(),
                gpu.adapter_name
            );
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Video {
            file,
            preset,
            sets,
            size,
            fps,
            duration,
            start,
            text,
            origin,
            format,
            loop_seamless,
            out,
        } => {
            if fps == 0 || !(0.1..=600.0).contains(&duration) {
                bail!("--fps must be at least 1 and --duration between 0.1 and 600 seconds");
            }
            let src = read_shader(&file)?;
            let frame = frame_for(&text, parse_size(&size)?)?;
            let ffmpeg = video::find_ffmpeg();
            let explicit = format.map(|f| match f {
                FormatArg::Mp4 => video::Format::Mp4,
                FormatArg::Gif => video::Format::Gif,
                FormatArg::Frames => video::Format::Frames,
            });
            let (fmt, note) = video::choose_format(explicit, out.as_deref(), ffmpeg.is_some())?;
            let stem = file
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "shader".into());
            let out = out.unwrap_or_else(|| match fmt {
                video::Format::Frames => PathBuf::from(format!("{stem}-frames")),
                f => PathBuf::from(format!("{stem}.{}", f.extension())),
            });
            if let Some(dir) = out.parent().filter(|d| !d.as_os_str().is_empty()) {
                std::fs::create_dir_all(dir)?;
            }
            let gpu = open_gpu()?;
            let prepared =
                prepare_shader(&gpu, &src, preset.as_deref(), &sets, &frame, origin.into())?;
            if let Some(n) = note {
                eprintln!("note: {n}");
            }
            let report = video::render_video(
                &gpu,
                &prepared,
                &video::VideoOptions {
                    fps,
                    duration,
                    start,
                    format: fmt,
                    out,
                    loop_seamless,
                    ffmpeg,
                },
            )?;
            for n in &report.notes {
                eprintln!("note: {n}");
            }
            println!(
                "wrote {} ({} frames, {}x{} at {} fps, {:.1}s of rendering, {:.1} ms per frame, {})",
                report.path.display(),
                report.frames,
                report.size.0,
                report.size.1,
                report.fps,
                report.render_secs,
                report.ms_per_frame,
                gpu.adapter_name
            );
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Preview {
            file,
            preset,
            sets,
            size,
            text,
            origin,
            protocol,
            fps,
            window,
        } => {
            if window {
                #[cfg(feature = "preview")]
                {
                    shaderlab::preview::run(shaderlab::preview::PreviewOptions {
                        file,
                        preset,
                        sets,
                        size: parse_size(&size)?,
                        text,
                        origin: origin.into(),
                    })?;
                    return Ok(ExitCode::SUCCESS);
                }
                #[cfg(not(feature = "preview"))]
                {
                    let _ = (&file, &preset, &sets, &size, &text, &origin);
                    bail!(
                        "this build has no separate preview window: reinstall with `--features preview`"
                    )
                }
            }
            #[cfg(feature = "tui")]
            {
                let _ = size;
                shaderlab::tui::run(shaderlab::tui::TuiOptions {
                    file,
                    preset,
                    sets,
                    protocol: protocol.into(),
                    text,
                    origin: origin.into(),
                    fps,
                })?;
                Ok(ExitCode::SUCCESS)
            }
            #[cfg(not(feature = "tui"))]
            {
                let _ = (
                    &file, &preset, &sets, &size, &text, &origin, &protocol, &fps,
                );
                bail!("this build has no terminal preview: reinstall with the default features")
            }
        }
        Cmd::ContactSheet {
            file,
            times,
            presets,
            size,
            text,
            origin,
            out,
        } => {
            let src = read_shader(&file)?;
            let cell = parse_size(&size)?;
            let frame = frame_for(&text, cell)?;
            let (cw, ch) = (frame.width, frame.height);
            let times: Vec<f32> = times
                .split(',')
                .map(|t| {
                    t.trim()
                        .parse::<f32>()
                        .with_context(|| format!("bad time '{t}'"))
                })
                .collect::<Result<_>>()?;
            let schema =
                params::parse_schema(params::strip_header(&src)).map_err(anyhow::Error::msg)?;
            let rows: Vec<Option<String>> = match presets.as_deref() {
                None => vec![None],
                Some("all") => std::iter::once(None)
                    .chain(schema.presets.iter().map(|p| Some(p.name.clone())))
                    .collect(),
                Some(list) => list
                    .split(',')
                    .map(|p| Some(p.trim().to_string()))
                    .collect(),
            };
            let gpu = open_gpu()?;
            let origin: Origin = origin.into();
            let mut cells = Vec::new();
            for row in &rows {
                for t in &times {
                    let px = render_frame(&gpu, &src, row.as_deref(), &[], &frame, *t, origin)?;
                    cells.push((
                        format!("{} t={t}s", row.as_deref().unwrap_or("defaults")),
                        px,
                    ));
                }
            }
            let (w, h, sheet) = contact_sheet(&cells, cw, ch, times.len() as u32);
            let stem = file
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "shader".into());
            let out = out.unwrap_or_else(|| PathBuf::from(format!("{stem}-sheet.png")));
            save_png(&out, w, h, &sheet)?;
            println!(
                "wrote {} ({} frames, {}x{})",
                out.display(),
                cells.len(),
                w,
                h
            );
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Check {
            paths,
            origin,
            epsilon,
            sharpness,
            budget_ms,
            skip,
            size,
            text,
            cpu_only,
        } => {
            let mut files = Vec::new();
            for p in &paths {
                if p.is_dir() {
                    let mut v: Vec<PathBuf> = std::fs::read_dir(p)?
                        .flatten()
                        .map(|e| e.path())
                        .filter(|f| f.extension().is_some_and(|x| x == "glsl"))
                        .collect();
                    v.sort();
                    files.extend(v);
                } else {
                    files.push(p.clone());
                }
            }
            if files.is_empty() {
                bail!("no shader files to check");
            }
            let gpu = if cpu_only {
                None
            } else {
                match open_gpu() {
                    Ok(g) => Some(g),
                    Err(e) => {
                        eprintln!("error: {e}");
                        return Ok(ExitCode::from(2));
                    }
                }
            };
            let opts = CheckOptions {
                origin: origin.into(),
                size: parse_size(&size)?,
                epsilon,
                sharpness_min: sharpness,
                budget_ms,
                skip,
                frame: text,
                ..CheckOptions::default()
            };
            let mut failed = 0;
            for f in &files {
                let name = f
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let report = check(gpu.as_ref(), &name, &read_shader(f)?, &opts);
                print!("{}", report.render());
                if !report.passed() {
                    failed += 1;
                }
            }
            println!(
                "{} shader{} checked, {} failed{}",
                files.len(),
                if files.len() == 1 { "" } else { "s" },
                failed,
                if cpu_only {
                    " (compile checks only)"
                } else {
                    ""
                }
            );
            Ok(if failed == 0 {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
        }
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(2)
        }
    }
}
