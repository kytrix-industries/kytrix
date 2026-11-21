//! base_ui 的引擎化基座（迁移新模块树 `kit/`）。
//!
//! # 迁移状态（P2 批次，2026-11-21）
//!
//! 本模块树是 `base_ui` 从 `gpui-kit` 迁到自研引擎 `ui_core` 的**目标 API**，当前处于
//! 「新旧并存」期：本文件公开的 `icon` / `assets` / `brand` / `size` / `format` / `motion` /
//! `animation` / `join` / `markdown` / `input` / `list` / `scroll_bar` / `table` / `tree` 是
//! 引擎化版本；旧顶层模块（`crate::icon`、`crate::assets`、`crate::markdown`、`crate::input`、
//! `crate::table`…）由尚未迁移的 gpui 组件继续使用，P2-5 迁完后删除。按 AGENTS §1.1，这里
//! **不提供**任何兼容层 / 转发函数 —— 两个模块树各自独立，组件迁移时整模块切换过去。//!
//! 本模块树**只依赖 `ui_core`、`smol_str`、`comrak` 等非 gpui 依赖**，任何 gpui 类型都不得
//! 出现在这里。
//!
//! `test_support` 只在测试构建里存在（`#[cfg(test)]`）：它依赖 `ui_core` 的 `test-support`
//! feature，而该 feature 由 `base_ui` 的 dev-dependencies 打开，发布物里不能引用。
//!
//! # 模块一览
//!
//! | 模块 | 职责 | 旧实现 |
//! |---|---|---|
//! | [`icon`] | 55 个 Phosphor 图标的枚举 / 名称 / 两档构建期 PNG / E7 图片节点挂载（`Fill` + 颜色调制） | `crate::icon` |
//! | [`assets`] | 品牌图与 Widget 预览图的注册表、路径查找、引擎异步解码接线、E7 pending 节点来源 | `crate::assets` |
//! | [`brand`] | 7 个品牌 logo（路径 / 字节 / 固有尺寸 / `Contain` 图片节点 / 可选 alt） | `crate::brand` |
//! | [`size`] | 控件尺寸阶梯（读主题 `ComponentTokens`，不再重复维护数值） | `crate::size` |
//! | [`format`] | 数字分位格式化（纯文本） | `crate::format` |
//! | [`motion`] | 深度条缓动的响应曲线（纯数学，不建帧循环） | `crate::motion` |
//! | [`animation`] | 弹层淡出是否该出帧 + 动画 tick（消隐走引擎 `Transient`） | `crate::animation` |
//! | [`join`] | 拼接控件共享边角的几何规则（纯逻辑） | `crate::join` |
//! | [`interactive`] | 通用交互状态机（hover / pressed / focus / disabled / loading → `StylePatch`） | `crate::interactive` |
//! | [`button`] | 按钮（变体 / 尺寸 / 图标 + 文字 / 加载 / 禁用 / on_click / a11y） | `crate::button` |
//! | [`toggle`] | 开关 / 复选 / 单选（键盘 Space、状态动画、a11y checked） | `crate::toggle` |
//! | [`badge`] | 状态标签（颜色 × 形状） | `crate::badge` |
//! | [`progress`] | 确定性进度条（轨道 + 指示器、a11y 数值通道） | `crate::progress` |
//! | [`spinner`] | 旋转加载指示器（自绘画布 + 引擎动画） | `crate::spinner` |
//! | [`separator`] | 分割线（横 / 纵） | `crate::separator` |
//! | [`empty_state`] | 空态说明（弱化文字） | `crate::empty_state` |
//! | [`alert`] | 对话框内容面板（4 类变体 / 输入校验 / 按钮行） | `crate::alert` |
//! | [`tab_bar`] | 标签栏（选中指示动画 / 方向键 / 关闭按钮 / a11y） | `crate::tab_bar` |
//! | [`markdown`] | comrak 解析 → 引擎富文本 run / InlineBox 视图 | `crate::markdown` |
//! | [`input`] | 应用层文本编辑状态机（caret / 选区 / IME / undo-redo / 拖选 / 双击选词） | `crate::input` |
//! | [`list`] | 等高 / 变高、静态 / 双端列表内核（E3 虚拟列表 + 实测回写 + 锚点） | `crate::static_*_list`、`crate::deque_*_list` |
//! | [`scroll_bar`] | 自绘滚动条（拖拽 / 轨道点击 / 淡出 / 双轴） | `crate::scroll_bar` |
//! | [`table`] | 列定义（Fixed/Flex/跨列）、表头排序、行虚拟化、多选、空态（E9 `Table`/`Row`/`Cell`/`ColumnHeader` 集合语义） | `crate::table` |
//! | [`tree`] | 展开折叠、缩进层级线、选择、键盘导航（实例级键作用域；E9 `Tree`/`TreeItem` + `level` 层级） | `crate::tree` |
//! | [`overlay`] | 通用锚定浮层：12 向 placement + flip + clamp、滚动跟随、模态遮罩、`Transient` 淡入淡出、E10 完成回调收尾（`AnimationRouter` 单槽路由）、E12 显式 a11y 角色、焦点陷阱 / 恢复 | `crate::overlay` + `crate::layer` 的浮层部分 |
//! | [`layer`] | 固定 z 层带（View < Dialog < Alert < Popup < ContextMenu < Tooltip）与 root 子节点排序仲裁 | `crate::layer` |
//! | [`tooltip`] | hover / focus 触发 + E10 一次性定时器延迟 + 12 向定位 + 文本 / 快捷键 + 不可交互 + 模态抑制 / 显式 a11y | `crate::tooltip` |
//! | [`menu`] | 菜单条目（分组 / 分隔线 / 快捷键 / 子菜单 / 危险项）+ **共享条目面板**（浮层 + E3 虚拟化行 + 键盘 + a11y） | `crate::menu` |
//! | [`dropdown`] | 触发器按钮 + 扁平条目面板（选中项 / 图标 / 键盘 / 宽度下限对齐触发器 / 长列表虚拟化） | `crate::dropdown` |
//! | [`select`] | 单选 / 多选 / 标签（badge 等价视觉）/ 清空 / 占位 / 禁用 / 图标 / 键盘（`ListBox` 语义行） | `crate::select` |
//! | [`context_menu`] | 右键坐标 → 浮层锚点、窗口级单例、关闭语义与上报顺序 | `crate::context_menu` |
//! | `test_support` | 基于 `ui_core::test_support` 的测试断言辅助（TestApp/Probe/FrameRecord） | 无（新基建） |
//!
//! # 组件形态（与引擎模型对齐）
//!
//! 1. `new(app, ...)`：建状态（`Signal` / `Transient`）与动作身份；
//! 2. `build(tree, parent)`：只建节点（容器 / 表头 / 轨道），行内容由 `mount` 后的
//!    [`list::List::sync`] 填充——引擎的虚拟列表行工厂只能返回**单个**节点，复合行由应用层
//!    在自己的行容器里再建子树（见 `list` 模块文档）；
//! 3. `mount(app, window, parent)` / `attach(app, window)`：把节点接进引擎的滚动 / 虚拟列表；
//! 4. `sync(app)`：帧驱动的同步（行内容填充、行高测量回写、悬停镜像、滚动条几何）。
//!    **没有** `on_frame` 自注册：引擎的 `App::on_frame` 只有一个钩子槽（后注册覆盖先注册），
//!    组合根应在此钩子里依次调用各组件 `sync`。测试里直接调 `sync` 即可（`TestApp::drain`
//!    不跑该钩子）。
//!
//! # 无障碍（E9 集合语义）
//!
//! 本批把容器组件的 a11y 声明对齐引擎 E9：`list` 保持引擎自动推导的 `List` / `ListBoxOption`
//! （不声明集合角色）；`table` 声明 `Table` / `Row` / `Cell` / `ColumnHeader`；`tree` 声明
//! `Tree` / `TreeItem`（扁平化虚拟树的嵌套深度走 `aria-level`）。组件不手写 `A11yNode` 结构：
//! 只声明角色 / 标签 / 状态与推导不出的列语义，父子关系由引擎的 E9「逻辑父」规则推导。
//!
//! # 设计约束（为什么这些模块在应用层）
//!
//! 设计 §8.5 / §10.2 把「资源文件、图标转换、主题语义色」划在应用层：引擎只提供
//! `ResourceCache`（字节 → 解码 → 图集 tile）与 E7 的 `NodeKind::Image`（`ImageFit` / 调制 /
//! 异步到达 / DPI 重解析）两个原语，不依赖 `usvg` / `resvg`。因此图标是**构建期**光栅化的 PNG
//! （`build.rs`），运行期只把它们放进引擎图集，再挂成 E7 图片节点并按主题色调制。
//!
//! 动画同样不在这里建帧循环：`motion` 只给曲线，`animation` 只给「弹层这一帧要不要画」的判据；
//! 真正的动画必须注册到引擎 `App::animate`（`Transient` + 显式节点集合），见设计 §3.7 / §10.3。

pub mod alert;
pub mod animation;
pub mod assets;
pub mod badge;
pub mod brand;
pub mod button;
pub mod context_menu;
pub mod dropdown;
pub mod empty_state;
pub mod format;
pub mod icon;
pub mod input;
pub mod interactive;
pub mod join;
pub mod layer;
pub mod list;
pub mod markdown;
pub mod menu;
pub mod motion;
pub mod overlay;
pub mod progress;
pub mod scroll_bar;
pub mod select;
pub mod separator;
pub mod size;
pub mod spinner;
pub mod tab_bar;
pub mod table;
pub mod toggle;
pub mod tooltip;
pub mod tree;

/// 测试断言辅助。**只在测试构建里存在**（依赖 `ui_core/test-support`，见本文件模块文档）。
#[cfg(test)]
pub mod test_support;
