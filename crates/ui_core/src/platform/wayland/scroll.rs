//! `wl_pointer` 滚轮/触控板的增量累积与「按源归一」（R02-10 / B8）。
//!
//! # 为什么两条通路不能相加
//!
//! v8+ compositor 对**同一格**物理滚轮在同一个 `wl_pointer.frame` 里同时发：
//!
//! - `axis`：连续值（v5 起一格滚轮规定为 10 个 surface-local 单位，mutter/wlroots 实际常见
//!   10 或 15）；
//! - `axis_value120`（v8+）：高精度离散步长，一格 = 120，与 Win32 的
//!   `WHEEL_DELTA = 120` 同量纲。
//!
//! 协议（`wayland.xml` 的 `axis_value120`）：*"This event replaces axis_discrete for v8
//! clients"* —— 但它**没有**取消 `axis`：`axis_discrete` 的文档写着 *"coupled with exactly
//! one axis event"*，即连续 `axis` 与离散值描述的是**同一次物理滚动**。旧实现把
//! `value120 / 120` 与 `axis / 10` 相加，于是物理上一格 = 1 + 1 = 2 行（用户感知「滚一格
//! 跳两行」，交易终端上相当于滚动灵敏度翻倍）。
//!
//! 正确做法是**按源归一、逐轴**：
//!
//! - **滚轮**（`axis_source != finger`）：每根轴优先用离散值（`value120 / 120`），该轴没有
//!   离散值时退回连续值（`axis / 10`）。水平轴在高精度设备上可能只有连续值，所以判定必须
//!   **逐轴**而不是整帧二选一；
//! - **手指 / 触控板**（`axis_source == finger`）：连续值就是像素增量，1:1 跟随（不做行换算，
//!   才能与系统惯性/加速度一致）；仅当服务端只发 `axis_value120` 不发 `axis` 的兼容情况才把
//!   `value120 / 120 * 10` 折算成像素。
//!
//! 累积器按 `wl_pointer.frame` 成批（一次 frame = 一个逻辑滚动输入），`surface.rs` 在
//! `Frame` 事件里取走该批并只产出一条 `InputEvent::Scroll`。
//!
//! 纯逻辑放在这里而不是 Linux-only 的 `platform/linux/surface.rs`：Wayland 驱动层在开发机
//! （Windows）上不编译，旧实现因此只能靠真机观察（审计 §13 第 6 条）。本模块由
//! `platform::wayland` 整体的 `cfg(any(target_os = "linux", test))` 门控。

/// 连续 `axis` 的单位 → 行换算（v5 起滚轮一格 = 10 个 surface-local 单位）。
const CONTINUOUS_AXIS_UNITS_PER_LINE: f32 = 10.0;

/// `axis_value120` 的单位：一格（与 Win32 `WHEEL_DELTA` 同量纲）。
const VALUE120_UNITS_PER_LINE: f32 = 120.0;

/// 一根滚动轴（协议 `wl_pointer.axis` 的 axis 枚举在纯逻辑层的表示）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScrollAxis {
    /// 水平滚动。
    Horizontal,
    /// 垂直滚动。
    Vertical,
}

/// 一次 `wl_pointer.frame` 内累积的滚动增量。
///
/// 两根轴各存两份：连续 `axis`（surface-local 单位）与离散 `axis_value120`（1/120 行）。
/// 两者是同一物理滚动的不同表示，[`Self::wheel_lines`] 按轴二选一、**绝不相加**。
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ScrollAccum {
    /// `axis_value120` 累积（1/120 行）。
    discrete_x: i32,
    discrete_y: i32,
    /// 连续 `axis` 累积（surface-local 单位）。
    continuous_x: f32,
    continuous_y: f32,
}

impl ScrollAccum {
    /// 累积一条 `wl_pointer.axis_value120`。
    pub(crate) fn note_discrete(&mut self, axis: ScrollAxis, value120: i32) {
        let slot = match axis {
            ScrollAxis::Horizontal => &mut self.discrete_x,
            ScrollAxis::Vertical => &mut self.discrete_y,
        };
        *slot = slot.saturating_add(value120);
    }

    /// 累积一条 `wl_pointer.axis`（连续值）。
    pub(crate) fn note_continuous(&mut self, axis: ScrollAxis, value: f32) {
        let slot = match axis {
            ScrollAxis::Horizontal => &mut self.continuous_x,
            ScrollAxis::Vertical => &mut self.continuous_y,
        };
        *slot += value;
    }

    /// 本批是否没有任何滚动分量（`Frame` 据此跳过空事件）。
    #[must_use]
    pub(crate) fn is_empty(self) -> bool {
        self.discrete_x == 0
            && self.discrete_y == 0
            && self.continuous_x == 0.0
            && self.continuous_y == 0.0
    }

    /// 清空本批（`Frame` 取走增量之后调用）。
    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }

    /// 滚轮：换算成「行」（`(水平, 垂直)`）。
    ///
    /// **每轴独立**：该轴有离散值就只用离散值（`value120 / 120`，精确），否则用连续值
    /// （`axis / 10`，v5 只发 `axis` 的服务端 / 水平轴常见）。两条通路描述的是同一次滚动，
    /// 相加会把一格算成两行。
    #[must_use]
    pub(crate) fn wheel_lines(self) -> (f32, f32) {
        (
            lines_for(self.discrete_x, self.continuous_x),
            lines_for(self.discrete_y, self.continuous_y),
        )
    }

    /// 手指 / 触控板：连续值按**像素** 1:1 跟随，`(水平, 垂直)`。
    ///
    /// 连续值为 0 的轴（服务端只发 `axis_value120`、没发 `axis` 的兼容情况）才把离散值折算成
    /// 像素（一格 = 10 个单位）：否则该轴的滚动会在触控板上完全丢失。
    #[must_use]
    pub(crate) fn finger_pixels(self) -> (f32, f32) {
        (
            pixels_for(self.discrete_x, self.continuous_x),
            pixels_for(self.discrete_y, self.continuous_y),
        )
    }
}

/// 单轴：离散优先、连续回退（**不叠加**）。
fn lines_for(discrete: i32, continuous: f32) -> f32 {
    if discrete != 0 {
        discrete as f32 / VALUE120_UNITS_PER_LINE
    } else {
        continuous / CONTINUOUS_AXIS_UNITS_PER_LINE
    }
}

/// 单轴：连续值即像素；没有连续值时才把 v120 折算成像素。
fn pixels_for(discrete: i32, continuous: f32) -> f32 {
    if continuous != 0.0 {
        continuous
    } else {
        discrete as f32 / VALUE120_UNITS_PER_LINE * CONTINUOUS_AXIS_UNITS_PER_LINE
    }
}

#[cfg(test)]
mod tests {
    use super::{CONTINUOUS_AXIS_UNITS_PER_LINE, ScrollAccum, ScrollAxis};

    /// 一格 = `axis_value120 = 120` = 1 行。
    #[test]
    fn discrete_scroll_is_one_line_per_120_units() {
        let mut one_notch = ScrollAccum::default();
        one_notch.note_discrete(ScrollAxis::Vertical, 120);
        assert_eq!(one_notch.wheel_lines().1, 1.0, "一格 = 120/120 = 1 行");

        let mut two_notches = ScrollAccum::default();
        two_notches.note_discrete(ScrollAxis::Vertical, 240);
        assert_eq!(two_notches.wheel_lines().1, 2.0, "两格 = 2 行");
    }

    /// v5 服务端只发 `axis`（一格 = 10 个单位）：也必须是 1 行，而不是 10/120 ≈ 0.083 行。
    #[test]
    fn a_v5_server_that_only_sends_axis_still_yields_one_line() {
        let mut one_notch = ScrollAccum::default();
        one_notch.note_continuous(ScrollAxis::Vertical, CONTINUOUS_AXIS_UNITS_PER_LINE);
        assert_eq!(
            one_notch.wheel_lines().1,
            1.0,
            "一格连续值 = 1 行（旧实现是 0.083）"
        );
    }

    /// B8 核心回归：同一格滚轮同时收到 `axis_value120 = 120` 与 `axis = 10` ⇒ **1 行**。
    ///
    /// 旧实现把两条通路相加（`120/120 + 10/10`）得到 2 行，即「滚一格跳两行」；
    /// 旧测试 `both_axis_paths_accumulate` 把这个错误行为钉成了契约。
    #[test]
    fn wheel_both_channels_yield_one_line() {
        let mut notch = ScrollAccum::default();
        notch.note_discrete(ScrollAxis::Vertical, 120);
        notch.note_continuous(ScrollAxis::Vertical, CONTINUOUS_AXIS_UNITS_PER_LINE);
        assert_eq!(
            notch.wheel_lines().1,
            1.0,
            "两条通路是同一格滚动的两种表示，必须二选一而不是相加"
        );
    }

    /// 高分辨率滚轮：`axis_value120 = 30`（一格的四分之一）与 `axis = 2.5` 是同一次滚动，
    /// 两条通路换算结果必须一致（0.25 行），不能一边 0.25 一边再加 0.25。
    #[test]
    fn high_resolution_value120_and_continuous_agree() {
        let mut from_discrete = ScrollAccum::default();
        from_discrete.note_discrete(ScrollAxis::Vertical, 30);
        let mut from_continuous = ScrollAccum::default();
        from_continuous.note_continuous(ScrollAxis::Vertical, 2.5);
        let mut both = ScrollAccum::default();
        both.note_discrete(ScrollAxis::Vertical, 30);
        both.note_continuous(ScrollAxis::Vertical, 2.5);

        assert_eq!(from_discrete.wheel_lines().1, 0.25);
        assert_eq!(from_continuous.wheel_lines().1, 0.25);
        assert_eq!(
            both.wheel_lines().1,
            0.25,
            "两路同时到达也不得叠加（旧实现是 0.5）"
        );
    }

    /// 逐轴独立判定：水平轴只有连续值、垂直轴只有离散值 ⇒ 两轴各自算出 1 行。
    #[test]
    fn per_axis_mixing_uses_each_axiss_available_channel() {
        let mut mixed = ScrollAccum::default();
        mixed.note_continuous(ScrollAxis::Horizontal, CONTINUOUS_AXIS_UNITS_PER_LINE);
        mixed.note_discrete(ScrollAxis::Vertical, 120);
        assert_eq!(
            mixed.wheel_lines(),
            (1.0, 1.0),
            "水平连续 / 垂直离散必须逐轴判定，整帧二选一会丢掉其中一根轴"
        );
    }

    /// 手指（触控板）：连续值就是像素增量，1:1 跟随 —— 同时到达的 v120 不叠加。
    #[test]
    fn finger_uses_continuous_pixels_and_ignores_value120() {
        let mut touchpad = ScrollAccum::default();
        touchpad.note_continuous(ScrollAxis::Vertical, -24.0);
        touchpad.note_discrete(ScrollAxis::Vertical, 120);
        assert_eq!(
            touchpad.finger_pixels().1,
            -24.0,
            "触控板按像素 1:1（不是行），且离散分量不得叠加"
        );
        // 与滚轮换算是两套单位：同一批数据在手指通路上是 -24 像素，在滚轮通路上是行。
        assert_eq!(touchpad.wheel_lines().1, 1.0, "滚轮通路用离散值 = 1 行");
    }

    /// 服务端只发 `axis_value120`、没发 `axis` 的手指滚动：必须折算成像素而不是整轴丢失。
    #[test]
    fn finger_falls_back_to_value120_when_axis_is_missing() {
        let mut touchpad = ScrollAccum::default();
        touchpad.note_discrete(ScrollAxis::Vertical, 120);
        assert_eq!(
            touchpad.finger_pixels().1,
            CONTINUOUS_AXIS_UNITS_PER_LINE,
            "一格 v120 折算成一格的连续单位（10 像素），不能变成 1 像素也不能丢失"
        );
    }

    /// 一个 frame 里的多条 `axis_value120` 先累积，`Frame` 取走时一次算清
    /// （高精度滚轮会把一格拆成多条事件；`clear()` 之后下一帧从零开始）。
    #[test]
    fn value120_events_accumulate_until_the_frame_is_flushed() {
        let mut scroll = ScrollAccum::default();
        scroll.note_discrete(ScrollAxis::Vertical, 60);
        scroll.note_discrete(ScrollAxis::Vertical, 60);
        assert_eq!(scroll.wheel_lines().1, 1.0, "两条 60 合起来是一格");
        assert!(!scroll.is_empty());
        scroll.clear();
        assert!(scroll.is_empty(), "clear 之后下一帧从零开始");
        assert_eq!(scroll.wheel_lines(), (0.0, 0.0));
        assert_eq!(scroll.finger_pixels(), (0.0, 0.0));
    }

    #[test]
    fn an_empty_accumulator_reports_empty() {
        let mut scroll = ScrollAccum::default();
        assert!(scroll.is_empty());
        scroll.note_continuous(ScrollAxis::Vertical, -3.0);
        assert!(!scroll.is_empty());
        scroll.clear();
        assert!(scroll.is_empty(), "clear 之后必须回到空态");
    }
}
