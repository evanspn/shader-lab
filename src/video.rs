//! `shaderlab video`: render many frames in ONE GPU session and encode them.
//!
//! The device and pipeline are created once; each frame only updates `iTime`, `iTimeDelta` and `iFrame` and reads the
//! picture back. Frames are streamed to `ffmpeg` (H.264, yuv420p, +faststart: plays on an iPhone) when it is on
//! `PATH`; otherwise an animated GIF is written with a clear note. `frames` writes a folder of PNGs.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Instant;

use anyhow::{Context, Result, bail};

use crate::gpu::{Gpu, Prepared};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Mp4,
    Gif,
    Frames,
}

impl Format {
    pub fn extension(self) -> &'static str {
        match self {
            Format::Mp4 => "mp4",
            Format::Gif => "gif",
            Format::Frames => "",
        }
    }
}

#[derive(Clone, Debug)]
pub struct VideoOptions {
    pub fps: u32,
    pub duration: f32,
    pub start: f32,
    pub format: Format,
    pub out: PathBuf,
    pub loop_seamless: bool,
    pub ffmpeg: Option<PathBuf>,
    /// threads the mp4 encoder may use (default [`DEFAULT_THREADS`]): ffmpeg left alone takes every core of the machine
    pub threads: u32,
}

#[derive(Debug)]
pub struct VideoReport {
    pub path: PathBuf,
    pub format: Format,
    pub frames: u32,
    pub fps: u32,
    pub size: (u32, u32),
    pub render_secs: f64,
    pub ms_per_frame: f64,
    pub notes: Vec<String>,
}

/// The encoder's thread cap: a long mp4 render must not pin the whole Mac.
pub const DEFAULT_THREADS: u32 = 2;

/// GIF files get large fast: cap the frame rate and width.
pub const GIF_MAX_FPS: u32 = 20;
pub const GIF_MAX_WIDTH: u32 = 640;

/// The `iTime` of output frame `n`.
pub fn frame_time(start: f32, fps: u32, n: u32) -> f32 {
    start + n as f32 / fps as f32
}

/// The ffmpeg to use: `SHADERLAB_NO_FFMPEG=1` hides it (tests, or to force the GIF path); `SHADERLAB_FFMPEG` names a binary.
pub fn find_ffmpeg() -> Option<PathBuf> {
    if std::env::var_os("SHADERLAB_NO_FFMPEG").is_some_and(|v| !v.is_empty()) {
        return None;
    }
    let name =
        std::env::var_os("SHADERLAB_FFMPEG").map_or_else(|| PathBuf::from("ffmpeg"), PathBuf::from);
    let ok = Command::new(&name)
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    ok.then_some(name)
}

/// Decide the format: an explicit one wins, then the output extension, then whatever can be made here.
pub fn choose_format(
    explicit: Option<Format>,
    out: Option<&Path>,
    have_ffmpeg: bool,
) -> Result<(Format, Option<String>)> {
    if let Some(f) = explicit {
        if f == Format::Mp4 && !have_ffmpeg {
            bail!(
                "--format mp4 needs ffmpeg on PATH (brew install ffmpeg); use --format gif to write a GIF without it"
            );
        }
        return Ok((f, None));
    }
    let ext = out
        .and_then(|p| p.extension())
        .map(|e| e.to_string_lossy().to_lowercase());
    match ext.as_deref() {
        Some("mp4") | Some("mov") if !have_ffmpeg => {
            bail!("{} needs ffmpeg on PATH (brew install ffmpeg); name the file .gif to write a GIF without it", out.unwrap().display())
        }
        Some("mp4") | Some("mov") => Ok((Format::Mp4, None)),
        Some("gif") => Ok((Format::Gif, None)),
        _ if have_ffmpeg => Ok((Format::Mp4, None)),
        _ => Ok((
            Format::Gif,
            Some("ffmpeg was not found, so this is an animated GIF (capped at 20 fps and 640 px wide). Install ffmpeg (brew install ffmpeg) for an iPhone-friendly mp4.".into()),
        )),
    }
}

trait Sink {
    fn push(&mut self, rgba: &[u8]) -> Result<()>;
    fn finish(self: Box<Self>) -> Result<()>;
}

struct FfmpegSink {
    child: Child,
    out: PathBuf,
}

impl FfmpegSink {
    fn start(
        ffmpeg: &Path,
        size: (u32, u32),
        fps: u32,
        out: &Path,
        threads: u32,
    ) -> Result<FfmpegSink> {
        // niced (priority 10) so the machine stays responsive for whoever is using it, with the encoder capped to `threads`
        let mut cmd = if cfg!(unix) && Path::new("/usr/bin/nice").exists() {
            let mut c = Command::new("/usr/bin/nice");
            c.args(["-n", "10"]).arg(ffmpeg);
            c
        } else {
            Command::new(ffmpeg)
        };
        let child = cmd
            .args([
                "-y",
                "-loglevel",
                "error",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgba",
            ])
            .args([
                "-s",
                &format!("{}x{}", size.0, size.1),
                "-r",
                &fps.to_string(),
                "-i",
                "-",
            ])
            // H.264 needs even dimensions: trim a stray odd pixel row/column rather than fail
            .args([
                "-filter_threads",
                "1",
                "-vf",
                "scale=trunc(iw/2)*2:trunc(ih/2)*2,format=yuv420p",
            ])
            .args([
                "-c:v",
                "libx264",
                "-preset",
                "veryfast",
                "-crf",
                "18",
                "-threads",
                &threads.max(1).to_string(),
                "-movflags",
                "+faststart",
            ])
            .arg(out)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("starting {}", ffmpeg.display()))?;
        Ok(FfmpegSink {
            child,
            out: out.to_path_buf(),
        })
    }
}

impl Sink for FfmpegSink {
    fn push(&mut self, rgba: &[u8]) -> Result<()> {
        let stdin = self
            .child
            .stdin
            .as_mut()
            .context("ffmpeg's input is closed")?;
        stdin
            .write_all(rgba)
            .context("ffmpeg stopped reading frames (see its message above, if any)")
    }
    fn finish(mut self: Box<Self>) -> Result<()> {
        drop(self.child.stdin.take());
        let output = self
            .child
            .wait_with_output()
            .context("waiting for ffmpeg")?;
        if !output.status.success() {
            bail!(
                "ffmpeg failed writing {}: {}",
                self.out.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }
}

struct GifSink {
    enc: gif::Encoder<std::fs::File>,
    src: (u32, u32),
    dst: (u32, u32),
    delay: u16,
}

impl GifSink {
    fn start(src: (u32, u32), dst: (u32, u32), fps: u32, out: &Path) -> Result<GifSink> {
        let file =
            std::fs::File::create(out).with_context(|| format!("creating {}", out.display()))?;
        let mut enc =
            gif::Encoder::new(file, dst.0 as u16, dst.1 as u16, &[]).context("starting the GIF")?;
        enc.set_repeat(gif::Repeat::Infinite)
            .context("starting the GIF")?;
        Ok(GifSink {
            enc,
            src,
            dst,
            delay: (100.0 / fps as f32).round().max(2.0) as u16,
        })
    }
}

impl Sink for GifSink {
    fn push(&mut self, rgba: &[u8]) -> Result<()> {
        let mut px = if self.src == self.dst {
            rgba.to_vec()
        } else {
            downscale(rgba, self.src, self.dst)
        };
        let mut frame =
            gif::Frame::from_rgba_speed(self.dst.0 as u16, self.dst.1 as u16, &mut px, 10);
        frame.delay = self.delay;
        self.enc.write_frame(&frame).context("writing a GIF frame")
    }
    fn finish(self: Box<Self>) -> Result<()> {
        Ok(())
    }
}

struct PngSink {
    dir: PathBuf,
    size: (u32, u32),
    n: u32,
}

impl Sink for PngSink {
    fn push(&mut self, rgba: &[u8]) -> Result<()> {
        let path = self.dir.join(format!("frame-{:05}.png", self.n));
        image::save_buffer(
            &path,
            rgba,
            self.size.0,
            self.size.1,
            image::ColorType::Rgba8,
        )
        .with_context(|| format!("writing {}", path.display()))?;
        self.n += 1;
        Ok(())
    }
    fn finish(self: Box<Self>) -> Result<()> {
        Ok(())
    }
}

/// Box-filter `rgba` (`src` size) down to `dst`.
pub fn downscale(rgba: &[u8], src: (u32, u32), dst: (u32, u32)) -> Vec<u8> {
    let mut out = vec![0u8; (dst.0 * dst.1 * 4) as usize];
    for y in 0..dst.1 {
        let (y0, y1) = (
            y * src.1 / dst.1,
            ((y + 1) * src.1 / dst.1).max(y * src.1 / dst.1 + 1),
        );
        for x in 0..dst.0 {
            let (x0, x1) = (
                x * src.0 / dst.0,
                ((x + 1) * src.0 / dst.0).max(x * src.0 / dst.0 + 1),
            );
            let mut acc = [0u32; 4];
            for sy in y0..y1.min(src.1) {
                for sx in x0..x1.min(src.0) {
                    let i = ((sy * src.0 + sx) * 4) as usize;
                    for (c, a) in acc.iter_mut().enumerate() {
                        *a += rgba[i + c] as u32;
                    }
                }
            }
            let n = ((y1.min(src.1) - y0) * (x1.min(src.0) - x0)).max(1);
            let o = ((y * dst.0 + x) * 4) as usize;
            for c in 0..4 {
                out[o + c] = (acc[c] / n) as u8;
            }
        }
    }
    out
}

/// Render `opts.duration` seconds of `prepared` and encode them. Returns what was written.
pub fn render_video(gpu: &Gpu, prepared: &Prepared, opts: &VideoOptions) -> Result<VideoReport> {
    let size = prepared.size();
    let mut notes = Vec::new();
    let mut fps = opts.fps.max(1);
    let mut dst = size;
    if opts.format == Format::Gif {
        if fps > GIF_MAX_FPS {
            fps = GIF_MAX_FPS;
            notes.push(format!(
                "GIF is capped at {GIF_MAX_FPS} fps (asked for {})",
                opts.fps
            ));
        }
        if size.0 > GIF_MAX_WIDTH {
            dst = (GIF_MAX_WIDTH, (size.1 * GIF_MAX_WIDTH / size.0).max(1));
            notes.push(format!(
                "GIF is capped at {GIF_MAX_WIDTH} px wide (scaled {}x{} -> {}x{})",
                size.0, size.1, dst.0, dst.1
            ));
        }
    }
    if opts.format == Format::Mp4 && (size.0 % 2 == 1 || size.1 % 2 == 1) {
        notes.push(format!(
            "H.264 needs even dimensions: {}x{} was trimmed to {}x{}",
            size.0,
            size.1,
            size.0 & !1,
            size.1 & !1
        ));
    }
    let frames = ((opts.duration * fps as f32).round() as u32).max(1);
    let mut sink: Box<dyn Sink> = match opts.format {
        Format::Mp4 => {
            let ffmpeg = opts
                .ffmpeg
                .as_deref()
                .context("ffmpeg is required for mp4")?;
            Box::new(FfmpegSink::start(
                ffmpeg,
                size,
                fps,
                &opts.out,
                opts.threads,
            )?)
        }
        Format::Gif => Box::new(GifSink::start(size, dst, fps, &opts.out)?),
        Format::Frames => {
            std::fs::create_dir_all(&opts.out)
                .with_context(|| format!("creating {}", opts.out.display()))?;
            Box::new(PngSink {
                dir: opts.out.clone(),
                size,
                n: 0,
            })
        }
    };

    let delta = 1.0 / fps as f32;
    let started = Instant::now();
    let mut buf = Vec::new();
    // a seamless loop cross-fades the last second into the first: render that tail first and keep it
    let fade = if opts.loop_seamless {
        (fps).min(frames / 2)
    } else {
        0
    };
    let mut tail: Vec<Vec<u8>> = Vec::new();
    for i in 0..fade {
        let n = frames + i;
        prepared
            .draw_rgba8(
                gpu,
                frame_time(opts.start, fps, n),
                delta,
                n as i32,
                &mut buf,
            )
            .map_err(anyhow::Error::msg)?;
        tail.push(buf.clone());
    }
    for n in 0..frames {
        prepared
            .draw_rgba8(
                gpu,
                frame_time(opts.start, fps, n),
                delta,
                n as i32,
                &mut buf,
            )
            .map_err(anyhow::Error::msg)?;
        if n < fade {
            // frame 0 shows the continuation of the end; by frame `fade` it has faded into the plain frame
            let k = n as f32 / fade as f32;
            for (b, t) in buf.iter_mut().zip(&tail[n as usize]) {
                *b = (*t as f32 * (1.0 - k) + *b as f32 * k).round() as u8;
            }
        }
        sink.push(&buf)?;
    }
    sink.finish()?;
    let render_secs = started.elapsed().as_secs_f64();
    Ok(VideoReport {
        path: opts.out.clone(),
        format: opts.format,
        frames,
        fps,
        size: if opts.format == Format::Gif {
            dst
        } else {
            size
        },
        render_secs,
        ms_per_frame: render_secs * 1000.0 / (frames + fade) as f64,
        notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_n_has_the_time_start_plus_n_over_fps() {
        assert_eq!(frame_time(0.0, 24, 0), 0.0);
        assert!((frame_time(2.0, 24, 12) - 2.5).abs() < 1e-6);
        assert!((frame_time(0.0, 30, 90) - 3.0).abs() < 1e-6);
    }

    #[test]
    fn the_format_follows_the_flag_then_the_extension_then_what_is_installed() {
        let p = |s| Some(Path::new(s));
        assert_eq!(
            choose_format(Some(Format::Gif), p("a.mp4"), true)
                .unwrap()
                .0,
            Format::Gif
        );
        assert_eq!(
            choose_format(None, p("a.mp4"), true).unwrap().0,
            Format::Mp4
        );
        assert_eq!(
            choose_format(None, p("a.gif"), true).unwrap().0,
            Format::Gif
        );
        assert_eq!(choose_format(None, None, true).unwrap().0, Format::Mp4);
        let (f, note) = choose_format(None, None, false).unwrap();
        assert_eq!(f, Format::Gif);
        assert!(note.unwrap().contains("brew install ffmpeg"));
        assert!(choose_format(Some(Format::Mp4), None, false).is_err());
        assert!(
            choose_format(None, p("a.mp4"), false)
                .unwrap_err()
                .to_string()
                .contains("ffmpeg")
        );
    }

    #[test]
    fn downscale_averages_boxes() {
        let src = [
            0u8, 0, 0, 255, 100, 100, 100, 255, 200, 200, 200, 255, 100, 100, 100, 255,
        ]; // 2x2
        let out = downscale(&src, (2, 2), (1, 1));
        assert_eq!(out, vec![100, 100, 100, 255]);
    }
}
