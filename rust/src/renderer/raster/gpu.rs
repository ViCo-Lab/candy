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

use crate::core::diag::CandyError;
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
            // InstanceDescriptor with all backends, default flags, default
            // backend options. wgpu 27's Instance::new takes &InstanceDescriptor.
            let desc = wgpu::InstanceDescriptor {
                backends: Backends::all(),
                flags: InstanceFlags::default(),
                memory_budget_thresholds: Default::default(),
                backend_options: Default::default(),
            };
            let instance = Instance::new(&desc);

            let adapter = instance
                .request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::HighPerformance,
                    compatible_surface: None,
                    force_fallback_adapter: false,
                })
                .await
                .map_err(|e| CandyError::Encode(format!("wgpu adapter: {e}")))?;

            // Request the adapter's actual limits rather than the conservative
            // `downlevel_defaults()`, which caps `max_storage_buffers_per_shader_stage`
            // at 4 — vello 0.7's compute shaders need 5, so the lower limit made
            // device creation panic. The adapter's own limits always satisfy the
            // device request and cover everything vello requires.
            let required_limits = adapter.limits().clone();
            let (device, queue) = adapter
                .request_device(&wgpu::DeviceDescriptor {
                    label: Some("candy gpu device"),
                    required_features: wgpu::Features::empty(),
                    required_limits,
                    experimental_features: wgpu::ExperimentalFeatures::disabled(),
                    memory_hints: wgpu::MemoryHints::default(),
                    trace: wgpu::Trace::Off,
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
        //
        // The SVG root carries `width`/`height` in *point* units (the scene's
        // page size) with a matching `viewBox`. vello renders the scene in that
        // native coordinate space, so feeding it a pixel-sized texture would
        // leave the content squished into the top-left corner of the canvas.
        // Rewrite the root viewport to the target pixel size (leaving the
        // `viewBox` in pt) so `usvg` applies the viewBox→viewport scale and the
        // scene fills the whole texture.
        let svg = set_svg_viewport_px(svg, width, height);
        let scene: Scene = vello_svg::render(&svg)
            .map_err(|e| CandyError::Encode(format!("vello_svg parse: {e:?}")))?;

        // 2. Create target texture (Rgba8Unorm, render target + copy source).
        let texture = self.device.create_texture(&TextureDescriptor {
            label: Some("candy frame texture"),
            size: Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: TextureDimension::D2,
            format: TextureFormat::Rgba8Unorm,
            usage: TextureUsages::RENDER_ATTACHMENT
                | TextureUsages::COPY_SRC
                | TextureUsages::STORAGE_BINDING,
            view_formats: &[],
        });
        let view = texture.create_view(&TextureViewDescriptor::default());

        // 3. Render scene → texture via vello's compute pipeline.
        let params = RenderParams {
            base_color: vello::peniko::Color::from_rgba8(255, 255, 255, 255), // opaque white
            width,
            height,
            antialiasing_method: vello::AaConfig::Area,
        };
        self.vello
            .render_to_texture(&self.device, &self.queue, &scene, &view, &params)
            .map_err(|e| CandyError::Encode(format!("vello render: {e}")))?;

        // 4. Copy texture → buffer → CPU.
        //
        // wgpu requires `bytes_per_row` for copy_texture_to_buffer to be a
        // multiple of `COPY_BYTES_PER_ROW_ALIGNMENT` (256). The tight row width
        // `width * 4` usually isn't, so we pad to the aligned stride and then
        // de-pad row-by-row below to give the encoder a tightly-packed buffer.
        let unpadded_bpr = (width as usize) * 4;
        let aligned_bpr = unpadded_bpr.next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as usize);
        let buffer = self.device.create_buffer(&BufferDescriptor {
            label: Some("candy frame readback"),
            size: (aligned_bpr as u64) * (height as u64),
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
                    bytes_per_row: Some(aligned_bpr as u32),
                    rows_per_image: Some(height),
                },
            },
            Extent3d { width, height, depth_or_array_layers: 1 },
        );
        self.queue.submit(Some(encoder.finish()));

        // 5. Map buffer and read back, de-padding each row to a tight stride.
        let slice = buffer.slice(..);
        slice.map_async(MapMode::Read, |_| {});
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| CandyError::Encode(format!("wgpu poll: {e}")))?;

        let rgba = {
            let data = slice.get_mapped_range();
            let mut out = Vec::with_capacity(unpadded_bpr * height as usize);
            for y in 0..height as usize {
                let start = y * aligned_bpr;
                let end = start + unpadded_bpr;
                out.extend_from_slice(&data[start..end]);
            }
            out
        };

        Ok(RenderedFrame {
            width: width as usize,
            height: height as usize,
            rgba,
        })
    }
}

/// Rewrite the root `<svg>` element's `width`/`height` (the viewport) to the
/// given pixel dimensions, leaving the `viewBox` (in pt) untouched.
///
/// `usvg` fits the `viewBox` into the viewport via a scale transform, so this
/// is what maps the scene's point-space geometry to the pixel-sized render
/// target. Only the first `width`/`height` attributes — those on the opening
/// `<svg ...>` tag — are touched; child elements live after the closing `>`
/// and are never affected.
fn set_svg_viewport_px(svg: &str, w: u32, h: u32) -> String {
    let open = match svg.find("<svg") {
        Some(i) => i,
        None => return svg.to_string(),
    };
    let close = match svg[open..].find('>') {
        Some(i) => open + i,
        None => return svg.to_string(),
    };
    let tag = &svg[open..=close];
    let tag = replace_attr(tag, "width", w);
    let tag = replace_attr(&tag, "height", h);
    let mut out = String::with_capacity(svg.len());
    out.push_str(&svg[..open]);
    out.push_str(&tag);
    out.push_str(&svg[close + 1..]);
    out
}

/// Replace the first `name="..."` attribute value within `s` with `value`.
fn replace_attr(s: &str, name: &str, value: u32) -> String {
    let needle = format!("{}=\"", name);
    let start = match s.find(&needle) {
        Some(i) => i,
        None => return s.to_string(),
    };
    let val_start = start + needle.len();
    let end = match s[val_start..].find('"') {
        Some(i) => val_start + i,
        None => return s.to_string(),
    };
    let mut out = String::with_capacity(s.len());
    out.push_str(&s[..val_start]);
    out.push_str(&value.to_string());
    out.push_str(&s[end..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_svg_viewport_px_rescales_root_only() {
        let svg = "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"453.543\" \
                   height=\"255.118\" viewBox=\"0 0 453.543 255.118\" \
                   xmlns:xlink=\"http://www.w3.org/1999/xlink\">\n\
                   <rect width=\"453.543\" height=\"255.118\" fill=\"white\"/>\n\
                   </svg>";
        let out = set_svg_viewport_px(svg, 1360, 765);
        // Root viewport is now pixel-sized...
        assert!(out.contains("width=\"1360\""), "root width not rewritten: {out}");
        assert!(out.contains("height=\"765\""), "root height not rewritten: {out}");
        // ...but the viewBox is preserved (pt) so usvg scales the scene.
        assert!(
            out.contains("viewBox=\"0 0 453.543 255.118\""),
            "viewBox must be preserved: {out}"
        );
        // Child element attributes are untouched.
        assert!(
            out.contains("<rect width=\"453.543\" height=\"255.118\""),
            "child attributes must be untouched: {out}"
        );
    }
}
