//! Global `#camera` pan/zoom/rotate: the SVG `<g transform>` string, the
//! pixel-space warp matrix, and the bilinear-sampled canvas warp.

use crate::core::ast::FrameData;

use super::PT_PER_CM;
use super::matrix::{Matrix, compose};

/// Synthetic label used by `#camera` (a global pan/zoom/rotate transform).
/// Never rendered as an object — the renderer reads its per-frame state and
/// applies it as a wrapping transform over the whole scene.
pub(crate) const CAMERA_LABEL: &str = "__camera__";

/// SVG `<g transform>` attribute for the camera (pan + zoom + rotate about the
/// page center), in Typst points.
pub(crate) fn camera_transform_svg(cam: &FrameData, page_w: f64, page_h: f64) -> String {
    let (cx, cy) = (page_w / 2.0, page_h / 2.0);
    let ncx = -cx;
    let ncy = -cy;
    let dx = cam.x * PT_PER_CM;
    let dy = cam.y * PT_PER_CM;
    let s = cam.scale;
    let r = cam.rotation;
    format!(
        "translate({cx} {cy}) rotate({r}) scale({s}) translate({ncx} {ncy}) translate({dx} {dy})"
    )
}

/// Forward camera matrix (scene → screen) in *pixel* space, for the pixel-path
/// warp. `ppi` is `pixel_per_pt`.
pub(crate) fn camera_matrix_px(cam: &FrameData, page_w_pt: f64, page_h_pt: f64, ppi: f32) -> Matrix {
    let (cx, cy) = (page_w_pt * ppi as f64 / 2.0, page_h_pt * ppi as f64 / 2.0);
    let dx = cam.x * PT_PER_CM * ppi as f64;
    let dy = cam.y * PT_PER_CM * ppi as f64;
    let s = cam.scale;
    let r = cam.rotation;
    compose(
        compose(
            compose(
                compose(Matrix::translation(cx, cy), Matrix::rotation(r)),
                Matrix::scaling(s),
            ),
            Matrix::translation(-cx, -cy),
        ),
        Matrix::translation(dx, dy),
    )
}

/// Bilinear-sample a RGBA canvas at `(x, y)` (in pixels). Out-of-bounds samples
/// return the scene background colour `bg` — the same fill native Typst paints
/// on the page, so a camera pan/zoom/rotate that exposes area outside the
/// original canvas reveals the configured background (e.g. a dark night sky),
/// not a hardcoded white edge.
pub(crate) fn sample_bilinear(
    src: &[u8],
    w: usize,
    h: usize,
    x: f64,
    y: f64,
    bg: [u8; 4],
) -> (u8, u8, u8, u8) {
    if x < 0.0 || y < 0.0 || x > w as f64 - 1.0 || y > h as f64 - 1.0 {
        return (bg[0], bg[1], bg[2], bg[3]);
    }
    let x0 = x.floor() as usize;
    let y0 = y.floor() as usize;
    let x1 = (x0 + 1).min(w - 1);
    let y1 = (y0 + 1).min(h - 1);
    let fx = x - x0 as f64;
    let fy = y - y0 as f64;
    let idx = |x: usize, y: usize| -> [u8; 4] {
        let i = (y * w + x) * 4;
        [src[i], src[i + 1], src[i + 2], src[i + 3]]
    };
    let p00 = idx(x0, y0);
    let p10 = idx(x1, y0);
    let p01 = idx(x0, y1);
    let p11 = idx(x1, y1);
    let lerp = |a: u8, b: u8, t: f64| (a as f64 + (b as f64 - a as f64) * t).round() as u8;
    let top = [
        lerp(p00[0], p10[0], fx),
        lerp(p00[1], p10[1], fx),
        lerp(p00[2], p10[2], fx),
        lerp(p00[3], p10[3], fx),
    ];
    let bot = [
        lerp(p01[0], p11[0], fx),
        lerp(p01[1], p11[1], fx),
        lerp(p01[2], p11[2], fx),
        lerp(p01[3], p11[3], fx),
    ];
    (
        lerp(top[0], bot[0], fy),
        lerp(top[1], bot[1], fy),
        lerp(top[2], bot[2], fy),
        lerp(top[3], bot[3], fy),
    )
}

/// Warp a fully-composited canvas through the inverse camera transform,
/// sampling the source with bilinear filtering. `bg` is the scene background
/// colour used for samples that fall outside the original canvas (so exposed
/// margins match native Typst's page fill instead of hardcoded white).
#[allow(clippy::too_many_arguments)]
pub(crate) fn warp_canvas_with_camera(
    canvas: &mut [u8],
    w: usize,
    h: usize,
    cam: &FrameData,
    page_w_pt: f64,
    page_h_pt: f64,
    ppi: f32,
    bg: [u8; 4],
) {
    let m = camera_matrix_px(cam, page_w_pt, page_h_pt, ppi);
    let inv = m.inverse();
    let src = canvas.to_vec();
    for y in 0..h {
        for x in 0..w {
            let (sx, sy) = inv.apply(x as f64, y as f64);
            let (r, g, b, a) = sample_bilinear(&src, w, h, sx, sy, bg);
            let di = (y * w + x) * 4;
            canvas[di] = r;
            canvas[di + 1] = g;
            canvas[di + 2] = b;
            canvas[di + 3] = a;
        }
    }
}
