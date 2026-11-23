//! `automation` feature 的实现：生产可见的输入注入与绘制观测。
//!
//! 这是**验收构建**（kytrix `--acceptance`）要的那一层：真实窗口跑着生产泵，脚本需要一个
//! 「注入一次输入」「这一帧画了多少图元 / 画了哪些文字」的读点。它与 `test-support` 是两件
//! 事 —— 后者只在测试构建里存在（`TestApp` / `FrameRecord` / oracle），本模块进发布物。
//!
//! # 边界
//!
//! - **注入**：直接交给 [`crate::app::input::dispatch_input`]（平台泵的同一条路径），因此
//!   命中 / 焦点 / 动作 / 滚动 / 指针捕获的语义与真机一致，不另起一套捷径。
//! - **只读**：`frame_index` / `painted_primitives` / `texts` 都只读已有状态，**不请求帧**
//!   （I3）。`painted_primitives` 读的是场景（`Window::scene`，跨帧保留），所以它回答的是
//!   「最近一帧绘制了什么」而不是「本帧重编了什么」；后者属于 `test-support` 的帧记录。
//! - **不改变任何引擎语义**：没有新的 Primitive 变体、没有新的失效边、不参与布局 / 绘制。

use smol_str::SmolStr;

use crate::{
    app::App,
    event::{InputEvent, InputOutcome},
    node::tree::NodeKind,
    window::{FrameIndex, WindowId},
};

pub(crate) fn inject_input(app: &mut App, window: WindowId, event: InputEvent) -> InputOutcome {
    app.dispatch_input(window, event)
}

pub(crate) fn frame_index(app: &App, window: WindowId) -> Option<FrameIndex> {
    app.window(window).map(crate::window::Window::frame_index)
}

pub(crate) fn painted_primitives(app: &App, window: WindowId) -> usize {
    app.window(window)
        .map_or(0, |window| window.scene().primitive_count())
}

pub(crate) fn texts(app: &App, window: WindowId) -> Vec<SmolStr> {
    let Some(window) = app.window(window) else {
        return Vec::new();
    };
    let tree = window.tree();
    let mut texts = Vec::new();
    let mut stack = vec![tree.root()];
    while let Some(node) = stack.pop() {
        let Ok(entry) = tree.get(node) else {
            continue;
        };
        if let NodeKind::Text(content) = entry.kind() {
            texts.push(content.text.clone());
        }
        stack.extend(entry.children().iter().rev().copied());
    }
    texts
}
