//! 组件文案面：静态文本与信号绑定文本的两种形态（P2-9）。
//!
//! kit 的部分组件（[`crate::kit::badge::Badge`] / [`crate::kit::empty_state::EmptyState`]）在
//! P2-4a 只接受构建期 `SmolStr`：文案写死进 `NodeKind::Text`，语言切换必须「先摘节点、再重建」
//! （AGENTS §15 的四步法；W1/W2 迁移分批承担了重建代价）。本模块把「一段文案」抽象成两种形态：
//!
//! - [`TextValue::Static`]：构建期固定（旧语义，节点上没有动态依赖边）；
//! - [`TextValue::Bound`]：`Signal<SmolStr>` 驱动（`bind_text` / `bind_a11y`），写信号即更新
//!   节点文本与可访问名，**不重建节点**。
//!
//! # 用法（组件内部）
//!
//! ```ignore
//! let initial = label.initial_text();          // Static = 原值；Bound = 空串（首帧之前）
//! let mut text = NodeBuilder::text(initial, style);
//! if label.is_bound() {
//!     let label = label.clone();
//!     text = text.bind_text(move |cx| label.read(cx));
//! }
//! ```
//!
//! `Bound` 的初始文本是空串、由首帧的文本 pass 求出真值（`bind_text` 早于布局 / 绘制）；
//! 需要可访问名的节点在同一个 `ReadCtx` pass 里用 [`TextValue::read`] 补 `bind_a11y.label`。
//! 静态形态**不**注册 `bind_text`：没有信号依赖的闭包只会多一次求值，且会让「静态文案」与
//! 「绑定文案」在依赖图里不可区分（测试无法用 `rendered = false` 的稳态帧证明前者）。

use smol_str::SmolStr;
use ui_core::node::{dependency::ReadCtx, signal::Signal};

/// 组件的文案：构建期静态值或信号绑定值。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TextValue {
    /// 构建期固定的文本（旧 `SmolStr` 语义）。
    Static(SmolStr),
    /// 由信号驱动的文本（写信号即更新，不重建节点）。
    Bound(Signal<SmolStr>),
}

impl TextValue {
    /// 静态文本。
    #[must_use]
    pub fn static_text(text: impl Into<SmolStr>) -> Self {
        Self::Static(text.into())
    }

    /// 绑定到信号。
    #[must_use]
    pub const fn bound(signal: Signal<SmolStr>) -> Self {
        Self::Bound(signal)
    }

    /// 是否绑定到信号。
    #[must_use]
    pub const fn is_bound(&self) -> bool {
        matches!(self, Self::Bound(_))
    }

    /// 建树期可用的文本：`Static` = 原值；`Bound` = 空串（首帧 `bind_text` 求出真值）。
    #[must_use]
    pub fn initial_text(&self) -> SmolStr {
        match self {
            Self::Static(text) => text.clone(),
            Self::Bound(_) => SmolStr::default(),
        }
    }

    /// 在动态 pass 里读当前文本（`Static` 不登记依赖；`Bound` 登记信号）。
    #[must_use]
    pub fn read(&self, cx: &mut ReadCtx<'_>) -> SmolStr {
        match self {
            Self::Static(text) => text.clone(),
            Self::Bound(signal) => cx.read(signal).clone(),
        }
    }
}

impl Default for TextValue {
    fn default() -> Self {
        Self::Static(SmolStr::default())
    }
}

impl From<&str> for TextValue {
    fn from(text: &str) -> Self {
        Self::Static(SmolStr::new(text))
    }
}

impl From<SmolStr> for TextValue {
    fn from(text: SmolStr) -> Self {
        Self::Static(text)
    }
}

impl From<String> for TextValue {
    fn from(text: String) -> Self {
        Self::Static(SmolStr::new(text))
    }
}

#[cfg(test)]
mod tests {
    use smol_str::SmolStr;
    use ui_core::{
        node::{builder::NodeBuilder, tree::NodeKind},
        test_support::app::TestApp,
    };

    use super::TextValue;

    /// 静态形态不注册动态依赖：读回原值、`is_bound` 为假。
    #[test]
    fn a_static_value_reads_back_without_a_signal() {
        let value = TextValue::static_text("Static");
        assert!(!value.is_bound());
        assert_eq!(value.initial_text(), "Static");
        let from_str: TextValue = "From str".into();
        assert_eq!(from_str.initial_text(), "From str");
        let from_smol: TextValue = SmolStr::new_static("From smol").into();
        assert_eq!(from_smol.initial_text(), "From smol");
        assert_eq!(TextValue::default().initial_text(), "");
    }

    /// 绑定形态：初始为空串、`is_bound`、读信号得到当前值；写信号后节点文本更新且节点 id 不变。
    #[test]
    fn a_bound_value_updates_the_text_node_without_a_rebuild() {
        let mut app = TestApp::new();
        let signal = app.app_mut().signal(SmolStr::new_static("综合"));
        let value = TextValue::bound(signal);
        assert!(value.is_bound());
        assert_eq!(value.initial_text(), "", "首帧之前没有静态文本");
        assert_eq!(
            *app.app().read(&signal),
            SmolStr::new_static("综合"),
            "信号真值不依赖节点"
        );

        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let node = {
            let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
            NodeBuilder::text("", ui_core::text::TextStyle::new(12.0))
                .bind_text(move |cx| value.read(cx))
                .build(tree, root)
                .expect("text node")
        };
        let _ = app.drain().expect("first frame");
        assert_eq!(text_of(&app, node), "综合");

        app.app_mut().write(&signal, SmolStr::new_static("General"));
        let _ = app.drain().expect("frame");
        assert_eq!(text_of(&app, node), "General", "写信号即更新，不重建");
    }

    fn text_of(app: &TestApp, node: ui_core::node::id::NodeId) -> SmolStr {
        let window = app.window_id();
        let tree = app.app().window(window).expect("window").tree();
        match tree.get(node).expect("node").kind() {
            NodeKind::Text(content) => content.text.clone(),
            other => panic!("expected text, got {other:?}"),
        }
    }
}
