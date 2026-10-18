use gpui_kit::{prelude::FluentBuilder, *};

use crate::style::theme::ThemeExt;

#[derive(IntoElement)]
pub struct Progress {
    id: ElementId,
    value: f32,
}

impl Progress {
    pub fn new(id: impl Into<ElementId>) -> Progress {
        Self {
            id: id.into(),
            value: 0.0,
        }
    }

    pub fn value(mut self, value: f32) -> Self {
        self.value = value.clamp(0., 100.);
        self
    }
}

impl RenderOnce for Progress {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Progress { id, value } = self;

        let colors = cx.theme();

        gpui_kit::base::Progress::new(id)
            .value(value)
            .w_full()
            .h_2()
            .child(
                gpui_kit::base::ProgressTrack::new()
                    .size_full()
                    .bg(colors.components.progress.track.hsla)
                    .rounded_xs()
                    .child(
                        gpui_kit::base::ProgressIndicator::new()
                            .w(relative(value / 100.0))
                            .h_full()
                            .rounded_xs()
                            .when(value < 100.0, |this| this.rounded_r(px(1.0)))
                            .bg(colors.components.progress.progress.hsla),
                    ),
            )
    }
}
