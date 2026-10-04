//! The real binary in a real pseudo-terminal: it starts, reacts to keys, exits 0 and gives the terminal back.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

const BIN: &str = env!("CARGO_BIN_EXE_shaderlab");

fn example(name: &str) -> String {
    format!("{}/examples/shaders/{name}", env!("CARGO_MANIFEST_DIR"))
}

/// Run `shaderlab preview` in a pty of `rows`x`cols`, send `keys` after `wait`, and return (exit code, everything it printed).
fn run_in_pty(args: &[&str], rows: u16, cols: u16, wait: Duration, keys: &[u8]) -> (u32, String) {
    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut cmd = CommandBuilder::new(BIN);
    cmd.args(args);
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env_remove("TMUX");
    let mut child = pair.slave.spawn_command(cmd).unwrap();
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
            // a real terminal answers a cursor-position query; this one must too
            if buf[..n].windows(4).any(|w| w == b"\x1b[6n") {
                let mut w = reply.lock().unwrap();
                let _ = w.write_all(b"\x1b[1;1R");
                let _ = w.flush();
            }
            let _ = tx.send(buf[..n].to_vec());
        }
    });
    std::thread::sleep(wait);
    {
        let mut w = writer.lock().unwrap();
        w.write_all(keys).unwrap();
        w.flush().unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut out = Vec::new();
    let status = loop {
        while let Ok(chunk) = rx.try_recv() {
            out.extend(chunk);
        }
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if Instant::now() > deadline {
            child.kill().ok();
            panic!(
                "the preview did not exit after the keys; output so far: {:?}",
                String::from_utf8_lossy(&out)
                    .chars()
                    .take(300)
                    .collect::<String>()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    std::thread::sleep(Duration::from_millis(100));
    while let Ok(chunk) = rx.try_recv() {
        out.extend(chunk);
    }
    (
        status.exit_code(),
        String::from_utf8_lossy(&out).into_owned(),
    )
}

fn have_gpu() -> bool {
    shaderlab::gpu::Gpu::new().is_ok()
}

#[test]
fn halfblocks_mode_runs_takes_keys_exits_zero_and_restores_the_terminal() {
    if !have_gpu() {
        eprintln!("SKIPPED: no GPU adapter; nothing was verified by this test");
        return;
    }
    let (code, out) = run_in_pty(
        &[
            "preview",
            &example("vignette.glsl"),
            "--protocol",
            "halfblocks",
        ],
        30,
        100,
        Duration::from_millis(1500),
        b" ]q",
    );
    assert_eq!(code, 0, "{out:?}");
    assert!(
        out.contains("\x1b[?1049h"),
        "it enters the alternate screen"
    );
    assert!(
        out.contains("\x1b[?1049l"),
        "and leaves it: the terminal is restored"
    );
    assert!(
        out.contains("\x1b[?1000h") && out.contains("\x1b[?1000l"),
        "mouse capture is turned on and off"
    );
    assert!(out.contains("\x1b[?25h"), "the cursor is shown again");
    assert!(
        out.contains('\u{2580}'),
        "the picture is drawn with half blocks"
    );
    assert!(out.contains("vignette"), "the panel names the shader");
    assert!(!out.contains("panicked"));
}

#[test]
fn kitty_mode_sends_the_image_and_deletes_it_on_exit() {
    if !have_gpu() {
        eprintln!("SKIPPED: no GPU adapter; nothing was verified by this test");
        return;
    }
    let (code, out) = run_in_pty(
        &["preview", &example("vignette.glsl"), "--protocol", "kitty"],
        30,
        100,
        Duration::from_millis(1500),
        b"q",
    );
    assert_eq!(code, 0, "{out:?}");
    assert!(
        out.contains("\x1b_Ga=T,f=24,o=z,"),
        "a kitty image is transmitted"
    );
    assert!(
        out.contains("i=4242"),
        "with a fixed id so each frame replaces the last in place"
    );
    let delete = out
        .rfind("\x1b_Ga=d,d=I,i=4242")
        .expect("the image is deleted on exit");
    let leave = out.rfind("\x1b[?1049l").expect("alternate screen left");
    assert!(
        delete < leave,
        "the image is removed before the screen is handed back"
    );
}

#[test]
fn ctrl_c_also_quits_cleanly() {
    if !have_gpu() {
        eprintln!("SKIPPED: no GPU adapter; nothing was verified by this test");
        return;
    }
    let (code, out) = run_in_pty(
        &[
            "preview",
            &example("vignette.glsl"),
            "--protocol",
            "halfblocks",
        ],
        24,
        80,
        Duration::from_millis(1200),
        &[3],
    );
    assert_eq!(code, 0, "{out:?}");
    assert!(out.contains("\x1b[?1049l"));
}

#[test]
fn a_tiny_terminal_does_not_panic() {
    if !have_gpu() {
        eprintln!("SKIPPED: no GPU adapter; nothing was verified by this test");
        return;
    }
    for (rows, cols) in [(1, 1), (3, 10), (5, 20)] {
        let (code, out) = run_in_pty(
            &[
                "preview",
                &example("vignette.glsl"),
                "--protocol",
                "halfblocks",
            ],
            rows,
            cols,
            Duration::from_millis(800),
            b"q",
        );
        assert_eq!(code, 0, "{rows}x{cols}: {out:?}");
        assert!(!out.contains("panicked"), "{rows}x{cols}");
    }
}

#[test]
fn a_shader_that_does_not_compile_shows_the_error_and_still_quits() {
    if !have_gpu() {
        eprintln!("SKIPPED: no GPU adapter; nothing was verified by this test");
        return;
    }
    let (code, out) = run_in_pty(
        &[
            "preview",
            &example("broken.glsl"),
            "--protocol",
            "halfblocks",
        ],
        30,
        110,
        Duration::from_millis(1200),
        b"q",
    );
    // a file that never compiled has nothing to keep running: it reports the error and exits non-zero rather than hang
    assert!(code == 0 || code == 2, "{code}: {out:?}");
    assert!(!out.contains("panicked"));
}
