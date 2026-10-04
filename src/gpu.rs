//! Run a Ghostty/Shadertoy-style fragment shader on the GPU (wgpu: Metal, Vulkan, DX12) over a frame.
//!
//! The shader defines `void mainImage(out vec4 fragColor, in vec2 fragCoord)` and may use
//! `iResolution`, `iTime`, `iTimeDelta`, `iFrame`, `iMouse`, `iDate` and `iChannel0` (the terminal frame).
//!
//! **Orientation matters, and the two worlds disagree** ([`Origin`]):
//!
//! * **Ghostty**: `fragCoord` has its origin at the TOP-left and y grows DOWNWARD; `iChannel0` is sampled with
//!   `fragCoord / iResolution.xy` and no flip. (Verified from the shader prefix Ghostty embeds in its binary.)
//! * **Shadertoy**: the origin is the BOTTOM-left and y grows UPWARD.
//!
//! Images, including everything this crate writes and reads back, have row 0 at the TOP. Output is rendered to a
//! 32-bit float target, so NaN, infinity and out-of-range values are visible to the checks before any clamping.

use std::time::Instant;

use wgpu::naga;

use crate::frame::Frame;

/// Which coordinate convention a shader was written for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Origin {
    /// Ghostty: origin top-left, y down (the default for files written for Ghostty).
    #[default]
    TopLeft,
    /// Shadertoy: origin bottom-left, y up.
    BottomLeft,
}

impl Origin {
    pub fn name(self) -> &'static str {
        match self {
            Origin::TopLeft => "top-left",
            Origin::BottomLeft => "bottom-left",
        }
    }
}

const PREFIX: &str = r#"#version 450 core
layout(set = 0, binding = 0) uniform Globals {
    vec3 iResolution;
    float iTime;
    float iTimeDelta;
    int iFrame;
    vec4 iMouse;
    vec4 iDate;
};
layout(set = 0, binding = 1) uniform texture2D iChannel0_tex;
layout(set = 0, binding = 2) uniform sampler iChannel0_smp;
#define iChannel0 sampler2D(iChannel0_tex, iChannel0_smp)
layout(location = 0) out vec4 _fragColor;
"#;

// WebGPU's framebuffer origin is top-left, which is Ghostty's convention as is. For a Shadertoy-style (bottom-left) shader
// the y of `fragCoord` is flipped, and `iChannel0` is uploaded bottom row first so that `fragCoord / iResolution.xy` still
// reads the matching pixel.
const SUFFIX_TOP_LEFT: &str = "\nvoid main() { mainImage(_fragColor, gl_FragCoord.xy); }\n";
const SUFFIX_BOTTOM_LEFT: &str = "\nvoid main() { mainImage(_fragColor, vec2(gl_FragCoord.x, iResolution.y - gl_FragCoord.y)); }\n";

const VERTEX: &str = r#"
@vertex
fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    var p = array<vec2<f32>, 3>(vec2(-1.0, -1.0), vec2(3.0, -1.0), vec2(-1.0, 3.0));
    return vec4<f32>(p[i], 0.0, 1.0);
}
"#;

/// The shader wrapped the way it is compiled: the uniforms and `main` around the user's code.
pub fn wrap(src: &str, origin: Origin) -> String {
    let suffix = if origin == Origin::TopLeft {
        SUFFIX_TOP_LEFT
    } else {
        SUFFIX_BOTTOM_LEFT
    };
    format!("{PREFIX}{src}{suffix}")
}

/// How many lines the wrapper adds before the user's code (to read compiler messages).
pub fn prefix_lines() -> usize {
    PREFIX.lines().count()
}

/// Compile and validate the shader on the CPU (no GPU needed). Errors name a line of the shader.
pub fn compile_check(src: &str) -> Result<(), String> {
    let full = wrap(src, Origin::TopLeft);
    let offset = prefix_lines();
    let module = naga::front::glsl::Frontend::default()
        .parse(
            &naga::front::glsl::Options::from(naga::ShaderStage::Fragment),
            &full,
        )
        .map_err(|e| shift_lines(&e.emit_to_string(&full), offset))?;
    naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(&module)
    .map_err(|e| format!("invalid shader: {}", e.as_inner()))?;
    if !src.contains("mainImage") {
        return Err(
            "the shader must define `void mainImage(out vec4 fragColor, in vec2 fragCoord)`".into(),
        );
    }
    Ok(())
}

/// Messages quote lines of the WRAPPED source (the uniforms are added before the user's code); report them as lines of
/// the user's own file instead: both the `┌─ glsl:LINE:COL` locations and the numbered gutter lines.
fn shift_lines(msg: &str, offset: usize) -> String {
    let mut out = String::new();
    for line in msg.lines() {
        let t = line.trim_start();
        let indent = &line[..line.len() - t.len()];
        // "┌─ glsl:19:15"
        if let Some(pos) = t.find("glsl:") {
            let rest = &t[pos + 5..];
            let mut it = rest.splitn(2, ':');
            if let (Some(l), Some(tail)) = (it.next(), it.next())
                && let Ok(n) = l.parse::<usize>()
            {
                out.push_str(&format!(
                    "{indent}{}line {}:{tail}\n",
                    &t[..pos],
                    n.saturating_sub(offset)
                ));
                continue;
            }
        }
        // "19 │     code" (the gutter number)
        let digits: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty()
            && let Some(rest) = t[digits.len()..].strip_prefix(" │")
            && let Ok(n) = digits.parse::<usize>()
        {
            let new_n = n.saturating_sub(offset).to_string();
            let pad = " ".repeat(digits.len().saturating_sub(new_n.len()));
            out.push_str(&format!("{indent}{pad}{new_n} │{rest}\n"));
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim_end().to_string()
}

#[derive(Debug)]
pub enum GpuError {
    /// No usable graphics adapter (headless machine, no driver).
    NoAdapter(String),
    Device(String),
}

impl std::fmt::Display for GpuError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GpuError::NoAdapter(m) => write!(
                f,
                "no GPU adapter available ({m}); shaders can still be compile-checked with --cpu-only"
            ),
            GpuError::Device(m) => write!(f, "could not open the GPU: {m}"),
        }
    }
}

impl std::error::Error for GpuError {}

pub struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pub adapter_name: String,
}

/// A GPU that can also present to a window surface (the live preview).
pub struct WindowGpu {
    pub gpu: Gpu,
    pub adapter: wgpu::Adapter,
    pub instance: wgpu::Instance,
}

impl Gpu {
    pub fn new() -> Result<Gpu, GpuError> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter =
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
                .map_err(|e| GpuError::NoAdapter(e.to_string()))?;
        let adapter_name = adapter.get_info().name;
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
                .map_err(|e| GpuError::Device(e.to_string()))?;
        Ok(Gpu {
            device,
            queue,
            adapter_name,
        })
    }

    /// Like [`Gpu::new`], but with an adapter that can draw to `target` (a window). The surface is created by the caller
    /// through the returned instance.
    pub fn for_window<'w>(
        target: impl Into<wgpu::SurfaceTarget<'w>>,
    ) -> Result<(WindowGpu, wgpu::Surface<'w>), GpuError> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let surface = instance
            .create_surface(target)
            .map_err(|e| GpuError::NoAdapter(format!("could not create a window surface: {e}")))?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            compatible_surface: Some(&surface),
            ..Default::default()
        }))
        .map_err(|e| GpuError::NoAdapter(e.to_string()))?;
        let adapter_name = adapter.get_info().name;
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
                .map_err(|e| GpuError::Device(e.to_string()))?;
        Ok((
            WindowGpu {
                gpu: Gpu {
                    device,
                    queue,
                    adapter_name,
                },
                adapter,
                instance,
            },
            surface,
        ))
    }

    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    /// Compile `src` and set everything up to draw it over `frame`. A shader error is returned as text,
    /// never a panic.
    pub fn prepare(&self, src: &str, frame: &Frame, origin: Origin) -> Result<Prepared, String> {
        compile_check(src)?;
        let d = &self.device;
        let (w, h) = (frame.width, frame.height);
        let guard = d.push_error_scope(wgpu::ErrorFilter::Validation);

        let frag = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("fragment"),
            source: wgpu::ShaderSource::Glsl {
                shader: wrap(src, origin).into(),
                stage: naga::ShaderStage::Fragment,
                defines: &[],
            },
        });
        let vert = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("vertex"),
            source: wgpu::ShaderSource::Wgsl(VERTEX.into()),
        });

        // the terminal texture: top row first for Ghostty; bottom row first for a Shadertoy-style shader
        let mut flipped = Vec::with_capacity(frame.rgba.len());
        if origin == Origin::TopLeft {
            flipped.extend_from_slice(&frame.rgba);
        } else {
            for row in (0..h).rev() {
                flipped.extend_from_slice(
                    &frame.rgba[(row * w * 4) as usize..((row + 1) * w * 4) as usize],
                );
            }
        }
        let tex = d.create_texture(&wgpu::TextureDescriptor {
            label: Some("iChannel0"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        self.queue.write_texture(
            tex.as_image_copy(),
            &flipped,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w * 4),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = d.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        let globals = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Globals"),
            size: 64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let entry = |binding, ty| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty,
            count: None,
        };
        let bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[
                entry(
                    0,
                    wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                ),
                entry(
                    1,
                    wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                ),
                entry(
                    2,
                    wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                ),
            ],
        });
        let bind = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: globals.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });
        let layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let pipeline = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: None,
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &vert,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &frag,
                entry_point: Some("main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba32Float,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let target = d.create_texture(&wgpu::TextureDescriptor {
            label: Some("output"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba32Float,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let padded = (w * 16).next_multiple_of(256);
        let readback = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: (padded * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        if let Some(e) = pollster::block_on(guard.pop()) {
            return Err(format!("the GPU rejected the shader: {e}"));
        }
        Ok(Prepared {
            width: w,
            height: h,
            padded,
            globals,
            bind,
            pipeline,
            target,
            readback,
        })
    }
}

pub struct Prepared {
    width: u32,
    height: u32,
    padded: u32,
    globals: wgpu::Buffer,
    bind: wgpu::BindGroup,
    pipeline: wgpu::RenderPipeline,
    target: wgpu::Texture,
    readback: wgpu::Buffer,
}

impl Prepared {
    fn set_time(&self, gpu: &Gpu, time: f32, delta: f32, frame_no: i32) {
        let mut g = [0u8; 64];
        let put = |b: &mut [u8; 64], off: usize, v: f32| {
            b[off..off + 4].copy_from_slice(&v.to_le_bytes())
        };
        put(&mut g, 0, self.width as f32);
        put(&mut g, 4, self.height as f32);
        put(&mut g, 8, 1.0);
        put(&mut g, 12, time);
        put(&mut g, 16, delta);
        g[20..24].copy_from_slice(&frame_no.to_le_bytes());
        gpu.queue.write_buffer(&self.globals, 0, &g);
    }

    fn encode(&self, gpu: &Gpu, copy: bool) -> wgpu::CommandBuffer {
        let mut enc = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        let view = self
            .target
            .create_view(&wgpu::TextureViewDescriptor::default());
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, Some(&self.bind), &[]);
            pass.draw(0..3, 0..1);
        }
        if copy {
            enc.copy_texture_to_buffer(
                self.target.as_image_copy(),
                wgpu::TexelCopyBufferInfo {
                    buffer: &self.readback,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(self.padded),
                        rows_per_image: Some(self.height),
                    },
                },
                wgpu::Extent3d {
                    width: self.width,
                    height: self.height,
                    depth_or_array_layers: 1,
                },
            );
        }
        enc.finish()
    }

    /// Draw one frame at `time` seconds; RGBA f32 (not clamped), top row first.
    pub fn draw(&self, gpu: &Gpu, time: f32) -> Result<Vec<f32>, String> {
        self.draw_at(gpu, time, 1.0 / 60.0, (time * 60.0) as i32)
    }

    /// Like [`draw`](Self::draw) with the time uniforms chosen by the caller: `iTime`, `iTimeDelta` and `iFrame`.
    pub fn draw_at(
        &self,
        gpu: &Gpu,
        time: f32,
        delta: f32,
        frame_no: i32,
    ) -> Result<Vec<f32>, String> {
        let mut out = Vec::new();
        self.draw_into(gpu, time, delta, frame_no, &mut out)?;
        Ok(out)
    }

    /// Draw a frame and write it as RGBA8 into `out` (reused between calls: video rendering allocates once).
    pub fn draw_rgba8(
        &self,
        gpu: &Gpu,
        time: f32,
        delta: f32,
        frame_no: i32,
        out: &mut Vec<u8>,
    ) -> Result<(), String> {
        let mut px = Vec::new();
        self.draw_into(gpu, time, delta, frame_no, &mut px)?;
        out.clear();
        out.extend(px.iter().map(|v| {
            if v.is_nan() {
                0
            } else {
                (v.clamp(0.0, 1.0) * 255.0).round() as u8
            }
        }));
        Ok(())
    }

    fn draw_into(
        &self,
        gpu: &Gpu,
        time: f32,
        delta: f32,
        frame_no: i32,
        out: &mut Vec<f32>,
    ) -> Result<(), String> {
        self.set_time(gpu, time, delta, frame_no);
        gpu.queue.submit(Some(self.encode(gpu, true)));
        let slice = self.readback.slice(..);
        slice.map_async(wgpu::MapMode::Read, |r| {
            let _ = r;
        });
        gpu.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| format!("GPU poll failed: {e}"))?;
        let data = slice
            .get_mapped_range()
            .map_err(|e| format!("could not read the frame back: {e}"))?;
        out.clear();
        out.reserve((self.width * self.height * 4) as usize);
        for row in 0..self.height {
            let start = (row * self.padded) as usize;
            let bytes = &data[start..start + (self.width * 16) as usize];
            out.extend(
                bytes
                    .chunks_exact(4)
                    .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])),
            );
        }
        drop(data);
        self.readback.unmap();
        Ok(())
    }

    /// Draw a frame into the internal Rgba32Float target without reading it back (the live preview presents it).
    pub fn render_only(&self, gpu: &Gpu, time: f32, delta: f32, frame_no: i32) {
        self.set_time(gpu, time, delta, frame_no);
        gpu.queue.submit(Some(self.encode(gpu, false)));
    }

    pub fn target_view(&self) -> wgpu::TextureView {
        self.target
            .create_view(&wgpu::TextureViewDescriptor::default())
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Average milliseconds per frame over `n` frames (each submitted and waited for; no readback).
    pub fn bench(&self, gpu: &Gpu, n: u32) -> Result<f64, String> {
        let _ = self.draw(gpu, 0.0)?; // warm up
        let start = Instant::now();
        for i in 0..n {
            self.set_time(gpu, i as f32 * 0.0167, 0.0167, i as i32);
            gpu.queue.submit(Some(self.encode(gpu, false)));
            gpu.device
                .poll(wgpu::PollType::wait_indefinitely())
                .map_err(|e| format!("GPU poll failed: {e}"))?;
        }
        Ok(start.elapsed().as_secs_f64() * 1000.0 / n.max(1) as f64)
    }
}

/// The real parameter header (exactly what applying a profile writes), then the GPU pipeline for `src` over `frame`.
pub fn prepare_shader(
    gpu: &Gpu,
    src: &str,
    preset: Option<&str>,
    sets: &[String],
    frame: &Frame,
    origin: Origin,
) -> Result<Prepared, String> {
    use crate::params::{self, RenderContext};
    let schema = params::parse_schema(params::strip_header(src))?;
    let values = params::values_from_args(&schema, preset, sets)?;
    let ctx = RenderContext {
        background: frame.background,
        ..RenderContext::default()
    };
    let text = params::render_ctx(src, &values, &ctx)?;
    gpu.prepare(&text, frame, origin)
}

/// f32 RGBA -> RGBA8 (clamped, rounded). NaN becomes 0.
pub fn to_rgba8(px: &[f32]) -> Vec<u8> {
    px.iter()
        .map(|v| {
            if v.is_nan() {
                0
            } else {
                (v.clamp(0.0, 1.0) * 255.0).round() as u8
            }
        })
        .collect()
}
