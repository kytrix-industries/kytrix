//! `base_ui` 新模块树（引擎化，P2 批次）。
//!
//! 这里的每个模块都只依赖 `ui_core`（节点 + Signal/Transient + Action/Event + 主题），**不含**
//! gpui 类型、`Entity`/`Context`/`Styled`。旧顶层模块（`static_sized_list` 等）在 P2-5 之前
//! 仍与原 gpui 版并存，两者之间没有兼容层（AGENTS §1.1）。
//!
//! 组件形态（与引擎模型对齐）：
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
//! 写作用域按批次划分：本批为容器与列表族（`list`/`scroll_bar`/`table`/`tree`），基础件
//! （`icon`/`asset`/`brand`/`size`/`format`/`motion`/`animation`/`join`/`test_support`/
//! `markdown`/`input`）由并行批次追加。

pub mod list;
pub mod scroll_bar;
pub mod table;
pub mod tree;
