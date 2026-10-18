use std::sync::{
    LazyLock,
    atomic::{AtomicU64, Ordering},
};

use gpui_kit::{
    AnyView, Context, ElementId, Empty, Entity, InteractiveElement, IntoElement, ParentElement,
    Render, SharedString, Styled, Window,
    base::{Easing, Presence, Transition, box_shadow},
    div,
    prelude::FluentBuilder as _,
    px,
};

use crate::{
    alert::Alert,
    animation::{ANIMATION_TICK, popup_should_render},
    layer::LayerExt,
    style::{palette::Appearance, theme::ThemeExt},
};

static DIALOG_ID: LazyLock<AtomicU64> = LazyLock::new(|| AtomicU64::new(0));

/// Which layer slot a dialog occupies, which is the slot it cleans once it has left.
#[derive(Copy, Clone)]
enum DialogSlot {
    /// A dialog mounted by [`LayerExt::dialog`].
    Main,
    /// An alert mounted by [`LayerExt::alert`]: the layer above the dialog one.
    Alert,
}

/// What a dialog draws.
enum DialogContent {
    /// A general dialog: the content view the caller mounted, with its type erased. Drawing it is
    /// a handle clone, and the view it names is what renders itself.
    View(AnyView),
    /// An alert: the entity is made by [`LayerExt::alert`] before the dialog is mounted, so that
    /// building an alert — which may make an input entity of its own — happens in the mount effect
    /// rather than in the middle of a frame.
    Alert(Entity<Alert>),
}

/// Generic dialog: backdrop, centered panel, enter/exit transition, and removal of itself from the
/// window layer once its exit transition has finished.
///
/// The panel only provides the background, corner radius, shadow and lift animation, while size,
/// padding and content layout are up to the content.
///
/// The content is a view entity the caller made and keeps a handle of its own to. The dialog holds
/// a second, strong reference, so the content stays alive for as long as the dialog draws it and is
/// released once the dialog has left the layer — state the content owns needs no cleanup code
/// anywhere else.
/// Mounted through [`LayerExt::dialog`], or through [`LayerExt::alert`] when it is an alert. A
/// general dialog is closed by its content emitting [`gpui_kit::DismissEvent`], and by nothing
/// else: the layer has no way to close a dialog from outside, because what a dialog is showing is
/// what decides when it is done.
pub struct Dialog {
    id: ElementId,
    opened: bool,
    /// Whether the caret has already been taken out of whatever held it before this dialog opened.
    ///
    /// A dialog is modal: whatever held the caret before it opened is behind it now, so the caret is
    /// dropped on the first frame the dialog is shown rather than left in a field the user can no
    /// longer see. The dialog's own content is drawn after that, and takes the caret back if it
    /// wants it.
    cleared_focus: bool,
    slot: DialogSlot,
    content: DialogContent,
}

impl Dialog {
    /// A dialog whose content is `content`, already erased to [`AnyView`] by the caller.
    pub(crate) fn new(content: impl Into<AnyView>) -> Self {
        let id = ElementId::NamedInteger(
            SharedString::new_static("dialog"),
            DIALOG_ID.fetch_add(1, Ordering::Relaxed),
        );

        Self {
            id,
            opened: true,
            cleared_focus: false,
            slot: DialogSlot::Main,
            content: DialogContent::View(content.into()),
        }
    }

    /// The same dialog on the alert slot, which is where [`LayerExt::alert`] mounts it. The alert it
    /// draws is already made: an alert is what the layer holds, not something the dialog builds.
    pub(crate) fn alert(alert: Entity<Alert>) -> Self {
        let id = ElementId::NamedInteger(
            SharedString::new_static("dialog"),
            DIALOG_ID.fetch_add(1, Ordering::Relaxed),
        );

        Self {
            id,
            opened: true,
            cleared_focus: false,
            slot: DialogSlot::Alert,
            content: DialogContent::Alert(alert),
        }
    }

    /// Whether this dialog is still open. A closed dialog is playing its exit transition, and a
    /// second [`Dialog::close`] reached while it does is a no-op rather than a fresh dismissal.
    pub(crate) fn is_open(&self) -> bool {
        self.opened
    }

    /// Requests a close: once the exit transition has finished the panel removes itself from the
    /// window layer.
    ///
    /// Reached from the content's [`gpui_kit::DismissEvent`], which is the subscription
    /// [`LayerExt::dialog`] installs, and from the alert's own buttons.
    pub fn close(&mut self) {
        self.opened = false;
    }
}

impl Render for Dialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let sample = Presence::new(self.id.clone(), self.opened)
            .transition(Transition::new(ANIMATION_TICK).easing(Easing::EaseInOut))
            .sample(window, cx);

        if !popup_should_render(self.opened, sample) {
            match self.slot {
                DialogSlot::Main => window.clean_dialog(&cx.entity(), cx),
                DialogSlot::Alert => window.clean_alert(&cx.entity(), cx),
            }

            return Empty.into_any_element();
        }

        // The caret is dropped on the first frame the dialog is *shown*, which is not the first
        // frame it draws: a dialog dismissed before it ever appeared draws nothing, so it must
        // leave the caret of whatever is on screen where it was.
        if self.opened && !self.cleared_focus {
            self.cleared_focus = true;
            window.blur(cx);
        }

        let content = match &mut self.content {
            DialogContent::View(view) => view.clone().into_any_element(),
            DialogContent::Alert(alert) => alert.clone().into_any_element(),
        };
        let theme = cx.theme();
        // The panel's edge is the one paint in the theme that reads the appearance rather than a
        // role: §8.3 gives the dark panel a divider-colored edge, where a shadow is nearly
        // invisible against a near-black page, and leaves the light one with its shadow alone. It
        // stays in the paint rather than in `DialogColors` because `ComponentColors::resolve`
        // takes only `Roles` (§10.2), and a role does not know the appearance. The flag costs one
        // comparison and no color-space conversion, so the paint is still a field read (§13.3 C2).
        let panel_edge =
            matches!(theme.appearance, Appearance::Dark).then(|| theme.roles.line.divider.hsla);

        div()
            .relative()
            .size_full()
            .occlude()
            .opacity(sample.progress)
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .bg(theme.components.dialog.backdrop.hsla),
            )
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .top(px(-8.0 * (1.0 - sample.progress)))
                            .rounded_sm()
                            .p_2()
                            .bg(theme.components.dialog.background.hsla)
                            .when_some(panel_edge, |this, edge| this.border_1().border_color(edge))
                            .shadow(vec![box_shadow(
                                0.,
                                25.,
                                50.,
                                -12.0,
                                theme.components.dialog.shadow.hsla,
                            )])
                            // A dialog that is leaving takes nothing of its own content: it is
                            // still drawn while it fades out, but what it drew no longer acts. The
                            // guard is a capture-phase listener, so it runs before the content's
                            // own handlers and the mouse-up that follows finds nothing pending:
                            // without it, the second click of a double-click would run the action
                            // the first one ran once more, on a dialog that is on its way out.
                            //
                            // It is on the panel rather than on the root, so that it swallows a
                            // press only where the content is — which is where that hazard is — and
                            // leaves the rest of the window alone. On the root it would swallow
                            // every press in the window for the whole fade, and a popup above the
                            // layer would be handed none of them: the mouse-down would stop before
                            // the popup's own listeners, so an open menu could neither be used nor
                            // dismissed by a press outside it. The dialog stays unreachable
                            // underneath for the whole fade regardless, because the root's
                            // `.occlude()` hitbox is there for as long as the root is drawn.
                            .when(!self.opened, |this| {
                                this.capture_any_mouse_down(|_, _, cx| cx.stop_propagation())
                            })
                            .child(content),
                    ),
            )
            .into_any_element()
    }
}
