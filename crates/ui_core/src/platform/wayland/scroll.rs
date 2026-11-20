//! `wl_pointer` 滚轮/触控板的增量累积与「按源归一」（R02-10 / B8 / RB-10）。
//!
//! # 三条离散通路是**同一次物理滚动**的不同表示
//!
//! - `axis`：连续值（v5 起一格滚轮规定为 10 个 surface-local 单位，mutter/wlroots 实际常见
//!   10 或 15；触控板是像素增量）；
//! - `axis_discrete`（v5–v7）：**整数格数**（1 = 一格）。协议原文 *"This event carries the axis
//!   value of the wl_pointer.axis event in discrete steps (e.g. mouse wheel clicks)"* 且 *"each
//!   axis_discrete event is always followed by exactly one axis event with the same axis number
//!   within the same wl_pointer.frame"*；v8 起不再发给支持 v8 的客户端，由 `axis_value120`
//!   取代；
//! - `axis_value120`（v8+）：高精度离散步长，一格 = 120（与 Win32 `WHEEL_DELTA` 同量纲）。
//!
//! 旧实现只处理 v8+ 的 `axis_value120` 与连续 `axis`，v5–v7 服务端发来的 `axis_discrete` 被
//! 直接丢弃 ⇒ 只剩 `axis / 10` 换算；mutter/wlroots 实测一格 `axis` 常是 15 ⇒ 一格滚出 1.5 行
//! （RB-10）。三者**不能相加**（描述同一次滚动），也不该让低版本服务端落到近似换算：
//! [`ScrollAccum::wheel_lines`] 逐轴按 `axis_discrete`（格数，精确）> `axis_value120`（/120）>
//! 连续 `axis`（/10）取第一个非零值。
//!
//! # `axis_source` 是**帧作用域**
//!
//! 协议（`wayland.xml` 的 `axis_source`）：*"It is sent before a wl_pointer.frame event and
//! carries the source information for all events within that frame"*，且 *"This event is
//! optional. If the source is unknown for a particular axis event sequence, no event is sent."*
//! ⇒ 源只描述**它所在的这一帧**，不是「一直沿用到下一次 source 为止」的持久状态。旧实现把
//! `axis_source == finger` 写进 `WaylandState.scroll_finger` 长存：手指手势之后的下一条**没有**
//! `axis_source` 的滚轮帧继续被当成手指（像素 1:1）⇒ 一格（10 或 15 个单位）只滚 10–15 px，
//! 约 10× 灵敏度。
//!
//! 本模块把源建成显式状态：[`ScrollAccum::note_source`] 记录**本帧**的源，
//! [`ScrollAccum::clear`]（`wl_pointer.frame` 结束时调用）把它复位成 [`ScrollSource::Unknown`]；
//! `Unknown` 按**滚轮**处理 —— 不发 `axis_source` 的服务端上绝大多数输入是滚轮，把未知源当
//! 手指才会造成 10× 灵敏度。`axis_stop` 的复位由「它所在的帧结束」覆盖：协议允许 `axis` 与
//! `axis_stop` 同帧（*"this indicates that axis movement in one axis has stopped but continues
//! in the other axis"*），在 `axis_stop` 当场清源会把**同帧另一根轴**的滚动错判成滚轮。
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

/// `wl_pointer.axis_source` 在纯逻辑层的表示（协议 v5+；事件本身是**可选**的）。
///
/// 只有 [`Self::Finger`] 改变单位语义（像素 1:1 + 连续阶段）；其余（含 [`Self::Unknown`]）
/// 都按离散滚轮处理。理由：协议允许服务端不发 `axis_source`（*"If the source is unknown for a
/// particular axis event sequence, no event is sent"*），而不发该事件的服务端上绝大多数输入是
/// 滚轮；`Unknown` 若按手指处理，一格（10 或 15 个单位）只会滚 10–15 px（约 10× 灵敏度）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ScrollSource {
    /// 本帧没有 `axis_source` 事件（源未知）⇒ 按滚轮处理（安全默认）。
    #[default]
    Unknown,
    /// 物理滚轮（协议 `wheel` / `wheel_tilt`）：离散格。
    Wheel,
    /// 手指 / 触控板（协议 `finger`）：连续像素增量，1:1 跟随。
    Finger,
    /// 非手指的连续坐标设备（协议 `continuous`，例如按住按钮时的滚动）：与修复前一致按行归一。
    Continuous,
}

/// 一次 `wl_pointer.frame` 内累积的滚动增量。
///
/// 每根轴各存三份（同一物理滚动的三种表示）：连续 `axis`（surface-local 单位）、
/// `axis_value120`（1/120 行）、`axis_discrete`（**格**）。[`Self::wheel_lines`] 按轴三选一、
/// **绝不相加**。
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ScrollAccum {
    /// 本帧的滚动源（`axis_source`，帧结束时复位，见模块文档）。
    source: ScrollSource,
    /// `axis_discrete` 累积（**格**：1 = 一格；v5–v7 服务端）。
    notches_x: i32,
    notches_y: i32,
    /// `axis_value120` 累积（1/120 行；v8+ 服务端）。
    value120_x: i32,
    value120_y: i32,
    /// 连续 `axis` 累积（surface-local 单位）。
    continuous_x: f32,
    continuous_y: f32,
}

impl ScrollAccum {
    /// 记录本帧的滚动源（`wl_pointer.axis_source`）。
    ///
    /// 协议保证一帧最多一条 `axis_source`，且它与 `axis_discrete` / `axis_value120` 的到达
    /// 顺序不确定（*"The order of wl_pointer.axis_discrete and wl_pointer.axis_source is not
    /// guaranteed"*）—— 所以源与增量分开记录、到 `Frame` 才一起消费。
    pub(crate) fn note_source(&mut self, source: ScrollSource) {
        self.source = source;
    }

    /// 累积一条 `wl_pointer.axis_discrete`（`discrete` = **格数**，1 = 一格；v5–v7）。
    ///
    /// 与 [`Self::note_value120`] 是**不同量纲**：这里 1 = 一格，value120 的 120 = 一格。
    pub(crate) fn note_notches(&mut self, axis: ScrollAxis, notches: i32) {
        let slot = match axis {
            ScrollAxis::Horizontal => &mut self.notches_x,
            ScrollAxis::Vertical => &mut self.notches_y,
        };
        *slot = slot.saturating_add(notches);
    }

    /// 累积一条 `wl_pointer.axis_value120`（`value120 = 120` 是一格；v8+）。
    pub(crate) fn note_value120(&mut self, axis: ScrollAxis, value120: i32) {
        let slot = match axis {
            ScrollAxis::Horizontal => &mut self.value120_x,
            ScrollAxis::Vertical => &mut self.value120_y,
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

    /// 本帧的滚动源是否手指 / 触控板（像素单位 + 连续阶段）。
    #[must_use]
    pub(crate) fn is_finger(self) -> bool {
        matches!(self.source, ScrollSource::Finger)
    }

    /// 本批是否没有任何滚动分量（`Frame` 据此跳过空事件）。
    #[must_use]
    pub(crate) fn is_empty(self) -> bool {
        self.notches_x == 0
            && self.notches_y == 0
            && self.value120_x == 0
            && self.value120_y == 0
            && self.continuous_x == 0.0
            && self.continuous_y == 0.0
    }

    /// 清空本批并把滚动源复位成 [`ScrollSource::Unknown`]（`wl_pointer.frame` 结束时调用）。
    ///
    /// **源必须随帧复位**：协议把 `axis_source` 定义为「本帧内事件」的来源信息；不复位就会
    /// 把上一帧的 `finger` 沿用给下一条没有 `axis_source` 的滚轮帧（RB-10）。
    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }

    /// 滚轮：换算成「行」（`(水平, 垂直)`）。
    ///
    /// **每轴独立、三路取一**：该轴有 `axis_discrete` 格数就只用它（精确）；否则用
    /// `axis_value120 / 120`；再否则用连续值 `axis / 10`。三者描述的是同一次滚动，相加会把
    /// 一格算成两行（B8），而忽略 `axis_discrete` 会让 v5–v7 服务端的一格按近似值算出 1.5 行
    /// （RB-10）。
    #[must_use]
    pub(crate) fn wheel_lines(self) -> (f32, f32) {
        (
            lines_for(self.notches_x, self.value120_x, self.continuous_x),
            lines_for(self.notches_y, self.value120_y, self.continuous_y),
        )
    }

    /// 手指 / 触控板：连续值按**像素** 1:1 跟随，`(水平, 垂直)`。
    ///
    /// 连续值为 0 的轴（服务端只发离散值、没发 `axis` 的兼容情况）才把离散值折算成像素
    /// （一格 = 10 个 surface-local 单位）：否则该轴的滚动会在触控板上完全丢失。
    #[must_use]
    pub(crate) fn finger_pixels(self) -> (f32, f32) {
        (
            pixels_for(self.notches_x, self.value120_x, self.continuous_x),
            pixels_for(self.notches_y, self.value120_y, self.continuous_y),
        )
    }
}

/// 单轴滚轮：`axis_discrete` 格数（精确）> `axis_value120` > 连续 `axis`（**不叠加**）。
fn lines_for(notches: i32, value120: i32, continuous: f32) -> f32 {
    if notches != 0 {
        notches as f32
    } else if value120 != 0 {
        value120 as f32 / VALUE120_UNITS_PER_LINE
    } else {
        continuous / CONTINUOUS_AXIS_UNITS_PER_LINE
    }
}

/// 单轴手指：连续值即像素；没有连续值时按「一格 = 10 单位」折算离散值。
fn pixels_for(notches: i32, value120: i32, continuous: f32) -> f32 {
    if continuous != 0.0 {
        continuous
    } else if notches != 0 {
        notches as f32 * CONTINUOUS_AXIS_UNITS_PER_LINE
    } else {
        value120 as f32 / VALUE120_UNITS_PER_LINE * CONTINUOUS_AXIS_UNITS_PER_LINE
    }
}

#[cfg(test)]
mod tests {
    use super::{CONTINUOUS_AXIS_UNITS_PER_LINE, ScrollAccum, ScrollAxis, ScrollSource};

    /// 一格 = `axis_value120 = 120` = 1 行。
    #[test]
    fn discrete_scroll_is_one_line_per_120_units() {
        let mut one_notch = ScrollAccum::default();
        one_notch.note_value120(ScrollAxis::Vertical, 120);
        assert_eq!(one_notch.wheel_lines().1, 1.0, "一格 = 120/120 = 1 行");

        let mut two_notches = ScrollAccum::default();
        two_notches.note_value120(ScrollAxis::Vertical, 240);
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

    /// RB-10：v5–v7 服务端的 `axis_discrete = 1` 是**一格**，同帧的连续 `axis` 是同一格的近似
    /// 表示（mutter/wlroots 实测 15 个单位）⇒ 必须恰好 1 行。
    ///
    /// 修复前只认 `axis_value120` 与连续值，于是 `15 / 10 = 1.5` 行（一格滚出一行半）。
    #[test]
    fn axis_discrete_notches_win_over_the_continuous_approximation() {
        let mut notch = ScrollAccum::default();
        notch.note_notches(ScrollAxis::Vertical, 1);
        notch.note_continuous(ScrollAxis::Vertical, 15.0);
        assert_eq!(
            notch.wheel_lines().1,
            1.0,
            "axis_discrete 的 1 是**一格**，不是 1 个 surface 单位；axis = 15 是同一格的近似值"
        );
    }

    /// 服务端只发 `axis_discrete`（没有连续值 / value120）：仍然按格数换算，不能整轴丢失。
    #[test]
    fn a_v5_server_that_only_sends_axis_discrete_still_yields_lines() {
        let mut accum = ScrollAccum::default();
        accum.note_notches(ScrollAxis::Vertical, 2);
        accum.note_notches(ScrollAxis::Horizontal, -1);
        assert_eq!(
            accum.wheel_lines(),
            (-1.0, 2.0),
            "两格 = 2 行；反向一格 = -1 行（轴方向保留）"
        );
    }

    /// RB-10：三路表示**绝不叠加**。同一格滚动的 `axis_discrete = 1`、`axis_value120 = 120`、
    /// `axis = 10` 同时到达 ⇒ 1 行（若相加是 3 行；若让连续值参与平均则会漂移）。
    #[test]
    fn the_three_representations_never_add_up() {
        let mut notch = ScrollAccum::default();
        notch.note_notches(ScrollAxis::Vertical, 1);
        notch.note_value120(ScrollAxis::Vertical, 120);
        notch.note_continuous(ScrollAxis::Vertical, CONTINUOUS_AXIS_UNITS_PER_LINE);
        assert_eq!(
            notch.wheel_lines().1,
            1.0,
            "格数是精确值：不得加上 value120 与连续值的表示"
        );
    }

    /// B8 核心回归：同一格滚轮同时收到 `axis_value120 = 120` 与 `axis = 10` ⇒ **1 行**。
    ///
    /// 旧实现把两条通路相加（`120/120 + 10/10`）得到 2 行，即「滚一格跳两行」；
    /// 旧测试 `both_axis_paths_accumulate` 把这个错误行为钉成了契约。
    #[test]
    fn wheel_both_channels_yield_one_line() {
        let mut notch = ScrollAccum::default();
        notch.note_value120(ScrollAxis::Vertical, 120);
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
        from_discrete.note_value120(ScrollAxis::Vertical, 30);
        let mut from_continuous = ScrollAccum::default();
        from_continuous.note_continuous(ScrollAxis::Vertical, 2.5);
        let mut both = ScrollAccum::default();
        both.note_value120(ScrollAxis::Vertical, 30);
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
        mixed.note_value120(ScrollAxis::Vertical, 120);
        assert_eq!(
            mixed.wheel_lines(),
            (1.0, 1.0),
            "水平连续 / 垂直离散必须逐轴判定，整帧二选一会丢掉其中一根轴"
        );

        let mut notched = ScrollAccum::default();
        notched.note_notches(ScrollAxis::Horizontal, 1);
        notched.note_value120(ScrollAxis::Vertical, 120);
        assert_eq!(
            notched.wheel_lines(),
            (1.0, 1.0),
            "axis_discrete 与 axis_value120 也必须逐轴各取其一"
        );
    }

    /// 手指（触控板）：连续值就是像素增量，1:1 跟随 —— 同时到达的 v120 不叠加。
    #[test]
    fn finger_uses_continuous_pixels_and_ignores_value120() {
        let mut touchpad = ScrollAccum::default();
        touchpad.note_source(ScrollSource::Finger);
        touchpad.note_continuous(ScrollAxis::Vertical, -24.0);
        touchpad.note_value120(ScrollAxis::Vertical, 120);
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
        touchpad.note_value120(ScrollAxis::Vertical, 120);
        assert_eq!(
            touchpad.finger_pixels().1,
            CONTINUOUS_AXIS_UNITS_PER_LINE,
            "一格 v120 折算成一格的连续单位（10 像素），不能变成 1 像素也不能丢失"
        );
    }

    /// RB-10：v5–v7 服务端只发 `axis_discrete`、没发 `axis` 的手指滚动同样要折算成像素。
    #[test]
    fn finger_falls_back_to_axis_discrete_when_axis_is_missing() {
        let mut touchpad = ScrollAccum::default();
        touchpad.note_notches(ScrollAxis::Vertical, 1);
        assert_eq!(
            touchpad.finger_pixels().1,
            CONTINUOUS_AXIS_UNITS_PER_LINE,
            "一格 axis_discrete 折算成像素（10），不能整轴丢失"
        );
    }

    /// RB-10：源只描述**本帧**。手指帧（像素 1:1）之后 `clear()`（= `wl_pointer.frame` 结束）
    /// ⇒ 下一帧没有 `axis_source` 时按滚轮处理：连续值 15 个单位 ≈ 1.5 行，而不是 15 像素。
    ///
    /// 修复前 `scroll_finger` 长存 ⇒ 手指手势之后的无 source 滚轮帧继续按像素 1:1（约 10×
    /// 灵敏度）。变异检查：让 `clear()` 保留 `source` ⇒ 本测试红。
    #[test]
    fn a_finger_frame_does_not_make_the_next_sourceless_frame_pixels() {
        let mut scroll = ScrollAccum::default();
        scroll.note_source(ScrollSource::Finger);
        scroll.note_continuous(ScrollAxis::Vertical, 24.0);
        assert!(scroll.is_finger(), "本帧有 axis_source = finger");
        assert_eq!(scroll.finger_pixels().1, 24.0, "手指按像素 1:1 跟随");
        scroll.clear();

        assert!(
            !scroll.is_finger(),
            "帧结束后源必须复位成 Unknown（axis_source 只描述本帧）"
        );
        scroll.note_continuous(ScrollAxis::Vertical, 15.0);
        assert_eq!(
            scroll.wheel_lines().1,
            1.5,
            "无 source 的滚轮帧按行换算（15/10），不是 15 像素"
        );
    }

    /// RB-10：`axis_source` 只影响它所在的帧，三个方向都钉住：
    /// 显式 `finger` 帧 → 像素；显式 `wheel` / `continuous` 帧 → 行；没有 source 帧（Unknown）
    /// → 行（安全默认）。
    #[test]
    fn axis_source_covers_only_its_own_frame() {
        for source in [
            ScrollSource::Wheel,
            ScrollSource::Continuous,
            ScrollSource::Unknown,
        ] {
            let mut scroll = ScrollAccum::default();
            scroll.note_source(source);
            scroll.note_continuous(ScrollAxis::Vertical, 20.0);
            assert!(!scroll.is_finger(), "{source:?} 不是手指");
            assert_eq!(
                scroll.wheel_lines().1,
                2.0,
                "{source:?} 必须按行归一（20/10），不得按像素 20"
            );
            scroll.clear();
        }

        // 同一批数据在 finger 帧里是 20 像素，换一帧就回到行单位。
        let mut scroll = ScrollAccum::default();
        scroll.note_source(ScrollSource::Finger);
        scroll.note_continuous(ScrollAxis::Vertical, 20.0);
        assert_eq!(scroll.finger_pixels().1, 20.0);
        scroll.clear();
        assert!(!scroll.is_finger());
    }

    /// 协议：`axis_source` 是**可选**的（未知源不发事件）⇒ 未知源必须是滚轮，不能沿用上一帧
    /// 的 finger。
    #[test]
    fn an_unknown_source_is_a_wheel_not_a_finger() {
        let mut scroll = ScrollAccum::default();
        scroll.note_continuous(ScrollAxis::Vertical, -CONTINUOUS_AXIS_UNITS_PER_LINE);
        assert!(!scroll.is_finger(), "默认 / 无 source 时按滚轮处理");
        assert_eq!(scroll.wheel_lines().1, -1.0);
    }

    /// 一个 frame 里的多条 `axis_value120` 先累积，`Frame` 取走时一次算清
    /// （高精度滚轮会把一格拆成多条事件；`clear()` 之后下一帧从零开始）。
    #[test]
    fn value120_events_accumulate_until_the_frame_is_flushed() {
        let mut scroll = ScrollAccum::default();
        scroll.note_value120(ScrollAxis::Vertical, 60);
        scroll.note_value120(ScrollAxis::Vertical, 60);
        assert_eq!(scroll.wheel_lines().1, 1.0, "两条 60 合起来是一格");
        assert!(!scroll.is_empty());
        scroll.clear();
        assert!(scroll.is_empty(), "clear 之后下一帧从零开始");
        assert_eq!(scroll.wheel_lines(), (0.0, 0.0));
        assert_eq!(scroll.finger_pixels(), (0.0, 0.0));
    }

    /// `axis_discrete` 同样在一帧内累积（协议：每轴每帧最多一条，但多轴 / 拆批都允许）。
    #[test]
    fn axis_discrete_notches_accumulate_until_the_frame_is_flushed() {
        let mut scroll = ScrollAccum::default();
        scroll.note_notches(ScrollAxis::Vertical, 1);
        scroll.note_notches(ScrollAxis::Vertical, -2);
        assert_eq!(scroll.wheel_lines().1, -1.0, "1 + (-2) = -1 格 = -1 行");
        scroll.clear();
        assert!(scroll.is_empty());
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
