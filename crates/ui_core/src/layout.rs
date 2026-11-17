//! 布局与命中测试：公开契约 + 子模块。
//!
//! 本模块是**公共模块根**（P01 §4.1）：契约类型直接定义在根文件，子模块用 `pub mod` 如实暴露路径。
//! 公开路径因此是 `ui_core::layout::MeasureFn` / `layout::LayoutOverride` / `layout::LayoutReport` /
//! `layout::hit_test::HitResult`（P01 §4.5 的路径表按「类型定义文件」解释）。
//!
//! 设计见 `design/phase-04-layout.md`。

pub mod hit_test;
pub(crate) mod measure;
pub(crate) mod taffy_tree;

use crate::math::{Rect, Size};

/// 布局轴（`EngineError::InvalidMeasure` 的 payload；P01 §6.2.2 #9）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Axis {
    /// 水平轴（宽）。
    X,
    /// 垂直轴（高）。
    Y,
}

/// 自定义叶子测量闭包（P05 的文本 / Markdown 行内流 / K 线坐标标签的入口）。
///
/// 参数形状与 taffy `compute_leaf_layout` 的 `MeasureFunction` **逐字一致**（零适配）：
/// 1. `known`：父布局已确定的尺寸（只在 `RunMode::ComputeSize` 时为 `Some`；`PerformLayout` 恒 `None`）；
/// 2. `available`：可用空间（`Definite(w)` / `MinContent` / `MaxContent`）—— 文本换行必须依据它，
///    因此**不能**只传「已知尺寸」。
///
/// 返回值是**内容盒**尺寸（taffy 会把 padding/border 加上去，并做 min/max clamp）。
///
/// **必须是纯函数**（设计 §4.3）：不得读 `Signal`、不得写状态、不得依赖帧序号；结果只允许依赖参数与
/// 闭包捕获的**不可变**配置（字体栈版本等）。违反会让布局缓存的命中条件失真。
/// 类型是 `Box<dyn Fn>`（不是 `FnMut`）：需要可变状态的闭包在注册处就编译失败。
///
/// 用 `taffy::Size<…>` 而不是引擎 `math::Size`：引擎 `Size` 按 P01 §6.3 冻结为纯 `f32`，无法表达
/// `Option` / 三态可用空间；taffy 已是公开面的一部分（`Style.inner`，DEVIATIONS F-35）。
///
/// # 三态可用空间的语义（B8：与真实布局**同源**）
///
/// 文本类 measure 必须对三种输入给出**同一套 break/wrap 逻辑**的结果（`text::ShapedText::measure`
/// 是引擎内的参考实现，`text::shape` 的 `measure_*` 测试钉住了"measure == rasterized"）：
///
/// | 输入 | 语义 | 文本实现的取值 |
/// |---|---|---|
/// | `MaxContent` | 不换行时的宽度 / 高度：**最长单行** | `max_size`（一行） |
/// | `MinContent` | 允许任意断行时能压到的最小宽度 / 对应高度：**最长不可断片段**（CJK 逐字、拉丁按词） | `min_size`（多行） |
/// | `Definite(w)` | 在 `w` 处的**真实断行结果**（与绘制同一次 `break_all_lines`）：宽度 ≤ `w` 的最长行、高度 = 实际行高之和 | 复用同一份 parley `Layout` 就地重排 |
///
/// 三态都必须应用 [`crate::text::TextStyle`] 的 `max_lines`（截断后的**可见高度**，不是全部行高）
/// 与 `overflow: Ellipsis`（省略号不改变行高，故不改变 measure 高度）。不满足这三条的 measure
/// 会让"测的"与"画的"分叉：taffy 按测量值排版，而 `layout_text` 按真实断行绘制。
///
/// # 与 `LayoutOverride` 的关系
///
/// 节点一旦有 [`LayoutOverride`]，measure **不参与**它的盒子（尺寸由应用给定）；文本仍会 shape
/// 并在自己的内容宽内断行。measure 与 override 同时存在时不要用测量值推断布局。
pub type MeasureFn =
    Box<dyn Fn(taffy::Size<Option<f32>>, taffy::Size<taffy::AvailableSpace>) -> Size>;

/// 布局覆盖（root 逃生口，设计 §4.3）：节点的布局盒由应用给定，跳过 taffy 的子布局与叶子测量。
///
/// 坐标空间：**相对父节点 border box 左上角**（= taffy `Layout.location` 的参照系，与普通节点完全一致），
/// 因此父节点移动时子树的覆盖矩形自动跟随，不需要应用重新计算绝对坐标。
///
/// 语义（`NodeBuilder::layout_override` 会同时把 `style.inner.position` 置为 `Absolute`）：
/// - 父**不**为它保留流内空间（它是出流盒，与 CSS `position: absolute` 一致）；
/// - `rect` 就是节点的 **border box**（含 padding/border），引擎不再做 min/max clamp；
/// - 该节点视为布局叶子：挂在它下面的子节点不会被布局（`NodeBuilder::build` 有 debug 断言）。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LayoutOverride {
    /// 逻辑像素矩形（相对父 border box）。
    pub rect: Rect,
}

impl LayoutOverride {
    /// 构造。
    #[must_use]
    pub const fn new(rect: Rect) -> Self {
        Self { rect }
    }
}

/// 一次布局运行的统计（`Window::compute_layout` 的返回值；P06 的帧记录与 L1 测试的断言对象）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LayoutReport {
    /// 本次被 taffy 重算布局的节点数（`PerformLayout` 且 cache 未命中）。
    pub laid_out: usize,
    /// 本次布局盒**发生变化**的节点数（绝对坐标 post-pass 写过 `bounds`）。
    pub moved: usize,
    /// taffy cache 命中次数（「只重算脏子树」的直接证据）。
    pub cache_hits: usize,
    /// taffy cache 未命中次数。
    pub cache_misses: usize,
    /// 本轮 post-pass **写过**子树包围盒的节点数（B9 的增量性证据）。
    ///
    /// 旧实现每轮布局无条件全树重算包围盒 ⇒ 该值恒等于节点总数；增量实现对"没变化的子树"
    /// 直接跳过，单节点变化时它应与**该节点的祖先链长度**同阶（见 `layout` 基准与
    /// `incremental_post_pass_touches_only_the_changed_path` 测试）。
    pub bounds_updated: usize,
}
