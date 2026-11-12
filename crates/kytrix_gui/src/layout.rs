//! 布局与命中测试：公开契约 + 子模块。
//!
//! 本模块是**公共模块根**（P01 §4.1）：契约类型直接定义在根文件，子模块用 `pub mod` 如实暴露路径。
//! 公开路径因此是 `kytrix_gui::layout::MeasureFn` / `layout::LayoutOverride` / `layout::LayoutReport` /
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
}
