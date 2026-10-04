//! `shaderlab video`: frame timing, orientation, the GIF fallback without ffmpeg, mp4 validity, seamless loops.
//! GPU tests skip with a message when there is no adapter; a skip is not a pass.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_shaderlab");

fn have_gpu() -> bool {
    shaderlab::gpu::Gpu::new().is_ok()
}

macro_rules! gpu_or_skip {
    () => {
        if !have_gpu() {
            eprintln!("SKIPPED: no GPU adapter; nothing was verified by this test");
            return;
        }
    };
}

fn run(args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut c = Command::new(BIN);
    c.args(args);
    for (k, v) in envs {
        c.env(k, v);
    }
    c.output().expect("binary runs")
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// A shader whose colour encodes the uniforms: R = iTime/10, G = iFrame/10, B = iTimeDelta*4.
fn time_shader(dir: &Path) -> PathBuf {
    let p = dir.join("time.glsl");
    std::fs::write(
        &p,
        "void mainImage(out vec4 c, in vec2 p) { c = vec4(iTime / 10.0, float(iFrame) / 10.0, iTimeDelta * 4.0, 1.0); }\n",
    )
    .unwrap();
    p
}

fn px(path: &Path, x: u32, y: u32) -> [u8; 4] {
    image::open(path).unwrap().to_rgba8().get_pixel(x, y).0
}

fn near(a: u8, b: f32) -> bool {
    (a as f32 - b).abs() <= 1.5
}

#[test]
fn frame_n_gets_itime_start_plus_n_over_fps_and_the_matching_iframe_and_delta() {
    gpu_or_skip!();
    let td = tempfile::tempdir().unwrap();
    let shader = time_shader(td.path());
    let out = td.path().join("frames");
    let o = run(
        &[
            "video",
            shader.to_str().unwrap(),
            "--format",
            "frames",
            "--size",
            "64x64",
            "--fps",
            "4",
            "--duration",
            "1",
            "--start",
            "2",
            "--out",
            out.to_str().unwrap(),
        ],
        &[],
    );
    assert!(o.status.success(), "{}", text(&o.stderr));
    for n in 0..4u32 {
        let f = out.join(format!("frame-{n:05}.png"));
        let p = px(&f, 10, 10);
        let t = 2.0 + n as f32 / 4.0;
        assert!(
            near(p[0], t / 10.0 * 255.0),
            "frame {n}: R {} wanted iTime {t}",
            p[0]
        );
        assert!(
            near(p[1], n as f32 / 10.0 * 255.0),
            "frame {n}: G {} wanted iFrame {n}",
            p[1]
        );
        assert!(
            near(p[2], 0.25 * 4.0 * 255.0),
            "frame {n}: B {} wanted iTimeDelta 0.25",
            p[2]
        );
    }
    assert!(
        !out.join("frame-00004.png").exists(),
        "exactly fps*duration frames"
    );
    assert!(text(&o.stdout).contains("4 frames"), "{}", text(&o.stdout));
}

#[test]
fn the_origin_flag_is_respected_by_video() {
    gpu_or_skip!();
    let td = tempfile::tempdir().unwrap();
    let shader = td.path().join("y.glsl");
    std::fs::write(
        &shader,
        "void mainImage(out vec4 c, in vec2 p) { c = vec4(p.y / iResolution.y, 0.0, 0.0, 1.0); }\n",
    )
    .unwrap();
    for (origin, top_dark) in [("top-left", true), ("bottom-left", false)] {
        let out = td.path().join(origin);
        let o = run(
            &[
                "video",
                shader.to_str().unwrap(),
                "--format",
                "frames",
                "--size",
                "64x64",
                "--fps",
                "1",
                "--duration",
                "1",
                "--origin",
                origin,
                "--out",
                out.to_str().unwrap(),
            ],
            &[],
        );
        assert!(o.status.success(), "{}", text(&o.stderr));
        let f = out.join("frame-00000.png");
        let (top, bottom) = (px(&f, 32, 1)[0], px(&f, 32, 62)[0]);
        if top_dark {
            assert!(
                top < 20 && bottom > 235,
                "top-left: y grows DOWN (top {top}, bottom {bottom})"
            );
        } else {
            assert!(
                top > 235 && bottom < 20,
                "bottom-left: y grows UP (top {top}, bottom {bottom})"
            );
        }
    }
}

#[test]
fn without_ffmpeg_the_default_is_a_gif_with_a_clear_note_and_the_right_frames() {
    gpu_or_skip!();
    let td = tempfile::tempdir().unwrap();
    let shader = time_shader(td.path());
    let out = td.path().join("clip.gif");
    let o = run(
        &[
            "video",
            shader.to_str().unwrap(),
            "--size",
            "64x64",
            "--fps",
            "6",
            "--duration",
            "1",
            "--out",
            out.to_str().unwrap(),
        ],
        &[("SHADERLAB_NO_FFMPEG", "1")],
    );
    assert!(o.status.success(), "{}", text(&o.stderr));
    let bytes = std::fs::read(&out).unwrap();
    assert_eq!(&bytes[..6], b"GIF89a");
    let mut dec = gif::DecodeOptions::new()
        .read_info(std::fs::File::open(&out).unwrap())
        .unwrap();
    let mut n = 0;
    while dec.read_next_frame().unwrap().is_some() {
        n += 1;
    }
    assert_eq!(n, 6);
    // no extension and no ffmpeg: the note explains the fallback
    let o = run(
        &[
            "video",
            shader.to_str().unwrap(),
            "--size",
            "64x64",
            "--fps",
            "6",
            "--duration",
            "1",
            "--out",
            td.path().join("auto").to_str().unwrap(),
        ],
        &[("SHADERLAB_NO_FFMPEG", "1")],
    );
    assert!(
        text(&o.stderr).contains("brew install ffmpeg") && text(&o.stdout).contains("auto"),
        "{}",
        text(&o.stderr)
    );
}

#[test]
fn a_gif_is_capped_in_size_and_rate_and_says_so() {
    gpu_or_skip!();
    let td = tempfile::tempdir().unwrap();
    let shader = time_shader(td.path());
    let out = td.path().join("big.gif");
    let o = run(
        &[
            "video",
            shader.to_str().unwrap(),
            "--size",
            "1280x720",
            "--fps",
            "30",
            "--duration",
            "0.5",
            "--format",
            "gif",
            "--out",
            out.to_str().unwrap(),
        ],
        &[],
    );
    assert!(o.status.success(), "{}", text(&o.stderr));
    let err = text(&o.stderr);
    assert!(
        err.contains("capped at 20 fps") && err.contains("640 px wide"),
        "{err}"
    );
    let dec = gif::DecodeOptions::new()
        .read_info(std::fs::File::open(&out).unwrap())
        .unwrap();
    assert_eq!((dec.width(), dec.height()), (640, 360));
}

#[test]
fn asking_for_mp4_without_ffmpeg_is_a_plain_error() {
    let td = tempfile::tempdir().unwrap();
    let shader = time_shader(td.path());
    let o = run(
        &[
            "video",
            shader.to_str().unwrap(),
            "--format",
            "mp4",
            "--out",
            td.path().join("x.mp4").to_str().unwrap(),
        ],
        &[("SHADERLAB_NO_FFMPEG", "1")],
    );
    assert_eq!(o.status.code(), Some(2));
    assert!(
        text(&o.stderr).contains("needs ffmpeg"),
        "{}",
        text(&o.stderr)
    );
    assert!(!text(&o.stderr).contains("panicked"));
}

#[test]
fn an_mp4_is_valid_h264_yuv420p_with_the_right_frame_count_and_even_dimensions() {
    gpu_or_skip!();
    let probe = Command::new("ffprobe").arg("-version").output();
    if !Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_ok_and(|o| o.status.success())
        || probe.is_err()
    {
        eprintln!(
            "SKIPPED: ffmpeg/ffprobe are not installed; the mp4 path was not verified by this test"
        );
        return;
    }
    let td = tempfile::tempdir().unwrap();
    let shader = time_shader(td.path());
    let out = td.path().join("clip.mp4");
    // 63x63 is odd: it must be trimmed to 62x62 instead of failing
    let o = run(
        &[
            "video",
            shader.to_str().unwrap(),
            "--size",
            "63x63",
            "--fps",
            "6",
            "--duration",
            "1",
            "--out",
            out.to_str().unwrap(),
        ],
        &[],
    );
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert!(
        text(&o.stderr).contains("even dimensions"),
        "{}",
        text(&o.stderr)
    );
    let p = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_name,pix_fmt,width,height,nb_frames",
            "-of",
            "default=nw=1",
        ])
        .arg(&out)
        .output()
        .unwrap();
    let s = text(&p.stdout);
    for want in [
        "codec_name=h264",
        "pix_fmt=yuv420p",
        "width=62",
        "height=62",
        "nb_frames=6",
    ] {
        assert!(s.contains(want), "ffprobe says:\n{s}\nmissing {want}");
    }
    // +faststart: the moov atom comes before the media data so it plays while downloading (iPhone, iMessage)
    let bytes = std::fs::read(&out).unwrap();
    let find = |tag: &[u8]| bytes.windows(4).position(|w| w == tag).unwrap();
    assert!(
        find(b"moov") < find(b"mdat"),
        "moov must precede mdat (faststart)"
    );
}

#[test]
fn a_seamless_loop_starts_where_the_clip_ends() {
    gpu_or_skip!();
    let td = tempfile::tempdir().unwrap();
    let shader = td.path().join("ramp.glsl");
    std::fs::write(
        &shader,
        "void mainImage(out vec4 c, in vec2 p) { c = vec4(iTime / 4.0, 0.0, 0.0, 1.0); }\n",
    )
    .unwrap();
    let mut firsts = Vec::new();
    for flag in [false, true] {
        let out = td.path().join(if flag { "loop" } else { "plain" });
        let mut args = vec![
            "video",
            shader.to_str().unwrap(),
            "--format",
            "frames",
            "--size",
            "32x32",
            "--fps",
            "4",
            "--duration",
            "4",
            "--out",
            out.to_str().unwrap(),
        ];
        if flag {
            args.push("--loop-seamless");
        }
        let o = run(&args, &[]);
        assert!(o.status.success(), "{}", text(&o.stderr));
        let first = px(&out.join("frame-00000.png"), 5, 5)[0];
        let last = px(&out.join("frame-00015.png"), 5, 5)[0];
        let mid = px(&out.join("frame-00008.png"), 5, 5)[0];
        assert!(
            near(mid, 2.0 / 4.0 * 255.0),
            "after the fade the frames are untouched"
        );
        firsts.push((first, last));
    }
    let (plain_first, plain_last) = firsts[0];
    let (loop_first, loop_last) = firsts[1];
    assert!(
        near(plain_first, 0.0) && plain_last > 230,
        "a plain clip jumps from bright to dark"
    );
    assert_eq!(plain_last, loop_last, "the last frame is not changed");
    assert!(
        near(loop_first, 255.0),
        "a seamless clip opens with the continuation of its end ({loop_first})"
    );
    assert!(
        (loop_first as i32 - loop_last as i32).abs() <= 20,
        "no jump at the loop point"
    );
}
