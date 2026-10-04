//! Where shaderlab keeps what it makes, and the user's shader library.
//!
//! `$SHADERLAB_HOME` (default `~/Pictures/shaderlab`) holds `renders/` (PNG stills), `videos/` (mp4 and gif), `sheets/` (contact
//! sheets) and `frames/` (frame folders). The user's own shaders live in `$XDG_CONFIG_HOME/shaderlab/shaders` (default
//! `~/.config/shaderlab/shaders`). Nothing is ever overwritten: a name that exists gets `-2`, `-3`, ... An explicit `--out` always wins.

use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Home {
    pub root: PathBuf,
    pub library: PathBuf,
}

impl Home {
    /// From the environment. `get` reads a variable (so tests can pass their own).
    pub fn from_vars(get: &dyn Fn(&str) -> Option<String>) -> Home {
        let nonempty = |k: &str| get(k).filter(|v| !v.is_empty());
        let home = nonempty("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let root = nonempty("SHADERLAB_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("Pictures").join("shaderlab"));
        let config = nonempty("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        Home {
            root,
            library: config.join("shaderlab").join("shaders"),
        }
    }

    pub fn from_env() -> Home {
        Home::from_vars(&|k| std::env::var(k).ok())
    }

    pub fn renders(&self) -> PathBuf {
        self.root.join("renders")
    }

    pub fn videos(&self) -> PathBuf {
        self.root.join("videos")
    }

    pub fn sheets(&self) -> PathBuf {
        self.root.join("sheets")
    }

    pub fn frames(&self) -> PathBuf {
        self.root.join("frames")
    }

    pub fn all_dirs(&self) -> [PathBuf; 5] {
        [
            self.renders(),
            self.videos(),
            self.sheets(),
            self.frames(),
            self.library.clone(),
        ]
    }

    pub fn ensure(&self) -> io::Result<()> {
        for d in self.all_dirs() {
            std::fs::create_dir_all(d)?;
        }
        Ok(())
    }

    /// The folder an output of this kind goes to by default.
    pub fn dir_for(&self, kind: OutKind) -> PathBuf {
        match kind {
            OutKind::Render => self.renders(),
            OutKind::Video => self.videos(),
            OutKind::Sheet => self.sheets(),
            OutKind::Frames => self.frames(),
        }
    }

    /// A fresh path in the right folder for `stem` + `ext` (empty `ext` for a folder). Creates the folder, never overwrites.
    pub fn out_path(&self, kind: OutKind, stem: &str, ext: &str) -> io::Result<PathBuf> {
        let dir = self.dir_for(kind);
        std::fs::create_dir_all(&dir)?;
        Ok(unique_path(&dir, stem, ext))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutKind {
    Render,
    Video,
    Sheet,
    Frames,
}

/// Keep a name filesystem-friendly: letters, digits, `-`, `_` and `.` only.
pub fn slug(s: &str) -> String {
    let out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                c
            } else {
                '-'
            }
        })
        .collect();
    let out = out.trim_matches('-').to_string();
    if out.is_empty() { "shader".into() } else { out }
}

/// `<shader>-<preset>-<w>x<h>-t<time>`
pub fn render_stem(shader: &str, preset: Option<&str>, size: (u32, u32), time: f32) -> String {
    format!(
        "{}-{}-{}x{}-t{}",
        slug(shader),
        slug(preset.unwrap_or("default")),
        size.0,
        size.1,
        trim_num(time)
    )
}

/// `<shader>-<preset>-<seconds>s`
pub fn video_stem(shader: &str, preset: Option<&str>, seconds: f32) -> String {
    format!(
        "{}-{}-{}s",
        slug(shader),
        slug(preset.unwrap_or("default")),
        trim_num(seconds)
    )
}

fn trim_num(v: f32) -> String {
    let s = format!("{v:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// `dir/stem.ext`, or `dir/stem-2.ext`, `-3`, ... when that exists. An empty `ext` names a folder.
pub fn unique_path(dir: &Path, stem: &str, ext: &str) -> PathBuf {
    let make = |n: u32| -> PathBuf {
        let name = if n == 1 {
            stem.to_string()
        } else {
            format!("{stem}-{n}")
        };
        dir.join(if ext.is_empty() {
            name
        } else {
            format!("{name}.{ext}")
        })
    };
    (1..)
        .map(make)
        .find(|p| !p.exists())
        .expect("an unused name exists")
}

/// What `import` does with a file, by its name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportKind {
    Shader,
    Render,
    Video,
    Sheet,
}

pub fn import_kind(path: &Path) -> Option<ImportKind> {
    let ext = path.extension()?.to_string_lossy().to_lowercase();
    let name = path.file_name()?.to_string_lossy().to_lowercase();
    match ext.as_str() {
        "glsl" | "frag" => Some(ImportKind::Shader),
        "png" | "jpg" | "jpeg" | "webp" if name.contains("sheet") => Some(ImportKind::Sheet),
        "png" | "jpg" | "jpeg" | "webp" => Some(ImportKind::Render),
        "mp4" | "mov" | "gif" | "webm" | "m4v" => Some(ImportKind::Video),
        _ => None,
    }
}

#[derive(Debug, Default)]
pub struct ImportReport {
    /// (source, destination)
    pub imported: Vec<(PathBuf, PathBuf)>,
    pub skipped: Vec<(PathBuf, String)>,
}

/// Copy (or with `move_files`, move) files into the right folders of `home`. A directory is imported file by file (one level).
/// The sources are only read unless `move_files` is set.
pub fn import(home: &Home, paths: &[PathBuf], move_files: bool) -> io::Result<ImportReport> {
    home.ensure()?;
    let mut report = ImportReport::default();
    let mut files: Vec<PathBuf> = Vec::new();
    for p in paths {
        if p.is_dir() {
            let mut inner: Vec<PathBuf> = std::fs::read_dir(p)?
                .flatten()
                .map(|e| e.path())
                .filter(|q| q.is_file())
                .collect();
            inner.sort();
            files.extend(inner);
        } else {
            files.push(p.clone());
        }
    }
    for f in files {
        if !f.is_file() {
            report.skipped.push((f, "not a file".into()));
            continue;
        }
        let Some(kind) = import_kind(&f) else {
            report
                .skipped
                .push((f, "not a shader, image or video".into()));
            continue;
        };
        let dir = match kind {
            ImportKind::Shader => home.library.clone(),
            ImportKind::Render => home.renders(),
            ImportKind::Video => home.videos(),
            ImportKind::Sheet => home.sheets(),
        };
        let stem = f
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".into());
        let ext = f
            .extension()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        // already in the right place: nothing to do
        if f.parent().and_then(|p| p.canonicalize().ok()) == dir.canonicalize().ok() {
            report.skipped.push((f, "already in the library".into()));
            continue;
        }
        let dest = unique_path(&dir, &stem, &ext);
        if move_files {
            if std::fs::rename(&f, &dest).is_err() {
                std::fs::copy(&f, &dest)?;
                std::fs::remove_file(&f)?;
            }
        } else {
            std::fs::copy(&f, &dest)?;
        }
        report.imported.push((f, dest));
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn the_home_follows_the_environment() {
        let h = Home::from_vars(&vars(&[("HOME", "/h")]));
        assert_eq!(h.root, PathBuf::from("/h/Pictures/shaderlab"));
        assert_eq!(h.library, PathBuf::from("/h/.config/shaderlab/shaders"));
        assert_eq!(h.renders(), PathBuf::from("/h/Pictures/shaderlab/renders"));
        let h = Home::from_vars(&vars(&[
            ("HOME", "/h"),
            ("SHADERLAB_HOME", "/data/lab"),
            ("XDG_CONFIG_HOME", "/cfg"),
        ]));
        assert_eq!(h.root, PathBuf::from("/data/lab"));
        assert_eq!(h.library, PathBuf::from("/cfg/shaderlab/shaders"));
        assert_eq!(h.videos(), PathBuf::from("/data/lab/videos"));
        assert_eq!(h.sheets(), PathBuf::from("/data/lab/sheets"));
        assert_eq!(h.frames(), PathBuf::from("/data/lab/frames"));
        // empty variables count as unset
        assert_eq!(
            Home::from_vars(&vars(&[("HOME", "/h"), ("SHADERLAB_HOME", "")])).root,
            PathBuf::from("/h/Pictures/shaderlab")
        );
    }

    #[test]
    fn names_say_what_made_them_and_never_collide() {
        assert_eq!(
            render_stem("ps3 visualizer", Some("hills"), (1280, 720), 5.0),
            "ps3-visualizer-hills-1280x720-t5"
        );
        assert_eq!(
            render_stem("a", None, (64, 64), 2.5),
            "a-default-64x64-t2.5"
        );
        assert_eq!(video_stem("rain/down", None, 10.0), "rain-down-default-10s");
        assert_eq!(slug("../../etc"), "..-..-etc".trim_matches('-'));
        assert!(!slug("../x").contains('/'));
        assert_eq!(slug("///"), "shader");
        let td = tempfile::tempdir().unwrap();
        let a = unique_path(td.path(), "x", "png");
        assert_eq!(a, td.path().join("x.png"));
        std::fs::write(&a, "1").unwrap();
        let b = unique_path(td.path(), "x", "png");
        assert_eq!(b, td.path().join("x-2.png"));
        std::fs::write(&b, "2").unwrap();
        assert_eq!(
            unique_path(td.path(), "x", "png"),
            td.path().join("x-3.png")
        );
        // folders too
        std::fs::create_dir(td.path().join("f")).unwrap();
        assert_eq!(unique_path(td.path(), "f", ""), td.path().join("f-2"));
    }

    #[test]
    fn out_path_creates_the_folder_and_does_not_overwrite() {
        let td = tempfile::tempdir().unwrap();
        let h = Home::from_vars(&vars(&[
            ("HOME", "/h"),
            ("SHADERLAB_HOME", td.path().to_str().unwrap()),
        ]));
        let p = h
            .out_path(OutKind::Render, "s-default-64x64-t1", "png")
            .unwrap();
        assert!(p.starts_with(td.path().join("renders")) && p.parent().unwrap().is_dir());
        std::fs::write(&p, "x").unwrap();
        assert_ne!(
            h.out_path(OutKind::Render, "s-default-64x64-t1", "png")
                .unwrap(),
            p
        );
        assert!(
            h.out_path(OutKind::Video, "v", "mp4")
                .unwrap()
                .starts_with(td.path().join("videos"))
        );
        assert!(
            h.out_path(OutKind::Sheet, "v", "png")
                .unwrap()
                .starts_with(td.path().join("sheets"))
        );
        assert!(
            h.out_path(OutKind::Frames, "v", "")
                .unwrap()
                .starts_with(td.path().join("frames"))
        );
    }

    #[test]
    fn import_sorts_files_by_kind_copies_by_default_and_never_overwrites() {
        let td = tempfile::tempdir().unwrap();
        let h = Home::from_vars(&vars(&[
            ("HOME", "/h"),
            ("SHADERLAB_HOME", td.path().join("lab").to_str().unwrap()),
            ("XDG_CONFIG_HOME", td.path().join("cfg").to_str().unwrap()),
        ]));
        let src = td.path().join("old");
        std::fs::create_dir(&src).unwrap();
        for n in [
            "wall.png",
            "clip.mp4",
            "loop.gif",
            "my.glsl",
            "contact-sheet.png",
            "notes.txt",
            "photo.JPG",
        ] {
            std::fs::write(src.join(n), n).unwrap();
        }
        let r = import(&h, std::slice::from_ref(&src), false).unwrap();
        assert_eq!(r.imported.len(), 6, "{r:?}");
        assert_eq!(r.skipped.len(), 1);
        assert!(r.skipped[0].1.contains("not a shader"));
        assert!(h.renders().join("wall.png").is_file() && h.renders().join("photo.JPG").is_file());
        assert!(h.videos().join("clip.mp4").is_file() && h.videos().join("loop.gif").is_file());
        assert!(h.sheets().join("contact-sheet.png").is_file());
        assert!(h.library.join("my.glsl").is_file());
        // the originals are untouched
        assert!(src.join("wall.png").is_file() && src.join("clip.mp4").is_file());
        // a second import does not overwrite
        let r2 = import(&h, &[src.join("wall.png")], false).unwrap();
        assert_eq!(r2.imported[0].1, h.renders().join("wall-2.png"));
        assert_eq!(
            std::fs::read_to_string(h.renders().join("wall.png")).unwrap(),
            "wall.png"
        );
        // --move removes the original, and a file already in place is left alone
        let r3 = import(&h, &[src.join("clip.mp4")], true).unwrap();
        assert!(!src.join("clip.mp4").exists() && r3.imported[0].1.is_file());
        let r4 = import(&h, &[h.renders().join("wall.png")], false).unwrap();
        assert!(r4.imported.is_empty() && r4.skipped[0].1.contains("already"));
        assert!(
            import(&h, &[td.path().join("missing.png")], false)
                .unwrap()
                .skipped[0]
                .1
                .contains("not a file")
        );
    }
}
