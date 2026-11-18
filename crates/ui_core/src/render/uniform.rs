//! `RootUniform`（112 B）的**唯一** CPU 侧构造点，三平台共用。
//!
//! 契约见 `render/shaders/contract.md` §6。此前 Windows / macOS / Linux 各自手写一份
//! `root_uniform` / `geometry_root`，逐 dword 硬编码下标（`uniform[8] = 1.0` 之类），三份实现
//! 之间没有任何交叉校验 —— 这正是 P0-1.1（实例基址错位）能长期潜伏的结构性原因。本模块把
//! 「哪个 dword 是哪个字段」收口成一处，平台后端只负责把 `[f32; DWORDS]` 送进各自的常量槽。
//!
//! 布局（全部 16 B 成员，见 contract §1 硬约束 1）：
//!
//! | dword | 字段                | 语义                                                     |
//! |-------|---------------------|----------------------------------------------------------|
//! | 0     | `frame.viewport`    | `(w_px, h_px, 2/w_px, 2/h_px)`                           |
//! | 4     | `frame.params`      | `(scale, scroll_x, scroll_y, 0)`；`.x` 是 DPI，`.yz` 是滚动偏移 |
//! | 8     | `draw.basis`        | `(m11, m12, m21, m22)`                                   |
//! | 12    | `draw.translation`  | `(tx, ty, 0, 0)`                                         |
//! | 16    | `draw.color0`       | linear premultiplied RGBA                                |
//! | 20    | `draw.clip_rect`    | `(x, y, w, h)` 物理像素                                  |
//! | 24    | `draw.clip`         | `.x = clip_index`、`.y = instance_base`、`.zw` 保留恒 0  |

use crate::math::Transform;

/// `RootUniform` 的 DWORD 数（112 B）。
pub(crate) const ROOT_UNIFORM_DWORDS: usize = 28;

/// `draw.clip.y` 的 dword 下标（实例基址）。
const INSTANCE_BASE_DWORD: usize = 25;

/// 一次绘制要用的常量块。
///
/// 实例绘制只需要 `frame`（`draw` 块保持零/恒等，shader 不读）；几何绘制需要全部字段。
/// 两种情形都由本类型的构造器产出，避免平台侧各自拼数组。
#[derive(Clone, Copy, Debug)]
pub(crate) struct RootUniform {
    words: [f32; ROOT_UNIFORM_DWORDS],
}

impl RootUniform {
    /// 帧级常量（实例绘制用）：`viewport` 来自目标物理尺寸，`scale` 来自窗口 DPI。
    ///
    /// `draw` 块写成恒等变换 + 不透明白，与旧实现一致：uber-quad 的 shader 不读 `draw`，
    /// 但保持「任何 dword 都有确定值」比留零更容易在像素回归里发现误读。
    pub(crate) fn frame(width: f32, height: f32, scale: f32) -> Self {
        let mut words = [0.0f32; ROOT_UNIFORM_DWORDS];
        words[0] = width;
        words[1] = height;
        words[2] = if width > 0.0 { 2.0 / width } else { 0.0 };
        words[3] = if height > 0.0 { 2.0 / height } else { 0.0 };
        words[4] = scale;
        // draw.basis = 恒等、draw.color0 = 不透明白。
        words[8] = 1.0;
        words[11] = 1.0;
        words[16] = 1.0;
        words[17] = 1.0;
        words[18] = 1.0;
        words[19] = 1.0;
        Self { words }
    }

    /// 在帧级常量之上覆盖一次几何绘制的 draw 块。
    ///
    /// `transform` 是**逻辑像素**的仿射变换（`Path`/`Geometry` 的局部空间 → 内容空间），
    /// `color` 是 linear premultiplied RGBA（`Path.fill` / `GeometryDraw.color` 的解析结果），
    /// `clip_rect` 是 clip 链交集 AABB（物理浮点 `x, y, w, h`）。
    pub(crate) fn geometry(
        frame: Self,
        transform: Transform,
        color: [f32; 4],
        clip_index: u32,
        clip_rect: [f32; 4],
    ) -> Self {
        let mut words = frame.words;
        let matrix = transform.to_array();
        words[8..12].copy_from_slice(&matrix[..4]);
        words[12..14].copy_from_slice(&matrix[4..6]);
        words[16..20].copy_from_slice(&color);
        words[20..24].copy_from_slice(&clip_rect);
        words[24] = clip_index as f32;
        // `.y = instance_base` 对几何绘制无意义（几何不走实例缓冲），保持 0。
        words[INSTANCE_BASE_DWORD] = 0.0;
        Self { words }
    }

    /// 写入本帧实例段的基址（`FrameOp::Instances.start`）。
    ///
    /// **这是 P0-1.1 的修复核心**：三平台的硬件实例基址语义不一致 ——
    ///
    /// - Vulkan 的 `firstInstance` **会**加到 `gl_InstanceIndex` 上；
    /// - D3D12 的 `StartInstanceLocation` **不会**加到 `SV_InstanceID` 上（这正是 HLSL SM 6.8
    ///   新增 `SV_StartInstanceLocation` 的动机，见 HLSL 提案 0015）；
    /// - Metal 的 `baseInstance` **会**加到 `instance_id` 上。
    ///
    /// 三套 shader 因此改为「`instances[instance_id + instance_base]`」，且三个后端**一律**
    /// 传 `baseInstance = 0` / `firstInstance = 0`：索引偏移只由本字段承担，平台差异消失。
    pub(crate) fn with_instance_base(mut self, start: u32) -> Self {
        self.words[INSTANCE_BASE_DWORD] = start as f32;
        self
    }

    /// 写入本 op 的**滚动偏移**（物理像素；`frame.params.yz`）。
    ///
    /// B6-2 的合成端偏移：滚动帧不重编码任何实例，GPU 侧只把该槽位的偏移写进 root constant，
    /// 由实例顶点 shader 在**最后一步**（`to_ndc` 之前）加到世界坐标上。因此实例字节里烘焙的
    /// 坐标始终是「滚动容器内容空间」，与偏移无关。
    ///
    /// 为什么用 `frame.params.yz` 而不是 `draw.clip.zw`：`draw` 块在实例路径上整体不被读取
    /// （`RootUniform::frame` 写恒等变换占位），而 `frame.params.x` 已被 DPI 占用，
    /// `.yz` 是 frame 块里唯一未使用的两个 dword；几何绘制用 `.zw` 同样语义一致
    /// （`geometry_root` 会把逻辑偏移直接并进 `draw.translation`）。
    ///
    /// **上传契约**：本字段落在 **frame 块**（dword 5/6），所以任何「每 op 变化」的后端都必须
    /// 每 op 推**整份** 112 B，不能只推 draw 块（dword 8..28）。三端现在都是整份上传：
    /// D3D12 `SetGraphicsRoot32BitConstants(0, 28, .., 0)` / Vulkan `cmd_push_constants([0,112))` /
    /// Metal `setVertexBytes(112)`。Windows 曾经只推 draw 块，滚动偏移因此永远停在 0
    /// （D-1：滚动容器内容不移动，而 CPU 侧偏移 / damage / `encoded_lanes` 全对）。
    pub(crate) fn with_scroll(mut self, offset: [f32; 2]) -> Self {
        self.words[5] = offset[0];
        self.words[6] = offset[1];
        self
    }

    /// 平台 API 需要 `*const c_void` / `&[f32]` 时的零拷贝视图。
    pub(crate) fn words(&self) -> &[f32; ROOT_UNIFORM_DWORDS] {
        &self.words
    }
}

#[cfg(test)]
mod tests {
    use super::{INSTANCE_BASE_DWORD, ROOT_UNIFORM_DWORDS, RootUniform};
    use crate::math::Transform;

    #[test]
    fn layout_is_112_bytes() {
        assert_eq!(ROOT_UNIFORM_DWORDS * 4, 112);
        assert_eq!(std::mem::size_of::<RootUniform>(), 112);
    }

    #[test]
    fn frame_block_matches_the_contract() {
        let uniform = RootUniform::frame(800.0, 600.0, 2.0);
        let words = uniform.words();
        assert_eq!(words[0], 800.0);
        assert_eq!(words[1], 600.0);
        assert_eq!(words[2], 2.0 / 800.0);
        assert_eq!(words[3], 2.0 / 600.0);
        assert_eq!(words[4], 2.0, "frame.params.x 是 DPI scale");
    }

    #[test]
    fn zero_sized_viewport_does_not_divide_by_zero() {
        let uniform = RootUniform::frame(0.0, 0.0, 1.0);
        assert_eq!(uniform.words()[2], 0.0);
        assert_eq!(uniform.words()[3], 0.0);
        assert!(uniform.words().iter().all(|value| value.is_finite()));
    }

    #[test]
    fn the_scroll_offset_lands_in_frame_params_yz() {
        let uniform = RootUniform::frame(64.0, 64.0, 2.0).with_scroll([0.0, -30.0]);
        assert_eq!(uniform.words()[4], 2.0, "DPI 不受影响");
        assert_eq!(uniform.words()[5], 0.0);
        assert_eq!(uniform.words()[6], -30.0);
        assert_eq!(uniform.words()[7], 0.0, "保留恒 0");
        // 默认（无滚动）是 0，因此不开滚动的帧与旧行为逐字节一致。
        assert_eq!(
            &RootUniform::frame(64.0, 64.0, 2.0).words()[5..8],
            &[0.0, 0.0, 0.0]
        );
    }

    #[test]
    fn instance_base_lands_in_draw_clip_y() {
        let uniform = RootUniform::frame(64.0, 64.0, 1.0).with_instance_base(37);
        assert_eq!(uniform.words()[INSTANCE_BASE_DWORD], 37.0);
        assert_eq!(uniform.words()[24], 0.0, "clip_index 不受影响");
    }

    #[test]
    fn geometry_overwrites_the_draw_block() {
        let frame = RootUniform::frame(800.0, 600.0, 1.5);
        let transform = Transform::scale(2.0, 5.0).then(Transform::translate(6.0, 7.0));
        let uniform = RootUniform::geometry(
            frame,
            transform,
            [0.1, 0.2, 0.3, 0.4],
            3,
            [4.0, 5.0, 100.0, 120.0],
        );
        let words = uniform.words();
        assert_eq!(&words[..8], &frame.words()[..8], "frame 块逐字保留");
        assert_eq!(&words[8..12], &[2.0, 0.0, 0.0, 5.0]);
        assert_eq!(&words[12..14], &[6.0, 7.0]);
        assert_eq!(&words[16..20], &[0.1, 0.2, 0.3, 0.4]);
        assert_eq!(&words[20..24], &[4.0, 5.0, 100.0, 120.0]);
        assert_eq!(words[24], 3.0);
        assert_eq!(words[INSTANCE_BASE_DWORD], 0.0, "几何绘制不吃实例基址");
    }
}
