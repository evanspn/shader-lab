//! shaderlab: render, preview and check Ghostty/Shadertoy-style terminal shaders headlessly.
//!
//! * [`params`]: the `@color` / `@float` / `@preset` annotation format and parameter substitution.
//! * [`frame`]: the synthetic terminal frame a shader is run over (and its text mask).
//! * [`font`]: the embedded bitmap font that draws it.
//! * With the default `render` feature: [`gpu`] (run a shader with wgpu), [`check`] (objective checks)
//!   and [`sheet`] (contact sheets).

pub mod font;
pub mod frame;
pub mod params;

#[cfg(feature = "render")]
pub mod check;
#[cfg(feature = "render")]
pub mod gpu;
#[cfg(feature = "render")]
pub mod sheet;
