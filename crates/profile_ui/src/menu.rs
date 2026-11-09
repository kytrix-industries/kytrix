//! The account menu's content: who is signed in, and the entries that change it.
//!
//! The menu is the panel of the account control the title bar draws. That control belongs to
//! [`crate::profile::ProfileMenu`]; this module draws the content and nothing else: the
//! account block, the entries, and the face the control draws.
//!
//! # What it does not draw
//!
//! Failures. A failure that has to be found by opening a menu is a failure the user does not
//! see; every failure this feature can produce is reported where the user already is — a
//! dialog for a sign-in, the log for a sign-out.

use std::sync::Arc;

use app_config::settings::AccountWalletKind;
use base_ui::{
    badge::Badge,
    brand::Brand,
    button::Button,
    icon::{Icon, IconKind},
    interactive::InteractiveVariantExt,
    menu::{Menu, MenuItem, MenuItemAction},
    size::UnifiedSizable,
    style::theme::ThemeExt,
    tooltip::{Tooltip, TooltipExt},
};
use gpui_kit::{
    AnyElement, App, ClipboardItem, Div, ElementId, FontWeight, Image, InteractiveElement,
    IntoElement, ParentElement, Pixels, SharedString, StatefulInteractiveElement, Styled, div, img,
    px,
};
use i18n::t;
use kytrix_service_provider::auth::model::PlanCode;

use crate::account::SignedInAccount;

/// The avatar of the account block, which is the face read at a glance.
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
pub(crate) const MENU_WIDTH: f32 = 280.0;

/// The entry that opens the account dashboard in the browser.
pub(crate) const OPEN_ACCOUNT_ITEM_ID: &str = "open-account";

/// The entry that signs the current account out.
pub(crate) const SIGN_OUT_ITEM_ID: &str = "sign-out";

/// What an activated menu entry asks for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProfileAction {
    OpenAccount,
    SignOut,
}

/// The action a menu entry stands for, or `None` for an entry this menu does not own.
pub(crate) fn profile_action(item_id: &str) -> Option<ProfileAction> {
    match item_id {
        OPEN_ACCOUNT_ITEM_ID => Some(ProfileAction::OpenAccount),
        SIGN_OUT_ITEM_ID => Some(ProfileAction::SignOut),
        _ => None,
    }
}

/// The content of the account menu, as it stands when one frame of it is drawn.
///
/// The menu holds the account it belongs to — the face with the wallet family's mark on its
/// lower-right corner, the subscription tier, and the shortened address, which offers the
/// whole address on hover, with the button that copies it — above the entries that open and
/// sign out the account. There is nothing else in it: somebody who is not signed in has the
/// title bar's control to press, not a menu to open.
pub(crate) struct AccountMenu {
    account: SignedInAccount,
    avatar: Option<Arc<Image>>,
    plan: Option<PlanCode>,
}

impl AccountMenu {
    pub(crate) fn new(
        account: SignedInAccount,
        avatar: Option<Arc<Image>>,
        plan: Option<PlanCode>,
    ) -> Self {
        Self {
            account,
            avatar,
            plan,
        }
    }

    /// The menu of entries: the account, and the two things to do with it.
    ///
    /// The entries are built on every frame the menu is drawn rather than moved into it once,
    /// because the account, the face and the tier are the menu's own state and can read
    /// differently from one frame to the next.
    ///
    /// Neither entry is ever disabled: a dashboard handoff that fails reports itself in an
    /// alert rather than making the entry unavailable, and a sign-out finishes before the
    /// click is over.
    pub(crate) fn entries(&self, id: &ElementId, cx: &App) -> Menu {
        let mut menu = Menu::new((id.clone(), "menu")).small();

        menu = menu.item(MenuItem::Custom(self.account_block(id, cx)));
        menu = menu.item(MenuItem::Separator);
        menu = menu.item(MenuItem::Action(MenuItemAction::new(
            OPEN_ACCOUNT_ITEM_ID,
            t!(cx, "window.account.open_account"),
        )));
        menu = menu.item(MenuItem::Action(MenuItemAction::new(
            SIGN_OUT_ITEM_ID,
            t!(cx, "window.account.sign_out"),
        )));

        menu
    }

    /// The account block: face, tier and the address it belongs to, stacked and centred.
    fn account_block(&self, id: &ElementId, cx: &App) -> AnyElement {
        let account = &self.account;
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
                        avatar_face(
                            self.avatar.as_ref(),
                            px(ACCOUNT_AVATAR_SIZE),
                            "TEST_PROFILE_ACCOUNT_AVATAR",
                        )
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
}

/// The account's face at `size`: its generated avatar, or the default glyph while none is
/// ready.
///
/// It is the same face the menu's account block draws, so a caller that draws its own account
/// entry can show the face of the menu behind it.
pub(crate) fn avatar_face(
    avatar: Option<&Arc<Image>>,
    size: Pixels,
    selector: &'static str,
) -> Div {
    let face = div()
        .size(size)
        .rounded_full()
        .debug_selector(move || selector.to_owned())
        .flex()
        .items_center()
        .justify_center();

    match avatar {
        Some(avatar) => face.child(img(avatar.clone()).size_full()),
        None => face.child(Icon::new(IconKind::User).size_4()),
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
