//! 引擎公开错误面。
//!
//! 只有强类型变体，不出现字符串错误或通用错误包装（AGENTS §3）。`Display` 为英文诊断文本，
//! 只用于日志 / CI / 开发者；面向用户的本地化文案由应用层按变体生成（i18n 留应用层）。
//!
//! **变体表随阶段演进增删，不保持向后兼容**（AGENTS §1.1；应用代码按变体匹配，不按数量）。
//! 静态几何增量更新（X5）相关的是 [`EngineError::UnknownGeometry`] /
//! [`EngineError::GeometryRangeOutOfBounds`] / [`EngineError::GeometryRangeUnaligned`] /
//! [`EngineError::GeometryRangeBytes`] / [`EngineError::GeometryTruncateGrows`]，
//! [`EngineError::GeometryRegister`] 由三端 GPU 后端（快照缺失 / 非法数据）生产。
//! `LayoutInvalid` 暂无生产者（保留给布局契约的显式失败路径）。

use std::path::PathBuf;

use smol_str::SmolStr;

use crate::{
    layout::Axis,
    node::{
        id::NodeId,
        signal::{DerivedId, SignalScope},
    },
    paint::atlas::AtlasKind,
    resources::DecodeCause,
    window::WindowId,
};

/// Wayland 协议层失败的分类（[`EngineError::Wayland`] 的 `kind`，R10-16）。
///
/// 分类依据是**处理策略**，不是错误码来源：
///
/// | 分类 | 触发者 | 调用方应做什么 |
/// |---|---|---|
/// | [`Self::CompositorGone`] | `EPIPE` / `ENOENT` / 连接被关闭（合成器崩溃、注销、VT 切换） | 提示用户并退出；重连需要重建整条管线 |
/// | [`Self::Protocol`] | `EPROTO` / `BadMessage`（发给未知对象 id 的事件、非法请求序列） | 客户端 bug：写日志 + 上报，不要重试 |
/// | [`Self::MissingGlobal`] | 必需 global 缺失 / 版本不足（`xdg_wm_base` / `wl_compositor` / …） | 环境不支持：提示用户（Linux 不回退 X11，I10） |
/// | [`Self::SurfaceLost`] | 单个 surface / role 对象失效（拿不到首个 configure、事件路由不到窗口） | 关闭该窗口或重建它 —— **不必**杀掉整个进程 |
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WaylandErrorKind {
    /// 合成器消失 / 连接断开（`EPIPE` / `ENOENT` / 读端 EOF）。
    CompositorGone,
    /// 协议错误（`EPROTO` / `BadMessage`）：客户端违反了协议。
    Protocol,
    /// 缺少必需的 global，或已绑定的版本低于要求。
    MissingGlobal,
    /// 单个 surface / role 对象已失效（窗口级失败）。
    SurfaceLost,
}

impl WaylandErrorKind {
    /// 稳定诊断名（`Display` 用；日志/CI 可 grep）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CompositorGone => "compositor gone",
            Self::Protocol => "protocol error",
            Self::MissingGlobal => "missing global",
            Self::SurfaceLost => "surface lost",
        }
    }

    /// 是否值得重试（只有窗口级的失效可以重建后重试）。
    #[must_use]
    pub const fn is_recoverable(self) -> bool {
        matches!(self, Self::SurfaceLost)
    }

    /// 把底层 IO 错误分类成 [`WaylandErrorKind`]（R10-16）。
    ///
    /// 依据是**处理策略**而不是错误文本：
    ///
    /// - `EPIPE` / `ECONNRESET` / `ENOENT` / `EBADF` ⇒ 连接没了（合成器崩溃、会话注销、
    ///   `WAYLAND_DISPLAY` 指向的 socket 被删）⇒ [`Self::CompositorGone`]；
    /// - `EPROTO` ⇒ 协议错误（客户端发了非法请求，或服务端发来了本端不认识的对象事件）
    ///   ⇒ [`Self::Protocol`]；
    /// - 其余（含 `WouldBlock` 之类的瞬时错误，调用方不该把它们送进来）归
    ///   [`Self::Protocol`]：宁可报「客户端 bug」也不要谎称「合成器还在」。
    #[must_use]
    pub fn from_io_error(error: &std::io::Error) -> Self {
        /// Linux `EPROTO`：libwayland 与 `calloop-wayland-source` 都把它当作协议错误上报。
        const EPROTO: i32 = 71;
        use std::io::ErrorKind;
        if error.raw_os_error() == Some(EPROTO) {
            return Self::Protocol;
        }
        match error.kind() {
            ErrorKind::BrokenPipe
            | ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::NotFound
            | ErrorKind::UnexpectedEof => Self::CompositorGone,
            _ => Self::Protocol,
        }
    }
}

impl std::fmt::Display for WaylandErrorKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// 引擎公开错误枚举。
///
/// 全部变体的 `source()` 均为 `None`：不包装外部错误类型，外部错误码（HRESULT / `VkResult` /
/// `MTLCommandBuffer` error）降为结构字段或可读文本，保证调用方可以穷尽 match 后决定处理策略。
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// D8 基线不满足。`actual` 按 `required` 的项顺序逐项拼接，便于人读比对。
    ///
    /// 文案必须同时给出实际值与最低要求、指向发布说明，且**不得**出现降级承诺（I10）。
    #[error(
        "GPU baseline not met: {os} requires {required}; detected {actual}. \
         This build has no software or no-GPU fallback; see the release notes for the supported \
         hardware baseline."
    )]
    UnsupportedGpu {
        os: &'static str,
        required: &'static str,
        actual: SmolStr,
    },

    /// 图形设备丢失。`log_path` 为已落盘的 DRED / validation layer 诊断文件。
    #[error("graphics device lost: {reason}; diagnostics written to {log_path}")]
    DeviceLost { reason: SmolStr, log_path: PathBuf },

    /// Wayland 协议层失败（R10-16）。
    ///
    /// **按 `kind` 分派**，不要把运行期断连折叠成 [`EngineError::AppInit`]：合成器消失要提示
    /// 「合成器断开，请重新登录会话」，协议错误是本客户端的 bug（需要日志 + 上报），两者对用户
    /// 与对 CI 的意义完全不同。Linux 不回退其它窗口系统（I10）。
    #[error("Wayland {kind}: {detail}")]
    Wayland {
        /// 失败分类（调用方据此决定「退出 / 重连 / 报 bug」）。
        kind: WaylandErrorKind,
        /// 具体诊断（含 libwayland 的原始错误文本 / 缺失的 global 名）。
        detail: SmolStr,
    },

    /// shader 产物加载失败。`target` 为 `"dxil"` / `"metallib"` / `"spirv"`。
    #[error("failed to load shader artifact for {target} at {path}")]
    ShaderLoad { target: &'static str, path: PathBuf },

    /// swapchain / offscreen 重建失败。
    #[error("swapchain reconfiguration failed: {detail}")]
    Swapchain { detail: SmolStr },

    /// DirectComposition 的**运行时**调用失败（visual 几何 / `Commit` / 交换链切换）。
    ///
    /// [`EngineError::AppInit`] 只用于 `Composition::new` 的 COM 创建阶段（窗口还建不出来）；
    /// 首帧 setup 与窗口生命周期内的 `SetContent` / `Commit` 失败都归这里（窗口已经存在，只是
    /// 不能再合成）。`detail` 带失败的操作名。
    #[error("DirectComposition call failed: {detail}")]
    Composition { detail: SmolStr },

    /// 帧管线失去活性（合成线程意外退出且无法重启 / 邮箱在有窗口注册时已关闭）。
    ///
    /// **不可重试**：帧管线是单点 —— 一旦死在用户看来就是「画面冻结、输入仍在工作」。这类
    /// 失败必须显式冒泡，绝不能静默退化成「UI 继续发布、下游无人消费」。`detail` 带失败阶段。
    #[error("frame pipeline stopped: {detail}")]
    FramePipeline { detail: SmolStr },

    /// `App::on_frame` 的单槽已被占用，第二个帧钩子安装者被拒绝。
    ///
    /// 单槽语义是「后注册覆盖先注册」：静默替换会让先安装的扇出（`widget::frame::FrameRouter`）
    /// 悄无声息地停摆。因此安装协议把「槽已占用」表达为错误，唯一允许的重复安装是同一个
    /// 路由器的幂等 `install`。消费者：`widget::frame::FrameRouter`。
    #[error("the application frame hook is already installed")]
    FrameHookInstalled,

    /// 平台报告的 DPI 缩放因子非法（非有限值或 `<= 0`）。唯一生产者是平台层 DPI 事件处理。
    #[error("invalid DPI scale factor {factor}: must be finite and greater than zero")]
    InvalidDpiScale { factor: f32 },

    /// 节点句柄不可用：从未存在、已卸载（代际不匹配），**不是可移除的子节点**（root），
    /// 或**不能作为文本命中目标**（v2.1 E5/§11.5：非 `NodeKind::Text` / 文本尚未 shape /
    /// 快照已过期 —— 语义统一为「该节点不能作为文本命中目标」，见
    /// `Window::text_byte_index_at`）。
    #[error(
        "stale node handle {node:?}: the node was unmounted or never existed in this window, \
         or cannot be used as a text hit target"
    )]
    StaleNode { node: NodeId },

    /// [`App::remove_node_with_scope`](crate::app::App::remove_node_with_scope) 的 `scope` 被
    /// **被删子树之外**的存活节点读取：释放它必然破坏那个存活绑定（作用域不属于这棵子树，或
    /// 与其它组件共享）。方法在**任何副作用之前**返回本错误（节点未卸载、作用域未释放）。
    ///
    /// `SignalScope` 与 `NodeId` 是正交句柄（E8），引擎没有归属映射；可证明的错配只有「子树
    /// 之外仍有存活读者」这一种。作用域没有任何存活读者、或只被被删子树读取时，引擎**无法**
    /// 区分错配与正确使用（见该方法的 `# 契约`）。
    #[error(
        "signal scope {scope:?} is still read by live node {reader:?}: it does not belong to the \
         subtree being removed ({node:?}) and releasing it would break that live binding"
    )]
    ScopeOwnershipMismatch {
        /// 调用方要求卸载的子树根。
        node: NodeId,
        /// 调用方传入的作用域（其存活信号仍被子树外的节点读取）。
        scope: SignalScope,
        /// 证明错配的存活读者（被删子树之外的节点）。
        reader: NodeId,
    },

    /// 派生值形成环（或该派生的闭包上次 panic 未放回）。
    #[error(
        "cyclic derived value {derived:?}: its compute closure depends on itself (directly or \
         through another derived value)"
    )]
    CyclicDerived { derived: DerivedId },

    /// measure 闭包返回了非有限值或负尺寸（设计 §4.8）。
    #[error(
        "invalid measure for {node:?} on the {axis:?} axis: {value} \
         (measure closures must return finite, non-negative sizes)"
    )]
    InvalidMeasure {
        node: NodeId,
        axis: Axis,
        value: f32,
    },

    /// 文本形状失败（设计 §4.8 / P01 §6.2.2 #13）：字体栈为空、行内盒字节偏移非法、超宽降级需上报。
    #[error("text shaping failed for {node:?}: {detail}")]
    TextShape { node: NodeId, detail: SmolStr },

    /// **无节点归属**的 shaping 失败（v2.1 E1，§11.1）：`App::shape_text` 的文本不属于任何
    /// 节点，因此无法用 [`EngineError::TextShape`] 的 `node` 字段定位。
    ///
    /// 触发：字体栈为空（没有任何候选家族）、`max_width` 非有限值或为负、parley 构建失败。
    /// 与 `TextShape` 分开的理由：调用方的处理策略不同 —— 节点版由节点失效通道重试，
    /// 独立版只能由应用修正输入（换字体栈 / 改正宽度）后重新 `shape_text`。
    #[error("standalone text shaping failed: {detail}")]
    TextShapeStandalone { detail: SmolStr },

    /// 应用给 canvas 文本的**显式**光栅 scale 非法（非有限值或 `<= 0`）。
    ///
    /// 生产者是 [`crate::app::App::set_text_scale`]（v2.1 E1b）：该值直接进光栅 ppem
    /// （`font_size × scale`），非法值会让 parley/swash 得到 NaN 尺寸 —— 必须在入口拒绝，
    /// 不能静默钳制（那会把应用的配置错误变成「字忽然变大/变小」）。与
    /// [`EngineError::InvalidDpiScale`] 分开：后者来自平台 DPI 事件，前者来自应用 API。
    #[error("invalid text scale {scale}: must be finite and greater than zero")]
    InvalidTextScale { scale: f32 },

    /// 图集无法容纳新图元（清空重建后仍失败，P02 §10.6 / P01 §6.2.2 #12）。
    #[error("atlas {kind:?} is exhausted: no free tile fits after a clear and rebuild")]
    AtlasExhausted { kind: AtlasKind },

    /// 图集扩层违反 fail-closed 契约：请求把 texture array 扩到 `layers` 层，但第 `layer` 层
    /// 没有整页上传（缺失或是增量矩形）。
    ///
    /// 扩层会重建整个数组（重建后的层内容未定义），因此只允许在「**每一层**的整页内容都已
    /// 在同一次重传里入队」时发生（RBT-01 不变量 2）。这里是 release 也生效的最后防线
    /// （不是 `debug_assert`）：出现即说明上层（`App::upload_image_page`）的记账被绕过，
    /// 继续重建会让该帧采样到半成品层 —— 必须显式失败而不是画错内容。
    #[error(
        "atlas {kind:?} growth to {layers} layers rejected: layer {layer} has no full-page upload"
    )]
    AtlasGrowthIncomplete {
        /// 哪个图集要扩层。
        kind: AtlasKind,
        /// 缺整页上传的层号。
        layer: u16,
        /// 请求的目标层数。
        layers: u32,
    },

    /// clip 链超过 `MAX_CLIP_DEPTH`（8 项，`paint/scene.rs`；三套 shader 的固定循环上限）。
    ///
    /// **编码期硬失败**：轴对齐矩形 clip 在编码期求交合并成一条表项（Skia `SkClipStack`
    /// 语义），只有圆角 clip 逐层占项；链长确实超限时 shader 只能少裁，静默丢最外层会让
    /// 子内容越界可见，因此这里明确报错（debug / release 行为一致）。
    #[error(
        "clip chain depth {depth} exceeds the limit {limit} (rect clips are merged at encode time; \
         each rounded clip costs one entry)"
    )]
    ClipDepthExceeded {
        /// 编码期的**圆角层数**（矩形项在编码期被求交合并成一条 scissor，不占预算；
        /// 链长上界 = 该值 + 0/1 条矩形项）。
        depth: u8,
        /// 圆角层数上限（`MAX_CLIP_DEPTH`）。
        limit: u8,
    },

    /// 静态几何注册 / 查找失败（设计 §4.8）。首个生产者是 P08 的渲染层：
    /// `FrameOp::Geometry` 的 `GeometryId` 没有对应的 GPU 缓冲（`GeometryStore` 属 P09）。
    #[error("geometry registration failed: {detail}")]
    GeometryRegister { detail: SmolStr },

    /// 几何 range 更新的目标句柄不存在：陈旧 / 已 release / 从未注册。
    ///
    /// 与「越界 / 非对齐」分开：陈旧句柄是**生命周期**错误（应用应停止更新或重注册），
    /// 越界是**参数**错误（应用应修正 range），两者的处理策略不同。
    #[error("unknown geometry handle: {geometry:?}")]
    UnknownGeometry {
        /// 陈旧 / 未注册的几何句柄。
        geometry: crate::paint::geometry::GeometryId,
    },

    /// 几何 range 更新越界：`range.start` 超过缓冲当前长度，或区间反向。
    ///
    /// 允许在缓冲区**末尾追加**（`range.end > len` 合法），因此这里只判 `start`。
    #[error(
        "geometry {geometry:?} {buffer:?} update range {range:?} is out of bounds \
         (current length {len} bytes)"
    )]
    GeometryRangeOutOfBounds {
        geometry: crate::paint::geometry::GeometryId,
        /// 作用于哪个缓冲（顶点 / 索引）。
        buffer: crate::paint::geometry::GeometryBuffer,
        /// 应用给的字节区间。
        range: std::ops::Range<u32>,
        /// 缓冲当前字节长度。
        len: u32,
    },

    /// 几何 range 更新未按元素对齐（顶点按 `vertex_stride`，索引按 `index_type.size()`）。
    #[error(
        "geometry {geometry:?} {buffer:?} update range {range:?} is not aligned to {align} bytes"
    )]
    GeometryRangeUnaligned {
        geometry: crate::paint::geometry::GeometryId,
        /// 作用于哪个缓冲（顶点 / 索引）。
        buffer: crate::paint::geometry::GeometryBuffer,
        /// 应用给的字节区间。
        range: std::ops::Range<u32>,
        /// 要求的对齐字节数。
        align: u32,
    },

    /// 几何 range 更新的载荷字节数与区间长度不一致（`bytes.len() != range.len()`）。
    #[error("geometry {geometry:?} {buffer:?} update carries {bytes} bytes for range {range:?}")]
    GeometryRangeBytes {
        geometry: crate::paint::geometry::GeometryId,
        /// 作用于哪个缓冲（顶点 / 索引）。
        buffer: crate::paint::geometry::GeometryBuffer,
        /// 应用给的字节区间。
        range: std::ops::Range<u32>,
        /// 实际提供的字节数。
        bytes: usize,
    },

    /// 几何截断的目标长度大于当前长度：截断只能缩短，增长必须走 range 更新（尾随追加）。
    ///
    /// 单独一个变体而不是复用 [`Self::GeometryRangeOutOfBounds`]：后者描述「range 越界」，
    /// 处理策略是修正 range 参数；这里的处理策略是**换 API**（`update_vertices` /
    /// `update_indices`），调用方按变体就能区分。
    #[error(
        "geometry {geometry:?} {buffer:?} truncate to {requested} elements would grow it \
         (current length {current} elements)"
    )]
    GeometryTruncateGrows {
        geometry: crate::paint::geometry::GeometryId,
        /// 作用于哪个缓冲（顶点 / 索引）。
        buffer: crate::paint::geometry::GeometryBuffer,
        /// 应用要求的元素个数。
        requested: u32,
        /// 缓冲当前元素个数。
        current: u32,
    },

    /// 引擎 / 平台初始化失败（`Application::run` 的前置条件；P08 的渲染器初始化也走这里）。
    #[error("engine initialization failed: {detail}")]
    AppInit { detail: SmolStr },

    /// 原生窗口创建 / 消息泵失败（`RegisterClassW` / `CreateWindowExW` / `GetMessageW`）。
    #[error("window creation failed: {detail}")]
    WindowCreate { detail: SmolStr },

    /// 剪贴板访问失败（被其它进程占用 / 内存分配失败）。
    #[error("clipboard access failed: {detail}")]
    Clipboard { detail: SmolStr },
    /// 目标节点不是滚动容器（未 `make_scroll`，或 `Style.clip != Clip::Bounds`）。
    #[error("node {node:?} is not scrollable (make_scroll / Clip::Bounds required)")]
    NotScrollable {
        /// 目标节点。
        node: NodeId,
    },
    /// 目标节点不是虚拟列表容器（未 `make_virtual_list`，或不是 `Clip::Bounds + position: relative`）。
    #[error("node {node:?} is not a virtual list (Clip::Bounds + position: relative required)")]
    NotVirtualList {
        /// 目标节点。
        node: NodeId,
    },

    /// 虚拟列表入口参数与当前条目数不符（E3 §11.3）。
    ///
    /// 两种来源共用同一变体（语义都是「入口参数与条目数不符」）：
    /// - `App::set_virtual_extents`：`extents.len() != count`（`expected = count`、
    ///   `actual = extents.len()`）；
    /// - `App::scroll_to_item`：`index >= count`（`expected = count`、`actual = index + 1`，
    ///   即 1-based 的请求位置，避免 `index == count` 时文案自相矛盾）。
    #[error("virtual list item count mismatch: expected {expected}, got {actual}")]
    VirtualListExtents {
        /// 当前条目数。
        expected: usize,
        /// 实际参数所指向的位置 / 长度。
        actual: usize,
    },

    /// 逐项行高非法（非有限值、负值，或累计高度溢出到非有限；E3 §11.3 入口校验）。
    ///
    /// 累计溢出的 `index` 是**使累计值首次变为非有限**的那一项。
    #[error(
        "virtual list extent at index {index} is invalid: {value} \
         (each extent must be finite and non-negative and their total must be finite)"
    )]
    InvalidVirtualExtent {
        /// 非法值在 `extents` 里的下标。
        index: usize,
        /// 应用给的值。
        value: f32,
    },
    /// 窗口已关闭 / 从未存在（P10：需要返回句柄或开启动画时无法静默）。
    #[error("unknown window: {window:?}")]
    UnknownWindow {
        /// 陈旧/未知的窗口句柄。
        window: WindowId,
    },
    /// 平台 a11y adapter 初始化失败（必须在窗口**首次显示之前**构造）。
    #[error("accessibility adapter initialization failed: {detail}")]
    A11yInit { detail: SmolStr },

    /// 图片资源解码 / 装箱失败（P13：`ResourceCache::insert`）。
    ///
    /// payload 是**强类型分类**：
    /// 调用方按 `DecodeCause` 决定「占位图 / 报错 / 卸载重试」，不需要解析字符串。
    #[error("image resource decode failed: {cause}")]
    ResourceDecode {
        /// 失败分类。
        cause: DecodeCause,
    },

    // ── E6（P1-E6）：窗口 chrome 的平台能力错误 ──────────────────────────────
    /// 平台没有该窗口能力的等价实现，且**不允许静默降级**（I10 / §11.6）。
    ///
    /// 生产者是 `App::request_window_control` / `App::set_traffic_light_position`：调用前的
    /// 能力判定由平台层给出（Windows 的 `ToggleFullscreen` / `BeginMove` / `BeginResize`；
    /// macOS 的 `BeginMove` / `BeginResize`；非 macOS 的交通灯位置）。`capability` 是稳定的
    /// 英文诊断名（日志 / CI 可 grep；面向用户的本地化文案由应用层按变体生成）。
    ///
    /// 已知的**文档化降级**（不是本错误）：`WindowOptions::icon` 在 macOS / Linux、
    /// `WindowOptions::decorations = None` 在 macOS / Linux —— 这些字段的文档写明被忽略的原因，
    /// 调用方在选择这些取值时就已被告知（不存在「请求了却不知道没生效」）。
    #[error("window {window:?} has no {capability} on this platform")]
    UnsupportedWindowCapability {
        /// 目标窗口。
        window: WindowId,
        /// 缺失能力的稳定诊断名。
        capability: &'static str,
    },

    /// 窗口图标（`WindowOptions::icon`）解码失败（E6；Windows 建窗路径）。
    ///
    /// 图标字节是**建窗参数**而不是可选装饰：非法字节 ⇒ 建窗失败（I10 不静默丢弃，也不假装
    /// 设置成功）。只在真的提供 `icon` 时可能发生。
    #[error("window {window:?} icon decode failed: {cause}")]
    WindowIcon {
        /// 目标窗口。
        window: WindowId,
        /// 强类型失败分类。
        cause: WindowIconCause,
    },
}

/// [`EngineError::WindowIcon`] 的强类型失败分类。
///
/// 与 [`DecodeCause`]（图片**资源**解码，进图集 / 有占位图语义）分开：窗口图标的处理策略只有
/// 「修字节」或「不传图标」，不需要资源层的容量 / 色彩管理分类。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowIconCause {
    /// 不是 PNG / ICO（E6 只支持这两种格式：任务栏图标的标准输入）。
    UnsupportedFormat,
    /// 字节损坏 / 解码器失败（截断、CRC 错、非法 ICO 目录等）。
    DecodeFailed,
    /// 解码出的尺寸超过引擎上限（`MAX_WINDOW_ICON_DIM`）。
    TooLarge {
        /// 解码出的像素宽。
        width: u32,
        /// 解码出的像素高。
        height: u32,
    },
}

impl std::fmt::Display for WindowIconCause {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedFormat => {
                formatter.write_str("unsupported format (PNG or ICO required)")
            }
            Self::DecodeFailed => formatter.write_str("decode failed"),
            Self::TooLarge { width, height } => write!(
                formatter,
                "decoded size {width}x{height} exceeds the {} px limit",
                crate::window::MAX_WINDOW_ICON_DIM
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paint::atlas::AtlasKind;

    fn unsupported_gpu() -> EngineError {
        EngineError::UnsupportedGpu {
            os: "windows",
            required: "FL 12_0 / SM 6.2",
            actual: SmolStr::new_static("FL 11_0 / SM 6_0"),
        }
    }

    fn all_variants() -> Vec<EngineError> {
        vec![
            unsupported_gpu(),
            EngineError::DeviceLost {
                reason: SmolStr::new_static("DXGI_ERROR_DEVICE_REMOVED"),
                log_path: PathBuf::from("logs/dred.txt"),
            },
            EngineError::Wayland {
                kind: WaylandErrorKind::MissingGlobal,
                detail: SmolStr::new_static("xdg_wm_base version 3 required"),
            },
            EngineError::ShaderLoad {
                target: "dxil",
                path: PathBuf::from("shaders/quad.dxil"),
            },
            EngineError::Swapchain {
                detail: SmolStr::new_static("CreateSwapChainForHwnd failed"),
            },
            EngineError::Composition {
                detail: SmolStr::new_static("set_geometry: 0x8007000E"),
            },
            EngineError::InvalidDpiScale { factor: 0.0 },
            EngineError::StaleNode {
                node: NodeId::for_test(3, 1),
            },
            EngineError::ScopeOwnershipMismatch {
                node: NodeId::for_test(9, 1),
                scope: SignalScope::from_index(2),
                reader: NodeId::for_test(10, 1),
            },
            EngineError::CyclicDerived {
                derived: DerivedId::for_test(0),
            },
            EngineError::InvalidMeasure {
                node: NodeId::for_test(4, 1),
                axis: Axis::X,
                value: f32::NAN,
            },
            EngineError::TextShape {
                node: NodeId::for_test(5, 1),
                detail: SmolStr::new_static("font stack is empty"),
            },
            EngineError::TextShapeStandalone {
                detail: SmolStr::new_static("font stack is empty"),
            },
            EngineError::InvalidTextScale { scale: 0.0 },
            EngineError::AtlasExhausted {
                kind: AtlasKind::Glyph,
            },
            EngineError::AtlasGrowthIncomplete {
                kind: AtlasKind::Image,
                layer: 1,
                layers: 2,
            },
            EngineError::GeometryRegister {
                detail: SmolStr::new_static("geometry id 0:0 has no GPU buffers"),
            },
            EngineError::AppInit {
                detail: SmolStr::new_static("d3d12 D3D12CreateDevice: 0x887A0004"),
            },
            EngineError::WindowCreate {
                detail: SmolStr::new_static("CreateWindowExW: 0x8007057E"),
            },
            EngineError::Clipboard {
                detail: SmolStr::new_static("OpenClipboard: 0x80070005"),
            },
            EngineError::ResourceDecode {
                cause: DecodeCause::Capacity {
                    width: 900,
                    height: 700,
                    page: (2048, 2048),
                    cause: crate::resources::CapacityCause::PageFull,
                },
            },
            EngineError::UnsupportedWindowCapability {
                window: crate::window::WindowId::for_test(7),
                capability: "fullscreen",
            },
            EngineError::WindowIcon {
                window: crate::window::WindowId::for_test(8),
                cause: WindowIconCause::DecodeFailed,
            },
        ]
    }

    #[test]
    fn resource_decode_display_names_the_cause() {
        let text = EngineError::ResourceDecode {
            cause: DecodeCause::UnsupportedFormat {
                format: SmolStr::new_static("gif"),
            },
        }
        .to_string();

        assert!(text.contains("image resource decode failed"), "{text}");
        assert!(text.contains("unsupported image format `gif`"), "{text}");
        assert!(text.contains("PNG, JPEG and WebP only"), "{text}");
    }

    #[test]
    /// `Capacity` 的文案必须说清「页已满」而不是「页被首图锁死」（P1-2.7 之后页尺寸固定为
    /// 最大档，容量失败只可能是页里放不下更多 tile）。
    fn resource_decode_capacity_display_names_the_full_page() {
        let text = EngineError::ResourceDecode {
            cause: DecodeCause::Capacity {
                width: 900,
                height: 700,
                page: (2048, 2048),
                cause: crate::resources::CapacityCause::PageFull,
            },
        }
        .to_string();

        assert!(text.contains("900x700"), "{text}");
        assert!(text.contains("2048x2048"), "{text}");
        assert!(text.contains("page is already full"), "{text}");
        assert!(
            !text.contains("locked by the first inserted image"),
            "不得再暗示页尺寸被首图锁定（P1-2.7 已修）：{text}"
        );
    }

    #[test]
    fn text_shape_display_names_node_and_detail() {
        let text = EngineError::TextShape {
            node: NodeId::for_test(6, 2),
            detail: SmolStr::new_static("inline box index 1 is not a char boundary"),
        }
        .to_string();

        assert!(text.contains("text shaping failed"), "{text}");
        assert!(text.contains("n6.2"), "{text}");
        assert!(text.contains("char boundary"), "{text}");
    }

    /// §11.1：`App::shape_text` 的失败没有节点归属，必须是独立变体（调用方按变体决定是
    /// 「等节点失效重试」还是「修正输入后重新 shape」）。
    #[test]
    fn standalone_text_shape_display_has_no_node_handle() {
        let text = EngineError::TextShapeStandalone {
            detail: SmolStr::new_static("max_width 0 must be finite and non-negative"),
        }
        .to_string();

        assert!(text.contains("standalone text shaping failed"), "{text}");
        assert!(text.contains("max_width"), "{text}");
        assert!(
            !text.contains("node"),
            "独立 shaping 错误不得暗示节点归属：{text}"
        );
    }

    #[test]
    fn atlas_exhausted_display_names_the_kind() {
        let text = EngineError::AtlasExhausted {
            kind: AtlasKind::Image,
        }
        .to_string();

        assert!(text.contains("Image"), "{text}");
        assert!(text.contains("exhausted"), "{text}");
        assert!(text.contains("clear and rebuild"), "{text}");
    }

    #[test]
    fn invalid_measure_display_names_node_axis_and_value() {
        let text = EngineError::InvalidMeasure {
            node: NodeId::for_test(2, 1),
            axis: Axis::Y,
            value: -1.5,
        }
        .to_string();

        assert!(text.contains("invalid measure"), "{text}");
        assert!(text.contains("n2.1"), "{text}");
        assert!(text.contains("Y"), "{text}");
        assert!(text.contains("-1.5"), "{text}");
        assert!(text.contains("finite, non-negative"), "{text}");
    }

    #[test]
    fn stale_node_display_mentions_the_state() {
        let text = EngineError::StaleNode {
            node: NodeId::for_test(3, 1),
        }
        .to_string();

        assert!(text.contains("n3.1"), "{text}");
        assert!(text.contains("unmounted or never existed"), "{text}");
        assert!(
            text.contains("text hit target"),
            "v2.1 E5 复用该变体表示「不能作为文本命中目标」：{text}"
        );
    }

    #[test]
    fn cyclic_derived_display_mentions_self_dependency() {
        let text = EngineError::CyclicDerived {
            derived: DerivedId::for_test(2),
        }
        .to_string();

        assert!(text.contains("DerivedId(2)"), "{text}");
        assert!(text.contains("itself"), "{text}");
    }

    #[test]
    fn engine_error_is_send_sync_static() {
        fn assert_send_sync_static<T: Send + Sync + 'static>() {}

        assert_send_sync_static::<EngineError>();
    }

    #[test]
    fn unsupported_gpu_display_lists_required_and_actual() {
        let text = unsupported_gpu().to_string();

        assert!(text.contains("windows"), "{text}");
        assert!(text.contains("FL 12_0 / SM 6.2"), "{text}");
        assert!(text.contains("FL 11_0 / SM 6_0"), "{text}");
        assert!(text.contains("release notes"), "{text}");
    }

    #[test]
    fn unsupported_gpu_display_has_no_fallback_promise() {
        let text = unsupported_gpu().to_string().to_lowercase();

        assert!(!text.contains("software rendering"), "{text}");
        assert!(!text.contains("falling back"), "{text}");
        assert!(!text.contains("try again"), "{text}");
    }

    #[test]
    fn device_lost_display_includes_path() {
        let text = EngineError::DeviceLost {
            reason: SmolStr::new_static("device removed"),
            log_path: PathBuf::from("logs/dred.txt"),
        }
        .to_string();

        assert!(text.contains("device removed"), "{text}");
        assert!(text.contains("logs/dred.txt"), "{text}");
    }

    #[test]
    fn wayland_error_display_names_the_kind_and_detail() {
        let text = EngineError::Wayland {
            kind: WaylandErrorKind::MissingGlobal,
            detail: SmolStr::new_static("xdg_wm_base"),
        }
        .to_string();

        assert!(text.contains("missing global"), "{text}");
        assert!(text.contains("xdg_wm_base"), "{text}");
    }

    /// R10-16：分类必须能区分「合成器消失」与「客户端 bug」—— 处理策略不同。
    #[test]
    fn wayland_error_kinds_are_classified_by_recovery_strategy() {
        assert!(!WaylandErrorKind::CompositorGone.is_recoverable());
        assert!(!WaylandErrorKind::Protocol.is_recoverable());
        assert!(!WaylandErrorKind::MissingGlobal.is_recoverable());
        assert!(
            WaylandErrorKind::SurfaceLost.is_recoverable(),
            "单个 surface 失效可以重建窗口，不必杀进程"
        );
        assert_eq!(WaylandErrorKind::Protocol.as_str(), "protocol error");
    }

    /// R10-16：`calloop` / libwayland 的 IO 失败必须落进正确的分类。
    #[test]
    fn io_errors_map_to_wayland_error_kinds() {
        use std::io::{Error, ErrorKind};

        for kind in [
            ErrorKind::BrokenPipe,
            ErrorKind::ConnectionReset,
            ErrorKind::ConnectionAborted,
            ErrorKind::NotFound,
            ErrorKind::UnexpectedEof,
        ] {
            assert_eq!(
                WaylandErrorKind::from_io_error(&Error::new(kind, "gone")),
                WaylandErrorKind::CompositorGone,
                "{kind:?} 表示连接没了"
            );
        }
        // EPROTO（Linux errno 71）：协议错误 ⇒ 客户端 bug，不能报「合成器消失」。
        assert_eq!(
            WaylandErrorKind::from_io_error(&Error::from_raw_os_error(71)),
            WaylandErrorKind::Protocol
        );
        assert_eq!(
            WaylandErrorKind::from_io_error(&Error::new(ErrorKind::InvalidInput, "bad")),
            WaylandErrorKind::Protocol
        );
    }

    #[test]
    fn shader_load_display_includes_target_and_path() {
        let text = EngineError::ShaderLoad {
            target: "spirv",
            path: PathBuf::from("shaders/quad.spv"),
        }
        .to_string();

        assert!(text.contains("spirv"), "{text}");
        assert!(text.contains("shaders/quad.spv"), "{text}");
    }

    #[test]
    fn swapchain_display_includes_detail() {
        let text = EngineError::Swapchain {
            detail: SmolStr::new_static("resize failed"),
        }
        .to_string();

        assert!(text.contains("resize failed"), "{text}");
    }

    #[test]
    fn composition_display_names_the_failing_operation() {
        let text = EngineError::Composition {
            detail: SmolStr::new_static("Device::Commit: 0x8007000E"),
        }
        .to_string();

        assert!(text.contains("DirectComposition"), "{text}");
        assert!(text.contains("Device::Commit"), "{text}");
        assert!(text.contains("0x8007000E"), "{text}");
    }

    #[test]
    fn invalid_dpi_scale_display_includes_factor() {
        let text = EngineError::InvalidDpiScale { factor: 2.5 }.to_string();

        assert!(text.contains("2.5"), "{text}");
    }

    /// E16b/F4：作用域错配错误必须点名「子树外的存活读者」—— 调用方据此定位错传的 `scope`。
    #[test]
    fn scope_ownership_mismatch_display_names_the_live_reader() {
        let text = EngineError::ScopeOwnershipMismatch {
            node: NodeId::for_test(9, 1),
            scope: SignalScope::from_index(2),
            reader: NodeId::for_test(10, 1),
        }
        .to_string();

        assert!(text.contains("live node"), "{text}");
        assert!(text.contains("releasing it would break"), "{text}");
    }

    /// E1b：canvas 文本的显式光栅 scale 由应用给（`App::set_text_scale`），非法值必须可区分于
    /// 平台 DPI 错误（后者调用方只能接受/退出，前者是应用输入 bug，可修正后重试）。
    #[test]
    fn invalid_text_scale_display_is_distinct_from_the_platform_dpi_error() {
        let text = EngineError::InvalidTextScale { scale: f32::NAN }.to_string();

        assert!(text.contains("invalid text scale"), "{text}");
        assert!(text.contains("finite and greater than zero"), "{text}");
        assert!(
            !text.contains("DPI"),
            "应用侧 scale 错误不得与平台 DPI 错误混淆：{text}"
        );
    }

    #[test]
    fn engine_error_source_is_none() {
        for error in all_variants() {
            assert!(
                std::error::Error::source(&error).is_none(),
                "{error:?} unexpectedly has a source"
            );
        }
    }

    #[test]
    fn engine_error_debug_contains_variant_name() {
        let debug = format!("{:?}", unsupported_gpu());

        assert!(debug.contains("UnsupportedGpu"), "{debug}");
    }

    /// E6：平台能力缺失必须可 grep（稳定诊断名）+ 指名窗口/能力，调用方按变体分派。
    #[test]
    fn unsupported_window_capability_display_names_window_and_capability() {
        let error = EngineError::UnsupportedWindowCapability {
            window: crate::window::WindowId::for_test(3),
            capability: "fullscreen",
        };
        let text = error.to_string();
        assert!(
            text.contains("WindowId(3)"),
            "window id 必须在文案里：{text}"
        );
        assert!(text.contains("fullscreen"), "{text}");
        assert!(text.contains("no fullscreen"), "{text}");
    }

    /// E6：图标解码失败是强类型分类（调用方不需要解析字符串），文案说清限制的格式。
    #[test]
    fn window_icon_display_names_the_cause() {
        let error = EngineError::WindowIcon {
            window: crate::window::WindowId::for_test(4),
            cause: WindowIconCause::UnsupportedFormat,
        };
        let text = error.to_string();
        assert!(text.contains("WindowId(4)"), "{text}");
        assert!(text.contains("PNG or ICO"), "{text}");

        let too_large = EngineError::WindowIcon {
            window: crate::window::WindowId::for_test(4),
            cause: WindowIconCause::TooLarge {
                width: 2048,
                height: 4096,
            },
        }
        .to_string();
        assert!(too_large.contains("2048x4096"), "{too_large}");
        assert!(too_large.contains("1024"), "{too_large}");
    }

    #[test]
    fn engine_error_usable_as_boxed_error() {
        let error: Box<dyn std::error::Error> = Box::new(unsupported_gpu());

        assert!(error.to_string().contains("GPU baseline not met"));
    }
}
