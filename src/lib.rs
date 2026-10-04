//! shaderlab: render, preview and check Ghostty/Shadertoy-style terminal shaders headlessly.
//!
//! * [`params`]: the `@color` / `@float` / `@preset` annotation format and parameter substitution.
//! * [`frame`]: the synthetic terminal frame a shader is run over (and its text mask).
//! * [`font`]: the embedded bitmap font that draws it.
//! * With the default `render` feature: [`gpu`] (run a shader with wgpu), [`check`] (objective checks)
//!   [`sheet`] (contact sheets) and [`video`] (many frames in one GPU session, to mp4/gif/PNG frames).

pub mod font;
pub mod frame;
pub mod home;
pub mod params;

#[cfg(feature = "render")]
pub mod check;
#[cfg(feature = "render")]
pub mod gpu;
#[cfg(feature = "tui")]
pub mod imgpane;
#[cfg(feature = "tui")]
pub mod pane;
#[cfg(feature = "render")]
pub mod preview;
#[cfg(feature = "render")]
pub mod regress;
#[cfg(feature = "render")]
pub mod sheet;
#[cfg(feature = "tui")]
pub mod termimg;
#[cfg(feature = "tui")]
pub mod tui;
#[cfg(feature = "render")]
pub mod video;
