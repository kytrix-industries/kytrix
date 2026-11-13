//! ② 焦点状态面板：焦点名 / Tab 序 / 操作提示（`design/phase-15-demo-showcase.md` §2.1）。
//!
//! 引擎不派发「焦点变化」事件（`design/phase-10-interaction.md` §6）：应用在每轮帧钩子里读
//! `App::focused`，把结果写成值语义的 [`FocusView`]，由 `bind_text` 更新文字。

use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    app::App,
    math::Rect,
    node::{id::NodeId, signal::Signal},
    window::WindowId,
};

use super::{DIM_RGBA, MUTED_RGBA, TEXT_RGBA, card, text_style};

/// 焦点面板的值（`Signal<FocusView>` 的载荷）。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FocusView {
    /// 焦点在 Tab 序里的下标；`None` = 没有焦点。
    index: Option<u8>,
    /// Tab 序里的可聚焦节点总数。
    total: u8,
    /// 焦点节点的可访问名。
    label: SmolStr,
}

impl Default for FocusView {
    fn default() -> Self {
        Self {
            index: None,
            total: 0,
            label: SmolStr::new_static("—"),
        }
    }
}

impl FocusView {
    /// 直接构造（测试与 [`FocusView::resolve`] 共用；`label` 是静态可访问名）。
    pub(crate) fn new(index: Option<usize>, total: usize, label: &'static str) -> Self {
        Self {
            index: index.and_then(|index| u8::try_from(index).ok()),
            total: u8::try_from(total).unwrap_or(u8::MAX),
            label: SmolStr::new_static(label),
        }
    }

    /// 从「当前焦点 + Tab 序表」算出视图。
    pub(crate) fn resolve(focused: Option<NodeId>, targets: &[(NodeId, &'static str)]) -> Self {
        match focused.and_then(|node| targets.iter().position(|(id, _)| *id == node)) {
            Some(index) => Self::new(Some(index), targets.len(), targets[index].1),
            None => Self::new(None, targets.len(), "—"),
        }
    }

    /// 「焦点：色块 1（红）」/「焦点：（无）」。
    pub(crate) fn line(&self) -> SmolStr {
        if self.index.is_some() {
            format_smolstr!("焦点：{}", self.label)
        } else {
            SmolStr::new_static("焦点：（无）")
        }
    }

    /// 「Tab 序 3 / 7」/「Tab 序 — / 7」。
    pub(crate) fn position(&self) -> SmolStr {
        match self.index {
            Some(index) => format_smolstr!("Tab 序 {} / {}", u16::from(index) + 1, self.total),
            None => format_smolstr!("Tab 序 — / {}", self.total),
        }
    }
}

/// 建焦点面板的文字（值来自 `view`）。
pub(crate) fn build(app: &mut App, window: WindowId, panel: NodeId, view: Signal<FocusView>) {
    let _ = card::bound_label(
        app,
        window,
        panel,
        Rect::new(18.0, 42.0, 276.0, 22.0),
        text_style(13.5, 400, TEXT_RGBA),
        move |cx| cx.read(&view).line(),
    );
    let _ = card::bound_label(
        app,
        window,
        panel,
        Rect::new(18.0, 70.0, 276.0, 20.0),
        text_style(12.5, 400, MUTED_RGBA),
        move |cx| cx.read(&view).position(),
    );
    for (index, hint) in [
        "Tab / Shift+Tab 循环切换",
        "Enter / Space 激活焦点控件",
        "Esc 退出（组合交给系统 IME）",
    ]
    .iter()
    .enumerate()
    {
        let _ = card::label(
            app,
            window,
            panel,
            Rect::new(18.0, 96.0 + index as f32 * 22.0, 276.0, 18.0),
            *hint,
            text_style(11.5, 400, DIM_RGBA),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::FocusView;

    #[test]
    fn focused_index_maps_to_the_position_line() {
        let view = FocusView::new(Some(1), 3, "乙");

        assert_eq!(view.line().as_str(), "焦点：乙");
        assert_eq!(view.position().as_str(), "Tab 序 2 / 3");
    }

    #[test]
    fn missing_focus_is_reported_as_none() {
        let view = FocusView::new(None, 3, "—");

        assert_eq!(view.line().as_str(), "焦点：（无）");
        assert_eq!(view.position().as_str(), "Tab 序 — / 3");
    }

    #[test]
    fn empty_target_table_still_renders() {
        let view = FocusView::new(None, 0, "—");

        assert_eq!(view.position().as_str(), "Tab 序 — / 0");
    }
}
