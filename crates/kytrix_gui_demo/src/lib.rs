//! Kytrix GUI 的**最小示例应用**（`design/phase-09-win32-demo.md` §7、
//! `design/phase-15-demo-showcase.md`）。
//!
//! 本文件只做模块声明（AGENTS §5.5）：场景实现在 [`scene`]，二进制入口在 `main.rs`。
//! 集成测试（`tests/scene.rs`）通过本库访问场景 API，因此业务代码不能堆在 `main.rs`。

pub mod scene;
