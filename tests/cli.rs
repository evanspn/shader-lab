//! The binary: CPU-only paths (no GPU needed) and, when a GPU exists, the PNG-producing commands.

use std::path::Path;
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_shaderlab");

fn shaders() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/shaders")
}

fn run(args: &[&str]) -> Output {
    Command::new(BIN).args(args).output().expect("binary runs")
}

fn text(o: &[u8]) -> String {
    String::from_utf8_lossy(o).into_owned()
}

fn have_gpu() -> bool {
    shaderlab::gpu::Gpu::new().is_ok()
}

#[test]
fn help_states_both_orientation_conventions() {
    let o = run(&["--help"]);
    let t = text(&o.stdout);
    for want in [
        "render",
        "contact-sheet",
        "check",
        "TOP-left",
        "BOTTOM-left",
        "y grows DOWN",
        "y grows UP",
        "row 0 at the top",
    ] {
        assert!(t.contains(want), "--help is missing '{want}':\n{t}");
    }
    assert!(!t.contains(" list "), "there is no list command");
    let c = text(&run(&["check", "--help"]).stdout);
    assert!(
        c.contains("--origin")
            && c.contains("--epsilon")
            && c.contains("--sharpness")
            && c.contains("--skip"),
        "{c}"
    );
    assert!(text(&run(&["--version"]).stdout).contains("shaderlab"));
}

#[test]
fn cpu_only_check_compiles_the_good_examples_and_fails_the_broken_one_with_its_line() {
    let ok = run(&[
        "check",
        "--cpu-only",
        shaders().join("vignette.glsl").to_str().unwrap(),
        shaders().join("rain-down.glsl").to_str().unwrap(),
    ]);
    assert!(ok.status.success(), "{}", text(&ok.stdout));
    assert!(text(&ok.stdout).contains("compile checks only"));
    let bad = run(&[
        "check",
        "--cpu-only",
        shaders().join("broken.glsl").to_str().unwrap(),
    ]);
    assert_eq!(bad.status.code(), Some(1));
    let t = text(&bad.stdout);
    assert!(
        t.contains("FAIL broken.glsl") && t.contains("no_such_function") && t.contains("line 6"),
        "{t}"
    );
}

#[test]
fn a_folder_is_checked_file_by_file_and_a_failure_makes_the_exit_code_non_zero() {
    let o = run(&["check", "--cpu-only", shaders().to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(1), "broken.glsl is in there");
    let t = text(&o.stdout);
    assert!(t.contains("7 shaders checked, 1 failed"), "{t}");
    for f in [
        "vignette.glsl",
        "rain-down.glsl",
        "shadertoy-glow.glsl",
        "blurry-wash.glsl",
        "rain-wrong-way.glsl",
        "claims-to-animate.glsl",
        "broken.glsl",
    ] {
        assert!(t.contains(f), "{f} missing from:\n{t}");
    }
}

#[test]
fn bad_input_is_a_plain_error_not_a_panic() {
    for args in [
        vec!["check"],
        vec!["render", "/nope/missing.glsl"],
        vec!["render", "x.glsl", "--size", "banana"],
        vec!["check", "--cpu-only", "/nope/x.glsl"],
    ] {
        let o = run(&args);
        assert_eq!(o.status.code(), Some(2), "{args:?}: {}", text(&o.stderr));
        assert!(
            text(&o.stderr).starts_with("error:"),
            "{args:?}: {}",
            text(&o.stderr)
        );
        assert!(!text(&o.stderr).contains("panicked"));
    }
}

#[test]
fn render_and_contact_sheet_write_pngs_of_the_right_size() {
    if !have_gpu() {
        eprintln!("SKIPPED: no GPU adapter; nothing was verified by this test");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let png = dir.path().join("r.png");
    let o = run(&[
        "render",
        shaders().join("rain-down.glsl").to_str().unwrap(),
        "--size",
        "400x225",
        "--time",
        "3",
        "--set",
        "density=0.3",
        "--out",
        png.to_str().unwrap(),
    ]);
    assert!(o.status.success(), "{}", text(&o.stderr));
    let img = image::open(&png).unwrap();
    assert_eq!((img.width(), img.height()), (400, 225));
    assert!(text(&o.stdout).contains("origin top-left"));
    // unknown preset / parameter / value are plain errors
    let o = run(&[
        "render",
        shaders().join("rain-down.glsl").to_str().unwrap(),
        "--preset",
        "nope",
        "--out",
        png.to_str().unwrap(),
    ]);
    assert_eq!(o.status.code(), Some(2));
    assert!(text(&o.stderr).contains("no preset 'nope'"));
    let o = run(&[
        "render",
        shaders().join("rain-down.glsl").to_str().unwrap(),
        "--set",
        "density=9",
        "--out",
        png.to_str().unwrap(),
    ]);
    assert_eq!(o.status.code(), Some(2));
    assert!(text(&o.stderr).contains("between"), "{}", text(&o.stderr));
    // a sheet: 2 presets-rows would need presets; here 3 times in one row
    let sheet = dir.path().join("s.png");
    let o = run(&[
        "contact-sheet",
        shaders().join("vignette.glsl").to_str().unwrap(),
        "--times",
        "0,2,5",
        "--presets",
        "all",
        "--size",
        "160x90",
        "--out",
        sheet.to_str().unwrap(),
    ]);
    assert!(o.status.success(), "{}", text(&o.stderr));
    let img = image::open(&sheet).unwrap();
    // defaults + 2 presets = 3 rows of 3 columns
    assert_eq!(
        (img.width(), img.height()),
        (3 * 160 + 2 * 4, 3 * 90 + 2 * 4)
    );
}

#[test]
fn check_on_the_gpu_exits_zero_for_a_good_shader_and_one_for_a_bad_one() {
    if !have_gpu() {
        eprintln!("SKIPPED: no GPU adapter; nothing was verified by this test");
        return;
    }
    let ok = run(&[
        "check",
        shaders().join("vignette.glsl").to_str().unwrap(),
        "--skip",
        "perf",
    ]);
    assert!(ok.status.success(), "{}", text(&ok.stdout));
    let bad = run(&[
        "check",
        shaders().join("blurry-wash.glsl").to_str().unwrap(),
        "--skip",
        "perf",
    ]);
    assert_eq!(bad.status.code(), Some(1));
    assert!(text(&bad.stdout).contains("FAIL text"));
    // the same blurry shader passes when the user explicitly skips the text check
    let skipped = run(&[
        "check",
        shaders().join("blurry-wash.glsl").to_str().unwrap(),
        "--skip",
        "text,perf",
    ]);
    assert!(skipped.status.success(), "{}", text(&skipped.stdout));
}
