//! Kytrix 自研跨平台 GUI 引擎。
//!
//! 本文件只做模块声明（AGENTS §5.5）；公共 API 通过模块路径暴露，不做 re-export
//! （workspace lint `pub_use = "forbid"`，见 `design/phase-01-foundation.md` §4.3）。

pub mod error;
pub mod math;
