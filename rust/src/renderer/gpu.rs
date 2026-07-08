//! GPU-accelerated frame rasterization via vello + wgpu.
//!
//! When the `gpu` cargo feature is enabled, candy can rasterize SVG frames on
//! the GPU instead of via `typst-render` (CPU). This is opt-in because it
//! pulls in heavy native GPU dependencies (wgpu, vello, vello_svg); users
//! without a GPU or who want a smaller build can stay on the default CPU path.
//!
//! # Pipeline
//!
//! 1. The Typst renderer produces an SVG string for the frame (same as the
//!    CPU path).
//! 2. `vello_svg::render` parses the SVG into a `vello::Scene` (vector scene
//!    graph).
//! 3. A wgpu offscreen device + texture is created (lazily, reused across
//!    frames by [`GpuRenderer::new`]).
//! 4. `vello::Renderer::render_to_texture` rasterizes the scene to the texture
//!    using GPU compute shaders.
//! 5. The texture is copied back to CPU memory as RGBA8 and returned in the
//!    same `RenderedFrame` shape the CPU path produces, so the downstream
//!    video encoder needs no changes.
//!
//! # Fallback
//!
//! If no GPU adapter is available (headless container, no drivers), the
//! functions in this module return `Err`. The CLI `--gpu` flag catches this
//! and falls back to `typst-render` automatically, so `--gpu` is always safe
//! to pass — it just becomes a no-op when no GPU is present.

use vello::{RenderParams, Renderer as VelloRenderer, Scene};
use wgpu::{
    Backends, BufferDescriptor, BufferUsages, CommandEncoderDescriptor, Device, Extent3d,
    Instance, InstanceFlags, MapMode, Queue, TextureAspect, TextureDescriptor, TextureDimension,
    TextureFormat, TextureUsages, TextureViewDescriptor,
};

use crate::core::error::CandyError;
use crate::renderer::RenderedFrame;

/// A reusable GPU rasterization context: wgpu device + queue + vello renderer.
///
/// Building a wgpu device is expensive (driver handshake, shader compilation),
/// so this struct is meant to be constructed once and reused across every
/// frame in an animation. `GpuRenderer::render_svg` is the per-frame entry
/// point.
pub struct GpuRenderer {
    device: Device,
    queue: Queue,
    vello: VelloRenderer,
}

impl GpuRenderer {
    /// Create a new GPU renderer. Requests a high-performance adapter with no
    /// surface (offscreen rendering only). Returns `Err` if no GPU is
    /// available — callers should fall back to the CPU path in that case.
    pub fn new() -> Result<Self, CandyError> {
        // Use pollster to block on wgpu's async device-creation futures.
        pollster::block_on(async {
            let instance = Instance::new(wgpu::InstanceDescriptor {
                backends: Backends::all(),
                flags: InstanceFlags::default(),
                memory_budget_thresholds: Default::default(),
            });

            let adapter = instance
                .request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::HighPerformance,
                    compatible_surface: None,
                    force_fallback_adapter: false,
                })
                .await
                .map_err(|e| CandyError::Encode(format!("wgpu adapter: {e}")))?;

            let (device, queue) = adapter
                .request_device(&wgpu::DeviceDescriptor {
                    label: Some("candy gpu device"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::downlevel_defaults(),
                    experimental_features: wgpu::ExperimentalFeatures::empty(),
                    memory_hints: wgpu::MemoryHints::default(),
                })
                .await
                .map_err(|e| CandyError::Encode(format!("wgpu device: {e}")))?;

            let vello = VelloRenderer::new(&device, vello::RendererOptions::default())
                .map_err(|e| CandyError::Encode(format!("vello renderer: {e}")))?;

            Ok(GpuRenderer { device, queue, vello })
        })
    }

    /// Rasterize an SVG string to an RGBA8 buffer at `width x height`.
    ///
    /// The SVG is parsed by `vello_svg`, rendered to a GPU texture by vello,
    /// then copied back to CPU memory. The returned `RenderedFrame` has the
    /// same shape as the CPU path's output, so the video encoder consumes it
    /// unchanged.
    pub fn render_svg(&mut self, svg: &str, width: u32, height: u32) -> Result<RenderedFrame, CandyError> {
        // 1. Parse SVG → vello Scene.
        let scene: Scene = vello_svg::render(svg)
            .map_err(|e| CandyError::Encode(format!("vello_svg parse: {e:?}")))?;

        // 2. Create target texture (Rgba8Unorm, render target + copy source).
        let texture = self.device.create_texture(&TextureDescriptor {
            label: Some("candy frame texture"),
            size: Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: TextureDimension::D2,
            format: TextureFormat::Rgba8Unorm,
            usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&TextureViewDescriptor::default());

        // 3. Render scene → texture via vello's compute pipeline.
        let params = RenderParams {
            base_color: peniko::Color::from_rgba8(255, 255, 255, 255), // opaque white
            width,
            height,
            antialiasing_method: vello::AaConfig::Area,
        };
        self.vello
            .render_to_texture(&self.device, &self.queue, &scene, &view, &params)
            .map_err(|e| CandyError::Encode(format!("vello render: {e}")))?;

        // 4. Copy texture → buffer → CPU.
        let bytes_per_row = width * 4;
        let buffer = self.device.create_buffer(&BufferDescriptor {
            label: Some("candy frame readback"),
            size: (bytes_per_row as u64) * (height as u64),
            usage: BufferUsages::COPY_DST | BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder = self
            .device
            .create_command_encoder(&CommandEncoderDescriptor {
                label: Some("candy frame copy"),
            });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: Some(height),
                },
            },
            Extent3d { width, height, depth_or_array_layers: 1 },
        );
        self.queue.submit(Some(encoder.finish()));

        // 5. Map buffer and read back.
        let slice = buffer.slice(..);
        slice.map_async(MapMode::Read, |_| {});
        self.device
            .poll(wgpu::PollType::Wait)
            .map_err(|e| CandyError::Encode(format!("wgpu poll: {e}")))?;

        let rgba = {
            let data = slice.get_mapped_range();
            data.to_vec()
        };

        Ok(RenderedFrame {
            width: width as usize,
            height: height as usize,
            rgba,
        })
    }
}
