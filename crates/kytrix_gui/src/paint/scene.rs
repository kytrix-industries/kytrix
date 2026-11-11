//! 绘制桶（`Lane`）与 clip 深度上限（P02 只有这两项）。
//!
//! **`Scene` / `LaneId` / lane 分配与绘制序在 P03 落地**（`design/phase-02-paint-foundation.md` §7.2、§19）：
//! P02 没有任何非测试消费者持有 `Scene`，提前落地会触发 `dead_code`（§2.4c）。
//!
//! 本模块整体 crate 私有（`pub(crate) mod scene;`）：应用层通过 [`Canvas`] 间接使用 `Lane`。
//!
//! [`Canvas`]: crate::paint::canvas::Canvas

use crate::paint::primitive::{ClipOp, Primitive};

/// clip 栈深度上限：**类型是 `u8`**，值 8。
///
/// 三处必须一致：本模块 `Lane::push` 的 debug 断言、`Canvas::push_clip` 的 `bool` 门限、
/// P10 三套 shader 的固定数组长度。
pub(crate) const MAX_CLIP_DEPTH: u8 = 8;

/// 一个绘制输出桶。内容按绘制序排列；`clip_depth` 是 C2/C3 的**增量校验状态**。
///
/// 只在 crate 内可见：应用层通过 `Canvas` 构造与读取（`design/phase-02-paint-foundation.md` §11）。
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Lane {
    primitives: Vec<Primitive>,
    clip_depth: u8,
}

impl Lane {
    /// 空 lane。消费者：`Canvas::new`。
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 唯一的压入点：更新 `clip_depth` 并做 C2/C3 的 debug 断言。消费者：`Canvas` 的 `push_*`。
    pub(crate) fn push(&mut self, primitive: Primitive) {
        match &primitive {
            Primitive::Clip(ClipOp::Push(_)) => {
                debug_assert!(usize::from(self.clip_depth()) < usize::from(MAX_CLIP_DEPTH));
                self.clip_depth = self.clip_depth.saturating_add(1);
            }
            Primitive::Clip(ClipOp::Pop) => {
                debug_assert!(usize::from(self.clip_depth()) > 0);
                self.clip_depth = self.clip_depth.saturating_sub(1);
            }
            _ => {}
        }
        self.primitives.push(primitive);
    }

    /// 当前 clip 嵌套深度。消费者：`Lane::push`（内部）与 `Canvas` 的 clip 门限。
    pub(crate) fn clip_depth(&self) -> u8 {
        self.clip_depth
    }

    /// 只读切片。消费者：`Canvas::primitives`。
    pub(crate) fn primitives(&self) -> &[Primitive] {
        &self.primitives
    }

    /// 消费者：`Canvas::is_empty`。
    pub(crate) fn is_empty(&self) -> bool {
        self.primitives.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::{Lane, MAX_CLIP_DEPTH};
    use crate::{
        math::{Point, Rect},
        paint::{
            atlas::{AtlasKind, AtlasTile},
            color::Color,
            primitive::{ClipOp, ClipRect, Image, Primitive, Quad, QuadPaint, Shadow},
        },
    };

    fn clip() -> ClipRect {
        ClipRect {
            rect: Rect::new(0.0, 0.0, 8.0, 8.0),
            corner_radius: [0.0; 4],
        }
    }

    fn quad() -> Primitive {
        Primitive::Quad(Quad {
            bounds: Rect::new(0.0, 0.0, 4.0, 4.0),
            corner_radius: [0.0; 4],
            paint: QuadPaint::Solid {
                fill: Color::WHITE,
                border: None,
            },
        })
    }

    fn image() -> Primitive {
        Primitive::Image(Image {
            bounds: Rect::new(0.0, 0.0, 4.0, 4.0),
            tile: AtlasTile {
                atlas: AtlasKind::Image,
                u0: 0.0,
                v0: 0.0,
                u1: 1.0,
                v1: 1.0,
                generation: 0,
            },
            modulation: Color::WHITE,
        })
    }

    fn shadow() -> Primitive {
        Primitive::Shadow(Shadow {
            rect: Rect::new(0.0, 0.0, 4.0, 4.0),
            offset: Point::new(0.0, 1.0),
            blur: 2.0,
            color: Color::BLACK,
        })
    }

    #[test]
    fn new_lane_is_empty() {
        let lane = Lane::new();
        assert!(lane.is_empty());
        assert!(lane.primitives().is_empty());
        assert_eq!(lane.clip_depth(), 0);
    }

    #[test]
    fn push_preserves_order() {
        let mut lane = Lane::new();
        lane.push(quad());
        lane.push(image());
        lane.push(shadow());
        let kinds: Vec<&str> = lane
            .primitives()
            .iter()
            .map(|primitive| match primitive {
                Primitive::Quad(_) => "quad",
                Primitive::Image(_) => "image",
                Primitive::Shadow(_) => "shadow",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, ["quad", "image", "shadow"]);
        assert!(!lane.is_empty());
    }

    #[test]
    fn push_tracks_clip_depth() {
        let mut lane = Lane::new();
        lane.push(quad());
        assert_eq!(lane.clip_depth(), 0);
        lane.push(Primitive::Clip(ClipOp::Push(clip())));
        assert_eq!(lane.clip_depth(), 1);
        lane.push(Primitive::Clip(ClipOp::Pop));
        assert_eq!(lane.clip_depth(), 0);
        assert_eq!(lane.primitives().len(), 3);
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "assertion failed")]
    fn pop_underflow_is_debug_assert() {
        let mut lane = Lane::new();
        lane.push(Primitive::Clip(ClipOp::Pop));
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "assertion failed")]
    fn depth_cap_is_debug_assert() {
        let mut lane = Lane::new();
        for _ in 0..MAX_CLIP_DEPTH {
            lane.push(Primitive::Clip(ClipOp::Push(clip())));
        }
        assert_eq!(lane.clip_depth(), MAX_CLIP_DEPTH);
        lane.push(Primitive::Clip(ClipOp::Push(clip())));
    }

    #[test]
    fn max_clip_depth_is_u8_8() {
        let depth: u8 = MAX_CLIP_DEPTH;
        assert_eq!(depth, 8);
    }
}
