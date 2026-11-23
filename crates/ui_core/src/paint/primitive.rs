//! 8 类绘制指令（I2 的唯一表示）。
//!
//! 坐标全为**绝对窗口逻辑像素**，没有 per-primitive 变换（`GeometryDraw.transform` 是唯一例外）。
//! 字段语义与「与附录 C 的差异」。

use std::{ops::Range, sync::Arc};

use crate::{
    math::{Point, Rect, Size, Transform},
    paint::{
        atlas::AtlasTile,
        canvas::Canvas,
        color::Color,
        geometry::{FULL_INDEX_RANGE, GeometryId},
    },
};

/// 一帧绘制指令的最小单位。顺序即绘制序（后画的覆盖先画的）。
#[derive(Clone, Debug, PartialEq)]
pub enum Primitive {
    Quad(Quad),
    GlyphRun(GlyphRun),
    Image(Image),
    Path(Path),
    Shadow(Shadow),
    Clip(ClipOp),
    Geometry(GeometryDraw),
    /// 自绘画布（`Arc` 共享：paint 每帧只加引用计数，不深拷贝画布内容，F-215）。
    ///
    /// **必须是 `Arc` 而不是 `Rc`**：图元要跨线程交给合成线程（B6-2 的 `SceneDelta`），
    /// `Rc` 会让整棵图元树 `!Send`。
    Canvas(Arc<Canvas>),
}

/// 背景 / 边框 / 渐变。
#[derive(Clone, Debug, PartialEq)]
pub struct Quad {
    /// 逻辑像素矩形；圆角与边框都在此矩形内。
    pub bounds: Rect,
    /// 四角半径，CSS 顺序 `[tl, tr, br, bl]`，逻辑像素。
    pub corner_radius: [f32; 4],
    pub paint: QuadPaint,
}

/// 着色模式。三选一由**类型**保证（不是两个 `Option` 的运行时不变量）。
#[derive(Clone, Debug, PartialEq)]
pub enum QuadPaint {
    /// 实色填充，可带内侧边框。
    Solid { fill: Color, border: Option<Border> },
    /// 双色渐变。v1 不允许同时带边框（`color1` 只有一个槽）。
    Gradient(Gradient),
}

/// 内侧边框（沿 `bounds` 内缘展开）。
///
/// `width` 是**逻辑像素**的绘制宽度；编码期按设备像素吸附成**整数物理像素且至少 1 px**
/// （`math::snap_stroke_width`，Skia hairline / WebRender 边框吸附）：1 dp 边框在 125% /
/// 150% / 175% 下是 1 / 2 / 2 物理像素的硬边，不是 1.25/1.5/1.75 px 的 AA 带（R04-4）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Border {
    /// 边框宽度（逻辑像素）。
    pub width: f32,
    pub color: Color,
}

/// 双色渐变。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Gradient {
    pub from: Color,
    pub to: Color,
    pub kind: GradientKind,
}

/// 渐变方向：`Horizontal` 沿 +x、`Vertical` 沿 +y、`Radial` 以 `bounds` 中心为圆心。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GradientKind {
    Horizontal,
    Vertical,
    Radial,
}

/// 一段已塑形的文本（P06 的输出交接形状）。
#[derive(Clone, Debug, PartialEq)]
pub struct GlyphRun {
    /// 整段文本的前景色（未预乘 `Color`）。
    pub color: Color,
    /// 已定位的字形，顺序 = 绘制顺序。
    pub glyphs: Vec<PositionedGlyph>,
}

/// 一个字形在图集里的矩形 + 它在逻辑空间的落点。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PositionedGlyph {
    pub tile: AtlasTile,
    /// 字形 quad 左上角（逻辑像素、绝对坐标，已含 run 偏移与基线）。
    pub position: Point,
    /// 字形 quad 的逻辑尺寸。契约：`scale * size` 必须是整数（P06 按设备 scale 光栅化）。
    pub size: Size,
}

/// 图集图片 + 调制色。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Image {
    pub bounds: Rect,
    pub tile: AtlasTile,
    /// 调制色：`texel * modulation`（预乘空间）。`Color::WHITE` = 不调制。
    pub modulation: Color,
}

/// 引擎三角化的矢量填充（v1 无 AA）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Path {
    /// 逻辑像素 AABB（damage / 命中用）。
    pub bounds: Rect,
    /// 已注册的三角化网格（`GeometryPipeline::Ui` 布局 + 恒等变换）。
    pub geometry: GeometryId,
    /// 实色填充。
    pub fill: Color,
}

/// 解析阴影：可分离 2D 高斯（σ = blur / 2）的解析近似。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shadow {
    /// 投影盒：**未偏移、未模糊**的矩形（逻辑像素）。
    pub rect: Rect,
    /// 偏移（逻辑像素）。
    pub offset: Point,
    /// 高斯**模糊半径**（逻辑像素，`>= 0`）。CSS `box-shadow` 的 `blur-radius` 语义：
    /// `σ = blur / 2`（Figma / Skia 的 `blur` 同义，见 `contract.md` §4）。
    pub blur: f32,
    /// 阴影色。
    pub color: Color,
    /// 四角半径 `[tl, tr, br, bl]`（逻辑像素，CSS 顺序）。与 quad 共用同一套规范化
    /// （`r <= min(w, h) / 2`）：`box-shadow` 跟随 `border-radius`，旧实现丢掉了这一项，
    /// 圆角卡片背后会露出尖角阴影（R05-9）。
    pub corner_radius: [f32; 4],
}

/// clip 栈指令。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ClipOp {
    /// 压栈：后续图元被 `rect`（含四角圆角）裁剪。子图元的绝对坐标不变。
    Push(ClipRect),
    /// 弹栈：恢复上一层。
    Pop,
}

/// 一层 clip（矩形 + 四角圆角）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClipRect {
    pub rect: Rect,
    /// 四角半径 `[tl, tr, br, bl]`，逻辑像素；全 0 = 直角。
    pub corner_radius: [f32; 4],
}

/// 静态几何的每帧部分（变换、颜色与索引子区间是唯一的 per-primitive 参数）。
///
/// **字段都必须被渲染层消费**：`transform` 进 `draw.basis`/`draw.translation`、
/// `color` 进 `draw.color0`（`contract.md` §6）、`range` 在 CPU 侧与 4096 B 页级上传的块表求交后
/// 变成逐块的 `DrawIndexed(first_index, index_count)`（E4 / §11.4；不进 `GpuPrimitive`）。
/// 此前 `Path.fill` 在编码期被丢弃、`transform` 被平台硬编码成单位矩阵，导致几何通道画成
/// 不透明白且无法平移 —— 这是 P0-1.2 的核心症状。
#[derive(Clone, Debug, PartialEq)]
pub struct GeometryDraw {
    pub id: GeometryId,
    /// 每帧 uniform 变换（局部空间 → 内容空间，**逻辑像素**；shader 再乘 DPI）。
    pub transform: Transform,
    /// 填充色 / 调制色（`GeometryPipeline::Ui` 是实色，`Textured` 是调制色）。
    pub color: Color,
    /// 索引**元素**区间（半开；与 `IndexType` 无关，`TriangleStrip` 同样按索引计）。
    ///
    /// - `0..u32::MAX` = 整份几何（[`GeometryDraw::full`]）；
    /// - 空区间 / `start >= end` = 不绘制（无 op、无 damage）；
    /// - 超出 `index_count` 的一端由引擎 **clamp**（几何可能被 `truncate_geometry` 变短，不报错）；
    /// - `TriangleList` 的 range 两端由引擎**向下**对齐到 3 的倍数（起点向下取整、长度向下取整：
    ///   起点不在三角形边界时，起点之前的至多 2 个索引一并画出，保证与整段绘制的三角形集合一致；
    ///   不足一个三角形的尾部索引不产生 draw）；`TriangleStrip` 不截断。
    ///
    /// 引擎把 range 与块表求交，**逐块**产出绘制 op（块间顺序 = 索引顺序）；应用不需要知道块
    /// 大小（§5.5 的块是引擎内部策略）。
    pub range: Range<u32>,
}

impl GeometryDraw {
    /// 整份几何（`range = 0..u32::MAX`；现有所有构造点的迁移目标）。空区间 = 不绘制。
    #[must_use]
    pub fn full(id: GeometryId, transform: Transform, color: Color) -> Self {
        Self {
            id,
            transform,
            color,
            range: FULL_INDEX_RANGE,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{mem::size_of, sync::Arc};

    use super::{
        Border, ClipOp, ClipRect, GeometryDraw, GlyphRun, Gradient, GradientKind, Image, Path,
        PositionedGlyph, Primitive, Quad, QuadPaint, Shadow,
    };
    use crate::{
        math::{Point, Rect, Size, Transform},
        paint::{
            atlas::{AtlasKind, AtlasTile},
            canvas::Canvas,
            color::Color,
            geometry::GeometryId,
        },
    };

    fn rect() -> Rect {
        Rect::new(0.0, 0.0, 16.0, 8.0)
    }

    fn tile(atlas: AtlasKind) -> AtlasTile {
        AtlasTile {
            atlas,
            page: 0,
            u0: 0.0,
            v0: 0.0,
            u1: 0.5,
            v1: 0.5,
            generation: 1,
            slot: 0,
            slot_epoch: 0,
        }
    }

    fn geometry_id() -> GeometryId {
        GeometryId::for_test(3, 1)
    }

    /// 8 个变体各一个样例（含嵌套 `Canvas`）。
    fn samples() -> Vec<(&'static str, Primitive)> {
        vec![
            (
                "Quad",
                Primitive::Quad(Quad {
                    bounds: rect(),
                    corner_radius: [1.0, 2.0, 3.0, 4.0],
                    paint: QuadPaint::Solid {
                        fill: Color::WHITE,
                        border: Some(Border {
                            width: 1.0,
                            color: Color::BLACK,
                        }),
                    },
                }),
            ),
            (
                "GlyphRun",
                Primitive::GlyphRun(GlyphRun {
                    color: Color::BLACK,
                    glyphs: vec![PositionedGlyph {
                        tile: tile(AtlasKind::Glyph),
                        position: Point::new(1.0, 2.0),
                        size: Size::new(4.0, 6.0),
                    }],
                }),
            ),
            (
                "Image",
                Primitive::Image(Image {
                    bounds: rect(),
                    tile: tile(AtlasKind::Image),
                    modulation: Color::WHITE,
                }),
            ),
            (
                "Path",
                Primitive::Path(Path {
                    bounds: rect(),
                    geometry: geometry_id(),
                    fill: Color::WHITE,
                }),
            ),
            (
                "Shadow",
                Primitive::Shadow(Shadow {
                    rect: rect(),
                    offset: Point::new(0.0, 2.0),
                    blur: 4.0,
                    color: Color::BLACK,
                    corner_radius: [1.0, 2.0, 3.0, 4.0],
                }),
            ),
            (
                "Clip",
                Primitive::Clip(ClipOp::Push(ClipRect {
                    rect: rect(),
                    corner_radius: [0.0; 4],
                })),
            ),
            (
                "Geometry",
                Primitive::Geometry(GeometryDraw::full(
                    geometry_id(),
                    Transform::IDENTITY,
                    Color::WHITE,
                )),
            ),
            ("Canvas", Primitive::Canvas(Arc::new(Canvas::new(rect())))),
        ]
    }

    #[test]
    fn all_variants_clone_eq_and_debug() {
        let samples = samples();
        assert_eq!(samples.len(), 8);
        for (name, sample) in &samples {
            // Clone 后相等（`Canvas` 变体依赖 `Canvas` 的 `PartialEq`）。
            let clone = sample.clone();
            assert_eq!(&clone, sample, "variant {name}");
            // Debug 必须能区分变体，便于 P06 帧记录定位。
            let text = format!("{sample:?}");
            assert!(text.contains(name), "variant {name} missing from {text}");
        }
        // 载荷字段往返：改一个字段必须改变相等性。
        match &samples[0].1 {
            Primitive::Quad(quad) => {
                let mut different = quad.clone();
                different.corner_radius[0] = 99.0;
                assert_ne!(&Primitive::Quad(different), &samples[0].1);
            }
            other => panic!("expected Quad, got {other:?}"),
        }
    }

    #[test]
    fn size_budget() {
        // 实测 80 B（= Quad 变体）；软上限 128 B，超出需评审。
        assert_eq!(size_of::<Primitive>(), 80);
        assert!(size_of::<Primitive>() <= 128);
    }

    /// E4：`range` 是**索引元素**区间，`full` 是 `0..u32::MAX` 哨兵；子区间不同 ⇒ 绘制不同。
    #[test]
    fn geometry_draw_range_is_part_of_the_value() {
        let full = GeometryDraw::full(geometry_id(), Transform::IDENTITY, Color::WHITE);
        assert_eq!(
            full.range,
            0..u32::MAX,
            "`full` 必须用整份哨兵（几何可增长）"
        );
        let mut subset = full.clone();
        subset.range = 3..9;
        assert_ne!(subset, full, "不同子区间是不同的绘制（参与相等性）");
        let mut empty = subset.clone();
        empty.range = 6..6;
        assert_ne!(empty, subset, "空区间与子区间不同");
    }

    #[test]
    fn quad_paint_encodings_are_mutually_exclusive() {
        let plain = QuadPaint::Solid {
            fill: Color::WHITE,
            border: None,
        };
        let bordered = QuadPaint::Solid {
            fill: Color::WHITE,
            border: Some(Border {
                width: 1.0,
                color: Color::BLACK,
            }),
        };
        let gradient = QuadPaint::Gradient(Gradient {
            from: Color::WHITE,
            to: Color::BLACK,
            kind: GradientKind::Horizontal,
        });
        // 三种形状互不相同（类型层面已保证「不会既是纯色又是渐变」）。
        assert_ne!(plain, bordered);
        assert_ne!(bordered, gradient);
        assert_ne!(plain, gradient);
        assert!(format!("{plain:?}").contains("Solid"));
        assert!(format!("{gradient:?}").contains("Gradient"));
        // 穷尽 match：新增变体必须显式处理。
        match gradient {
            QuadPaint::Solid { .. } => panic!("expected gradient"),
            QuadPaint::Gradient(Gradient { kind, .. }) => {
                assert_eq!(kind, GradientKind::Horizontal)
            }
        }
    }

    #[test]
    fn shadow_keeps_unoffset_rect() {
        let rect = Rect::new(4.0, 4.0, 8.0, 8.0);
        let shadow = Shadow {
            rect,
            offset: Point::new(10.0, 20.0),
            blur: 2.0,
            color: Color::BLACK,
            corner_radius: [0.0; 4],
        };
        assert_eq!(shadow.rect, rect);
    }

    #[test]
    fn gradient_kind_has_three_distinct_variants() {
        let kinds = [
            GradientKind::Horizontal,
            GradientKind::Vertical,
            GradientKind::Radial,
        ];
        for (index, left) in kinds.iter().enumerate() {
            for right in &kinds[index + 1..] {
                assert_ne!(left, right);
            }
            // Debug 名字必须唯一（P06 帧记录靠它区分）。
            assert!(!format!("{left:?}").is_empty());
        }
        assert_eq!(format!("{:?}", GradientKind::Radial), "Radial");
    }

    #[test]
    fn clip_op_push_pop_roundtrip() {
        let clip = ClipRect {
            rect: rect(),
            corner_radius: [2.0, 0.0, 2.0, 0.0],
        };
        assert_eq!(ClipOp::Push(clip), ClipOp::Push(clip));
        assert_ne!(ClipOp::Pop, ClipOp::Push(clip));
        // 圆角不同即不同：不同 clip 层不可互换。
        let other = ClipRect {
            rect: rect(),
            corner_radius: [0.0; 4],
        };
        assert_ne!(ClipOp::Push(clip), ClipOp::Push(other));
        assert!(format!("{:?}", ClipOp::Push(clip)).contains("Push"));
        assert!(format!("{:?}", ClipOp::Pop).contains("Pop"));
    }
}
