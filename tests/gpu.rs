//! End-to-end on the GPU: the examples must pass, each fixture must fail for the right reason, and `--origin` must really
//! flip the world. With no GPU adapter (headless CI) these skip with a message; they never pass while claiming a check.

use std::path::Path;

use shaderlab::check::{CheckOptions, Report, check};
use shaderlab::frame::Frame;
use shaderlab::gpu::{self, Gpu, Origin};
use shaderlab::params::{self, RenderContext};

macro_rules! gpu_or_skip {
    () => {
        match Gpu::new() {
            Ok(g) => g,
            Err(e) => {
                eprintln!("SKIPPED: {e}; nothing was verified by this test");
                return;
            }
        }
    };
}

fn example(name: &str) -> String {
    std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("examples/shaders")
            .join(name),
    )
    .unwrap()
}

fn run(g: &Gpu, name: &str, origin: Origin) -> Report {
    check(
        Some(g),
        name,
        &example(name),
        &CheckOptions {
            origin,
            perf_frames: 5,
            ..CheckOptions::default()
        },
    )
}

fn failed(r: &Report) -> Vec<&str> {
    r.findings
        .iter()
        .filter(|f| !f.passed)
        .map(|f| f.check.as_str())
        .collect()
}

#[test]
fn the_examples_that_are_meant_to_be_good_pass_every_check() {
    let g = gpu_or_skip!();
    for (name, origin) in [
        ("vignette.glsl", Origin::TopLeft),
        ("rain-down.glsl", Origin::TopLeft),
        ("shadertoy-glow.glsl", Origin::BottomLeft),
    ] {
        let r = run(&g, name, origin);
        assert!(r.passed(), "{}", r.render());
        // every check ran (nothing silently skipped)
        let ran: Vec<&str> = r.findings.iter().map(|f| f.check.as_str()).collect();
        for c in [
            "compiles",
            "finite",
            "not blank",
            "text",
            "animation",
            "motion",
            "speed",
        ] {
            assert!(
                ran.contains(&c),
                "{name}: the {c} check did not run: {ran:?}"
            );
        }
        assert!(r.frame_ms.is_some_and(|ms| ms > 0.0));
    }
}

#[test]
fn a_blurry_wash_is_caught_by_the_text_preservation_check() {
    let g = gpu_or_skip!();
    let r = run(&g, "blurry-wash.glsl", Origin::TopLeft);
    assert_eq!(failed(&r), vec!["text"], "{}", r.render());
    let detail = &r
        .findings
        .iter()
        .find(|f| f.check == "text")
        .unwrap()
        .detail;
    assert!(
        detail.contains("text pixels changed by up to") && detail.contains("edge energy"),
        "both the pixel and the sharpness test fire: {detail}"
    );
}

#[test]
fn the_sharpness_check_alone_catches_a_blur_that_keeps_text_pixels_within_epsilon() {
    let g = gpu_or_skip!();
    // a very loose pixel epsilon lets the change through, so only the edge-energy test can fail it
    let r = check(
        Some(&g),
        "blurry-wash.glsl",
        &example("blurry-wash.glsl"),
        &CheckOptions {
            epsilon: 255,
            skip: vec!["perf".into()],
            ..CheckOptions::default()
        },
    );
    assert_eq!(failed(&r), vec!["text"], "{}", r.render());
    let detail = &r
        .findings
        .iter()
        .find(|f| f.check == "text")
        .unwrap()
        .detail;
    assert!(
        detail.contains("edge energy") && !detail.contains("changed by"),
        "{detail}"
    );
}

#[test]
fn a_shader_moving_the_wrong_way_fails_its_declared_motion() {
    let g = gpu_or_skip!();
    let r = run(&g, "rain-wrong-way.glsl", Origin::TopLeft);
    assert_eq!(failed(&r), vec!["motion"], "{}", r.render());
    let detail = &r
        .findings
        .iter()
        .find(|f| f.check == "motion")
        .unwrap()
        .detail;
    assert!(
        detail.contains("declared down") && detail.contains("dy -"),
        "it moved UP: {detail}"
    );
}

#[test]
fn a_static_shader_that_pretends_to_animate_fails() {
    let g = gpu_or_skip!();
    let r = run(&g, "claims-to-animate.glsl", Origin::TopLeft);
    assert_eq!(failed(&r), vec!["animation"], "{}", r.render());
}

#[test]
fn a_shader_that_does_not_compile_fails_and_names_the_line_of_the_file() {
    let g = gpu_or_skip!();
    let r = run(&g, "broken.glsl", Origin::TopLeft);
    assert_eq!(failed(&r), vec!["compiles"]);
    let detail = &r
        .findings
        .iter()
        .find(|f| f.check == "compiles")
        .unwrap()
        .detail;
    assert!(
        detail.contains("no_such_function") && detail.contains("line 6"),
        "{detail}"
    );
    // nothing else ran after a compile failure
    assert_eq!(r.findings.len(), 1);
}

/// Mean light added to a plain frame by the glow example, in the top fifth and the bottom fifth of the picture.
fn glow_top_and_bottom(g: &Gpu, origin: Origin) -> (f32, f32) {
    let frame = Frame::sample(320, 180);
    let mut plain = Frame::sample(320, 180);
    for p in plain.rgba.chunks_mut(4) {
        p[..3].copy_from_slice(&[frame.background.0, frame.background.1, frame.background.2]);
    }
    let src = example("shadertoy-glow.glsl");
    let ctx = RenderContext {
        background: frame.background,
        ..RenderContext::default()
    };
    let text = params::render_ctx(&src, &Default::default(), &ctx).unwrap();
    let out = gpu::to_rgba8(
        &g.prepare(&text, &plain, origin)
            .unwrap()
            .draw(g, 0.0)
            .unwrap(),
    );
    let band = |rows: std::ops::Range<usize>| -> f32 {
        let mut sum = 0.0;
        for y in rows.clone() {
            for x in 0..320 {
                let i = (y * 320 + x) * 4;
                sum += (out[i + 2] as f32 - plain.rgba[i + 2] as f32).max(0.0);
            }
        }
        sum / (rows.len() * 320) as f32
    };
    (band(0..36), band(144..180))
}

#[test]
fn the_origin_flag_really_flips_which_edge_is_the_top() {
    let g = gpu_or_skip!();
    // written the Shadertoy way: the glow is at the TOP only when the origin is bottom-left
    let (top, bottom) = glow_top_and_bottom(&g, Origin::BottomLeft);
    assert!(
        top > 5.0 && bottom < 0.5,
        "--origin bottom-left: glow at the top of the picture (top {top}, bottom {bottom})"
    );
    // run as if it were a Ghostty shader (the default), the same file puts the glow at the BOTTOM: the classic mistake
    let (top, bottom) = glow_top_and_bottom(&g, Origin::TopLeft);
    assert!(
        bottom > 5.0 && top < 0.5,
        "--origin top-left: glow at the bottom (top {top}, bottom {bottom})"
    );
}

#[test]
fn the_check_is_deterministic_and_text_frames_from_a_png_work() {
    let g = gpu_or_skip!();
    let dir = tempfile::tempdir().unwrap();
    let png = dir.path().join("frame.png");
    let f = Frame::sample(400, 225);
    image::save_buffer(&png, &f.rgba, 400, 225, image::ColorType::Rgba8).unwrap();
    let opts = CheckOptions {
        frame: Some(png),
        skip: vec!["perf".into()],
        ..CheckOptions::default()
    };
    let a = check(Some(&g), "v", &example("vignette.glsl"), &opts);
    let b = check(Some(&g), "v", &example("vignette.glsl"), &opts);
    assert!(a.passed(), "{}", a.render());
    assert_eq!(a.render(), b.render());
}

#[test]
fn opacity_zero_returns_the_terminal_untouched_and_params_apply_exactly_as_in_the_real_path() {
    let g = gpu_or_skip!();
    let frame = Frame::sample(320, 180);
    let src = example("rain-down.glsl");
    let schema = params::parse_schema(&src).unwrap();
    let ctx = RenderContext {
        background: frame.background,
        ..RenderContext::default()
    };
    let off = params::render_ctx(
        &src,
        &params::values_from_args(&schema, None, &["opacity=0".into()]).unwrap(),
        &ctx,
    )
    .unwrap();
    let out = gpu::to_rgba8(
        &g.prepare(&off, &frame, Origin::TopLeft)
            .unwrap()
            .draw(&g, 4.0)
            .unwrap(),
    );
    assert!(out == frame.rgba, "opacity 0 must be a pass-through");
    let on = params::render_ctx(
        &src,
        &params::values_from_args(&schema, None, &["opacity=1".into(), "density=0.5".into()])
            .unwrap(),
        &ctx,
    )
    .unwrap();
    let out = gpu::to_rgba8(
        &g.prepare(&on, &frame, Origin::TopLeft)
            .unwrap()
            .draw(&g, 4.0)
            .unwrap(),
    );
    assert!(out != frame.rgba, "and with opacity 1 the rain shows");
}
