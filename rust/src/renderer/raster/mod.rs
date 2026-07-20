//! Frame rasterization: CPU SVG→pixels ([`cpu`]) and GPU (feature-gated
//! [`gpu`]).

pub mod cpu;

#[cfg(feature = "gpu")]
pub mod gpu;
