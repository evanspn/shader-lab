//! The regression checks must actually catch regressions: each check class passes on a good shader and FAILS on a deliberately
//! broken variant of it (a changed colour, a flipped scene, a flat block, a slowed shader, a popping frame, a lurch in
//! brightness, text that is painted over).

use shaderlab::gpu::Gpu;
use shaderlab::regress::{self, Options, Row, Status};

fn gpu() -> Option<Gpu> {
    Gpu::new().ok()
}

/// A small landscape-ish shader with switches for each way of breaking it.
fn shader(
    colour: &str,
    flip: bool,
    flat: bool,
    heavy: u32,
    pop: bool,
    lurch: bool,
    paint_text: bool,
) -> String {
    format!(
        r#"// @float amount 0.5 0 1 "Amount"
void mainImage(out vec4 fragColor, in vec2 fragCoord) {{
    vec2 uv = fragCoord / iResolution.xy;
    vec4 term = texture(iChannel0, uv);
    float keep = {keep};
    if (keep > 0.5) {{ fragColor = term; return; }}
    {pop}
    float y = {y};
    vec3 col = mix({colour}, vec3(0.1, 0.3, 0.2), y);
    col *= 0.7 + 0.3 * (0.5 + 0.5 * sin(uv.x * 6.0 + iTime * 0.8));
    {flat}
    for (int i = 0; i < {heavy}; i++) {{ col += 1e-6 * sin(float(i) + col * 3.0); }}
    if (popf) {{ col = col.zyx * 1.8; }}
    {lurch}
    fragColor = vec4(col, 1.0);
}}
"#,
        keep = if paint_text {
            "0.0"
        } else {
            "gp_textMask(fragCoord, term)"
        },
        pop = if pop {
            "bool popf = fract(floor(iTime * 24.0 + 0.5) / 61.0 + 0.001) < 0.02;"
        } else {
            "bool popf = false;"
        },
        y = if flip { "1.0 - uv.y" } else { "uv.y" },
        flat = if flat {
            "if (uv.x > 0.15 && uv.x < 0.75 && uv.y > 0.5 && uv.y < 0.9) { col = vec3(0.2, 0.3, 0.6); }"
        } else {
            ""
        },
        lurch = if lurch {
            "col *= 1.0 + 0.5 * step(9.0, iTime);"
        } else {
            ""
        },
    )
}

const SKY: &str = "vec3(0.5, 0.7, 1.0)";

fn good() -> String {
    shader(SKY, false, false, 1, false, false, false)
}

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Fixture {
        Fixture {
            dir: tempfile::tempdir().unwrap(),
        }
    }
    fn file(&self) -> std::path::PathBuf {
        self.dir.path().join("demo.glsl")
    }
    fn opts(&self) -> Options {
        let mut o = Options::for_repo(self.dir.path());
        o.machine = Some("test-machine".into());
        o
    }
    fn write(&self, src: &str) {
        std::fs::write(self.file(), src).unwrap();
    }
    fn run(&self, o: &Options) -> Vec<Row> {
        regress::run(&gpu().unwrap(), &[self.file()], o)
    }
    /// Record goldens and the baseline from `src`.
    fn accept(&self, src: &str) {
        self.write(src);
        let mut o = self.opts();
        o.update = true;
        let rows = self.run(&o);
        assert!(rows.iter().all(|r| r.status != Status::Fail), "{rows:?}");
    }
}

fn failed(rows: &[Row], check: &str) -> bool {
    rows.iter()
        .any(|r| r.status == Status::Fail && r.check.starts_with(check))
}

fn skip_without_gpu() -> bool {
    if gpu().is_none() {
        eprintln!("SKIPPED: no GPU adapter; nothing was verified by this test");
        return true;
    }
    false
}

#[test]
fn a_good_shader_passes_every_class_and_the_update_writes_tiny_synthetic_goldens() {
    if skip_without_gpu() {
        return;
    }
    let fx = Fixture::new();
    fx.accept(&good());
    // (perf is compared by its own test: timing numbers are not stable while other tests share the GPU)
    let mut o = fx.opts();
    o.only = ["golden", "orient", "text", "coverage", "temporal"]
        .map(String::from)
        .to_vec();
    let rows = fx.run(&o);
    let bad: Vec<&Row> = rows.iter().filter(|r| r.status == Status::Fail).collect();
    assert!(bad.is_empty(), "{bad:?}");
    for class in ["golden", "orient", "text", "coverage", "temporal", "seam"] {
        assert!(
            rows.iter()
                .any(|r| r.check.starts_with(class) && r.status == Status::Pass),
            "no passing {class} row: {rows:?}"
        );
    }
    // only small PNGs and the accepted values, under tests/golden
    let mut total = 0;
    for e in walk(&fx.dir.path().join("tests/golden")) {
        let ext = e
            .extension()
            .map(|x| x.to_string_lossy().into_owned())
            .unwrap_or_default();
        assert!(ext == "png" || ext == "meta", "{e:?}");
        total += std::fs::metadata(&e).unwrap().len();
    }
    assert!(total < 200_000, "goldens are {total} bytes");
    assert!(fx.dir.path().join("perf-baseline.json").exists());
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut v = Vec::new();
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        if e.path().is_dir() {
            v.extend(walk(&e.path()));
        } else {
            v.push(e.path());
        }
    }
    v
}

#[test]
fn a_changed_colour_fails_the_golden() {
    if skip_without_gpu() {
        return;
    }
    let fx = Fixture::new();
    fx.accept(&good());
    fx.write(&shader(
        "vec3(0.9, 0.4, 0.3)",
        false,
        false,
        1,
        false,
        false,
        false,
    ));
    let mut o = fx.opts();
    o.only = vec!["golden".into()];
    assert!(failed(&fx.run(&o), "golden"));
}

#[test]
fn a_flipped_scene_fails_the_orientation_goldens() {
    if skip_without_gpu() {
        return;
    }
    let fx = Fixture::new();
    fx.accept(&good());
    fx.write(&shader(SKY, true, false, 1, false, false, false));
    let mut o = fx.opts();
    o.only = vec!["orient".into()];
    let rows = fx.run(&o);
    assert!(failed(&rows, "orient"), "{rows:?}");
}

#[test]
fn a_flat_block_fails_the_coverage_and_flat_check() {
    if skip_without_gpu() {
        return;
    }
    let fx = Fixture::new();
    fx.accept(&good());
    fx.write(&shader(SKY, false, true, 1, false, false, false));
    let mut o = fx.opts();
    o.only = vec!["coverage".into()];
    let rows = fx.run(&o);
    assert!(
        rows.iter()
            .any(|r| r.status == Status::Fail && r.detail.contains("flat block")),
        "{rows:?}"
    );
}

#[test]
fn a_slowed_shader_fails_the_perf_baseline_but_only_on_the_machine_it_was_recorded_on() {
    if skip_without_gpu() {
        return;
    }
    let fx = Fixture::new();
    fx.accept(&good());
    fx.write(&shader(SKY, false, false, 4000, false, false, false));
    let mut o = fx.opts();
    o.only = vec!["perf".into()];
    let rows = fx.run(&o);
    assert!(failed(&rows, "perf"), "{rows:?}");
    // on another machine the numbers mean nothing: skipped, with the reason said
    o.machine = Some("some-other-machine".into());
    let rows = fx.run(&o);
    assert!(
        rows.iter().all(|r| r.status == Status::Skip)
            && rows.iter().any(|r| r.detail.contains("not compared")),
        "{rows:?}"
    );
}

#[test]
fn a_popping_frame_and_a_lurch_in_brightness_fail_the_temporal_check() {
    if skip_without_gpu() {
        return;
    }
    let fx = Fixture::new();
    fx.accept(&good());
    let mut o = fx.opts();
    o.only = vec!["temporal".into()];
    o.fast = true;
    fx.write(&shader(SKY, false, false, 1, true, false, false));
    let rows = fx.run(&o);
    assert!(
        rows.iter()
            .any(|r| r.status == Status::Fail && r.detail.contains("pops")),
        "{rows:?}"
    );
    fx.write(&shader(SKY, false, false, 1, false, true, false));
    let rows = fx.run(&o);
    assert!(
        rows.iter()
            .any(|r| r.status == Status::Fail && r.detail.contains("lurches")),
        "{rows:?}"
    );
    // and the good one is fine
    fx.write(&good());
    assert!(!fx.run(&o).iter().any(|r| r.status == Status::Fail));
}

#[test]
fn painting_over_the_text_fails_the_text_check() {
    if skip_without_gpu() {
        return;
    }
    let fx = Fixture::new();
    fx.accept(&good());
    fx.write(&shader(SKY, false, false, 1, false, false, true));
    let mut o = fx.opts();
    o.only = vec!["text".into()];
    assert!(failed(&fx.run(&o), "text"));
}

#[test]
fn a_missing_golden_is_a_failure_that_says_how_to_fix_it() {
    if skip_without_gpu() {
        return;
    }
    let fx = Fixture::new();
    fx.write(&good());
    let mut o = fx.opts();
    o.only = vec!["golden".into()];
    let rows = fx.run(&o);
    assert!(
        rows.iter()
            .any(|r| r.status == Status::Fail && r.detail.contains("--update")),
        "{rows:?}"
    );
}

#[test]
fn the_cli_exits_non_zero_on_a_failure_and_zero_when_all_is_well() {
    if skip_without_gpu() {
        return;
    }
    let fx = Fixture::new();
    fx.accept(&good());
    let bin = env!("CARGO_BIN_EXE_shaderlab");
    let run = |args: &[&str]| {
        std::process::Command::new(bin)
            .args(args)
            .current_dir(fx.dir.path())
            .output()
            .unwrap()
    };
    // (the real machine id, not the test override, so perf is excluded here)
    let ok = run(&[
        "regress",
        "demo.glsl",
        "--only",
        "golden,orient,text,coverage",
    ]);
    assert!(
        ok.status.success(),
        "{}",
        String::from_utf8_lossy(&ok.stdout)
    );
    fx.write(&shader(
        "vec3(0.9, 0.4, 0.3)",
        false,
        false,
        1,
        false,
        false,
        false,
    ));
    let bad = run(&["regress", "demo.glsl", "--only", "golden"]);
    assert!(!bad.status.success());
    assert!(String::from_utf8_lossy(&bad.stdout).contains("FAIL demo [default] golden"));
}
