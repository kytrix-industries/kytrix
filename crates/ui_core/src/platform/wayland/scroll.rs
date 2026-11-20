//! `wl_pointer` 滚轮/触控板的**事件翻译边界 + 累积与源状态机**（R02-10 / B8 / RB-10 / RBL-01 /
//! RBL-02）。
//!
//! # 三条离散通路是**同一次物理滚动**的不同表示
//!
//! - `axis`：连续值。**协议不规定单位**（`wayland.xml` 的 `wl_pointer.axis` 没有量纲条文）；
//!   「一格滚轮 = 10 个 surface-local 单位」是 libinput / GTK / Qt 的事实约定，mutter/wlroots
//!   常见 10 或 15 —— 客户端**不得依赖**该约定，这里只把它当滚轮通路的后备换算；
//! - `axis_discrete`（v5–v7）：**整数格数**（`discrete` 1 = 一格）。协议原文 *"This event
//!   carries the axis value of the wl_pointer.axis event in discrete steps (e.g. mouse wheel
//!   clicks)"*，且 *"each axis_discrete event is always followed by exactly one axis event with
//!   the same axis number within the same wl_pointer.frame"*；v8 起不再发给支持 v8 的客户端；
//! - `axis_value120`（v8+）：高精度离散步长，一格 = 120（与 Win32 `WHEEL_DELTA` 同量纲）。
//!
//! 三者**不能相加**（描述同一次滚动），也不该让低版本服务端落到近似换算：
//! [`ScrollAccum::wheel_lines`] 逐轴按 `axis_discrete`（格数，精确）> `axis_value120`（/120）>
//! 连续 `axis`（/约定值）取第一个非零值。
//!
//! **v8+ 用 `axis_value120` 取代 `axis_discrete` 的依据**（NBL-04）：`wayland.xml:2481-2482`
//! 原文 *"This event replaces the wl_pointer.axis_discrete event in clients supporting
//! wl_pointer version 8 or later"*；同文件 `:2484-2486` *"Where a wl_pointer.axis_source event
//! occurs in the same wl_pointer.frame, the axis source applies to this event"*，与
//! `axis_source` 的帧作用域条文（`:2377-2404`）共同支持「v8+ 服务端发 value120、v5–v7 服务端发
//! axis_discrete、二者与连续 axis 是同一次滚动的不同表示」这一模型 —— 因此三条通路在本模块里
//! 是**同一量纲族的三选一**，而不是三次独立滚动。
//!
//! # 滚动源（`axis_source`）是**手势作用域**，离散通道是滚轮的充分证据（RBL-02）
//!
//! 协议（`wayland.xml` 的 `axis_source`）：*"It is sent before a wl_pointer.frame event and
//! carries the source information for all events within that frame"*；*"This event is optional.
//! If the source is unknown for a particular axis event **sequence**, no event is sent"*；
//! *"Only one wl_pointer.axis_source event is permitted per frame"*。即：**若发**，它只描述本帧；
//! 但协议**没有**「每帧必发」的保证 —— 只在手势首帧发一次的实现是合法的。
//!
//! 因此源按**手势**而不是按帧建模（[`PointerScroll`]）：
//!
//! 1. `axis_source` 到达即更新为权威源；源**保持到整个手势结束**
//!    （finger 手势协议保证以 `axis_stop` 结束：*"If the source is wl_pointer.axis_source.finger,
//!    a wl_pointer.axis_stop event will be sent when the user lifts the finger off the device"*）。
//!    这样「finger 手势内某些帧没发 source」仍按 finger 处理；
//! 2. **离散通道是滚轮的充分证据**：一帧里出现 `axis_discrete` / `axis_value120` ⇒ 该帧一定
//!    是离散滚轮（触控板 / 连续设备不发离散通道：`axis_discrete` 原文 *"continuous scrolling
//!    devices like two-finger scrolling on touchpads do not have discrete steps and do not
//!    generate this event"*），即使上一手势的源还是 finger 也按滚轮算 —— 这条把「finger 之后
//!    无 source 的滚轮帧」彻底堵死；
//! 3. 帧里既无 source 也无离散通道（纯连续 `axis`）⇒ 沿用最后一次已知源，未知则按**滚轮**
//!    （不发 `axis_source` 的服务端上绝大多数输入是滚轮；把未知源当手指会把一格 10–15 单位
//!    当 10–15 px，约 10× 灵敏度）；
//! 4. **`axis_stop` 只停一根轴**（NBL-01）：`axis_stop{axis}` 的语义是「这根轴的 axis 序列
//!    结束」（同文件 `axis_stop` 原文：*"Any wl_pointer.axis events with the same axis_source
//!    after this event should be considered as the start of a new axis motion"*），而对角滚动时
//!    合成器可以**先停一根轴、另一根继续**（`wl_pointer.frame` 原文：*"When a wl_pointer.axis
//!    and a wl_pointer.axis_stop event occur within the same frame, this indicates that axis
//!    movement in one axis has stopped but continues in the other axis"*）。所以本模块维护
//!    「手势内仍活跃的轴集合」：**只有全部轴都停**才产出一条零增量 `End` 并在帧收尾复位源；
//!    任一轴仍活跃时 stop 只把该轴移出集合，继续轴（同帧 / 后续帧）仍按当前源解释 —— 否则
//!    触控板对角滚动会在中途掉回滚轮单位（约 10× 灵敏度）且提前给应用发 `End`；
//! 5. **换源兜底**：`axis_stop` 对 wheel / continuous 源可能根本不发（协议：*"clients must not
//!    rely on receiving an axis_stop event for these sources"*），若某合成器只停一根轴就再也
//!    不发 stop，活跃集合会永远非空。此时**下一次 `axis_source` 与当前源不同**视为新的 axis
//!    序列：若旧手势还有活跃轴，先产出一条零增量 `End` 收尾（客户端兜底；协议没有「换源 =
//!    手势结束」的条文，但不收尾会让应用永远等不到 End），再清空活跃集合、更新源。
//!
//! `Continuous`（协议 `continuous`，按钮滚动等）当前与 `Wheel` 同义（都按行归一），保留该变体
//! 只为忠实映射协议枚举。
//!
//! # 为什么放在 `platform/wayland/`
//!
//! `platform/linux/**` 只在 Linux 目标编译，开发机（Windows）的 `cargo test` 覆盖不到；而
//! 「协议事件 → 累积器 → 单位/相位/帧收尾」这条边界必须能被独立验证（RBL-01）。本模块定义
//! 自己的纯枚举 [`ScrollEvent`]：`platform/linux/surface.rs` 的 `Dispatch<wl_pointer>` 只做
//! 「`wl_pointer::Event` → [`ScrollEvent`]」的**机械翻译**（该翻译是 Linux-only 的几行匹配，
//! 在 `surface.rs` 的 Linux 单测里覆盖），所有判定都在 [`PointerScroll::apply`] 里（Windows +
//! Linux 单测）。

use crate::event::ScrollPhase;

/// 连续 `axis` 的单位 → 行换算（**约定**，不是协议规定：libinput / GTK / Qt 的一格约定，
/// mutter/wlroots 常见 10 或 15；客户端不得依赖该值）。
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
/// 都按离散滚轮处理。`Wheel` / `Continuous` / `Unknown` 目前行为相同（都按行），保留前两者
/// 只为忠实映射协议枚举（协议里 `continuous` 是「非手指的连续坐标设备」，未来若要给它单独
/// 单位再启用）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ScrollSource {
    /// 还没有任何 `axis_source`（源未知）⇒ 按滚轮处理（安全默认）。
    #[default]
    Unknown,
    /// 物理滚轮（协议 `wheel` / `wheel_tilt`）：离散格。
    Wheel,
    /// 手指 / 触控板（协议 `finger`）：连续像素增量，1:1 跟随。
    Finger,
    /// 非手指的连续坐标设备（协议 `continuous`）：当前与 [`Self::Wheel`] 同义（按行）。
    Continuous,
}

/// 一个 `wl_pointer` 滚动相关事件在纯逻辑层的表示。
///
/// `platform/linux/surface.rs` 的 `Dispatch<wl_pointer>` 只负责把协议事件**机械翻译**成这个
/// 枚举；单位、相位、源作用域、帧收尾全部由 [`PointerScroll::apply`] 决定（RBL-01）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ScrollEvent {
    /// 本帧的 `axis_source`（权威源，保持到 `axis_stop` 所在帧收尾）。
    Source(ScrollSource),
    /// `axis_discrete`：`notches` 是**格数**（v5–v7）。
    Discrete {
        /// 轴。
        axis: ScrollAxis,
        /// 格数（1 = 一格）。
        notches: i32,
    },
    /// `axis_value120`：`value120 = 120` 是一格（v8+）。
    Value120 {
        /// 轴。
        axis: ScrollAxis,
        /// 1/120 行。
        value120: i32,
    },
    /// 连续 `axis`（surface-local 单位；手指手势即像素）。
    Continuous {
        /// 轴。
        axis: ScrollAxis,
        /// 连续值。
        value: f32,
    },
    /// `axis_stop`：**一根轴**的 axis 序列结束（协议事件自带轴号）。
    Stop {
        /// 停止的轴（只停这一根，另一根可以继续）。
        axis: ScrollAxis,
    },
    /// `wl_pointer.frame`：本批事件收尾。
    Frame,
}

/// 一次帧收尾要投递的滚动事件（由 [`PointerScroll::apply`] 产出）。
///
/// `platform/linux/surface.rs` 把它原样装进 `InputEvent::Scroll`（取反后的 delta、单位、相位）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ScrollOutput {
    /// `(水平, 垂直)`，已按引擎约定取反（与 `InputEvent::Scroll.delta` 同向）。
    pub delta: (f32, f32),
    /// `true` = 像素单位（手指）；`false` = 行单位（滚轮 / 未知源）。
    pub finger: bool,
    /// 相位：滚轮是 `Discrete`，手指手势是 `Begin` / `Update` / `End`。
    pub phase: ScrollPhase,
}

/// 一次帧内的滚动增量（纯换算，不含源）。
///
/// 每根轴各存三份（同一物理滚动的三种表示）：连续 `axis`（surface-local 单位）、
/// `axis_value120`（1/120 行）、`axis_discrete`（**格**）。[`Self::wheel_lines`] 按轴三选一、
/// **绝不相加**。
#[derive(Clone, Copy, Debug, Default)]
struct ScrollAccum {
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
    /// 累積一條 `wl_pointer.axis_discrete`（`notches` = **格数**，1 = 一格；v5–v7）。
    ///
    /// 与 [`Self::note_value120`] 是**不同量纲**：这里 1 = 一格，value120 的 120 = 一格。
    /// 协议规定同一帧同一轴最多一条 `axis_discrete`（*"A wl_pointer.frame must not contain more
    /// than one axis_discrete event per axis type"*）；这里的 `saturating_add` 只是**防御性**
    /// 实现（不 panic / 不溢出），不是把非法输入固化成契约（NBL-02）。
    fn note_notches(&mut self, axis: ScrollAxis, notches: i32) {
        let slot = match axis {
            ScrollAxis::Horizontal => &mut self.notches_x,
            ScrollAxis::Vertical => &mut self.notches_y,
        };
        *slot = slot.saturating_add(notches);
    }

    /// 累积一条 `wl_pointer.axis_value120`（`value120 = 120` 是一格；v8+）。
    fn note_value120(&mut self, axis: ScrollAxis, value120: i32) {
        let slot = match axis {
            ScrollAxis::Horizontal => &mut self.value120_x,
            ScrollAxis::Vertical => &mut self.value120_y,
        };
        *slot = slot.saturating_add(value120);
    }

    /// 累积一条 `wl_pointer.axis`（连续值）。
    fn note_continuous(&mut self, axis: ScrollAxis, value: f32) {
        let slot = match axis {
            ScrollAxis::Horizontal => &mut self.continuous_x,
            ScrollAxis::Vertical => &mut self.continuous_y,
        };
        *slot += value;
    }

    /// 本帧是否带离散通道（`axis_discrete` / `axis_value120` 任一非零）。
    fn has_discrete(self) -> bool {
        self.notches_x != 0 || self.notches_y != 0 || self.value120_x != 0 || self.value120_y != 0
    }

    /// 本帧是否没有任何滚动分量。
    fn is_empty(self) -> bool {
        !self.has_discrete() && self.continuous_x == 0.0 && self.continuous_y == 0.0
    }

    /// 清空本帧的增量（帧收尾与 `PointerScroll` 的源状态无关：源由后者按手势管理）。
    fn clear(&mut self) {
        *self = Self::default();
    }

    /// 滚轮：换算成「行」（`(水平, 垂直)`）。
    ///
    /// **每轴独立、三路取一**：该轴有 `axis_discrete` 格数就只用它（精确）；否则用
    /// `axis_value120 / 120`；再否则用连续值 `/ 约定值`。三者描述同一次滚动，相加会把一格算成
    /// 两行（B8），忽略 `axis_discrete` 会让 v5–v7 服务端的一格落到近似换算（RB-10）。
    fn wheel_lines(self) -> (f32, f32) {
        (
            lines_for(self.notches_x, self.value120_x, self.continuous_x),
            lines_for(self.notches_y, self.value120_y, self.continuous_y),
        )
    }

    /// 手指 / 触控板：连续值就是**像素**增量，1:1 跟随。
    ///
    /// 离散通道只会出现在滚轮帧（见模块文档第 2 条），所以这里不需要离散回退。
    fn finger_pixels(self) -> (f32, f32) {
        (self.continuous_x, self.continuous_y)
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

/// 滚动输入的累积 + 手势源状态机：`wl_pointer` 滚动事件 → 每次帧收尾至多一条滚动输出。
///
/// 生命周期与 `WaylandState` 一致（一个 seat 的指针滚动状态）。帧与帧之间的相位/源状态都在
/// 这里，驱动层不再持有任何滚动语义（RBL-01）。
#[derive(Default)]
pub(crate) struct PointerScroll {
    /// 本帧增量（`Frame` 收尾时清空）。
    accum: ScrollAccum,
    /// 最近一次已知源：`axis_source` 到达即更新；整个手势结束（全部轴 `axis_stop`）后复位成
    /// `Unknown`（模块文档第 1、4 条）。
    source: ScrollSource,
    /// 本帧「全部轴都已停」：帧收尾时先按旧源解释同帧 axis，再复位源 / 清空活跃轴（第 4 条）。
    stop_pending: bool,
    /// 手指手势是否进行中（已产出 `Begin` / `Update`，全部轴停止后结束）。
    active: bool,
    /// 手势内仍活跃的轴：收到过滚动数据、且尚未收到它的 `axis_stop`（NBL-01）。
    ///
    /// 对角滚动时合成器可以只停一根轴、另一根继续；只有集合清空才算手势结束。
    horizontal_active: bool,
    vertical_active: bool,
}

impl PointerScroll {
    /// 喂入一个滚动事件，返回**本事件当场要投递的**滚动输出。
    ///
    /// - `Source`：更新权威源；源变化且旧手势还有活跃轴时先产出零增量 `End`（换源兜底，第 5 条）；
    /// - `Discrete` / `Value120` / `Continuous`：累积并把该轴标记为活跃，返回 `None`；
    /// - `Stop { axis }`：只把该轴移出活跃集合；集合非空 ⇒ `None`（另一根轴继续），集合空 ⇒
    ///   零增量 `End` 并登记「帧收尾复位源」；
    /// - `Frame`：产出本批的滚动输出（没有分量时 `None`），清空增量；本帧全部轴停止时在收尾
    ///   复位源并清空活跃轴。
    pub(crate) fn apply(&mut self, event: ScrollEvent) -> Option<ScrollOutput> {
        match event {
            ScrollEvent::Source(source) => {
                let changed = source != self.source;
                // `axis_source` 描述**随后**的 axis 事件（协议原文见模块文档）；换源意味着新的
                // axis 序列（可能是另一台设备）。若旧手势还有活跃轴（某些源根本不发 axis_stop，
                // 或合成器只停了一根轴），必须先收尾，否则应用永远等不到 `End`（第 5 条）。
                let output = if changed && (self.horizontal_active || self.vertical_active) {
                    let finger = self.frame_is_finger();
                    self.active = false;
                    Some(ScrollOutput {
                        delta: (0.0, 0.0),
                        finger,
                        phase: ScrollPhase::End,
                    })
                } else {
                    None
                };
                if changed {
                    // 新序列开始：旧序列的活跃轴 / 待复位标记不再适用（新序列若还要 stop，
                    // 会重新登记自己的集合与 pending）。
                    self.horizontal_active = false;
                    self.vertical_active = false;
                    self.stop_pending = false;
                }
                self.source = source;
                output
            }
            ScrollEvent::Discrete { axis, notches } => {
                self.accum.note_notches(axis, notches);
                self.mark_axis_active(axis);
                None
            }
            ScrollEvent::Value120 { axis, value120 } => {
                self.accum.note_value120(axis, value120);
                self.mark_axis_active(axis);
                None
            }
            ScrollEvent::Continuous { axis, value } => {
                self.accum.note_continuous(axis, value);
                self.mark_axis_active(axis);
                None
            }
            ScrollEvent::Stop { axis } => {
                // `axis_stop` 只停**一根轴**：对角滚动时另一根轴可以继续（协议 frame 原文见模块
                // 文档第 4 条）。只要还有活跃轴就不产 `End`、不复位源，同帧 / 后续帧的 axis
                // 继续按当前源解释 —— 否则触控板对角滚动中途会掉回滚轮（约 10× 灵敏度）。
                match axis {
                    ScrollAxis::Horizontal => self.horizontal_active = false,
                    ScrollAxis::Vertical => self.vertical_active = false,
                }
                if self.horizontal_active || self.vertical_active {
                    return None;
                }
                // 全部轴都停了 ⇒ 手势结束：零增量 End（单位取尚未复位的源），帧收尾复位源。
                self.active = false;
                self.stop_pending = true;
                Some(ScrollOutput {
                    delta: (0.0, 0.0),
                    finger: self.frame_is_finger(),
                    phase: ScrollPhase::End,
                })
            }
            ScrollEvent::Frame => self.flush_frame(),
        }
    }

    /// 帧收尾：产出本批滚动输出、清空增量，并在本帧全部轴停止时复位源与活跃集合。
    fn flush_frame(&mut self) -> Option<ScrollOutput> {
        let stop_pending = std::mem::take(&mut self.stop_pending);
        let finger = self.frame_is_finger();
        let output = if self.accum.is_empty() {
            None
        } else {
            let (x, y) = if finger {
                self.accum.finger_pixels()
            } else {
                self.accum.wheel_lines()
            };
            let phase = if finger {
                let phase = if self.active {
                    ScrollPhase::Update
                } else {
                    ScrollPhase::Begin
                };
                self.active = true;
                phase
            } else {
                ScrollPhase::Discrete
            };
            // 协议轴方向与引擎 scroll delta 方向相反 ⇒ 这里统一取反（与旧实现一致）。
            Some(ScrollOutput {
                delta: (-x, -y),
                finger,
                phase,
            })
        };
        self.accum.clear();
        if stop_pending {
            // 整个手势结束（全部轴都 stop 过）⇒ 手势作用域收尾：之后的帧没有新 source 时从
            // `Unknown`（= 滚轮）重新开始，不会把上一手势的 finger 泄漏到滚轮帧（RB-10 的
            // 原始缺陷）。活跃集合清空：同帧最后一条 axis 之后到达的数据属于新序列。
            self.source = ScrollSource::Unknown;
            self.horizontal_active = false;
            self.vertical_active = false;
        }
        output
    }

    /// 把该轴标记为手势内活跃（收到过滚动数据）。
    fn mark_axis_active(&mut self, axis: ScrollAxis) {
        match axis {
            ScrollAxis::Horizontal => self.horizontal_active = true,
            ScrollAxis::Vertical => self.vertical_active = true,
        }
    }

    /// 本帧按手指（像素）解释吗：**离散通道是滚轮的充分证据**，否则看手势源。
    ///
    /// 这条规则同时覆盖两种历史缺陷：v5–v7 的 `axis_discrete` 帧不再被近似换算成 1.5 行；
    /// finger 手势之后的滚轮帧即使没有 `axis_source` 也不会沿用像素单位。
    fn frame_is_finger(&self) -> bool {
        !self.accum.has_discrete() && matches!(self.source, ScrollSource::Finger)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CONTINUOUS_AXIS_UNITS_PER_LINE, PointerScroll, ScrollAccum, ScrollAxis, ScrollEvent,
        ScrollPhase, ScrollSource,
    };

    /// 一个 finger 帧的便捷构造（源 + 垂直连续值 + 帧收尾）。
    fn finger_frame(value: f32) -> Vec<ScrollEvent> {
        vec![
            ScrollEvent::Source(ScrollSource::Finger),
            ScrollEvent::Continuous {
                axis: ScrollAxis::Vertical,
                value,
            },
            ScrollEvent::Frame,
        ]
    }

    // ── 纯换算（ScrollAccum）──────────────────────────────────────────────────────────

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

    /// v5 服务端只发 `axis`：一格约定值（10 单位）= 1 行，而不是 10/120 ≈ 0.083 行。
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
    /// 表示（按约定常见 10–15 个单位）⇒ 必须恰好 1 行。
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
    /// 旧实现把两条通路相加（`120/120 + 10/10`）得到 2 行，即「滚一格跳两行」。
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

    /// 手指通路只按连续值 1:1：离散通道的存在与否不改变换算（离散帧永远不会走这条通路，
    /// 见 [`PointerScroll::frame_is_finger`] 的规则）。
    #[test]
    fn finger_pixels_are_the_continuous_values() {
        let mut touchpad = ScrollAccum::default();
        touchpad.note_continuous(ScrollAxis::Vertical, -24.0);
        assert_eq!(
            touchpad.finger_pixels().1,
            -24.0,
            "触控板按像素 1:1（不是行）"
        );
        // 与滚轮换算是两套单位：同一批数据在滚轮通路上是行。
        touchpad.note_value120(ScrollAxis::Vertical, 120);
        assert_eq!(touchpad.wheel_lines().1, 1.0, "滚轮通路用离散值 = 1 行");
    }

    /// 一个 frame 里的多条 `axis_value120` 先累积，`Frame` 取走时一次算清
    /// （高精度滚轮会把一格拆成多条事件；清空之后下一帧从零开始）。
    #[test]
    fn value120_events_accumulate_until_the_frame_is_flushed() {
        let mut scroll = ScrollAccum::default();
        scroll.note_value120(ScrollAxis::Vertical, 60);
        scroll.note_value120(ScrollAxis::Vertical, 60);
        assert_eq!(scroll.wheel_lines().1, 1.0, "两条 60 合起来是一格");
        assert!(!scroll.is_empty());
        let mut device = PointerScroll::default();
        assert_eq!(
            device.apply(ScrollEvent::Frame),
            None,
            "没有分量时不产出事件"
        );
        scroll.clear();
        assert!(scroll.is_empty(), "clear 之后下一帧从零开始");
        assert_eq!(scroll.wheel_lines(), (0.0, 0.0));
        assert_eq!(scroll.finger_pixels(), (0.0, 0.0));
    }

    /// `axis_discrete` 在同一帧内**同一轴**不得出现两条（协议原文：*"A wl_pointer.frame must not
    /// contain more than one axis_discrete event per axis type"*）—— 非法输入不需要契约，因此这里
    /// 没有「同轴两条离散值累加」的测试；`note_notches` 的饱和加法只是防御（不 panic / 不溢出），
    /// 不是把非法输入固化成行为约定（NBL-02）。
    #[test]
    fn per_axis_discrete_notches_are_single_events_by_protocol() {
        let mut scroll = ScrollAccum::default();
        scroll.note_notches(ScrollAxis::Vertical, 1);
        scroll.note_notches(ScrollAxis::Horizontal, -1);
        assert_eq!(
            scroll.wheel_lines(),
            (-1.0, 1.0),
            "两根轴各一条（合法输入）"
        );
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

    // ── 接线（PointerScroll：事件 → 累积器 → 单位/相位/帧收尾）──────────────────────────

    /// RBL-01 ①：`Source(Finger)` + 连续值 ⇒ 像素单位 + `Begin`；下一帧 ⇒ `Update`（手势延续）。
    #[test]
    fn a_finger_frame_yields_pixels_and_keeps_the_gesture_open() {
        let mut scroll = PointerScroll::default();
        let mut outputs = Vec::new();
        for event in finger_frame(-24.0) {
            if let Some(output) = scroll.apply(event) {
                outputs.push(output);
            }
        }
        assert_eq!(outputs.len(), 1, "一个 frame 只产出一条滚动事件");
        assert_eq!(
            outputs[0].delta,
            (0.0, 24.0),
            "手指按像素 1:1，方向取反（协议正方向 ↔ 引擎 delta）"
        );
        assert!(outputs[0].finger, "finger 源 ⇒ 像素单位");
        assert_eq!(outputs[0].phase, ScrollPhase::Begin);

        // 手势延续：同一手势内第二帧没有 `axis_source`（协议只保证「若发则只描述本帧」）。
        assert_eq!(
            scroll.apply(ScrollEvent::Continuous {
                axis: ScrollAxis::Vertical,
                value: -12.0,
            }),
            None
        );
        let update = scroll
            .apply(ScrollEvent::Frame)
            .expect("第二帧必须产出滚动事件");
        assert_eq!(update.delta, (0.0, 12.0));
        assert!(update.finger, "手势内没有 source 的帧仍按 finger 处理");
        assert_eq!(update.phase, ScrollPhase::Update);
    }

    /// RBL-02 ②：一帧出现 `axis_value120` ⇒ 该帧是滚轮（行单位、`Discrete`），**即使**手势源
    /// 仍是 finger（触控板不发离散通道）。
    #[test]
    fn a_value120_frame_is_a_wheel_even_inside_a_finger_gesture() {
        let mut scroll = PointerScroll::default();
        for event in finger_frame(-24.0) {
            scroll.apply(event);
        }
        // 同一手势内（没有 axis_stop）来了一帧 value120。
        assert_eq!(
            scroll.apply(ScrollEvent::Value120 {
                axis: ScrollAxis::Vertical,
                value120: 120,
            }),
            None
        );
        let output = scroll
            .apply(ScrollEvent::Frame)
            .expect("离散帧必须产出滚动事件");
        assert_eq!(output.delta, (0.0, -1.0), "120/120 = 1 行");
        assert!(
            !output.finger,
            "离散通道是滚轮的充分证据：不得沿用上一手势的 finger"
        );
        assert_eq!(output.phase, ScrollPhase::Discrete);
    }

    /// RBL-02 ② / RB-10：`axis_discrete` 帧同样按滚轮（行），格数直接进输出 —— 这是 v5–v7
    /// 服务端一格一行的接线证据（`note_notches` 被真正调用）。
    #[test]
    fn an_axis_discrete_frame_is_a_wheel_and_carries_its_notches() {
        let mut scroll = PointerScroll::default();
        assert_eq!(
            scroll.apply(ScrollEvent::Discrete {
                axis: ScrollAxis::Vertical,
                notches: 1,
            }),
            None
        );
        // 同帧的连续近似值（常见 15）不得让它变成 1.5 行。
        scroll.apply(ScrollEvent::Continuous {
            axis: ScrollAxis::Vertical,
            value: 15.0,
        });
        let output = scroll
            .apply(ScrollEvent::Frame)
            .expect("一格滚轮必须产出一条滚动事件");
        assert_eq!(output.delta, (0.0, -1.0), "axis_discrete = 1 格 = 1 行");
        assert!(!output.finger);
        assert_eq!(output.phase, ScrollPhase::Discrete);
    }

    /// RBL-02 ③：一帧既无 source 也无离散通道（纯连续 `axis`）⇒ 沿用最后一次已知源（finger）。
    #[test]
    fn a_sourceless_continuous_frame_keeps_the_last_known_source() {
        let mut scroll = PointerScroll::default();
        scroll.apply(ScrollEvent::Source(ScrollSource::Finger));
        scroll.apply(ScrollEvent::Continuous {
            axis: ScrollAxis::Vertical,
            value: 30.0,
        });
        let first = scroll.apply(ScrollEvent::Frame).expect("第一帧必须有输出");
        assert!(first.finger);

        // 第二帧完全没有 axis_source（协议允许「每 sequence 只发一次」）。
        scroll.apply(ScrollEvent::Continuous {
            axis: ScrollAxis::Vertical,
            value: -20.0,
        });
        let second = scroll.apply(ScrollEvent::Frame).expect("第二帧必须有输出");
        assert!(
            second.finger,
            "纯连续帧没有新 source ⇒ 沿用最后一次已知源（finger），不得退化成滚轮"
        );
        assert_eq!(second.delta, (0.0, 20.0));
        assert_eq!(second.phase, ScrollPhase::Update);
    }

    /// RBL-02 ①/④（NBL-01 之后仍是单轴语义）：单轴 finger 手势的 `axis_stop{axis}` 产出零增量
    /// `End`（单位按当时源），帧收尾后源复位 ⇒ 下一条没有 source 的纯连续帧按滚轮（行）处理
    /// （原始 RB-10 缺陷的回归防线）。
    #[test]
    fn a_single_axis_gesture_ends_with_one_end_and_resets_the_source() {
        let mut scroll = PointerScroll::default();
        for event in finger_frame(-24.0) {
            scroll.apply(event);
        }
        let end = scroll
            .apply(ScrollEvent::Stop {
                axis: ScrollAxis::Vertical,
            })
            .expect("最后一根轴停止 ⇒ 当场产出 End");
        assert_eq!(end.delta, (0.0, 0.0), "End 是零增量事件");
        assert!(end.finger, "finger 手势的 End 仍是像素单位");
        assert_eq!(end.phase, ScrollPhase::End);
        assert_eq!(scroll.apply(ScrollEvent::Frame), None, "stop 帧没有数据");

        // 手势结束后的无 source 帧：按滚轮（行），不是像素 —— 旧实现在这里约 10× 灵敏度。
        scroll.apply(ScrollEvent::Continuous {
            axis: ScrollAxis::Vertical,
            value: 15.0,
        });
        let wheel = scroll
            .apply(ScrollEvent::Frame)
            .expect("滚轮帧必须产出事件");
        assert!(!wheel.finger, "全部轴 stop 之后源复位为 Unknown（按滚轮）");
        assert_eq!(wheel.delta, (0.0, -1.5), "15 / 约定 10 = 1.5 行");
        assert_eq!(wheel.phase, ScrollPhase::Discrete);
    }

    /// NBL-01 ①：对角手势先停一根轴、另一根继续 ⇒ **不产 `End`、不复位源**，继续轴在后续帧
    /// 仍按 finger 像素解释（变异「任意 stop 即整手势结束」⇒ 本测试红）。
    #[test]
    fn stopping_one_axis_keeps_the_two_axis_gesture_alive() {
        let mut scroll = PointerScroll::default();
        scroll.apply(ScrollEvent::Source(ScrollSource::Finger));
        // 第一帧：两轴都在动（水平 -12、垂直 -24）。
        scroll.apply(ScrollEvent::Continuous {
            axis: ScrollAxis::Horizontal,
            value: -12.0,
        });
        scroll.apply(ScrollEvent::Continuous {
            axis: ScrollAxis::Vertical,
            value: -24.0,
        });
        let first = scroll.apply(ScrollEvent::Frame).expect("第一帧必须有输出");
        assert!(first.finger);
        assert_eq!(first.delta, (12.0, 24.0));

        // 垂直轴先停：水平轴还活跃 ⇒ 不得提前结束手势。
        assert_eq!(
            scroll.apply(ScrollEvent::Stop {
                axis: ScrollAxis::Vertical,
            }),
            None,
            "还有轴在动：stop 不得产出 End、也不得复位源"
        );
        scroll.apply(ScrollEvent::Continuous {
            axis: ScrollAxis::Horizontal,
            value: -6.0,
        });
        let continuing = scroll
            .apply(ScrollEvent::Frame)
            .expect("继续轴必须产出事件");
        assert!(
            continuing.finger,
            "继续的那根轴必须仍按手势源（finger）解释，不得掉回滚轮"
        );
        assert_eq!(continuing.delta, (6.0, 0.0));
        assert_eq!(continuing.phase, ScrollPhase::Update, "手势仍在进行");

        // 最后一根轴停下才算结束。
        let end = scroll
            .apply(ScrollEvent::Stop {
                axis: ScrollAxis::Horizontal,
            })
            .expect("全部轴停止 ⇒ 产出 End");
        assert_eq!(end.phase, ScrollPhase::End);
        assert!(end.finger);
    }

    /// NBL-01 ②：两轴都停 ⇒ **恰好一条** `End`，帧收尾后源复位（下一条无 source 帧按滚轮）。
    #[test]
    fn stopping_both_axes_produces_exactly_one_end_and_resets_the_source() {
        let mut scroll = PointerScroll::default();
        scroll.apply(ScrollEvent::Source(ScrollSource::Finger));
        scroll.apply(ScrollEvent::Continuous {
            axis: ScrollAxis::Horizontal,
            value: -10.0,
        });
        scroll.apply(ScrollEvent::Continuous {
            axis: ScrollAxis::Vertical,
            value: -20.0,
        });
        scroll.apply(ScrollEvent::Frame);

        assert_eq!(
            scroll.apply(ScrollEvent::Stop {
                axis: ScrollAxis::Horizontal,
            }),
            None,
            "还剩垂直轴 ⇒ 不产 End"
        );
        let end = scroll
            .apply(ScrollEvent::Stop {
                axis: ScrollAxis::Vertical,
            })
            .expect("最后一根轴停止 ⇒ 产出一条 End");
        assert_eq!(end.phase, ScrollPhase::End);
        assert_eq!(scroll.apply(ScrollEvent::Frame), None, "End 不重复");

        scroll.apply(ScrollEvent::Continuous {
            axis: ScrollAxis::Vertical,
            value: -15.0,
        });
        let wheel = scroll
            .apply(ScrollEvent::Frame)
            .expect("后续帧必须产出事件");
        assert!(!wheel.finger, "全部轴 stop 之后源复位 ⇒ 按滚轮");
        assert_eq!(wheel.delta, (0.0, 1.5));
        assert_eq!(wheel.phase, ScrollPhase::Discrete);
    }

    /// NBL-01 ④：`axis_stop` 只停一根轴；同帧另一根轴的 `axis` 属于**同一个手势**，仍按停止前
    /// 的源解释（协议：一帧里 axis + axis_stop = 一根轴停、另一根继续）。
    #[test]
    fn axis_stop_with_a_continuing_axis_in_the_same_frame_keeps_the_pre_stop_source() {
        let mut scroll = PointerScroll::default();
        scroll.apply(ScrollEvent::Source(ScrollSource::Finger));
        // 第一帧两轴都在动 ⇒ 两轴都进入活跃集合。
        scroll.apply(ScrollEvent::Continuous {
            axis: ScrollAxis::Horizontal,
            value: -9.0,
        });
        scroll.apply(ScrollEvent::Continuous {
            axis: ScrollAxis::Vertical,
            value: -4.0,
        });
        scroll.apply(ScrollEvent::Frame);

        // 同一帧：水平轴 stop，垂直轴还在动。
        assert_eq!(
            scroll.apply(ScrollEvent::Stop {
                axis: ScrollAxis::Horizontal,
            }),
            None,
            "垂直轴仍活跃 ⇒ 不产 End"
        );
        scroll.apply(ScrollEvent::Continuous {
            axis: ScrollAxis::Vertical,
            value: -18.0,
        });
        let output = scroll
            .apply(ScrollEvent::Frame)
            .expect("继续轴必须产出事件");
        assert!(
            output.finger,
            "axis_stop 同帧的 axis 属于停止前的手势（不得错判成滚轮）"
        );
        assert_eq!(output.delta, (0.0, 18.0));
        assert_eq!(output.phase, ScrollPhase::Update);
    }

    /// NBL-01 兜底：某合成器只停一根轴就再也不发 stop ⇒ 下一次**换源**视为新手势信号，
    /// 先用零增量 `End` 收尾旧手势，再按新源开始（否则应用永远等不到 End）。
    #[test]
    fn a_new_source_closes_a_gesture_whose_remaining_axis_never_stops() {
        let mut scroll = PointerScroll::default();
        scroll.apply(ScrollEvent::Source(ScrollSource::Finger));
        scroll.apply(ScrollEvent::Continuous {
            axis: ScrollAxis::Horizontal,
            value: -12.0,
        });
        scroll.apply(ScrollEvent::Continuous {
            axis: ScrollAxis::Vertical,
            value: -12.0,
        });
        scroll.apply(ScrollEvent::Frame);
        // 只停水平轴，垂直轴永远不再收到 stop。
        assert_eq!(
            scroll.apply(ScrollEvent::Stop {
                axis: ScrollAxis::Horizontal,
            }),
            None
        );
        // 下一次滚轮输入带来新源 ⇒ 兜底收尾旧手势。
        let end = scroll
            .apply(ScrollEvent::Source(ScrollSource::Wheel))
            .expect("换源必须收尾旧手势（零增量 End）");
        assert_eq!(end.phase, ScrollPhase::End);
        assert!(end.finger, "旧手势是 finger ⇒ End 仍按像素单位");

        // 新序列按新源（滚轮）解释。
        scroll.apply(ScrollEvent::Value120 {
            axis: ScrollAxis::Vertical,
            value120: 120,
        });
        let wheel = scroll
            .apply(ScrollEvent::Frame)
            .expect("新序列必须产出事件");
        assert!(!wheel.finger);
        assert_eq!(wheel.delta, (0.0, -1.0));
        assert_eq!(wheel.phase, ScrollPhase::Discrete);
    }

    /// NBL-01：对一根**从未动过**的轴发 `axis_stop` 不得结束仍在进行的手势。
    #[test]
    fn an_axis_stop_for_an_axis_that_never_moved_does_not_end_the_gesture() {
        let mut scroll = PointerScroll::default();
        for event in finger_frame(-24.0) {
            scroll.apply(event);
        }
        assert_eq!(
            scroll.apply(ScrollEvent::Stop {
                axis: ScrollAxis::Horizontal,
            }),
            None,
            "水平轴从未活跃：它的 stop 不得结束垂直轴的手势"
        );
        let end = scroll
            .apply(ScrollEvent::Stop {
                axis: ScrollAxis::Vertical,
            })
            .expect("垂直轴停止才结束手势");
        assert_eq!(end.phase, ScrollPhase::End);
    }

    /// RBL-01 M-a 覆盖：`Frame` 必须清空增量，否则两帧会累加（帧作用域失效）。
    #[test]
    fn frames_do_not_accumulate_across_the_frame_boundary() {
        let mut scroll = PointerScroll::default();
        scroll.apply(ScrollEvent::Value120 {
            axis: ScrollAxis::Vertical,
            value120: 120,
        });
        let first = scroll.apply(ScrollEvent::Frame).expect("第一帧必须有输出");
        assert_eq!(first.delta, (0.0, -1.0));

        scroll.apply(ScrollEvent::Value120 {
            axis: ScrollAxis::Vertical,
            value120: 240,
        });
        let second = scroll.apply(ScrollEvent::Frame).expect("第二帧必须有输出");
        assert_eq!(
            second.delta,
            (0.0, -2.0),
            "第二帧只含自己的 240/120 = 2 行（若 Frame 忘了清空会是 3 行）"
        );
    }

    /// 未知源 + 无离散通道 ⇒ 按滚轮（安全默认）。
    #[test]
    fn an_unknown_source_is_a_wheel_not_a_finger() {
        let mut scroll = PointerScroll::default();
        scroll.apply(ScrollEvent::Continuous {
            axis: ScrollAxis::Vertical,
            value: -CONTINUOUS_AXIS_UNITS_PER_LINE,
        });
        let output = scroll
            .apply(ScrollEvent::Frame)
            .expect("滚轮帧必须产出事件");
        assert!(!output.finger, "未知源按滚轮处理");
        assert_eq!(output.delta, (0.0, 1.0));
        assert_eq!(output.phase, ScrollPhase::Discrete);
    }

    /// 显式 `wheel` / `continuous` 源都按行（`Continuous` 当前与 `Wheel` 同义）。
    #[test]
    fn explicit_non_finger_sources_are_lines() {
        for source in [ScrollSource::Wheel, ScrollSource::Continuous] {
            let mut scroll = PointerScroll::default();
            scroll.apply(ScrollEvent::Source(source));
            scroll.apply(ScrollEvent::Continuous {
                axis: ScrollAxis::Horizontal,
                value: CONTINUOUS_AXIS_UNITS_PER_LINE,
            });
            let output = scroll
                .apply(ScrollEvent::Frame)
                .expect("非手指源必须产出事件");
            assert!(!output.finger, "{source:?} 必须按行");
            assert_eq!(output.delta, (-1.0, 0.0));
        }
    }

    /// 空帧（只有 `frame`，没有滚动分量）不产出事件；一根从未活跃的轴 `axis_stop` 视为唯一
    /// 活跃轴已停（集合本来就是空）⇒ 产出一条 `End`，帧收尾不会重复。
    #[test]
    fn an_empty_frame_yields_no_event() {
        let mut scroll = PointerScroll::default();
        assert_eq!(scroll.apply(ScrollEvent::Frame), None);
        assert_eq!(
            scroll
                .apply(ScrollEvent::Stop {
                    axis: ScrollAxis::Vertical,
                })
                .map(|output| output.phase),
            Some(ScrollPhase::End)
        );
        assert_eq!(scroll.apply(ScrollEvent::Frame), None, "End 只产出一次");
    }
}
