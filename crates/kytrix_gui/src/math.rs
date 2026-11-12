//! 逻辑像素几何与「逻辑 → 物理像素」的唯一换算点（I11）。
//!
//! 所有几何以逻辑像素 `f32` 存储，只在 paint / present 边界做一次缩放与舍入。换算分三种语义，
//! 职责严格分离（见 `design/phase-01-foundation.md` §7）：
//!
//! - [`Rect::to_physical_f32`]：不取整的物理浮点矩形，用于 SDF 图元的 `GpuPrimitive.bounds` 与 `clip_rect`；
//! - [`Rect::to_physical`]：逐边取整，用于图集图元（`GlyphRun` / `Image`）的四边形；
//! - [`Rect::to_physical_covering`]：向外 floor/ceil，用于 damage 与 scissor 的安全侧。
//!
//! 逐边取整保证相邻图元共享边精确重合（无 1px 缝）：同一逻辑坐标只有一种表达式，
//! 舍入输入逐位相同则输出相同。

use std::ops::{Add, Mul, Sub};

/// `i32` 可表示范围的安全端点；浮点转整型前先夹到此区间。
const I32_MIN_F: f32 = i32::MIN as f32;
const I32_MAX_F: f32 = i32::MAX as f32;

/// 逻辑像素坐标点（左上原点，y 向下）。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub const ZERO: Self = Self { x: 0.0, y: 0.0 };

    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

impl Add for Point {
    type Output = Self;

    fn add(self, rhs: Self) -> Self {
        Self {
            x: self.x + rhs.x,
            y: self.y + rhs.y,
        }
    }
}

impl Sub for Point {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self {
        Self {
            x: self.x - rhs.x,
            y: self.y - rhs.y,
        }
    }
}

impl Mul<f32> for Point {
    type Output = Self;

    fn mul(self, rhs: f32) -> Self {
        Self {
            x: self.x * rhs,
            y: self.y * rhs,
        }
    }
}

/// 逻辑像素尺寸。允许 0 与负数（负数一律视为空，不做静默归一化）。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Size {
    pub width: f32,
    pub height: f32,
}

impl Size {
    pub const ZERO: Self = Self {
        width: 0.0,
        height: 0.0,
    };

    pub const fn new(width: f32, height: f32) -> Self {
        Self { width, height }
    }

    /// 宽度或高度非有限或 `<= 0` 即空（NaN / ±∞ 也算空，安全方向）。
    pub fn is_empty(self) -> bool {
        !self.width.is_finite()
            || !self.height.is_finite()
            || self.width <= 0.0
            || self.height <= 0.0
    }

    pub fn area(self) -> f32 {
        self.width * self.height
    }
}

impl Mul<f32> for Size {
    type Output = Self;

    fn mul(self, rhs: f32) -> Self {
        Self {
            width: self.width * rhs,
            height: self.height * rhs,
        }
    }
}

/// 逻辑像素轴对齐矩形，`origin + size` 表示，半开区间 `[min, max)`。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub origin: Point,
    pub size: Size,
}

impl Rect {
    pub const fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            origin: Point { x, y },
            size: Size { width, height },
        }
    }

    pub const fn from_origin_size(origin: Point, size: Size) -> Self {
        Self { origin, size }
    }

    pub fn min(self) -> Point {
        self.origin
    }

    pub fn max(self) -> Point {
        Point {
            x: self.origin.x + self.size.width,
            y: self.origin.y + self.size.height,
        }
    }

    /// 同 [`Size::is_empty`]（非有限或 `<= 0`）；另外原点非有限（无法绘制）也判空。
    pub fn is_empty(self) -> bool {
        self.size.is_empty() || !self.origin.x.is_finite() || !self.origin.y.is_finite()
    }

    /// 半开区间判定：`min <= p < max`；空矩形恒 `false`。
    pub fn contains(self, point: Point) -> bool {
        let max = self.max();
        !self.is_empty()
            && point.x >= self.origin.x
            && point.x < max.x
            && point.y >= self.origin.y
            && point.y < max.y
    }

    pub fn translate(self, by: Point) -> Self {
        Self {
            origin: self.origin + by,
            size: self.size,
        }
    }

    /// 四边内缩；负值即外扩。结果可能为空，不做 clamp。
    pub fn inset(self, edges: Edges) -> Self {
        Self {
            origin: Point {
                x: self.origin.x + edges.left,
                y: self.origin.y + edges.top,
            },
            size: Size {
                width: self.size.width - edges.left - edges.right,
                height: self.size.height - edges.top - edges.bottom,
            },
        }
    }

    /// 四边等量外扩（物理像素域的外扩由 `render/damage` 负责，这里只做逻辑量）。
    pub fn outset(self, amount: f32) -> Self {
        self.inset(Edges::uniform(-amount))
    }

    /// 并集：任一为空时返回另一个（`∅ ∪ R = R`）；两个不同的空矩形 union 仍为空，
    /// 但不保证返回哪一个的值（空矩形的位置没有几何意义）。
    pub fn union(self, other: Self) -> Self {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }

        let min = Point {
            x: self.min().x.min(other.min().x),
            y: self.min().y.min(other.min().y),
        };
        let max = Point {
            x: self.max().x.max(other.max().x),
            y: self.max().y.max(other.max().y),
        };

        Self {
            origin: min,
            size: Size {
                width: max.x - min.x,
                height: max.y - min.y,
            },
        }
    }

    /// 交集；不相交时返回零面积矩形。
    pub fn intersect(self, other: Self) -> Self {
        if !self.is_convertible() || !other.is_convertible() {
            return Self::default();
        }

        let x0 = self.min().x.max(other.min().x);
        let y0 = self.min().y.max(other.min().y);
        let x1 = self.max().x.min(other.max().x);
        let y1 = self.max().y.min(other.max().y);

        Self {
            origin: Point { x: x0, y: y0 },
            size: Size {
                width: (x1 - x0).max(0.0),
                height: (y1 - y0).max(0.0),
            },
        }
    }

    /// 半开区间重叠判定；任一不可换算（非有限或负尺寸）返回 `false`，空矩形恒 `false`。
    pub fn intersects(self, other: Self) -> bool {
        !self.intersect(other).is_empty()
    }

    /// 未取整的物理浮点矩形：`x0 = x·s`、`x1 = (x + w)·s`、`w = x1 − x0`（y 同理）。
    /// 非有限或负尺寸返回 `[0.0; 4]`。
    pub fn to_physical_f32(self, scale: Scale) -> [f32; 4] {
        if !self.is_convertible() {
            return [0.0; 4];
        }

        let factor = scale.factor();
        let x0 = self.origin.x * factor;
        let y0 = self.origin.y * factor;
        let x1 = (self.origin.x + self.size.width) * factor;
        let y1 = (self.origin.y + self.size.height) * factor;

        if !x0.is_finite() || !y0.is_finite() || !x1.is_finite() || !y1.is_finite() {
            return [0.0; 4];
        }

        let width = x1 - x0;
        let height = y1 - y0;

        // 两个有限端点相减仍可能溢出为 ±inf（例如 `-9.19e37` 到 `f32::MAX`）。
        if !width.is_finite() || !height.is_finite() {
            return [0.0; 4];
        }

        [x0, y0, width, height]
    }

    /// 逐边取整的物理像素矩形：图集图元四边形。
    pub fn to_physical(self, scale: Scale) -> PhysicalRect {
        if !self.is_convertible() {
            return EMPTY_PHYSICAL;
        }

        let factor = scale.factor();
        let x0 = round_physical(self.origin.x * factor);
        let y0 = round_physical(self.origin.y * factor);
        let x1 = round_physical((self.origin.x + self.size.width) * factor);
        let y1 = round_physical((self.origin.y + self.size.height) * factor);

        PhysicalRect::from_edges(x0, y0, x1, y1)
    }

    /// 向外 floor/ceil 的物理像素矩形：damage 与 scissor 的安全侧。
    pub fn to_physical_covering(self, scale: Scale) -> PhysicalRect {
        if !self.is_convertible() {
            return EMPTY_PHYSICAL;
        }

        let factor = scale.factor();
        let x0 = floor_physical(self.origin.x * factor);
        let y0 = floor_physical(self.origin.y * factor);
        let x1 = ceil_physical((self.origin.x + self.size.width) * factor);
        let y1 = ceil_physical((self.origin.y + self.size.height) * factor);

        PhysicalRect::from_edges(x0, y0, x1, y1)
    }

    /// 换算前置条件：四个分量有限且尺寸非负。
    fn is_convertible(self) -> bool {
        self.origin.x.is_finite()
            && self.origin.y.is_finite()
            && self.size.width.is_finite()
            && self.size.height.is_finite()
            && self.size.width >= 0.0
            && self.size.height >= 0.0
    }
}

/// 逻辑像素四边距离（CSS 顺序：上 / 右 / 下 / 左）。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Edges {
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub left: f32,
}

impl Edges {
    pub const ZERO: Self = Self {
        top: 0.0,
        right: 0.0,
        bottom: 0.0,
        left: 0.0,
    };

    pub const fn new(top: f32, right: f32, bottom: f32, left: f32) -> Self {
        Self {
            top,
            right,
            bottom,
            left,
        }
    }

    pub const fn uniform(value: f32) -> Self {
        Self {
            top: value,
            right: value,
            bottom: value,
            left: value,
        }
    }
}

/// DPI 缩放因子：逻辑像素 × factor = 物理像素。构造即校验（有限且 > 0）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Scale(f32);

impl Scale {
    pub const ONE: Self = Self(1.0);

    /// `None` 仅当 `factor` 非有限或 `<= 0`；调用方必须转成 `EngineError::InvalidDpiScale`，
    /// 不得静默回退到 1.0（I10）。
    pub fn new(factor: f32) -> Option<Self> {
        (factor.is_finite() && factor > 0.0).then_some(Self(factor))
    }

    pub const fn factor(self) -> f32 {
        self.0
    }

    /// 物理 → 逻辑（输入事件换算）。这是唯一的反向换算点。
    pub fn to_logical(self, physical: f32) -> f32 {
        physical / self.0
    }
}

/// 2D 仿射变换：`x' = m11·x + m21·y + tx`，`y' = m12·x + m22·y + ty`（列向量约定）。
///
/// GPU uniform 契约：打包为两个 `vec4` —— `(m11, m12, m21, m22)` 与 `(tx, ty, 0, 0)`
/// （6 个有效 float + 2 padding），P10 的 `contract.md` 必须镜像该布局。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Transform {
    m11: f32,
    m12: f32,
    m21: f32,
    m22: f32,
    tx: f32,
    ty: f32,
}

impl Transform {
    pub const IDENTITY: Self = Self {
        m11: 1.0,
        m12: 0.0,
        m21: 0.0,
        m22: 1.0,
        tx: 0.0,
        ty: 0.0,
    };

    pub const fn translate(tx: f32, ty: f32) -> Self {
        Self {
            m11: 1.0,
            m12: 0.0,
            m21: 0.0,
            m22: 1.0,
            tx,
            ty,
        }
    }

    pub const fn scale(sx: f32, sy: f32) -> Self {
        Self {
            m11: sx,
            m12: 0.0,
            m21: 0.0,
            m22: sy,
            tx: 0.0,
            ty: 0.0,
        }
    }

    /// 复合：先应用 `self`，再应用 `next`（即 `next × self`）。
    pub fn then(self, next: Self) -> Self {
        Self {
            m11: next.m11 * self.m11 + next.m21 * self.m12,
            m12: next.m12 * self.m11 + next.m22 * self.m12,
            m21: next.m11 * self.m21 + next.m21 * self.m22,
            m22: next.m12 * self.m21 + next.m22 * self.m22,
            tx: next.m11 * self.tx + next.m21 * self.ty + next.tx,
            ty: next.m12 * self.tx + next.m22 * self.ty + next.ty,
        }
    }

    pub fn apply(self, point: Point) -> Point {
        Point {
            x: self.m11 * point.x + self.m21 * point.y + self.tx,
            y: self.m12 * point.x + self.m22 * point.y + self.ty,
        }
    }

    /// `[m11, m12, m21, m22, tx, ty]`，与 uniform 契约前 6 项同序。
    pub fn to_array(self) -> [f32; 6] {
        [self.m11, self.m12, self.m21, self.m22, self.tx, self.ty]
    }
}

/// 物理像素矩形，半开区间 `[x0, x1) × [y0, y1)`，左上原点；不变量 `x0 <= x1 && y0 <= y1`。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PhysicalRect {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

impl PhysicalRect {
    /// 要求调用方已保证有序；debug 下断言，release 由调用方不变量保证。
    pub fn new(x0: i32, y0: i32, x1: i32, y1: i32) -> Self {
        debug_assert!(x0 <= x1 && y0 <= y1, "PhysicalRect must be ordered");

        Self { x0, y0, x1, y1 }
    }

    /// 由任意端点构造，自动 clamp 为有序（换算路径专用）。
    fn from_edges(x0: i32, y0: i32, x1: i32, y1: i32) -> Self {
        Self {
            x0,
            y0,
            x1: x1.max(x0),
            y1: y1.max(y0),
        }
    }

    /// 宽 = `x1.saturating_sub(x0)`（任何输入都不下溢）。
    ///
    /// debug 下断言 `x0 <= x1`：公开字段的不变量由构造方保证，库内一律经 [`PhysicalRect::new`]
    /// 或 `from_edges` 构造。
    pub fn width(self) -> i32 {
        debug_assert!(self.x0 <= self.x1, "PhysicalRect x invariant violated");

        self.x1.saturating_sub(self.x0)
    }

    /// 高 = `y1.saturating_sub(y0)`；不变量断言同 [`PhysicalRect::width`]。
    pub fn height(self) -> i32 {
        debug_assert!(self.y0 <= self.y1, "PhysicalRect y invariant violated");

        self.y1.saturating_sub(self.y0)
    }

    /// 宽或高为 0 即空。饱和矩形（宽/高溢出到 `i32::MAX`）不为空。
    pub fn is_empty(self) -> bool {
        self.width() == 0 || self.height() == 0
    }

    /// 面积（`i64`，避免 `i32` 溢出）：damage 阈值与测试断言共用的唯一实现。
    #[must_use]
    pub fn area(self) -> i64 {
        i64::from(self.width()) * i64::from(self.height())
    }

    pub fn union(self, other: Self) -> Self {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }

        Self {
            x0: self.x0.min(other.x0),
            y0: self.y0.min(other.y0),
            x1: self.x1.max(other.x1),
            y1: self.y1.max(other.y1),
        }
    }

    /// 不相交时退化为零面积矩形。
    pub fn intersect(self, other: Self) -> Self {
        let x0 = self.x0.max(other.x0);
        let y0 = self.y0.max(other.y0);
        let x1 = self.x1.min(other.x1);
        let y1 = self.y1.min(other.y1);

        Self {
            x0,
            y0,
            x1: x1.max(x0),
            y1: y1.max(y0),
        }
    }
}

const EMPTY_PHYSICAL: PhysicalRect = PhysicalRect {
    x0: 0,
    y0: 0,
    x1: 0,
    y1: 0,
};

fn saturate(value: f32) -> f32 {
    value.clamp(I32_MIN_F, I32_MAX_F)
}

fn round_physical(value: f32) -> i32 {
    saturate(value).round() as i32
}

fn floor_physical(value: f32) -> i32 {
    saturate(value).floor() as i32
}

fn ceil_physical(value: f32) -> i32 {
    saturate(value).ceil() as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCALES: [f32; 5] = [1.0, 1.25, 1.5, 2.0, 3.0];
    const EDGES: [f32; 12] = [
        0.0, 0.5, 1.0, 2.0, 3.0, 4.0, 6.0, 8.0, 10.0, 10.5, -2.0, -10.5,
    ];
    const ROUNDED_EDGES: [[i32; 12]; 5] = [
        [0, 1, 1, 2, 3, 4, 6, 8, 10, 11, -2, -11],
        [0, 1, 1, 3, 4, 5, 8, 10, 13, 13, -3, -13],
        [0, 1, 2, 3, 5, 6, 9, 12, 15, 16, -3, -16],
        [0, 1, 2, 4, 6, 8, 12, 16, 20, 21, -4, -21],
        [0, 2, 3, 6, 9, 12, 18, 24, 30, 32, -6, -32],
    ];

    fn scale(factor: f32) -> Scale {
        Scale::new(factor).expect("test scale must be valid")
    }

    #[test]
    fn point_arithmetic() {
        let a = Point::new(1.5, -2.0);
        let b = Point::new(0.5, 4.0);

        assert_eq!(a + b, Point::new(2.0, 2.0));
        assert_eq!(a - b, Point::new(1.0, -6.0));
        assert_eq!(a * 2.0, Point::new(3.0, -4.0));
        assert_eq!(a + Point::ZERO, a);
    }

    #[test]
    fn size_area_and_empty() {
        assert_eq!(Size::new(3.0, 4.0).area(), 12.0);
        assert!(Size::new(0.0, 4.0).is_empty());
        assert!(Size::new(-1.0, 4.0).is_empty());
        assert!(Size::new(f32::NAN, 4.0).is_empty());
        assert!(!Size::new(0.5, 0.5).is_empty());
    }

    #[test]
    fn rect_min_max() {
        let rect = Rect::new(1.5, 2.5, 3.0, 4.0);

        assert_eq!(rect.min(), Point::new(1.5, 2.5));
        assert_eq!(rect.max(), Point::new(4.5, 6.5));
    }

    #[test]
    fn rect_is_empty() {
        assert!(Rect::new(0.0, 0.0, 0.0, 10.0).is_empty());
        assert!(Rect::new(0.0, 0.0, 10.0, 0.0).is_empty());
        assert!(Rect::new(0.0, 0.0, -10.0, 10.0).is_empty());
        assert!(Rect::new(f32::NAN, 0.0, 10.0, 10.0).is_empty());
        assert!(!Rect::new(0.0, 0.0, 10.0, 10.0).is_empty());
    }

    #[test]
    fn rect_contains_half_open() {
        let rect = Rect::new(0.0, 0.0, 10.0, 10.0);

        assert!(rect.contains(Point::new(0.0, 0.0)));
        assert!(rect.contains(Point::new(9.999, 5.0)));
        assert!(!rect.contains(Point::new(10.0, 5.0)));
        assert!(!rect.contains(Point::new(5.0, 10.0)));
        assert!(!Rect::default().contains(Point::ZERO));
    }

    #[test]
    fn rect_translate_inset_outset() {
        let rect = Rect::new(0.0, 0.0, 10.0, 10.0);

        assert_eq!(
            rect.translate(Point::new(2.0, 3.0)),
            Rect::new(2.0, 3.0, 10.0, 10.0)
        );
        assert_eq!(
            rect.inset(Edges::uniform(4.0)),
            Rect::new(4.0, 4.0, 2.0, 2.0)
        );
        assert_eq!(
            rect.outset(2.0),
            rect.inset(Edges::uniform(-2.0)),
            "outset must equal negative inset"
        );
        assert_eq!(
            rect.inset(Edges::uniform(6.0)),
            Rect::new(6.0, 6.0, -2.0, -2.0)
        );
    }

    #[test]
    fn rect_union_identity() {
        let a = Rect::new(-2.0, -3.0, 4.0, 5.0);
        let b = Rect::new(5.0, 6.0, 2.0, 2.0);

        assert_eq!(Rect::default().union(a), a);
        assert_eq!(a.union(Rect::default()), a);
        assert_eq!(a.union(a), a);
        assert_eq!(a.union(b), Rect::new(-2.0, -3.0, 9.0, 11.0));
    }

    #[test]
    fn rect_intersect_disjoint_is_empty() {
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        let b = Rect::new(5.0, 5.0, 10.0, 10.0);
        let far = Rect::new(100.0, 100.0, 10.0, 10.0);

        assert_eq!(a.intersect(b), Rect::new(5.0, 5.0, 5.0, 5.0));
        assert!(a.intersects(b));
        assert!(a.intersect(far).is_empty());
        assert!(!a.intersects(far));
        assert_eq!(a.intersect(far), Rect::new(100.0, 100.0, 0.0, 0.0));
    }

    #[test]
    fn scale_rejects_non_positive_and_non_finite() {
        assert!(Scale::new(0.0).is_none());
        assert!(Scale::new(-1.0).is_none());
        assert!(Scale::new(f32::NAN).is_none());
        assert!(Scale::new(f32::INFINITY).is_none());

        for factor in SCALES {
            assert!(Scale::new(factor).is_some(), "{factor}");
        }
    }

    #[test]
    fn scale_to_logical_is_inverse() {
        for factor in SCALES {
            let scale = scale(factor);
            for logical in [0.0f32, 1.0, 10.5, -3.25] {
                let round_trip = scale.to_logical(scale.factor() * logical);
                assert!(
                    (round_trip - logical).abs() < 1e-4,
                    "factor {factor} logical {logical} got {round_trip}"
                );
            }
        }
    }

    #[test]
    fn physical_rounding_table() {
        for (factor, expected) in SCALES.iter().zip(ROUNDED_EDGES.iter()) {
            let scale = scale(*factor);
            for (edge, want) in EDGES.iter().zip(expected.iter()) {
                let got = Rect::new(*edge, 0.0, 0.0, 0.0).to_physical(scale).x0;
                assert_eq!(got, *want, "edge {edge} at scale {factor}");
            }
        }
    }

    #[test]
    fn physical_edges_are_contiguous() {
        let scale = scale(1.25);
        let bounds = [0.0f32, 10.0, 20.0, 33.5, 100.0];
        let rects: Vec<PhysicalRect> = bounds
            .windows(2)
            .map(|pair| Rect::new(pair[0], 0.0, pair[1] - pair[0], 10.0).to_physical(scale))
            .collect();

        let expected_x0 = [0, 13, 25, 42];
        for (rect, want) in rects.iter().zip(expected_x0.iter()) {
            assert_eq!(rect.x0, *want);
        }
        for pair in rects.windows(2) {
            assert_eq!(pair[0].x1, pair[1].x0, "shared edge must not leave a seam");
        }
        assert_eq!(rects[rects.len() - 1].x1, 125);
    }

    #[test]
    fn physical_negative_rounding() {
        let scale = scale(1.25);

        assert_eq!(Rect::new(-2.0, 0.0, 0.0, 0.0).to_physical(scale).x0, -3);
        assert_eq!(Rect::new(-10.5, 0.0, 0.0, 0.0).to_physical(scale).x0, -13);
    }

    #[test]
    fn physical_covering_superset() {
        let rects = [
            Rect::new(0.0, 0.0, 10.0, 10.0),
            Rect::new(10.5, 20.25, 0.25, 0.25),
            Rect::new(-3.25, 7.5, 16.75, 2.5),
        ];

        for factor in SCALES {
            let scale = scale(factor);
            for rect in rects {
                let exact = rect.to_physical(scale);
                let covering = rect.to_physical_covering(scale);

                assert!(covering.x0 <= exact.x0, "{rect:?} at {factor}");
                assert!(covering.y0 <= exact.y0, "{rect:?} at {factor}");
                assert!(covering.x1 >= exact.x1, "{rect:?} at {factor}");
                assert!(covering.y1 >= exact.y1, "{rect:?} at {factor}");
                assert!(exact.x0 - covering.x0 <= 1, "{rect:?} at {factor}");
                assert!(exact.y0 - covering.y0 <= 1, "{rect:?} at {factor}");
                assert!(covering.x1 - exact.x1 <= 1, "{rect:?} at {factor}");
                assert!(covering.y1 - exact.y1 <= 1, "{rect:?} at {factor}");
            }
        }
    }

    #[test]
    fn physical_covering_keeps_subpixel_visible() {
        let rect = Rect::new(10.5, 20.25, 0.25, 0.25);
        let scale = scale(1.0);

        assert!(rect.to_physical(scale).is_empty());
        assert_eq!(rect.to_physical(scale), PhysicalRect::new(11, 20, 11, 21));
        assert_eq!(
            rect.to_physical_covering(scale),
            PhysicalRect::new(10, 20, 11, 21)
        );
    }

    #[test]
    fn non_finite_inputs_are_empty() {
        let scale = scale(1.5);
        let bad = [
            Rect::new(f32::NAN, 0.0, 10.0, 10.0),
            Rect::new(0.0, f32::INFINITY, 10.0, 10.0),
            Rect::new(0.0, 0.0, f32::INFINITY, 10.0),
            Rect::new(0.0, 0.0, f32::NEG_INFINITY, 10.0),
        ];

        for rect in bad {
            assert!(rect.to_physical(scale).is_empty(), "{rect:?}");
            assert!(rect.to_physical_covering(scale).is_empty(), "{rect:?}");
            assert_eq!(rect.to_physical_f32(scale), [0.0; 4], "{rect:?}");
        }
    }

    #[test]
    fn negative_size_normalizes_to_empty() {
        let scale = scale(2.0);
        let bad = [
            Rect::new(0.0, 0.0, -10.0, 10.0),
            Rect::new(0.0, 0.0, 10.0, -10.0),
        ];

        for rect in bad {
            assert!(rect.to_physical(scale).is_empty(), "{rect:?}");
            assert!(rect.to_physical_covering(scale).is_empty(), "{rect:?}");
            assert_eq!(rect.to_physical_f32(scale), [0.0; 4], "{rect:?}");
        }
    }

    #[test]
    fn huge_coordinates_saturate() {
        let scale = scale(1.25);

        let huge = Rect::new(1e30, 1e30, 1e30, 1e30).to_physical(scale);
        assert_eq!(huge.x0, i32::MAX);
        assert_eq!(huge.x1, i32::MAX);
        assert!(huge.is_empty());
        assert_eq!(huge.width(), 0);

        let negative = Rect::new(-1e30, -1e30, 2e30, 2e30).to_physical(scale);
        assert!(negative.x0 <= negative.x1);
        assert!(negative.y0 <= negative.y1);
        assert_eq!(negative.x0, i32::MIN);
        assert_eq!(negative.x1, i32::MAX);
    }

    #[test]
    fn physical_rect_union_intersect() {
        let a = PhysicalRect::new(0, 0, 10, 10);
        let b = PhysicalRect::new(5, 5, 15, 15);

        assert_eq!(PhysicalRect::default().union(a), a);
        assert_eq!(a.union(b), PhysicalRect::new(0, 0, 15, 15));
        assert_eq!(a.intersect(b), PhysicalRect::new(5, 5, 10, 10));

        let far = PhysicalRect::new(100, 100, 110, 110);
        let empty_intersection = a.intersect(far);
        assert!(empty_intersection.is_empty());
        assert_eq!(empty_intersection, PhysicalRect::new(100, 100, 100, 100));
    }

    #[test]
    fn physical_rect_width_height_never_negative() {
        let empty = PhysicalRect::default();

        assert_eq!(empty.width(), 0);
        assert_eq!(empty.height(), 0);
        assert!(empty.is_empty());
    }

    #[test]
    fn physical_rect_saturated_width_height() {
        let saturated = PhysicalRect::new(i32::MIN, i32::MIN, i32::MAX, i32::MAX);

        assert_eq!(saturated.width(), i32::MAX);
        assert_eq!(saturated.height(), i32::MAX);
        assert!(!saturated.is_empty());
    }

    #[test]
    fn transform_identity_is_noop() {
        let point = Point::new(3.5, -7.25);

        assert_eq!(Transform::IDENTITY.apply(point), point);
        assert_eq!(
            Transform::IDENTITY.to_array(),
            [1.0, 0.0, 0.0, 1.0, 0.0, 0.0]
        );
    }

    #[test]
    fn transform_translate_scale_apply() {
        assert_eq!(
            Transform::translate(3.0, 4.0).apply(Point::new(1.0, 1.0)),
            Point::new(4.0, 5.0)
        );
        assert_eq!(
            Transform::scale(2.0, 3.0).apply(Point::new(1.0, 2.0)),
            Point::new(2.0, 6.0)
        );
    }

    #[test]
    fn transform_then_is_apply_then_apply() {
        let first = Transform::translate(3.0, 4.0);
        let second = Transform::scale(2.0, 3.0);
        let composed = first.then(second);
        let point = Point::new(1.0, 2.0);

        assert_eq!(composed.apply(point), second.apply(first.apply(point)));
    }

    #[test]
    fn transform_to_array_order() {
        let composed = Transform::translate(3.0, 4.0).then(Transform::scale(2.0, 3.0));

        assert_eq!(composed.to_array(), [2.0, 0.0, 0.0, 3.0, 6.0, 12.0]);
        assert_eq!(
            composed.apply(Point::new(1.0, 2.0)),
            Transform::scale(2.0, 3.0).apply(Point::new(4.0, 6.0))
        );
    }

    #[test]
    fn to_physical_f32_positive_values() {
        let subpixel = Rect::new(10.5, 20.25, 0.25, 0.25);

        assert_eq!(
            subpixel.to_physical_f32(scale(1.0)),
            [10.5, 20.25, 0.25, 0.25]
        );
        assert_eq!(
            subpixel.to_physical_f32(scale(1.25)),
            [13.125, 25.3125, 0.3125, 0.3125]
        );
        assert_eq!(
            subpixel.to_physical_f32(scale(1.5)),
            [15.75, 30.375, 0.375, 0.375]
        );

        let asymmetric = Rect::from_origin_size(Point::new(3.5, 7.25), Size::new(20.0, 11.5));
        assert_eq!(
            asymmetric.to_physical_f32(scale(1.5)),
            [5.25, 10.875, 30.0, 17.25]
        );
    }

    #[test]
    fn to_physical_f32_overflow_is_empty() {
        // 两个有限端点相减溢出为 +inf：浮点路径必须返回空，整型路径按饱和语义处理。
        let overflow = Rect::new(-9.191715e37, 0.0, f32::MAX, 10.0);
        let scale = scale(1.0);

        assert_eq!(overflow.to_physical_f32(scale), [0.0; 4]);
        assert_eq!(
            overflow.to_physical(scale),
            PhysicalRect::new(i32::MIN, 0, i32::MAX, 10)
        );
        assert_eq!(
            overflow.to_physical_covering(scale),
            PhysicalRect::new(i32::MIN, 0, i32::MAX, 10)
        );
    }

    #[test]
    fn to_physical_covering_subpixel_table() {
        let subpixel = Rect::new(10.5, 20.25, 0.25, 0.25);

        assert_eq!(
            subpixel.to_physical_covering(scale(1.0)),
            PhysicalRect::new(10, 20, 11, 21)
        );
        assert_eq!(
            subpixel.to_physical_covering(scale(1.25)),
            PhysicalRect::new(13, 25, 14, 26)
        );
        assert_eq!(
            subpixel.to_physical_covering(scale(1.5)),
            PhysicalRect::new(15, 30, 17, 31)
        );
        assert_eq!(
            subpixel.to_physical_covering(scale(2.0)),
            PhysicalRect::new(21, 40, 22, 41)
        );
        assert_eq!(
            subpixel.to_physical_covering(scale(3.0)),
            PhysicalRect::new(31, 60, 33, 62)
        );
    }

    #[test]
    fn covering_edges_differ_from_round() {
        let rect = Rect::new(1.3, 0.0, 0.4, 1.0);
        let scale = scale(2.0);

        // x0 = 2.6 → floor 2 / round 3；x1 = 3.4 → ceil 4 / round 3。
        assert_eq!(rect.to_physical(scale), PhysicalRect::new(3, 0, 3, 2));
        assert_eq!(
            rect.to_physical_covering(scale),
            PhysicalRect::new(2, 0, 4, 2)
        );
    }

    #[test]
    fn rect_inset_asymmetric_edges() {
        // CSS 顺序 (top, right, bottom, left)。
        assert_eq!(
            Rect::new(0.0, 0.0, 10.0, 10.0).inset(Edges::new(1.0, 2.0, 3.0, 4.0)),
            Rect::new(4.0, 1.0, 4.0, 6.0)
        );
        // 等价几何（left=1, top=2, right=3, bottom=4）：left/top 在前才得到 (1, 2, 6, 4)。
        assert_eq!(
            Rect::new(0.0, 0.0, 10.0, 10.0).inset(Edges::new(2.0, 3.0, 4.0, 1.0)),
            Rect::new(1.0, 2.0, 6.0, 4.0)
        );
    }

    #[test]
    fn rect_outset_exact_values() {
        let rect = Rect::from_origin_size(Point::ZERO, Size::new(10.0, 10.0));

        assert_eq!(rect.outset(2.0), Rect::new(-2.0, -2.0, 14.0, 14.0));
    }

    #[test]
    fn size_is_empty_non_finite() {
        assert!(Size::new(f32::INFINITY, 4.0).is_empty());
        assert!(Size::new(4.0, f32::NEG_INFINITY).is_empty());
        assert!(Size::new(f32::NAN, 4.0).is_empty());
        assert!(!Size::new(4.0, 4.0).is_empty());
    }
}
