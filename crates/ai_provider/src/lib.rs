//! Local llama.cpp inference for Kytrix.
//!
//! The crate loads one GGUF model into a dedicated worker thread and serves requests
//! through [`llama::engine::Engine`]. The model and its context never leave that
//! thread, which is what keeps llama.cpp's non-`Send` context sound.

pub mod error;
pub mod llama;
