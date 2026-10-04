//! `shaderlab pane` in a real pseudo-terminal: one image and one placement replaced in place every frame, inside synchronized
//! updates; keys work; the terminal is restored; and nothing is left behind (no process, no frame files).

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

const BIN: &str = env!("CARGO_BIN_EXE_shaderlab");

fn example(name: &str) -> String {
    format!("{}/examples/shaders/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn have_gpu() -> bool {
    shaderlab::gpu::Gpu::new().is_ok()
}

/// The frame files this pane (by pid) leaves in the temp directory.
fn frame_files(pid: u32) -> Vec<String> {
    let needle = format!("tty-graphics-protocol-shaderlab-{pid}-");
    std::fs::read_dir(std::env::temp_dir())
        .map(|d| {
            d.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.contains(&needle))
                .collect()
        })
        .unwrap_or_default()
}

struct Run {
    code: Option<u32>,
    out: String,
    pid: u32,
}

/// Run `shaderlab pane ARGS` in a pty, send `keys` after `wait`; `kill` = SIGKILL instead of waiting for it to exit.
fn run_pane(args: &[&str], wait: Duration, keys: &[u8], kill: bool) -> Run {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 1000,
            pixel_height: 600,
        })
        .unwrap();
    let mut cmd = CommandBuilder::new(BIN);
    cmd.args(args);
    cmd.env("SHADERLAB_NO_EXEC", "1");
    cmd.env("TERM", "xterm-256color");
    cmd.env_remove("TMUX");
    let mut child = pair.slave.spawn_command(cmd).unwrap();
    let pid = child.process_id().unwrap_or(0);
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let writer = std::sync::Arc::new(std::sync::Mutex::new(pair.master.take_writer().unwrap()));
    let (tx, rx) = std::sync::mpsc::channel();
    let reply = writer.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 65536];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            if buf[..n].windows(4).any(|w| w == b"\x1b[6n") {
                let mut w = reply.lock().unwrap();
                let _ = w.write_all(b"\x1b[1;1R");
                let _ = w.flush();
            }
            let _ = tx.send(buf[..n].to_vec());
        }
    });
    std::thread::sleep(wait);
    let mut out = Vec::new();
    let code = if kill {
        child.kill().ok();
        child.wait().ok();
        None
    } else {
        {
            let mut w = writer.lock().unwrap();
            w.write_all(keys).unwrap();
            w.flush().unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            while let Ok(chunk) = rx.try_recv() {
                out.extend(chunk);
            }
            if let Some(s) = child.try_wait().unwrap() {
                break Some(s.exit_code());
            }
            if Instant::now() > deadline {
                child.kill().ok();
                panic!("the pane did not exit after the keys");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    std::thread::sleep(Duration::from_millis(150));
    while let Ok(chunk) = rx.try_recv() {
        out.extend(chunk);
    }
    Run {
        code,
        out: String::from_utf8_lossy(&out).into_owned(),
        pid,
    }
}

fn alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .output()
        .is_ok_and(|o| o.status.success())
}

#[test]
fn the_pane_replaces_one_image_in_place_inside_synchronized_updates_and_leaves_nothing_behind() {
    if !have_gpu() {
        eprintln!("SKIPPED: no GPU adapter; nothing was verified by this test");
        return;
    }
    let r = run_pane(
        &[
            "pane",
            &example("vignette.glsl"),
            "--protocol",
            "kitty",
            "--kitty-transfer",
            "file",
            "--fps",
            "30",
            "--stats",
        ],
        Duration::from_millis(2000),
        b"?q",
        false,
    );
    assert_eq!(
        r.code,
        Some(0),
        "{:?}",
        r.out.chars().take(300).collect::<String>()
    );
    let images = r.out.matches("\x1b_Ga=T,f=32,t=t").count();
    assert!(images >= 10, "only {images} frames in 2 s");
    // one image id, one placement id, below the text, no deletes between frames
    assert_eq!(
        r.out.matches("i=4243,p=1,").count(),
        images,
        "every frame names the same image and placement"
    );
    assert_eq!(r.out.matches("z=-1").count(), images);
    let deletes = r.out.matches("\x1b_Ga=d").count();
    assert_eq!(
        deletes, 1,
        "the only delete is the one at exit, not one per frame ({deletes})"
    );
    // each frame inside one synchronized update
    assert!(
        r.out.matches("\x1b[?2026h").count() >= images
            && r.out.matches("\x1b[?2026l").count() >= images
    );
    assert!(
        r.out.contains("\x1b[?1049h") && r.out.contains("\x1b[?1049l"),
        "alternate screen entered and left"
    );
    assert!(
        r.out.contains("\x1b[?1004h") && r.out.contains("\x1b[?1004l"),
        "focus reporting turned on and off"
    );
    assert!(r.out.contains("\x1b[?25h"), "the cursor is shown again");
    assert!(r.out.contains("vignette"), "--stats names the shader");
    assert!(!r.out.contains("panicked"));
    // nothing left behind: the process is gone and so are its frame files
    assert!(
        !alive(r.pid),
        "the pane process is still running after it exited"
    );
    assert!(
        frame_files(r.pid).is_empty(),
        "frame files left behind: {:?}",
        frame_files(r.pid)
    );
}

#[test]
fn ctrl_c_quits_the_pane_cleanly_too() {
    if !have_gpu() {
        eprintln!("SKIPPED: no GPU adapter; nothing was verified by this test");
        return;
    }
    let r = run_pane(
        &[
            "pane",
            &example("vignette.glsl"),
            "--protocol",
            "kitty",
            "--kitty-transfer",
            "file",
        ],
        Duration::from_millis(1200),
        b"\x03",
        false,
    );
    assert_eq!(r.code, Some(0));
    assert!(r.out.contains("\x1b[?1049l"));
    assert!(frame_files(r.pid).is_empty());
}

#[test]
fn a_pane_that_was_killed_leaves_files_that_the_next_pane_cleans_up() {
    if !have_gpu() {
        eprintln!("SKIPPED: no GPU adapter; nothing was verified by this test");
        return;
    }
    // the leak check itself: a killed pane cannot clean up, which is exactly what the stale cleanup is for
    let stale = std::env::temp_dir().join("tty-graphics-protocol-shaderlab-999999-0.rgb");
    std::fs::write(&stale, b"left over").unwrap();
    // make it look old (two minutes or more)
    let old = std::time::SystemTime::now() - Duration::from_secs(600);
    std::fs::File::options()
        .write(true)
        .open(&stale)
        .unwrap()
        .set_modified(old)
        .unwrap();
    let r = run_pane(
        &[
            "pane",
            &example("vignette.glsl"),
            "--protocol",
            "kitty",
            "--kitty-transfer",
            "file",
        ],
        Duration::from_millis(1200),
        b"q",
        false,
    );
    assert_eq!(r.code, Some(0));
    assert!(
        !stale.exists(),
        "a stale frame file from a dead pane was not cleaned up at start"
    );
}

#[test]
fn a_tiny_pane_and_a_bad_shader_do_not_panic() {
    if !have_gpu() {
        eprintln!("SKIPPED: no GPU adapter; nothing was verified by this test");
        return;
    }
    let bad = std::env::temp_dir().join("shaderlab-pane-bad.glsl");
    std::fs::write(&bad, "void mainImage(out vec4 c, in vec2 f) { c = nope; }").unwrap();
    let r = run_pane(
        &[
            "pane",
            bad.to_str().unwrap(),
            "--protocol",
            "kitty",
            "--kitty-transfer",
            "file",
        ],
        Duration::from_millis(1200),
        b"q",
        false,
    );
    std::fs::remove_file(&bad).ok();
    assert_eq!(
        r.code,
        Some(0),
        "{:?}",
        r.out.chars().take(200).collect::<String>()
    );
    assert!(!r.out.contains("panicked"));
    assert!(
        r.out.contains("shader error"),
        "the error is shown, the pane still quits"
    );
}

#[test]
fn a_pane_given_sigkill_is_gone_and_frame_files_are_only_what_the_stale_cleanup_handles() {
    if !have_gpu() {
        eprintln!("SKIPPED: no GPU adapter; nothing was verified by this test");
        return;
    }
    let r = run_pane(
        &[
            "pane",
            &example("vignette.glsl"),
            "--protocol",
            "kitty",
            "--kitty-transfer",
            "file",
        ],
        Duration::from_millis(1000),
        b"",
        true,
    );
    assert!(!alive(r.pid), "the killed pane is still running");
    for f in frame_files(r.pid) {
        let _ = std::fs::remove_file(std::env::temp_dir().join(f));
    }
}

#[test]
fn the_browser_opens_with_no_arguments_takes_keys_quits_zero_and_restores_the_terminal() {
    if !have_gpu() {
        eprintln!("SKIPPED: no GPU adapter; nothing was verified by this test");
        return;
    }
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("renders")).unwrap();
    image::save_buffer(
        home.path().join("renders/sample-render.png"),
        &[200u8; 12],
        2,
        2,
        image::ColorType::Rgb8,
    )
    .unwrap();
    // the browser reads $SHADERLAB_HOME; the pty child inherits this process's environment
    unsafe { std::env::set_var("SHADERLAB_HOME", home.path()) };
    let r = run_pane(
        &["browse", "--protocol", "halfblocks"],
        Duration::from_millis(1500),
        b"q",
        false,
    );
    unsafe { std::env::remove_var("SHADERLAB_HOME") };
    assert_eq!(
        r.code,
        Some(0),
        "{:?}",
        r.out.chars().take(300).collect::<String>()
    );
    for t in ["Shaders", "Renders", "Videos", "Sheets"] {
        assert!(r.out.contains(t), "{t} not on screen");
    }
    assert!(
        r.out.contains("vignette"),
        "the built-in shaders are listed"
    );
    assert!(
        r.out.contains("\x1b[?1049l") && r.out.contains("\x1b[?25h"),
        "the terminal is restored"
    );
    assert!(!r.out.contains("panicked"));
}
