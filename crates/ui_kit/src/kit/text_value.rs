use smol_str::SmolStr;
use ui_core::node::{dependency::ReadCtx, signal::Signal};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TextValue {
    Static(SmolStr),
    Bound(Signal<SmolStr>),
}

impl TextValue {
    #[must_use]
    pub fn static_text(text: impl Into<SmolStr>) -> Self {
        Self::Static(text.into())
    }

    #[must_use]
    pub const fn bound(signal: Signal<SmolStr>) -> Self {
        Self::Bound(signal)
    }

    #[must_use]
    pub const fn is_bound(&self) -> bool {
        matches!(self, Self::Bound(_))
    }

    #[must_use]
    pub fn initial_text(&self) -> SmolStr {
        match self {
            Self::Static(text) => text.clone(),
            Self::Bound(_) => SmolStr::default(),
        }
    }

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
