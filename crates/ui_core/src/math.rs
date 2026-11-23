use std::ops::{Add, AddAssign, Div, Mul, Neg, Sub, SubAssign};

const I32_MIN_F: f32 = i32::MIN as f32;
const I32_MAX_F: f32 = i32::MAX as f32;

macro_rules! float_length {
    ($name:ident) => {
        #[repr(transparent)]
        #[derive(Clone, Copy, Debug, Default, PartialEq, PartialOrd)]
        pub struct $name(f32);

        impl $name {
            pub const ZERO: Self = Self(0.0);

            pub const fn new(value: f32) -> Self {
                Self(value)
            }

            pub const fn value(self) -> f32 {
                self.0
            }

            #[must_use]
            pub fn is_finite(self) -> bool {
                self.0.is_finite()
            }

            #[must_use]
            pub fn min(self, other: Self) -> Self {
                Self(self.0.min(other.0))
            }

            #[must_use]
            pub fn max(self, other: Self) -> Self {
                Self(self.0.max(other.0))
            }

            #[must_use]
            pub fn abs(self) -> Self {
                Self(self.0.abs())
            }
        }

        impl Add for $name {
            type Output = Self;

            fn add(self, rhs: Self) -> Self {
                Self(self.0 + rhs.0)
            }
        }

        impl Add<f32> for $name {
            type Output = Self;

            fn add(self, rhs: f32) -> Self {
                Self(self.0 + rhs)
            }
        }

        impl AddAssign for $name {
            fn add_assign(&mut self, rhs: Self) {
                self.0 += rhs.0;
            }
        }

        impl Sub for $name {
            type Output = Self;

            fn sub(self, rhs: Self) -> Self {
                Self(self.0 - rhs.0)
            }
        }

        impl Sub<f32> for $name {
            type Output = Self;

            fn sub(self, rhs: f32) -> Self {
                Self(self.0 - rhs)
            }
        }

        impl SubAssign for $name {
            fn sub_assign(&mut self, rhs: Self) {
                self.0 -= rhs.0;
            }
        }

        impl Mul<f32> for $name {
            type Output = Self;

            fn mul(self, rhs: f32) -> Self {
                Self(self.0 * rhs)
            }
        }

        impl Div<f32> for $name {
            type Output = Self;

            fn div(self, rhs: f32) -> Self {
                Self(self.0 / rhs)
            }
        }

        impl Div for $name {
            type Output = f32;

            fn div(self, rhs: Self) -> f32 {
                self.0 / rhs.0
            }
        }

        impl Neg for $name {
            type Output = Self;

            fn neg(self) -> Self {
                Self(-self.0)
            }
        }

        impl From<$name> for f32 {
            fn from(value: $name) -> f32 {
                value.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

float_length!(LogicalPx);

impl LogicalPx {
    #[must_use]
    pub fn clamp(self, min: Self, max: Self) -> Self {
        Self(self.0.clamp(min.0, max.0))
    }
}

float_length!(PhysicalPx);

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DevicePx(i32);

impl DevicePx {
    pub const ZERO: Self = Self(0);

    pub const fn new(value: i32) -> Self {
        Self(value)
    }

    pub const fn value(self) -> i32 {
        self.0
    }

    #[must_use]
    pub fn min(self, other: Self) -> Self {
        Self(self.0.min(other.0))
    }

    #[must_use]
    pub fn max(self, other: Self) -> Self {
        Self(self.0.max(other.0))
    }
}

impl Add for DevicePx {
    type Output = Self;

    fn add(self, rhs: Self) -> Self {
        Self(self.0.saturating_add(rhs.0))
    }
}

impl Add<i32> for DevicePx {
    type Output = Self;

    fn add(self, rhs: i32) -> Self {
        Self(self.0.saturating_add(rhs))
    }
}

impl AddAssign for DevicePx {
    fn add_assign(&mut self, rhs: Self) {
        self.0 = self.0.saturating_add(rhs.0);
    }
}

impl Sub for DevicePx {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self {
        Self(self.0.saturating_sub(rhs.0))
    }
}

impl Sub<i32> for DevicePx {
    type Output = Self;

    fn sub(self, rhs: i32) -> Self {
        Self(self.0.saturating_sub(rhs))
    }
}

impl SubAssign for DevicePx {
    fn sub_assign(&mut self, rhs: Self) {
        self.0 = self.0.saturating_sub(rhs.0);
    }
}

impl Neg for DevicePx {
    type Output = Self;

    fn neg(self) -> Self {
        Self(self.0.saturating_neg())
    }
}

impl PartialEq<i32> for DevicePx {
    fn eq(&self, other: &i32) -> bool {
        self.0 == *other
    }
}

impl PartialOrd<i32> for DevicePx {
    fn partial_cmp(&self, other: &i32) -> Option<std::cmp::Ordering> {
        self.0.partial_cmp(other)
    }
}

impl From<DevicePx> for i32 {
    fn from(value: DevicePx) -> i32 {
        value.0
    }
}

impl From<DevicePx> for f32 {
    fn from(value: DevicePx) -> f32 {
        value.0 as f32
    }
}

impl std::fmt::Display for DevicePx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[must_use]
pub const fn px(value: f32) -> LogicalPx {
    LogicalPx::new(value)
}

#[must_use]
pub const fn physical_px(value: f32) -> PhysicalPx {
    PhysicalPx::new(value)
}

#[must_use]
pub const fn dp(value: i32) -> DevicePx {
    DevicePx::new(value)
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point<T> {
    pub x: T,
    pub y: T,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Size<T> {
    pub width: T,
    pub height: T,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect<T> {
    pub origin: Point<T>,
    pub size: Size<T>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Edges<T> {
    pub top: T,
    pub right: T,
    pub bottom: T,
    pub left: T,
}

pub type LogicalPoint = Point<LogicalPx>;

pub type LogicalSize = Size<LogicalPx>;

pub type LogicalRect = Rect<LogicalPx>;

pub type PhysicalPoint = Point<PhysicalPx>;

pub type PhysicalSize = Size<PhysicalPx>;

pub type PhysicalRect = Rect<PhysicalPx>;

pub type DevicePoint = Point<DevicePx>;

pub type DeviceSize = Size<DevicePx>;

pub type LogicalEdges = Edges<LogicalPx>;

macro_rules! point_size {
    ($t:ident) => {
        impl Point<$t> {
            pub const ZERO: Self = Self {
                x: $t::ZERO,
                y: $t::ZERO,
            };

            pub const fn new(x: $t, y: $t) -> Self {
                Self { x, y }
            }
        }

        impl Add for Point<$t> {
            type Output = Self;

            fn add(self, rhs: Self) -> Self {
                Self {
                    x: self.x + rhs.x,
                    y: self.y + rhs.y,
                }
            }
        }

        impl Sub for Point<$t> {
            type Output = Self;

            fn sub(self, rhs: Self) -> Self {
                Self {
                    x: self.x - rhs.x,
                    y: self.y - rhs.y,
                }
            }
        }

        impl AddAssign for Point<$t> {
            fn add_assign(&mut self, rhs: Self) {
                self.x += rhs.x;
                self.y += rhs.y;
            }
        }

        impl SubAssign for Point<$t> {
            fn sub_assign(&mut self, rhs: Self) {
                self.x -= rhs.x;
                self.y -= rhs.y;
            }
        }

        impl Size<$t> {
            pub const ZERO: Self = Self {
                width: $t::ZERO,
                height: $t::ZERO,
            };

            pub const fn new(width: $t, height: $t) -> Self {
                Self { width, height }
            }
        }

        impl Add for Size<$t> {
            type Output = Self;

            fn add(self, rhs: Self) -> Self {
                Self {
                    width: self.width + rhs.width,
                    height: self.height + rhs.height,
                }
            }
        }

        impl Sub for Size<$t> {
            type Output = Self;

            fn sub(self, rhs: Self) -> Self {
                Self {
                    width: self.width - rhs.width,
                    height: self.height - rhs.height,
                }
            }
        }
    };
}

macro_rules! point_size_scaled {
    ($t:ident) => {
        impl Point<$t> {
            #[must_use]
            pub fn is_finite(self) -> bool {
                self.x.is_finite() && self.y.is_finite()
            }
        }

        impl Size<$t> {
            #[must_use]
            pub fn is_empty(self) -> bool {
                !self.width.is_finite()
                    || !self.height.is_finite()
                    || self.width <= $t::ZERO
                    || self.height <= $t::ZERO
            }

            #[must_use]
            pub fn is_finite(self) -> bool {
                self.width.is_finite() && self.height.is_finite()
            }
        }

        impl Mul<f32> for Point<$t> {
            type Output = Self;

            fn mul(self, rhs: f32) -> Self {
                Self {
                    x: self.x * rhs,
                    y: self.y * rhs,
                }
            }
        }

        impl Div<f32> for Point<$t> {
            type Output = Self;

            fn div(self, rhs: f32) -> Self {
                Self {
                    x: self.x / rhs,
                    y: self.y / rhs,
                }
            }
        }

        impl Neg for Point<$t> {
            type Output = Self;

            fn neg(self) -> Self {
                Self {
                    x: -self.x,
                    y: -self.y,
                }
            }
        }

        impl Mul<f32> for Size<$t> {
            type Output = Self;

            fn mul(self, rhs: f32) -> Self {
                Self {
                    width: self.width * rhs,
                    height: self.height * rhs,
                }
            }
        }

        impl Div<f32> for Size<$t> {
            type Output = Self;

            fn div(self, rhs: f32) -> Self {
                Self {
                    width: self.width / rhs,
                    height: self.height / rhs,
                }
            }
        }
    };
}

macro_rules! rect_edges {
    ($t:ident) => {
        impl Rect<$t> {
            pub const fn new(x: $t, y: $t, width: $t, height: $t) -> Self {
                Self {
                    origin: Point { x, y },
                    size: Size { width, height },
                }
            }

            pub const fn from_origin_size(origin: Point<$t>, size: Size<$t>) -> Self {
                Self { origin, size }
            }

            #[must_use]
            pub fn min(self) -> Point<$t> {
                self.origin
            }

            #[must_use]
            pub fn max(self) -> Point<$t> {
                Point {
                    x: self.origin.x + self.size.width,
                    y: self.origin.y + self.size.height,
                }
            }

            #[must_use]
            pub fn is_empty(self) -> bool {
                self.size.is_empty() || !self.origin.is_finite()
            }

            #[must_use]
            pub fn is_finite(self) -> bool {
                self.origin.is_finite() && self.size.is_finite()
            }

            #[must_use]
            pub fn contains(self, point: Point<$t>) -> bool {
                let max = self.max();
                !self.is_empty()
                    && point.x >= self.origin.x
                    && point.x < max.x
                    && point.y >= self.origin.y
                    && point.y < max.y
            }

            #[must_use]
            pub fn translate(self, by: Point<$t>) -> Self {
                Self {
                    origin: self.origin + by,
                    size: self.size,
                }
            }

            #[must_use]
            pub fn inset(self, edges: Edges<$t>) -> Self {
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

            #[must_use]
            pub fn outset(self, amount: $t) -> Self {
                self.inset(Edges::<$t>::uniform(-amount))
            }

            #[must_use]
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

            #[must_use]
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
                        width: (x1 - x0).max($t::ZERO),
                        height: (y1 - y0).max($t::ZERO),
                    },
                }
            }

            #[must_use]
            pub fn intersects(self, other: Self) -> bool {
                !self.intersect(other).is_empty()
            }

            #[must_use]
            pub fn is_convertible(self) -> bool {
                self.origin.is_finite()
                    && self.size.is_finite()
                    && self.size.width >= $t::ZERO
                    && self.size.height >= $t::ZERO
            }
        }

        impl Edges<$t> {
            pub const ZERO: Self = Self {
                top: $t::ZERO,
                right: $t::ZERO,
                bottom: $t::ZERO,
                left: $t::ZERO,
            };

            pub const fn new(top: $t, right: $t, bottom: $t, left: $t) -> Self {
                Self {
                    top,
                    right,
                    bottom,
                    left,
                }
            }

            pub const fn uniform(value: $t) -> Self {
                Self {
                    top: value,
                    right: value,
                    bottom: value,
                    left: value,
                }
            }

            #[must_use]
            pub fn is_finite(self) -> bool {
                self.top.is_finite()
                    && self.right.is_finite()
                    && self.bottom.is_finite()
                    && self.left.is_finite()
            }
        }

        impl Add for Edges<$t> {
            type Output = Self;

            fn add(self, rhs: Self) -> Self {
                Self {
                    top: self.top + rhs.top,
                    right: self.right + rhs.right,
                    bottom: self.bottom + rhs.bottom,
                    left: self.left + rhs.left,
                }
            }
        }
    };
}

point_size!(LogicalPx);

point_size_scaled!(LogicalPx);

rect_edges!(LogicalPx);

point_size!(PhysicalPx);

point_size_scaled!(PhysicalPx);

rect_edges!(PhysicalPx);

point_size!(DevicePx);

impl Size<LogicalPx> {
    #[must_use]
    pub fn area(self) -> f32 {
        self.width.value() * self.height.value()
    }
}

impl Size<DevicePx> {
    #[must_use]
    pub fn area(self) -> i64 {
        i64::from(self.width.value()) * i64::from(self.height.value())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceRect {
    pub x0: DevicePx,
    pub y0: DevicePx,
    pub x1: DevicePx,
    pub y1: DevicePx,
}

impl DeviceRect {
    #[must_use]
    pub fn new(x0: DevicePx, y0: DevicePx, x1: DevicePx, y1: DevicePx) -> Self {
        debug_assert!(x0 <= x1 && y0 <= y1, "DeviceRect must be ordered");

        Self { x0, y0, x1, y1 }
    }

    #[must_use]
    pub fn from_edges(x0: DevicePx, y0: DevicePx, x1: DevicePx, y1: DevicePx) -> Self {
        Self {
            x0,
            y0,
            x1: x1.max(x0),
            y1: y1.max(y0),
        }
    }

    #[must_use]
    pub fn from_origin_size(origin: DevicePoint, size: DeviceSize) -> Self {
        Self {
            x0: origin.x,
            y0: origin.y,
            x1: origin.x + size.width,
            y1: origin.y + size.height,
        }
    }

    #[must_use]
    pub fn width(self) -> DevicePx {
        debug_assert!(self.x1 >= self.x0 && self.y1 >= self.y0);

        self.x1 - self.x0
    }

    #[must_use]
    pub fn height(self) -> DevicePx {
        debug_assert!(self.x1 >= self.x0 && self.y1 >= self.y0);

        self.y1 - self.y0
    }

    #[must_use]
    pub fn size(self) -> DeviceSize {
        DeviceSize {
            width: self.width(),
            height: self.height(),
        }
    }

    #[must_use]
    pub fn is_empty(self) -> bool {
        self.width() <= DevicePx::ZERO || self.height() <= DevicePx::ZERO
    }

    #[must_use]
    pub fn area(self) -> i64 {
        self.size().area()
    }

    #[must_use]
    pub fn union(self, other: Self) -> Self {
        if self == Self::default() {
            return other;
        }
        if other == Self::default() {
            return self;
        }

        Self {
            x0: self.x0.min(other.x0),
            y0: self.y0.min(other.y0),
            x1: self.x1.max(other.x1),
            y1: self.y1.max(other.y1),
        }
    }

    #[must_use]
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

    #[must_use]
    pub fn overlaps_inclusive(self, other: Self) -> bool {
        self.x0 <= other.x1 && other.x0 <= self.x1 && self.y0 <= other.y1 && other.y0 <= self.y1
    }

    #[must_use]
    pub fn translate(self, by: DevicePoint) -> Self {
        Self {
            x0: self.x0 + by.x,
            y0: self.y0 + by.y,
            x1: self.x1 + by.x,
            y1: self.y1 + by.y,
        }
    }
}

impl Default for DeviceRect {
    fn default() -> Self {
        Self {
            x0: DevicePx::ZERO,
            y0: DevicePx::ZERO,
            x1: DevicePx::ZERO,
            y1: DevicePx::ZERO,
        }
    }
}

impl PhysicalRect {
    #[must_use]
    pub fn to_aabb(self) -> [f32; 4] {
        [
            self.origin.x.value(),
            self.origin.y.value(),
            self.size.width.value(),
            self.size.height.value(),
        ]
    }
}

macro_rules! corner_radius {
    ($name:ident, $t:ident) => {
        #[must_use]
        pub fn $name(radii: [$t; 4], width: $t, height: $t) -> [$t; 4] {
            let width_value = width.value();
            let height_value = height.value();
            if width_value.is_nan()
                || height_value.is_nan()
                || !width_value.is_finite()
                || !height_value.is_finite()
            {
                return [$t::ZERO; 4];
            }
            if width_value <= 0.0 || height_value <= 0.0 {
                return [$t::ZERO; 4];
            }
            let sanitized = radii.map(|radius| {
                let value = radius.value();
                $t::new(if value.is_nan() { 0.0 } else { value.max(0.0) })
            });
            let mut scale = 1.0f32;
            for (side, sum) in [
                (width_value, sanitized[0].value() + sanitized[1].value()),
                (width_value, sanitized[3].value() + sanitized[2].value()),
                (height_value, sanitized[0].value() + sanitized[3].value()),
                (height_value, sanitized[1].value() + sanitized[2].value()),
            ] {
                if sum > side {
                    scale = scale.min(side / sum);
                }
            }
            let half_min = 0.5 * width_value.min(height_value);
            sanitized.map(|radius| $t::new((radius.value() * scale).min(half_min)))
        }
    };
}

corner_radius!(normalize_logical_corner_radius, LogicalPx);

corner_radius!(normalize_physical_corner_radius, PhysicalPx);

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DevicePixelRatio(f32);

impl DevicePixelRatio {
    pub const ONE: Self = Self(1.0);

    #[must_use]
    pub fn new(factor: f32) -> Option<Self> {
        (factor.is_finite() && factor > 0.0).then_some(Self(factor))
    }

    #[must_use]
    pub const fn factor(self) -> f32 {
        self.0
    }

    #[must_use]
    pub fn to_physical(self, value: LogicalPx) -> PhysicalPx {
        PhysicalPx::new(value.value() * self.0)
    }

    #[must_use]
    pub fn to_logical(self, value: PhysicalPx) -> LogicalPx {
        LogicalPx::new(value.value() / self.0)
    }

    #[must_use]
    pub fn to_physical_size(self, size: LogicalSize) -> PhysicalSize {
        PhysicalSize {
            width: self.to_physical(size.width),
            height: self.to_physical(size.height),
        }
    }

    #[must_use]
    pub fn to_physical_rect(self, rect: LogicalRect) -> PhysicalRect {
        if !rect.is_convertible() {
            return PhysicalRect::default();
        }
        let factor = self.0;
        let x0 = rect.origin.x.value() * factor;
        let y0 = rect.origin.y.value() * factor;
        let x1 = (rect.origin.x.value() + rect.size.width.value()) * factor;
        let y1 = (rect.origin.y.value() + rect.size.height.value()) * factor;
        if ![x0, y0, x1, y1].iter().all(|value| value.is_finite()) {
            return PhysicalRect::default();
        }
        let width = x1 - x0;
        let height = y1 - y0;
        if !width.is_finite() || !height.is_finite() {
            return PhysicalRect::default();
        }
        PhysicalRect::from_origin_size(
            PhysicalPoint {
                x: PhysicalPx::new(x0),
                y: PhysicalPx::new(y0),
            },
            PhysicalSize {
                width: PhysicalPx::new(width),
                height: PhysicalPx::new(height),
            },
        )
    }

    #[must_use]
    pub fn to_device_rect(self, rect: LogicalRect) -> DeviceRect {
        if !rect.is_convertible() {
            return DeviceRect::default();
        }
        let factor = self.0;
        let x0 = rect.origin.x.value() * factor;
        let y0 = rect.origin.y.value() * factor;
        let x1 = (rect.origin.x.value() + rect.size.width.value()) * factor;
        let y1 = (rect.origin.y.value() + rect.size.height.value()) * factor;
        DeviceRect::from_edges(
            round_device(x0),
            round_device(y0),
            round_device(x1),
            round_device(y1),
        )
    }

    #[must_use]
    pub fn to_covering_device_rect(self, rect: LogicalRect) -> DeviceRect {
        if !rect.is_convertible() {
            return DeviceRect::default();
        }
        let factor = self.0;
        let x0 = rect.origin.x.value() * factor;
        let y0 = rect.origin.y.value() * factor;
        let x1 = (rect.origin.x.value() + rect.size.width.value()) * factor;
        let y1 = (rect.origin.y.value() + rect.size.height.value()) * factor;
        DeviceRect::from_edges(
            floor_device(x0),
            floor_device(y0),
            ceil_device(x1),
            ceil_device(y1),
        )
    }

    #[must_use]
    pub fn to_device_point(self, point: LogicalPoint) -> DevicePoint {
        DevicePoint {
            x: round_device(point.x.value() * self.0),
            y: round_device(point.y.value() * self.0),
        }
    }

    #[must_use]
    pub fn to_device_size(self, size: LogicalSize) -> DeviceSize {
        DeviceSize {
            width: round_device(size.width.value() * self.0),
            height: round_device(size.height.value() * self.0),
        }
    }

    #[must_use]
    pub fn to_logical_point(self, point: DevicePoint) -> LogicalPoint {
        LogicalPoint {
            x: LogicalPx::new(point.x.value() as f32 / self.0),
            y: LogicalPx::new(point.y.value() as f32 / self.0),
        }
    }

    #[must_use]
    pub fn to_logical_size(self, size: DeviceSize) -> LogicalSize {
        LogicalSize {
            width: LogicalPx::new(size.width.value() as f32 / self.0),
            height: LogicalPx::new(size.height.value() as f32 / self.0),
        }
    }

    #[must_use]
    pub fn to_logical_rect(self, rect: DeviceRect) -> LogicalRect {
        LogicalRect::from_origin_size(
            LogicalPoint {
                x: LogicalPx::new(rect.x0.value() as f32 / self.0),
                y: LogicalPx::new(rect.y0.value() as f32 / self.0),
            },
            LogicalSize {
                width: LogicalPx::new(rect.width().value() as f32 / self.0),
                height: LogicalPx::new(rect.height().value() as f32 / self.0),
            },
        )
    }

    #[must_use]
    pub fn stroke_width(self, width: LogicalPx) -> PhysicalPx {
        let width = width.value();
        if !width.is_finite() || width <= 0.0 {
            return PhysicalPx::ZERO;
        }
        let physical = width * self.0;
        if !physical.is_finite() {
            return PhysicalPx::ZERO;
        }
        PhysicalPx::new(round_half_up(physical).max(1.0))
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Transform {
    m11: f32,
    m12: f32,
    m21: f32,
    m22: f32,
    tx: LogicalPx,
    ty: LogicalPx,
}

impl Transform {
    pub const IDENTITY: Self = Self {
        m11: 1.0,
        m12: 0.0,
        m21: 0.0,
        m22: 1.0,
        tx: LogicalPx::ZERO,
        ty: LogicalPx::ZERO,
    };

    pub const fn translate(tx: LogicalPx, ty: LogicalPx) -> Self {
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
            tx: LogicalPx::ZERO,
            ty: LogicalPx::ZERO,
        }
    }

    #[must_use]
    pub fn rotate(radians: f32) -> Self {
        let (sin, cos) = radians.sin_cos();
        Self {
            m11: cos,
            m12: sin,
            m21: -sin,
            m22: cos,
            tx: LogicalPx::ZERO,
            ty: LogicalPx::ZERO,
        }
    }

    #[must_use]
    pub fn then(self, next: Self) -> Self {
        Self {
            m11: next.m11 * self.m11 + next.m21 * self.m12,
            m12: next.m12 * self.m11 + next.m22 * self.m12,
            m21: next.m11 * self.m21 + next.m21 * self.m22,
            m22: next.m12 * self.m21 + next.m22 * self.m22,
            tx: LogicalPx::new(
                next.m11 * self.tx.value() + next.m21 * self.ty.value() + next.tx.value(),
            ),
            ty: LogicalPx::new(
                next.m12 * self.tx.value() + next.m22 * self.ty.value() + next.ty.value(),
            ),
        }
    }

    #[must_use]
    pub fn apply(self, point: LogicalPoint) -> LogicalPoint {
        LogicalPoint {
            x: LogicalPx::new(self.m11 * point.x.value() + self.m21 * point.y.value()) + self.tx,
            y: LogicalPx::new(self.m12 * point.x.value() + self.m22 * point.y.value()) + self.ty,
        }
    }

    #[must_use]
    pub fn inverse(self) -> Option<Self> {
        let det = self.m11 * self.m22 - self.m12 * self.m21;
        let norm = self
            .m11
            .abs()
            .max(self.m12.abs())
            .max(self.m21.abs())
            .max(self.m22.abs());
        if !det.is_finite() || !norm.is_finite() {
            return None;
        }
        if norm == 0.0 || det.abs() <= f32::EPSILON * norm * norm {
            return None;
        }
        let inv = 1.0 / det;
        Some(Self {
            m11: self.m22 * inv,
            m12: -self.m12 * inv,
            m21: -self.m21 * inv,
            m22: self.m11 * inv,
            tx: LogicalPx::new((self.m21 * self.ty.value() - self.m22 * self.tx.value()) * inv),
            ty: LogicalPx::new((self.m12 * self.tx.value() - self.m11 * self.ty.value()) * inv),
        })
    }

    #[must_use]
    pub fn is_invertible(self) -> bool {
        self.inverse().is_some()
    }

    #[must_use]
    pub fn transform_rect(self, rect: LogicalRect) -> LogicalRect {
        let corners = [
            self.apply(rect.min()),
            self.apply(LogicalPoint::new(rect.max().x, rect.min().y)),
            self.apply(rect.max()),
            self.apply(LogicalPoint::new(rect.min().x, rect.max().y)),
        ];
        if corners.iter().any(|corner| !corner.is_finite()) {
            return LogicalRect::default();
        }
        let x0 = corners
            .iter()
            .map(|corner| corner.x)
            .fold(LogicalPx::new(f32::INFINITY), LogicalPx::min);
        let y0 = corners
            .iter()
            .map(|corner| corner.y)
            .fold(LogicalPx::new(f32::INFINITY), LogicalPx::min);
        let x1 = corners
            .iter()
            .map(|corner| corner.x)
            .fold(LogicalPx::new(f32::NEG_INFINITY), LogicalPx::max);
        let y1 = corners
            .iter()
            .map(|corner| corner.y)
            .fold(LogicalPx::new(f32::NEG_INFINITY), LogicalPx::max);
        LogicalRect::new(x0, y0, x1 - x0, y1 - y0)
    }

    #[must_use]
    pub fn to_array(self) -> [f32; 6] {
        [
            self.m11,
            self.m12,
            self.m21,
            self.m22,
            self.tx.value(),
            self.ty.value(),
        ]
    }
}

fn saturate(value: f32) -> f32 {
    value.clamp(I32_MIN_F, I32_MAX_F)
}

fn round_half_up(value: f32) -> f32 {
    let shifted = value + 0.5;
    if shifted.is_finite() {
        shifted.floor()
    } else {
        value
    }
}

fn saturate_i32(value: f32) -> f32 {
    if value.is_nan() { 0.0 } else { saturate(value) }
}

fn round_device(value: f32) -> DevicePx {
    DevicePx::new(saturate_i32(value).round() as i32)
}

fn floor_device(value: f32) -> DevicePx {
    DevicePx::new(saturate_i32(value).floor() as i32)
}

fn ceil_device(value: f32) -> DevicePx {
    DevicePx::new(saturate_i32(value).ceil() as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FACTORS: [f32; 6] = [1.0, 1.25, 1.5, 1.75, 2.0, 3.0];

    const EDGES: [f32; 14] = [
        -10.5, -2.0, -1.0, 0.0, 0.5, 1.0, 2.0, 3.0, 4.0, 6.0, 8.0, 10.0, 10.5, 33.5,
    ];

    const ROUNDED_EDGES: [[i32; 14]; 6] = [
        [-11, -2, -1, 0, 1, 1, 2, 3, 4, 6, 8, 10, 11, 34],
        [-13, -3, -1, 0, 1, 1, 3, 4, 5, 8, 10, 13, 13, 42],
        [-16, -3, -2, 0, 1, 2, 3, 5, 6, 9, 12, 15, 16, 50],
        [-18, -4, -2, 0, 1, 2, 4, 5, 7, 11, 14, 18, 18, 59],
        [-21, -4, -2, 0, 1, 2, 4, 6, 8, 12, 16, 20, 21, 67],
        [-32, -6, -3, 0, 2, 3, 6, 9, 12, 18, 24, 30, 32, 101],
    ];

    fn dpr(factor: f32) -> DevicePixelRatio {
        DevicePixelRatio::new(factor).expect("test dpr must be valid")
    }

    fn rect(x: f32, y: f32, width: f32, height: f32) -> LogicalRect {
        LogicalRect::new(px(x), px(y), px(width), px(height))
    }

    fn device(x0: i32, y0: i32, x1: i32, y1: i32) -> DeviceRect {
        DeviceRect::new(dp(x0), dp(y0), dp(x1), dp(y1))
    }

    #[test]
    fn unit_arithmetic() {
        assert_eq!(px(1.5) + px(0.5), px(2.0));
        assert_eq!(px(1.5) - px(0.5), px(1.0));
        assert_eq!(px(1.5) * 2.0, px(3.0));
        assert_eq!(px(3.0) / 2.0, px(1.5));
        assert_eq!(-px(2.0), px(-2.0));
        assert!(px(1.0) < px(2.0));
        assert!(px(2.0) > px(1.999));
        assert_eq!(f32::from(px(3.5)), 3.5);
        assert_eq!(px(2.0) / px(4.0), 0.5);
        assert_eq!(physical_px(3.0) / physical_px(1.5), 2.0);

        let mut value = px(1.0);
        value += px(2.0);
        assert_eq!(value, px(3.0));
        value -= px(1.0);
        assert_eq!(value, px(2.0));
    }

    #[test]
    fn device_px_arithmetic_saturates() {
        assert_eq!(dp(1) + dp(2), dp(3));
        assert_eq!(dp(1) - dp(2), dp(-1));
        assert_eq!(dp(i32::MAX) + dp(1), dp(i32::MAX));
        assert_eq!(dp(i32::MIN) - dp(1), dp(i32::MIN));
        assert_eq!(-dp(i32::MIN), dp(i32::MAX));
        assert_eq!(dp(2) + 3, dp(5));
        assert_eq!(dp(2) - 3, dp(-1));
        assert!(dp(4) == 4);
        assert!(dp(4) > 3);
        assert_eq!(i32::from(dp(7)), 7);
        assert_eq!(f32::from(dp(7)), 7.0);
    }

    #[test]
    fn point_size_rect_construction() {
        let point = LogicalPoint::new(px(1.5), px(2.5));
        let size = LogicalSize::new(px(3.0), px(4.0));
        let bounds = LogicalRect::from_origin_size(point, size);

        assert_eq!(bounds.min(), point);
        assert_eq!(bounds.max(), LogicalPoint::new(px(4.5), px(6.5)));
        assert_eq!(size.area(), 12.0);
        assert_eq!(LogicalSize::new(px(0.0), px(4.0)).area(), 0.0);
        assert_eq!(DeviceSize::new(dp(4), dp(5)).area(), 20);
    }

    #[test]
    fn size_is_empty_for_non_positive_or_non_finite() {
        assert!(LogicalSize::new(px(0.0), px(4.0)).is_empty());
        assert!(LogicalSize::new(px(-1.0), px(4.0)).is_empty());
        assert!(LogicalSize::new(px(f32::NAN), px(4.0)).is_empty());
        assert!(LogicalSize::new(px(f32::INFINITY), px(4.0)).is_empty());
        assert!(!LogicalSize::new(px(0.5), px(0.5)).is_empty());
    }

    #[test]
    fn rect_is_empty_and_contains_half_open() {
        assert!(rect(0.0, 0.0, 0.0, 10.0).is_empty());
        assert!(rect(0.0, 0.0, -10.0, 10.0).is_empty());
        assert!(rect(f32::NAN, 0.0, 10.0, 10.0).is_empty());

        let bounds = rect(0.0, 0.0, 10.0, 10.0);
        assert!(bounds.contains(LogicalPoint::new(px(0.0), px(0.0))));
        assert!(bounds.contains(LogicalPoint::new(px(9.999), px(5.0))));
        assert!(!bounds.contains(LogicalPoint::new(px(10.0), px(5.0))));
        assert!(!bounds.contains(LogicalPoint::new(px(5.0), px(10.0))));
        assert!(!LogicalRect::default().contains(LogicalPoint::ZERO));
    }

    #[test]
    fn rect_translate_inset_outset() {
        let bounds = rect(0.0, 0.0, 10.0, 10.0);

        assert_eq!(
            bounds.translate(LogicalPoint::new(px(2.0), px(3.0))),
            rect(2.0, 3.0, 10.0, 10.0)
        );
        assert_eq!(
            bounds.inset(LogicalEdges::uniform(px(4.0))),
            rect(4.0, 4.0, 2.0, 2.0)
        );
        assert_eq!(bounds.outset(px(2.0)), rect(-2.0, -2.0, 14.0, 14.0));
        assert_eq!(
            bounds.inset(LogicalEdges::new(px(1.0), px(2.0), px(3.0), px(4.0))),
            rect(4.0, 1.0, 4.0, 6.0)
        );
        assert_eq!(
            bounds.inset(LogicalEdges::uniform(px(6.0))),
            rect(6.0, 6.0, -2.0, -2.0)
        );
    }

    #[test]
    fn rect_union_and_intersect() {
        let a = rect(-2.0, -3.0, 4.0, 5.0);
        let b = rect(5.0, 6.0, 2.0, 2.0);

        assert_eq!(LogicalRect::default().union(a), a);
        assert_eq!(a.union(LogicalRect::default()), a);
        assert_eq!(a.union(a), a);
        assert_eq!(a.union(b), rect(-2.0, -3.0, 9.0, 11.0));

        let left = rect(0.0, 0.0, 10.0, 10.0);
        let right = rect(5.0, 5.0, 10.0, 10.0);
        let far = rect(100.0, 100.0, 10.0, 10.0);
        assert_eq!(left.intersect(right), rect(5.0, 5.0, 5.0, 5.0));
        assert!(left.intersects(right));
        assert!(!left.intersects(far));
        assert_eq!(left.intersect(far), rect(100.0, 100.0, 0.0, 0.0));
    }

    #[test]
    fn dpr_rejects_non_positive_and_non_finite() {
        assert!(DevicePixelRatio::new(0.0).is_none());
        assert!(DevicePixelRatio::new(-1.0).is_none());
        assert!(DevicePixelRatio::new(f32::NAN).is_none());
        assert!(DevicePixelRatio::new(f32::INFINITY).is_none());
        for factor in FACTORS {
            assert!(DevicePixelRatio::new(factor).is_some(), "{factor}");
            assert_eq!(dpr(factor).factor(), factor);
        }
    }

    #[test]
    fn dpr_round_trips_lengths() {
        for factor in FACTORS {
            let dpr = dpr(factor);
            for logical in [0.0f32, 1.0, 10.5, -3.25] {
                let round_trip = dpr.to_logical(dpr.to_physical(px(logical)));
                assert!(
                    (round_trip.value() - logical).abs() < 1e-4,
                    "factor {factor} logical {logical} got {round_trip:?}"
                );
            }
        }
    }

    #[test]
    fn pixel_snapped_int_rect_rounds_each_edge() {
        for (factor, expected) in [
            (1.0, (0, 0, 10, 10)),
            (1.25, (0, 0, 13, 13)),
            (1.5, (0, 0, 15, 15)),
            (1.75, (0, 0, 18, 18)),
            (2.0, (0, 0, 20, 20)),
            (3.0, (0, 0, 30, 30)),
        ] {
            let snapped = dpr(factor).to_device_rect(rect(0.0, 0.0, 10.0, 10.0));
            assert_eq!(
                (
                    snapped.x0.value(),
                    snapped.y0.value(),
                    snapped.x1.value(),
                    snapped.y1.value()
                ),
                expected,
                "factor {factor}"
            );
        }

        let snapped = dpr(1.25).to_device_rect(rect(10.3, 5.2, 10.0, 4.0));
        assert_eq!(snapped, device(13, 7, 25, 12));
        assert_eq!(snapped.width(), dp(12));
        assert_eq!(snapped.height(), dp(5));
    }

    #[test]
    fn pixel_snapped_int_rect_edge_table_at_every_scale() {
        for (factor, expected) in FACTORS.iter().zip(ROUNDED_EDGES.iter()) {
            let dpr = dpr(*factor);
            for (edge, want) in EDGES.iter().zip(expected.iter()) {
                let snapped = dpr.to_device_rect(rect(*edge, 0.0, 0.0, 0.0));
                assert_eq!(snapped.x0, dp(*want), "edge {edge} at scale {factor}");
                assert_eq!(snapped.x1, dp(*want), "edge {edge} at scale {factor}");
            }
        }
    }

    #[test]
    fn pixel_snapped_int_rect_keeps_shared_edges_contiguous() {
        let dpr = dpr(1.25);
        let bounds = [0.0f32, 10.0, 20.0, 33.5, 100.0];
        let rects: Vec<DeviceRect> = bounds
            .windows(2)
            .map(|pair| dpr.to_device_rect(rect(pair[0], 0.0, pair[1] - pair[0], 10.0)))
            .collect();

        assert_eq!(
            rects.iter().map(|rect| rect.x0.value()).collect::<Vec<_>>(),
            [0, 13, 25, 42]
        );
        for pair in rects.windows(2) {
            assert_eq!(pair[0].x1, pair[1].x0, "shared edge must not leave a seam");
        }
        assert_eq!(rects[rects.len() - 1].x1, dp(125));
    }

    #[test]
    fn pixel_snapped_int_rect_rounds_half_away_from_zero() {
        let dpr = dpr(1.0);

        assert_eq!(
            dpr.to_device_rect(rect(-2.5, -1.5, 4.0, 6.0)),
            device(-3, -2, 2, 5)
        );
        assert_eq!(dpr.to_device_rect(rect(-2.0, 0.0, 0.0, 0.0)).x0, dp(-2));
        assert_eq!(dpr.to_device_rect(rect(-10.5, 0.0, 0.0, 0.0)).x0, dp(-11));
        assert_eq!(dpr.to_device_rect(rect(-0.5, 0.0, 0.0, 0.0)).x0, dp(-1));
    }

    #[test]
    fn pixel_snapped_int_rect_does_not_round_the_size_separately() {
        let dpr = dpr(1.5);

        assert_eq!(
            dpr.to_device_rect(rect(0.3, 0.0, 10.3, 10.0)),
            device(0, 0, 16, 15)
        );
        assert_eq!(
            dpr.to_device_size(LogicalSize::new(px(10.3), px(10.0))),
            DeviceSize::new(dp(15), dp(15))
        );
    }

    #[test]
    fn covering_rect_is_a_superset_of_the_snapped_rect() {
        let rects = [
            rect(0.0, 0.0, 10.0, 10.0),
            rect(10.5, 20.25, 0.25, 0.25),
            rect(-3.25, 7.5, 16.75, 2.5),
            rect(0.3, 0.0, 10.3, 10.0),
        ];

        for factor in FACTORS {
            let dpr = dpr(factor);
            for bounds in rects {
                let snapped = dpr.to_device_rect(bounds);
                let covering = dpr.to_covering_device_rect(bounds);

                assert!(covering.x0 <= snapped.x0, "{bounds:?} at {factor}");
                assert!(covering.y0 <= snapped.y0, "{bounds:?} at {factor}");
                assert!(covering.x1 >= snapped.x1, "{bounds:?} at {factor}");
                assert!(covering.y1 >= snapped.y1, "{bounds:?} at {factor}");
                assert!(snapped.x0.value() - covering.x0.value() <= 1);
                assert!(snapped.y0.value() - covering.y0.value() <= 1);
                assert!(covering.x1.value() - snapped.x1.value() <= 1);
                assert!(covering.y1.value() - snapped.y1.value() <= 1);
            }
        }
    }

    #[test]
    fn covering_rect_keeps_subpixel_visible() {
        let subpixel = rect(10.5, 20.25, 0.25, 0.25);

        assert_eq!(dpr(1.0).to_device_rect(subpixel), device(11, 20, 11, 21));
        assert_eq!(
            dpr(1.0).to_covering_device_rect(subpixel),
            device(10, 20, 11, 21)
        );
        assert_eq!(
            dpr(1.25).to_covering_device_rect(subpixel),
            device(13, 25, 14, 26)
        );
        assert_eq!(
            dpr(1.5).to_covering_device_rect(subpixel),
            device(15, 30, 17, 31)
        );
        assert_eq!(
            dpr(2.0).to_covering_device_rect(subpixel),
            device(21, 40, 22, 41)
        );
        assert_eq!(
            dpr(3.0).to_covering_device_rect(subpixel),
            device(31, 60, 33, 62)
        );
    }

    #[test]
    fn covering_differs_from_snapping_on_fractional_edges() {
        let bounds = rect(1.3, 0.0, 0.4, 1.0);

        assert_eq!(dpr(2.0).to_device_rect(bounds), device(3, 0, 3, 2));
        assert_eq!(dpr(2.0).to_covering_device_rect(bounds), device(2, 0, 4, 2));

        let wide = rect(0.0, 0.0, 256.8, 1.0);
        assert_eq!(dpr(1.75).to_device_rect(wide).x1, dp(449));
        assert_eq!(dpr(1.75).to_covering_device_rect(wide).x1, dp(450));
    }

    #[test]
    fn physical_rect_is_continuous_and_unsnapped() {
        let physical = dpr(1.25).to_physical_rect(rect(10.5, 20.25, 0.25, 0.25));

        assert_eq!(physical.origin.x, physical_px(13.125));
        assert_eq!(physical.origin.y, physical_px(25.3125));
        assert_eq!(physical.size.width, physical_px(0.3125));
        assert_eq!(physical.size.height, physical_px(0.3125));
        assert_eq!(physical.to_aabb(), [13.125, 25.3125, 0.3125, 0.3125]);

        let asymmetric = LogicalRect::from_origin_size(
            LogicalPoint::new(px(3.5), px(7.25)),
            LogicalSize::new(px(20.0), px(11.5)),
        );
        assert_eq!(
            dpr(1.5).to_physical_rect(asymmetric).to_aabb(),
            [5.25, 10.875, 30.0, 17.25]
        );
    }

    #[test]
    fn invalid_inputs_share_one_empty_value() {
        let bad = [
            rect(f32::NAN, 0.0, 10.0, 10.0),
            rect(0.0, f32::INFINITY, 10.0, 10.0),
            rect(0.0, 0.0, f32::INFINITY, 10.0),
            rect(0.0, 0.0, f32::NEG_INFINITY, 10.0),
            rect(0.0, 0.0, -10.0, 10.0),
            rect(0.0, 0.0, 10.0, -10.0),
        ];

        for bounds in bad {
            let dpr = dpr(1.5);
            let device = dpr.to_device_rect(bounds);
            let covering = dpr.to_covering_device_rect(bounds);
            let physical = dpr.to_physical_rect(bounds);

            assert_eq!(device, DeviceRect::default(), "{bounds:?}");
            assert_eq!(covering, DeviceRect::default(), "{bounds:?}");
            assert!(device.is_empty(), "{bounds:?}");
            assert_eq!(physical.to_aabb(), [0.0; 4], "{bounds:?}");
            assert_eq!(physical, PhysicalRect::default(), "{bounds:?}");
            assert_eq!(
                DeviceRect::default().union(device),
                DeviceRect::default(),
                "{bounds:?}"
            );
        }

        let overflow = rect(-9.191715e37, 0.0, f32::MAX, 10.0);
        assert_eq!(dpr(1.0).to_physical_rect(overflow), PhysicalRect::default());
        assert_eq!(
            dpr(1.0).to_device_rect(overflow),
            device(i32::MIN, 0, i32::MAX, 10)
        );
        assert_eq!(
            dpr(1.0).to_covering_device_rect(overflow),
            device(i32::MIN, 0, i32::MAX, 10)
        );
    }

    #[test]
    fn huge_coordinates_saturate() {
        let huge = dpr(1.25).to_device_rect(rect(1e30, 1e30, 1e30, 1e30));
        assert_eq!(huge.x0, dp(i32::MAX));
        assert_eq!(huge.x1, dp(i32::MAX));
        assert!(huge.is_empty());
        assert_eq!(huge.width(), dp(0));

        let negative = dpr(1.25).to_device_rect(rect(-1e30, -1e30, 2e30, 2e30));
        assert!(negative.x0 <= negative.x1);
        assert!(negative.y0 <= negative.y1);
        assert_eq!(negative.x0, dp(i32::MIN));
        assert_eq!(negative.x1, dp(i32::MAX));
    }

    #[test]
    fn device_rect_union_and_intersect() {
        let a = device(0, 0, 10, 10);
        let b = device(5, 5, 15, 15);

        assert_eq!(DeviceRect::default().union(a), a);
        assert_eq!(a.union(DeviceRect::default()), a);
        assert_eq!(a.union(b), device(0, 0, 15, 15));
        assert_eq!(a.intersect(b), device(5, 5, 10, 10));

        let far = device(100, 100, 110, 110);
        let empty_intersection = a.intersect(far);
        assert!(empty_intersection.is_empty());
        assert_eq!(empty_intersection, device(100, 100, 100, 100));
    }

    #[test]
    fn device_rect_union_keeps_positioned_zero_size_rects() {
        let positioned = device(100, 100, 100, 100);
        let origin_empty = DeviceRect::default();

        assert_eq!(positioned.union(origin_empty), positioned);
        assert_eq!(origin_empty.union(positioned), positioned);
        assert_eq!(
            positioned.union(device(0, 0, 10, 10)),
            device(0, 0, 100, 100)
        );
        assert_eq!(
            device(0, 0, 10, 10).union(positioned),
            device(0, 0, 100, 100)
        );
    }

    #[test]
    fn device_rect_bounds_are_ordered_and_non_negative() {
        let empty = DeviceRect::default();

        assert_eq!(empty.width(), dp(0));
        assert_eq!(empty.height(), dp(0));
        assert_eq!(empty.area(), 0);
        assert!(empty.is_empty());

        let saturated = device(i32::MIN, i32::MIN, i32::MAX, i32::MAX);

        assert_eq!(saturated.width(), dp(i32::MAX));
        assert_eq!(saturated.height(), dp(i32::MAX));
        assert!(!saturated.is_empty());
    }

    #[test]
    fn device_rect_from_edges_orders_them() {
        assert_eq!(
            DeviceRect::from_edges(dp(10), dp(10), dp(2), dp(2)),
            device(10, 10, 10, 10)
        );
        assert_eq!(
            device(1, 2, 3, 4).translate(DevicePoint::new(dp(2), dp(3))),
            device(3, 5, 5, 7)
        );
        assert_eq!(device(0, 0, 4, 4).size(), DeviceSize::new(dp(4), dp(4)));
        assert_eq!(
            DeviceRect::from_origin_size(
                DevicePoint::new(dp(1), dp(2)),
                DeviceSize::new(dp(3), dp(4))
            ),
            device(1, 2, 4, 6)
        );
    }

    #[test]
    fn device_rect_intersect_clamps_touching_edges_to_zero() {
        assert_eq!(
            device(0, 0, 10, 10).intersect(device(-5, -5, 0, 0)),
            device(0, 0, 0, 0)
        );
    }

    #[test]
    fn overlaps_inclusive_counts_tangent_rects() {
        let a = device(0, 0, 10, 10);

        assert!(a.overlaps_inclusive(device(5, 5, 15, 15)));
        assert!(a.overlaps_inclusive(a));
        assert!(a.overlaps_inclusive(device(10, 0, 20, 10)), "共享边算相交");
        assert!(a.overlaps_inclusive(device(0, 10, 10, 20)), "共享角算相交");
        assert!(a.overlaps_inclusive(device(-1, -1, 0, 0)), "共享角算相交");
        assert!(!a.overlaps_inclusive(device(11, 0, 20, 10)));
        assert!(!a.overlaps_inclusive(device(0, 11, 10, 20)));
    }

    #[test]
    fn dpr_helpers_convert_points_and_sizes() {
        let dpr = dpr(1.5);

        assert_eq!(
            dpr.to_device_point(LogicalPoint::new(px(10.5), px(0.5))),
            DevicePoint::new(dp(16), dp(1))
        );
        assert_eq!(
            dpr.to_device_size(LogicalSize::new(px(10.0), px(3.0))),
            DeviceSize::new(dp(15), dp(5))
        );
        assert_eq!(
            dpr.to_logical_point(DevicePoint::new(dp(15), dp(3))),
            LogicalPoint::new(px(10.0), px(2.0))
        );
        assert_eq!(
            dpr.to_logical_size(DeviceSize::new(dp(15), dp(3))),
            LogicalSize::new(px(10.0), px(2.0))
        );
        assert_eq!(
            dpr.to_physical_size(LogicalSize::new(px(2.0), px(4.0))),
            PhysicalSize::new(physical_px(3.0), physical_px(6.0))
        );
        assert_eq!(
            dpr.to_logical_rect(device(0, 0, 15, 6)),
            rect(0.0, 0.0, 10.0, 4.0)
        );
    }

    #[test]
    fn device_point_and_size_round_half_away_from_zero() {
        let scale = dpr(1.0);

        assert_eq!(
            scale.to_device_point(LogicalPoint::new(px(2.5), px(-2.5))),
            DevicePoint::new(dp(3), dp(-3))
        );
        assert_eq!(
            scale.to_device_size(LogicalSize::new(px(2.5), px(3.5))),
            DeviceSize::new(dp(3), dp(4))
        );
        assert_eq!(
            dpr(1.5).to_device_point(LogicalPoint::new(px(0.3), px(0.5))),
            DevicePoint::new(dp(0), dp(1))
        );
    }

    #[test]
    fn device_point_and_size_map_non_finite_to_zero() {
        let dpr = dpr(1.25);

        assert_eq!(
            dpr.to_device_point(LogicalPoint::new(px(f32::NAN), px(f32::INFINITY))),
            DevicePoint::new(dp(0), dp(i32::MAX))
        );
        assert_eq!(
            dpr.to_device_size(LogicalSize::new(px(f32::NEG_INFINITY), px(f32::NAN))),
            DeviceSize::new(dp(i32::MIN), dp(0))
        );
    }

    #[test]
    fn stroke_width_snaps_to_integer_device_pixels() {
        for (factor, expected) in [
            (1.0, [1.0, 1.0, 2.0, 3.0, 2.0]),
            (1.25, [1.0, 1.0, 3.0, 3.0, 3.0]),
            (1.5, [2.0, 1.0, 3.0, 4.0, 4.0]),
            (1.75, [2.0, 1.0, 4.0, 4.0, 4.0]),
            (2.0, [2.0, 1.0, 4.0, 5.0, 5.0]),
            (3.0, [3.0, 2.0, 6.0, 8.0, 7.0]),
        ] {
            for (width, want) in [1.0f32, 0.5, 2.0, 2.5, 2.4].iter().zip(expected.iter()) {
                let got = dpr(factor).stroke_width(px(*width));
                assert_eq!(
                    got.value(),
                    *want,
                    "width {width} at scale {factor} must snap to {want} physical px"
                );
                assert_eq!(
                    got.value().fract(),
                    0.0,
                    "吸附结果必须是整数物理像素：{got:?}"
                );
            }
        }
    }

    #[test]
    fn stroke_width_is_at_least_one_physical_pixel() {
        for factor in FACTORS {
            for width in [0.01f32, 0.1, 0.25, 0.4, 0.49] {
                assert_eq!(
                    dpr(factor).stroke_width(px(width)),
                    physical_px(1.0),
                    "width {width} at scale {factor} 必须至少 1 物理像素"
                );
            }
        }
    }

    #[test]
    fn stroke_width_defends_against_invalid_input() {
        assert_eq!(dpr(1.25).stroke_width(px(0.0)), physical_px(0.0));
        assert_eq!(dpr(1.25).stroke_width(px(-1.0)), physical_px(0.0));
        assert_eq!(dpr(1.25).stroke_width(px(f32::NAN)), physical_px(0.0));
        assert_eq!(dpr(1.25).stroke_width(px(f32::INFINITY)), physical_px(0.0));

        let huge = dpr(1.25).stroke_width(px(1e30));
        assert!(huge.is_finite() && huge.value() >= 1.0);
    }

    #[test]
    fn stroke_width_is_identity_at_integer_device_scales() {
        for (factor, width) in [(1.0, 1.0), (1.0, 2.0), (2.0, 1.0), (2.0, 2.0)] {
            assert_eq!(
                dpr(factor).stroke_width(px(width)),
                physical_px(width * factor),
                "整数 DPI 下宽度吸附必须是恒等"
            );
        }
    }

    #[test]
    fn transform_identity_is_noop() {
        let point = LogicalPoint::new(px(3.5), px(-7.25));

        assert_eq!(Transform::IDENTITY.apply(point), point);
        assert_eq!(
            Transform::IDENTITY.to_array(),
            [1.0, 0.0, 0.0, 1.0, 0.0, 0.0]
        );
    }

    #[test]
    fn transform_translate_scale_apply() {
        assert_eq!(
            Transform::translate(px(3.0), px(4.0)).apply(LogicalPoint::new(px(1.0), px(1.0))),
            LogicalPoint::new(px(4.0), px(5.0))
        );
        assert_eq!(
            Transform::scale(2.0, 3.0).apply(LogicalPoint::new(px(1.0), px(2.0))),
            LogicalPoint::new(px(2.0), px(6.0))
        );
    }

    #[test]
    fn transform_then_is_apply_then_apply() {
        let first = Transform::translate(px(3.0), px(4.0));
        let second = Transform::scale(2.0, 3.0);
        let composed = first.then(second);
        let point = LogicalPoint::new(px(1.0), px(2.0));

        assert_eq!(composed.apply(point), second.apply(first.apply(point)));
    }

    #[test]
    fn transform_to_array_order() {
        let composed = Transform::translate(px(3.0), px(4.0)).then(Transform::scale(2.0, 3.0));

        assert_eq!(composed.to_array(), [2.0, 0.0, 0.0, 3.0, 6.0, 12.0]);
        assert_eq!(
            composed.apply(LogicalPoint::new(px(1.0), px(2.0))),
            Transform::scale(2.0, 3.0).apply(LogicalPoint::new(px(4.0), px(6.0)))
        );
    }

    #[test]
    fn transform_inverse_roundtrips_points() {
        let transform = Transform::translate(px(12.0), px(-7.0))
            .then(Transform::scale(2.0, 0.5))
            .then(Transform::rotate(std::f32::consts::FRAC_PI_4));
        let inverse = transform.inverse().expect("仿射可逆");
        for point in [
            LogicalPoint::new(px(0.0), px(0.0)),
            LogicalPoint::new(px(13.0), px(21.0)),
            LogicalPoint::new(px(-5.5), px(3.25)),
        ] {
            let roundtrip = inverse.apply(transform.apply(point));
            assert!(
                (roundtrip.x.value() - point.x.value()).abs() < 1e-4
                    && (roundtrip.y.value() - point.y.value()).abs() < 1e-4,
                "{point:?} → {roundtrip:?}"
            );
        }
    }

    #[test]
    fn degenerate_transforms_have_no_inverse() {
        assert!(
            Transform::scale(0.0, 1.0).inverse().is_none(),
            "压扁到零面积的变换不可逆"
        );
        assert!(!Transform::scale(0.0, 1.0).is_invertible());
        assert!(
            Transform::scale(f32::NAN, 1.0).inverse().is_none(),
            "NaN 变换不可逆（不得 panic）"
        );
        assert!(
            Transform::scale(f32::INFINITY, 1.0).inverse().is_none(),
            "∞ 变换不可逆"
        );
        assert_eq!(
            Transform::scale(0.0, 1.0).transform_rect(rect(0.0, 0.0, 10.0, 10.0)),
            rect(0.0, 0.0, 0.0, 10.0),
            "退化变换的包围盒是零宽矩形（不是 NaN）"
        );
    }

    #[test]
    fn transform_rect_is_aabb_of_corners() {
        let rotated = Transform::rotate(std::f32::consts::FRAC_PI_4);
        let bounds = rotated.transform_rect(rect(0.0, 0.0, 10.0, 10.0));
        assert!(
            (bounds.size.width.value() - 14.1421).abs() < 1e-3,
            "{bounds:?}"
        );
        assert!(
            (bounds.size.height.value() - 14.1421).abs() < 1e-3,
            "{bounds:?}"
        );
    }
}
