//! The account entry in the window header: an avatar that opens a menu.
//!
//! It is a [dropdown](base_ui::dropdown::Dropdown) in all but its trigger and its
//! contents: the popup is the one [`Overlay`] already draws, so this control places and
//! fades and dismisses its menu exactly as the pane controls do. What it holds is
//! nothing: it draws the account it is handed and calls back when the user asks for
//! something. Everything that has to be remembered — whether an attempt is running, what
//! the plan is, whether a sign-out failed — belongs to whoever builds it.

use std::{rc::Rc, sync::Arc};

use app_config::settings::AccountWalletKind;
use base_ui::{
    badge::Badge,
    brand::Brand,
    button::Button,
    icon::{Icon, IconKind},
    interactive::InteractiveVariantExt,
    menu::{Menu, MenuItem, MenuItemAction},
    overlay::Overlay,
    size::UnifiedSizable,
    style::theme::ThemeExt,
    tooltip::{Tooltip, TooltipExt},
};
use gpui_kit::{
    AnyElement, App, ClipboardItem, Div, ElementId, FontWeight, Image, InteractiveElement,
    IntoElement, ParentElement, RenderOnce, Role, SharedString, StatefulInteractiveElement,
    StyleRefinement, Styled, Window,
    base::{Align, Placement, StyledExt},
    div, img,
    prelude::FluentBuilder,
    px,
};
use i18n::t;
use kytrix_service_provider::auth::model::PlanCode;

use crate::account::SignedInAccount;

/// The avatar of the trigger, inside a header whose row is 48 pixels tall.
const TRIGGER_AVATAR_SIZE: f32 = 28.0;

/// The avatar of the account block, which is the same face read at a glance.
const ACCOUNT_AVATAR_SIZE: f32 = 56.0;

/// The wallet family's mark, as a badge on the corner of that face.
const ACCOUNT_BADGE_SIZE: f32 = 20.0;

/// The mark inside the badge's plate: what is left of the plate is the ring that cuts the
/// badge out of the face behind it.
const ACCOUNT_BADGE_LOGO_SIZE: f32 = 14.0;

/// How wide the menu is.
///
/// It is a fixed width because the block inside it is a centred identity whose widest
/// line is an address, and a menu that resized as an address changed would move the
/// entries under the pointer that opened it.
const MENU_WIDTH: f32 = 280.0;

/// The entry that starts a sign-in.
pub(crate) const SIGN_IN_ITEM_ID: &str = "sign-in";

/// The entry that signs the current account out.
pub(crate) const SIGN_OUT_ITEM_ID: &str = "sign-out";

/// What an activated menu entry asks for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProfileAction {
    SignIn,
    SignOut,
}

/// The action a menu entry stands for, or `None` for an entry this menu does not own.
pub(crate) fn profile_action(item_id: &str) -> Option<ProfileAction> {
    match item_id {
        SIGN_IN_ITEM_ID => Some(ProfileAction::SignIn),
        SIGN_OUT_ITEM_ID => Some(ProfileAction::SignOut),
        _ => None,
    }
}

/// An avatar that opens the account menu.
///
/// # What it draws
///
/// Signed out, the trigger is the default avatar glyph and the menu holds one entry:
/// sign in. Signed in, the trigger is the account's own avatar and the menu holds the
/// account it belongs to — the face with the wallet family's mark on its lower-right
/// corner, the subscription tier, and the shortened address, which offers the whole
/// address on hover, with the button that copies it — above the entry that signs out.
/// The badge belongs to the face in the menu alone: the trigger is drawn wherever the
/// header puts it, and a mark on it there would be a claim about a menu nobody opened.
///
/// # What it does not draw
///
/// Failures. A failure that has to be found by opening a menu is a failure the user
/// does not see; every failure this feature can produce is reported where the user
/// already is — a dialog for a sign-in, the log for a sign-out.
#[derive(IntoElement)]
pub struct AvatarDropDown {
    id: ElementId,
    label: SharedString,
    avatar: Option<Arc<Image>>,
    account: Option<SignedInAccount>,
    plan: Option<PlanCode>,
    style: StyleRefinement,
    on_sign_in: Option<Rc<dyn Fn(&mut Window, &mut App)>>,
    on_sign_out: Option<Rc<dyn Fn(&mut Window, &mut App)>>,
}

impl AvatarDropDown {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            avatar: None,
            account: None,
            plan: None,
            style: StyleRefinement::default(),
            on_sign_in: None,
            on_sign_out: None,
        }
    }

    /// The account the menu is for; `None` means nobody is signed in.
    pub fn account(mut self, account: Option<SignedInAccount>) -> Self {
        self.account = account;
        self
    }

    /// The generated avatar of that account, once it has one.
    pub fn avatar(mut self, avatar: Option<Arc<Image>>) -> Self {
        self.avatar = avatar;
        self
    }

    /// The subscription tier of that account, or `None` while it is not known.
    pub fn plan(mut self, plan: Option<PlanCode>) -> Self {
        self.plan = plan;
        self
    }

    pub fn on_sign_in(mut self, on_sign_in: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_sign_in = Some(Rc::new(on_sign_in));
        self
    }

    pub fn on_sign_out(mut self, on_sign_out: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_sign_out = Some(Rc::new(on_sign_out));
        self
    }

    /// The avatar slot: the account's face, or the default glyph standing in for it.
    fn avatar_slot(&self, size: f32, selector: &'static str, cx: &App) -> Div {
        div()
            .size(px(size))
            .rounded_full()
            .debug_selector(move || selector.to_owned())
            .flex()
            .items_center()
            .justify_center()
            .when_some(self.avatar.as_ref(), |this, avatar| {
                this.child(img(avatar.clone()).size_full())
            })
            .when_none(&self.avatar, |this| {
                this.child(
                    Icon::new(IconKind::User)
                        .size_4()
                        .text_color(cx.theme().roles.fg.muted.hsla),
                )
            })
    }

    /// The account block: face, tier and the address it belongs to, stacked and centred.
    fn account_block(&self, id: &ElementId, account: &SignedInAccount, cx: &App) -> AnyElement {
        let address = account.wallet_address();
        let copy_button = Button::new((id.clone(), "copy-address"))
            .ghost()
            .small()
            .px(px(4.0))
            .aria_label(t!(cx, "window.account.copy_wallet_address"))
            .with_tooltip(t!(cx, "window.account.copy_wallet_address"))
            .on_click(move |_, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(address.to_string()));
            })
            .child(Icon::new(IconKind::CopySimpleBold).size_3());

        div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .items_center()
            .gap_2()
            .rounded_sm()
            .p_2()
            .debug_selector(|| "TEST_PROFILE_ACCOUNT".into())
            .child(
                // The face is what the badge hangs off, so it is the box the badge is
                // placed against: the mark belongs to the avatar rather than to a line
                // of the column.
                div()
                    .relative()
                    .child(
                        self.avatar_slot(ACCOUNT_AVATAR_SIZE, "TEST_PROFILE_ACCOUNT_AVATAR", cx)
                            .bg(cx.theme().roles.surface.raised.hsla),
                    )
                    .child(wallet_badge(account.wallet_kind(), cx)),
            )
            .child(self.plan_slot(id, cx))
            .child(
                div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap_1()
                    // The room the copy control takes on the right is held open on the
                    // left by the same control, hidden: the two sides then split what is
                    // left of the row evenly, so the address keeps the centre of the menu.
                    .child(
                        div().flex_1().flex().items_center().justify_end().child(
                            Button::new((id.clone(), "copy-address-room"))
                                .ghost()
                                .small()
                                .px(px(4.0))
                                .child(Icon::new(IconKind::CopySimpleBold).size_3())
                                .invisible(),
                        ),
                    )
                    .child(
                        // The line is an address the user may want whole, and this slot is
                        // what lets it shrink: the tooltip sits inside the slot rather than
                        // around it, so a long address still shortens instead of pushing
                        // the button out of the menu.
                        div().min_w_0().child(
                            Tooltip::new(
                                (id.clone(), "address"),
                                div()
                                    .truncate()
                                    .font_weight(FontWeight::BOLD)
                                    .debug_selector(|| "TEST_PROFILE_ADDRESS".into())
                                    .child(account.shortened_address()),
                            )
                            .content(
                                div()
                                    .debug_selector(|| "TEST_PROFILE_ADDRESS_TOOLTIP".into())
                                    .child(account.wallet_address()),
                            ),
                        ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .items_center()
                            .justify_start()
                            .debug_selector(|| "TEST_PROFILE_COPY".into())
                            .child(copy_button),
                    ),
            )
            .into_any_element()
    }

    /// The subscription tier, or the stand-in that says it is not known.
    fn plan_slot(&self, id: &ElementId, cx: &App) -> AnyElement {
        let mut badge = Badge::new().solid().small();
        let (label, selector) = match self.plan {
            Some(plan) => {
                badge = badge.child(SharedString::new_static(plan.display_name()));
                let selector = match plan {
                    PlanCode::Free => "TEST_PROFILE_PLAN_FREE",
                    PlanCode::Plus => "TEST_PROFILE_PLAN_PLUS",
                    PlanCode::Pro => "TEST_PROFILE_PLAN_PRO",
                };
                (SharedString::new_static(plan.display_name()), selector)
            }
            None => {
                badge = badge.child(Icon::new(IconKind::Minus).size_3());
                (
                    t!(cx, "window.account.plan_unavailable"),
                    "TEST_PROFILE_PLAN_UNKNOWN",
                )
            }
        };

        div()
            .id((id.clone(), "plan"))
            .flex_none()
            .aria_label(label)
            .debug_selector(move || selector.to_owned())
            .child(badge)
            .into_any_element()
    }

    /// The menu of entries: a sign-in, or an account and a sign-out.
    ///
    /// The menu is built here rather than in the panel, because the entries are drawn
    /// from data that is gone by the time the panel is: the panel is built on every frame
    /// it is drawn, and a menu whose entries were moved out on the first of those frames
    /// would be an empty one on the next.
    ///
    /// Neither entry is ever disabled. Signing in is offered again the moment an attempt
    /// ends, and an attempt ends when its alert is read, so there is no moment in which
    /// the entry is the one thing the user cannot ask for; signing out finishes before the
    /// click is over.
    fn entries(&self, id: &ElementId, cx: &App) -> Menu {
        let mut menu = Menu::new((id.clone(), "menu")).small();

        match self.account.as_ref() {
            None => {
                menu = menu.item(MenuItem::Action(MenuItemAction::new(
                    SIGN_IN_ITEM_ID,
                    t!(cx, "window.account.sign_in"),
                )));
            }
            Some(account) => {
                menu = menu.item(MenuItem::Custom(self.account_block(id, account, cx)));
                menu = menu.item(MenuItem::Separator);
                menu = menu.item(MenuItem::Action(MenuItemAction::new(
                    SIGN_OUT_ITEM_ID,
                    t!(cx, "window.account.sign_out"),
                )));
            }
        }

        menu
    }
}

/// The wallet family's mark, as a badge on the lower-right corner of the account face.
///
/// The plate under the mark is the colour the account block's own background resolves to
/// over the menu, so the badge reads as a hole cut in the face rather than a sticker laid
/// on it: the mark keeps its edges whatever the generated avatar puts behind them. The
/// hairline around the plate is what keeps the badge's own edge, which a plate painted in
/// the colour behind it would otherwise lose wherever the face is that colour too.
fn wallet_badge(wallet_kind: AccountWalletKind, cx: &App) -> Div {
    let brand = match wallet_kind {
        AccountWalletKind::Ethereum => Brand::ethereum(),
        AccountWalletKind::Solana => Brand::solana(),
    };
    let theme = cx.theme();

    // The badge is centred on the face's own lower-right edge rather than on the corner of
    // the box around it: the face is round, and a badge on the box's corner would float
    // beside the cheek. Half of it therefore falls outside that box — the centre sits on
    // the circle's 45° point, which is `1 - 1/√2` of the radius inside the box's corner.
    let offset = ACCOUNT_AVATAR_SIZE * 0.5 * (1.0 - std::f32::consts::FRAC_1_SQRT_2)
        - ACCOUNT_BADGE_SIZE * 0.5;

    div()
        .absolute()
        .right(px(offset))
        .bottom(px(offset))
        .size(px(ACCOUNT_BADGE_SIZE))
        .rounded_full()
        .bg(theme.roles.surface.overlay.hsla)
        .flex()
        .items_center()
        .justify_center()
        .debug_selector(|| "TEST_PROFILE_BRAND".into())
        .child(brand.size(px(ACCOUNT_BADGE_LOGO_SIZE)))
}

impl Styled for AvatarDropDown {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for AvatarDropDown {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let id = self.id.clone();
        let label = self.label.clone();
        let avatar = self.avatar_slot(TRIGGER_AVATAR_SIZE, "TEST_PROFILE_TRIGGER_AVATAR", cx);
        let entries = self.entries(&id, cx);
        let style = self.style;
        let on_sign_in = self.on_sign_in.clone();
        let on_sign_out = self.on_sign_out.clone();

        // Whether the menu is open is this control's own, keyed by its id, the way it is for
        // every other popup in `base_ui`: the trigger reads it to look pressed, and the
        // overlay closes on a press outside it.
        let open_state = window.use_keyed_state((id.clone(), "open"), cx, |_, _| false);
        let opened = *open_state.read(cx);

        let trigger_label = label.clone();
        let trigger_open = open_state.clone();
        let trigger_id = id.clone();
        let panel_id = id.clone();

        Overlay::new(
            id,
            open_state,
            move |_, _, _| {
                Button::new((trigger_id.clone(), "trigger"))
                    .justify_center()
                    // The header row is the window's drag area, so a control in it takes the
                    // pointer explicitly — and the press is on the box around the button
                    // rather than on the button, which is where the avatar is drawn: an
                    // occluding avatar would stop the hit test before it reached the press.
                    .occlude()
                    .rounded_full()
                    .ghost()
                    .p_0()
                    .size_8()
                    .aria_label(trigger_label)
                    .debug_selector(|| "TEST_PROFILE_TRIGGER".into())
                    .child(avatar)
                    // While the menu is open the press belongs to the menu, which closes on
                    // the way down like any press outside it; a trigger that opened here
                    // would reopen what the menu had just closed.
                    .when(!opened, |this| {
                        this.on_click(move |_, _, cx| trigger_open.write(cx, true))
                    })
                    .into_any_element()
            },
            move |context, _, _| {
                let on_sign_in = on_sign_in.clone();
                let on_sign_out = on_sign_out.clone();

                // The panel closes the menu it belongs to before it reports anything: a
                // callback that opens a dialog opens it over a menu that is already gone.
                let opened = context.open_state;
                let menu = entries
                    .on_item_click(move |_, item_id, window, cx| {
                        opened.write(cx, false);

                        match profile_action(item_id.as_ref()) {
                            Some(ProfileAction::SignIn) => {
                                if let Some(on_sign_in) = on_sign_in.as_ref() {
                                    on_sign_in(window, cx);
                                }
                            }
                            Some(ProfileAction::SignOut) => {
                                if let Some(on_sign_out) = on_sign_out.as_ref() {
                                    on_sign_out(window, cx);
                                }
                            }
                            None => {}
                        }
                    })
                    .into_any_element();

                // The panel is the menu as far as assistive technology is concerned, and its
                // width is fixed by this control rather than by the trigger: the account
                // block inside it is centred on the menu.
                div()
                    .id((panel_id.clone(), "menu-panel"))
                    .role(Role::Menu)
                    .aria_label(label.clone())
                    .debug_selector(|| "TEST_PROFILE_MENU".into())
                    .w(px(MENU_WIDTH))
                    .child(menu)
                    .into_any_element()
            },
        )
        .placement(Placement::Bottom)
        .align(Align::End)
        .refine_style(&style)
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use app_config::settings::AccountWalletKind;
    use base_ui::{button::Button, interactive::InteractiveVariantExt, size::UnifiedSizable};
    use gpui_kit::{
        App, Context, InteractiveElement as _, IntoElement, Modifiers, MouseButton,
        ParentElement as _, Render, StatefulInteractiveElement, Styled as _, TestAppContext,
        Window, div,
    };
    use kytrix_service_provider::auth::model::PlanCode;

    use super::{AvatarDropDown, MENU_WIDTH};
    use crate::account::SignedInAccount;

    const ETHEREUM: &str = "0x0123456789abcdef0123456789abcdef01234567";

    /// An avatar above a button: the trigger sits where a header's controls would be,
    /// so a menu that covers more than it draws is a menu that steals their clicks.
    struct Harness {
        clicks: Rc<Cell<usize>>,
        menu: Box<dyn Fn(&App) -> AvatarDropDown>,
    }

    impl Render for Harness {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let clicks = self.clicks.clone();

            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .child((self.menu)(cx))
                .child(
                    div().debug_selector(|| "TEST_PROFILE_TARGET".into()).child(
                        Button::new("test-profile-target")
                            .tonal()
                            .small()
                            .child("target")
                            .on_click(move |_, _, _| clicks.set(clicks.get() + 1)),
                    ),
                )
        }
    }

    fn harness(
        cx: &mut TestAppContext,
        menu: impl Fn(&App) -> AvatarDropDown + 'static,
    ) -> (Rc<Cell<usize>>, &mut gpui_kit::VisualTestContext) {
        cx.update(|cx| {
            base_ui::style::theme::install(cx);
            i18n::global::embedded::install(cx).expect("embedded locales should load");
        });
        let clicks = Rc::new(Cell::new(0));
        let rendered = clicks.clone();
        let (_, cx) = cx.add_window_view(move |_, _| Harness {
            clicks: rendered,
            menu: Box::new(menu),
        });
        cx.simulate_resize(gpui_kit::size(gpui_kit::px(400.0), gpui_kit::px(400.0)));
        cx.run_until_parked();

        (clicks, cx)
    }

    /// The menu as the header mounts it with nobody signed in: nothing to show.
    fn nobody(cx: &mut TestAppContext) -> (Rc<Cell<usize>>, &mut gpui_kit::VisualTestContext) {
        harness(cx, |_| AvatarDropDown::new("test-profile", "Account menu"))
    }

    /// Presses what `selector` names and paints what has settled.
    ///
    /// The press is delivered the way a window delivers it — the button goes down, the
    /// window draws the frame that follows, and only then does it come up — because a
    /// control that closes on the way down and opens on the way up behaves differently
    /// when the two are delivered without a frame between them.
    fn press(cx: &mut gpui_kit::VisualTestContext, selector: &'static str) {
        let target = cx
            .debug_bounds(selector)
            .expect("the harness draws what it is asked to press")
            .center();
        cx.simulate_mouse_down(target, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        cx.simulate_mouse_up(target, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }

    /// Opens the menu by pressing its trigger.
    fn open_menu(cx: &mut gpui_kit::VisualTestContext) {
        press(cx, "TEST_PROFILE_TRIGGER");
    }

    /// An account menu that was never opened draws no popup, so a click under its
    /// trigger belongs to whatever is drawn there.
    #[gpui_kit::test]
    fn a_closed_menu_should_not_swallow_a_click_behind_it(cx: &mut TestAppContext) {
        let (clicks, cx) = nobody(cx);

        let target = cx
            .debug_bounds("TEST_PROFILE_TARGET")
            .expect("the harness draws its target");
        cx.simulate_click(target.center(), Modifiers::none());

        assert_eq!(
            clicks.get(),
            1,
            "a menu that has never been opened must not cover what is behind it"
        );
    }

    /// The trigger is the only thing drawn while the menu is closed.
    #[gpui_kit::test]
    fn a_closed_menu_should_draw_only_its_trigger(cx: &mut TestAppContext) {
        let (_, cx) = nobody(cx);

        assert!(cx.debug_bounds("TEST_PROFILE_TRIGGER").is_some());
        assert!(cx.debug_bounds("TEST_PROFILE_MENU").is_none());
    }

    /// The menu is one width whether or not there is an account in it: the block it
    /// holds is a centred identity, and a menu that resized as an address changed
    /// would move the entries under the pointer that opened it.
    #[gpui_kit::test]
    fn a_menu_should_be_the_same_width_with_and_without_an_account(cx: &mut TestAppContext) {
        let (_, cx) = harness(cx, |_| {
            AvatarDropDown::new("test-profile", "Account menu")
                .account(Some(SignedInAccount::new(
                    ETHEREUM,
                    AccountWalletKind::Ethereum,
                )))
                .plan(Some(PlanCode::Plus))
        });
        open_menu(cx);

        let menu = cx
            .debug_bounds("TEST_PROFILE_MENU")
            .expect("the menu should be drawn once it is opened");

        assert_eq!(menu.size.width, gpui_kit::px(MENU_WIDTH));
    }

    /// Pressing the trigger of an open menu is a press outside the menu: it closes it, and
    /// it must not open it again on the way up.
    ///
    /// This is what a [`base_ui::select::Select`] does, and what a
    /// [`base_ui::dropdown::Dropdown`] does: the panel closes on the press going down, so a
    /// trigger that answered the press itself would reopen it on the way up.
    #[gpui_kit::test]
    fn pressing_the_trigger_of_an_open_menu_should_close_it(cx: &mut TestAppContext) {
        let (_, cx) = nobody(cx);
        open_menu(cx);
        assert!(cx.debug_bounds("TEST_PROFILE_MENU").is_some());

        press(cx, "TEST_PROFILE_TRIGGER");

        assert!(
            cx.debug_bounds("TEST_PROFILE_MENU").is_none(),
            "a press on the trigger is a press outside the menu"
        );
    }
}
