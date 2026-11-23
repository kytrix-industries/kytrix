//! The account menu's content: who is signed in, and the entries that change it.
//!
//! The menu is the panel of the account control the title bar draws. That control is
//! [`crate::profile::ProfileMenu`]; this module builds the panel's content and nothing else: the
//! account block (the generated face with the wallet family's mark on its lower-right corner, the
//! subscription tier, the shortened address with the control that copies the whole thing) and the
//! two entries that open the dashboard and sign out.
//!
//! It is built into the panel of a `kit::overlay::Overlay` rather than into a `kit::dropdown`: a
//! dropdown's entries are text and icons, and the first thing this menu holds is an account block
//! — a face, a badge on that face, a tier badge and an address row with its own controls. The
//! overlay gives the same thing a dropdown gives (an anchored panel, a band, outside-click
//! dismissal, menu a11y) without prescribing the content.
//!
//! # Rebuilt, not re-bound
//!
//! Kit `Button` labels are build-time values, so a block that contains buttons cannot follow a
//! language change through `bind_*`; `crates/widgets/AGENTS.md` §15.1 freezes the通法 for that:
//! the block records the values it was built from, [`crate::profile::ProfileMenu::sync`] compares
//! the current ones, and a difference rebuilds the block under the same keys. What can be bound
//! **is** bound here — the tier badge's label reads a signal — so a tier that arrives after the
//! menu opened costs the badge a new text rather than a rebuild.
//!
//! # What it does not draw
//!
//! Failures. A failure that has to be found by opening a menu is a failure the user does not see;
//! every failure this feature can produce is reported where the user already is — a dialog for a
//! sign-in, the log for a sign-out. See [`crate::dialogs`].

use app_config::settings::AccountWalletKind;
use base_ui::{
    kit::{
        badge::Badge,
        brand::{Brand, BrandKind},
        button::Button,
        icon::{IconKind, IconSpec},
        layer::Layer,
        overlay::{AnimationRouter, Placement},
        size::{UnifiedSizable, UnifiedSize},
        tooltip::{Tooltip, TooltipContent, TooltipOptions},
    },
    style_builder::StyleBuilder,
    theme::{handle::ThemeHandle, role::Role},
};
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use kytrix_service_provider::auth::model::PlanCode;
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    app::App,
    error::EngineError,
    node::{builder::NodeBuilder, id::NodeId, image::ImageFit, signal::Signal, tree::NodeTree},
    paint::style::StylePatch,
    text::{TextStyle, TextWrap},
    window::WindowId,
};

use crate::{account::SignedInAccount, host::ProfileHost, profile::FaceTexture};

/// How wide the menu is.
///
/// It is a fixed width because the block inside it is a centred identity whose widest line is an
/// address, and a menu that resized as an address changed would move the entries under the
/// pointer that opened it.
pub(crate) const MENU_WIDTH: f32 = 280.0;

/// The kit menu surface's own padding, on each side of the content.
///
/// The panel *is* the menu: its width is the content plus its padding, so the content is sized to
/// leave [`MENU_WIDTH`] for the panel (the control's test asserts the panel's measured width).
const PANEL_PADDING: f32 = 4.0;

/// How wide the content of the menu is.
const MENU_CONTENT_WIDTH: f32 = MENU_WIDTH - 2.0 * PANEL_PADDING;

/// The face of the account block, which is the face read at a glance.
pub(crate) const ACCOUNT_AVATAR_SIZE: f32 = 56.0;

/// The wallet family's mark, as a badge on the corner of that face.
const ACCOUNT_BADGE_SIZE: f32 = 20.0;

/// The mark inside the badge's plate: what is left of the plate is the ring that cuts the badge
/// out of the face behind it.
const ACCOUNT_BADGE_LOGO_SIZE: f32 = 14.0;

/// The gap between the lines of the account block.
const ACCOUNT_BLOCK_GAP: f32 = 6.0;

/// The padding inside the account block.
const ACCOUNT_BLOCK_PADDING: f32 = 8.0;

/// The account block's debug id (a test's read point; node ids are diagnostics, not action
/// identities, so they stay the same in every instance).
const ACCOUNT_BLOCK_ID: &str = "TEST_PROFILE_ACCOUNT";

/// The account block's face debug id.
const ACCOUNT_FACE_ID: &str = "TEST_PROFILE_ACCOUNT_AVATAR";

/// The wallet family's mark debug id.
const WALLET_BADGE_ID: &str = "TEST_PROFILE_BRAND";

/// The tier's debug id.
const PLAN_ID: &str = "TEST_PROFILE_PLAN";

/// The address row's debug id.
const ADDRESS_ROW_ID: &str = "TEST_PROFILE_ACCOUNT_ADDRESS_ROW";

/// The shortened address's debug id.
const ADDRESS_ID: &str = "TEST_PROFILE_ADDRESS";

/// The account menu's content root debug id.
const MENU_ROOT_ID: &str = "TEST_PROFILE_MENU";

/// What a control inside the menu asks the account control to do.
///
/// The entries are kit buttons, whose click callbacks get an [`ActionCtx`] and no handle on the
/// account control itself, so a click *requests* the work: the control takes the request in its
/// `sync` and runs the flow there, where it has the whole control in hand. The same channel
/// serves the trigger's own press, which is why the request carries it too.
///
/// [`ActionCtx`]: ui_core::interaction::ActionCtx
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MenuRequest {
    /// The trigger was pressed: open or close the menu, or start a sign-in when nobody is signed
    /// in.
    Trigger,
    /// Copy the complete wallet address.
    CopyAddress,
    /// Open the account dashboard in the browser.
    OpenAccount,
    /// Ask whether to sign the account out.
    SignOut,
    /// The waiting alert was read: end the sign-in attempt behind it.
    CancelSignIn,
    /// The sign-out question was answered with yes.
    ConfirmSignOut,
}

/// The signals the menu content reads and writes.
///
/// They are minted by the control into its own scope in `build` and handed to the content, so a
/// button's callback only has to write the signal it was given.
pub(crate) struct MenuSignals {
    /// What the account control was asked to do (see [`MenuRequest`]).
    pub request: Signal<Option<MenuRequest>>,
    /// The complete address the last copy request put on the clipboard (what a shell, and a test,
    /// reads as what was last offered).
    pub copied: Signal<Option<SmolStr>>,
    /// The tier's label: the plan's name, or the stand-in the menu says when it is unknown.
    pub plan: Signal<SmolStr>,
}

/// What the content was built from, so the control can tell when it has to be rebuilt.
///
/// The address, the tier and the account's own identity are followed by signals; what is left is
/// the language (every static sentence) and which face is on screen (an engine image node cannot
/// be re-pointed at another resource).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MenuShape {
    /// The language the static copy was built in.
    pub language: LanguageCode,
    /// The generation of the face the block was built with (`None` = no generated face yet).
    pub face: Option<u64>,
}

/// The account menu's content, as built into one overlay panel.
pub(crate) struct MenuContent {
    /// The content root (removed before the content is rebuilt or the control released).
    pub root: NodeId,
    /// The entry that opens the dashboard.
    pub open_account: Button,
    /// The entry that signs the account out.
    pub sign_out: Button,
    /// The control that asks for the complete address to be copied.
    pub copy: Button,
    /// The hint that shows the complete address when the shortened one is hovered.
    pub address_hint: Tooltip,
    /// The values the content was built from.
    pub shape: MenuShape,
}

impl MenuContent {
    /// Builds the menu's content into `panel`, the overlay's own panel node.
    ///
    /// Every kit key is derived from the control's per-instance prefix (`key`), so two controls
    /// in one application register distinct action identities and release only their own handlers
    /// (`crates/widgets/AGENTS.md` §1-11).
    ///
    /// # Errors
    /// `UnknownWindow` / `StaleNode` / icon or brand resource errors from the components.
    pub(crate) fn build(
        app: &mut App,
        host: &ProfileHost,
        window: WindowId,
        panel: NodeId,
        key: &str,
        account: &SignedInAccount,
        avatar: Option<&FaceTexture>,
        signals: &MenuSignals,
    ) -> Result<Self, EngineError> {
        let build = ContentBuild {
            window,
            key,
            account,
            avatar,
            theme: host.theme().clone(),
            translate: host.translate().clone(),
            language: host.language(app),
            layer: host.layer().clone(),
            animation: host.animation(),
            signals,
        };
        let root = build.root(app, panel)?;
        let (_, copy) = build.account_block(app, root)?;
        build.separator(app, root)?;
        let open_account = build.entry(
            app,
            root,
            "open-account",
            t_smol!(&build.translate, "window.account.open_account"),
            MenuRequest::OpenAccount,
        )?;
        let sign_out = build.entry(
            app,
            root,
            "sign-out",
            t_smol!(&build.translate, "window.account.sign_out"),
            MenuRequest::SignOut,
        )?;
        let address = build
            .address_node(app, root)
            .ok_or(EngineError::StaleNode { node: root })?;
        let address_hint = Tooltip::new(
            app,
            build.theme.clone(),
            build.layer.clone(),
            build.animation.clone(),
            format_smolstr!("{key}:address-hint"),
            TooltipOptions {
                // The hint sits under the line it explains, like the control's own hint sits
                // under the face.
                placement: Placement::BOTTOM,
                ..TooltipOptions::default()
            },
        );
        address_hint.attach(app, window, address);
        address_hint.set_content(Some(TooltipContent::new(account.wallet_address().clone())));

        Ok(Self {
            root,
            open_account,
            sign_out,
            copy,
            address_hint,
            shape: MenuShape {
                language: build.language,
                face: avatar.map(|face| face.generation),
            },
        })
    }

    /// Syncs the content's own components (hover states, the address hint's delay).
    ///
    /// The overlay panel itself is synced by the control; this is what the content holds on its
    /// own.
    ///
    /// # Errors
    /// The buttons' interactive state errors (`StaleNode` after the panel went away).
    pub(crate) fn sync(&self, app: &mut App) -> Result<(), EngineError> {
        self.open_account.sync(app)?;
        self.sign_out.sync(app)?;
        self.copy.sync(app)?;
        self.address_hint.sync(app);

        Ok(())
    }

    /// Releases the kit components of the content (its nodes are removed by the caller first).
    ///
    /// The order is the framework's (E16): the content root is removed before this runs, so the
    /// components only have to give their action handlers and signal scopes back.
    pub(crate) fn release(self, app: &mut App) {
        self.address_hint.release(app);
        self.copy.release(app);
        self.open_account.release(app);
        self.sign_out.release(app);
    }
}

/// The values one content build works from.
///
/// The block is a dozen controls over four layers (root → block → face / tier / address row →
/// control), so the build is a set of small methods that share the values they all read instead
/// of threading eight parameters through each of them.
struct ContentBuild<'a> {
    window: WindowId,
    key: &'a str,
    account: &'a SignedInAccount,
    avatar: Option<&'a FaceTexture>,
    theme: ThemeHandle,
    translate: Translate,
    language: LanguageCode,
    layer: Layer,
    animation: AnimationRouter,
    signals: &'a MenuSignals,
}

impl ContentBuild<'_> {
    /// The content root, at the one width the menu was decided to be.
    fn root(&self, app: &mut App, panel: NodeId) -> Result<NodeId, EngineError> {
        NodeBuilder::boxed()
            .id(MENU_ROOT_ID)
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .width(MENU_CONTENT_WIDTH)
                    .min_width_0()
                    .gap(2.0)
                    .build(),
            )
            .build(tree_mut(app, self.window)?, panel)
    }

    /// The account block: face, tier and address, stacked and centred.
    ///
    /// Returns the block and the copy control it holds, so the control has the one component it
    /// must release (the block is otherwise plain nodes).
    fn account_block(
        &self,
        app: &mut App,
        parent: NodeId,
    ) -> Result<(NodeId, Button), EngineError> {
        let block = NodeBuilder::boxed()
            .id(ACCOUNT_BLOCK_ID)
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .width_full()
                    .min_width_0()
                    .items_center()
                    .gap(ACCOUNT_BLOCK_GAP)
                    .padding(ACCOUNT_BLOCK_PADDING)
                    .build(),
            )
            .build(tree_mut(app, self.window)?, parent)?;
        let face_box = self.face(app, block)?;
        self.wallet_badge(app, face_box)?;
        self.plan(app, block)?;
        let copy = self.address_row(app, block)?;

        Ok((block, copy))
    }

    /// The face the block is drawn with, inside the box the wallet mark hangs off.
    fn face(&self, app: &mut App, parent: NodeId) -> Result<NodeId, EngineError> {
        let surface = self.theme.clone();
        let face_box = NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .relative()
                    .size(ACCOUNT_AVATAR_SIZE, ACCOUNT_AVATAR_SIZE)
                    .build(),
            )
            .build(tree_mut(app, self.window)?, parent)?;
        let face = NodeBuilder::boxed()
            .id(ACCOUNT_FACE_ID)
            .style(
                StyleBuilder::new()
                    .size(ACCOUNT_AVATAR_SIZE, ACCOUNT_AVATAR_SIZE)
                    .items_center()
                    .justify_center()
                    .build(),
            )
            .bind_style(move |cx| StylePatch {
                background: Some(surface.role(cx, Role::SurfaceRaised)),
                ..StylePatch::default()
            })
            .build(tree_mut(app, self.window)?, face_box)?;
        match self.avatar {
            Some(texture) => {
                NodeBuilder::image_pending(texture.pending, ImageFit::Cover)
                    .style(
                        StyleBuilder::new()
                            .size(ACCOUNT_AVATAR_SIZE, ACCOUNT_AVATAR_SIZE)
                            .build(),
                    )
                    .build(tree_mut(app, self.window)?, face)?;
            }
            None => {
                IconSpec::new(IconKind::User)
                    .with_size(UnifiedSize::Large.icon_size())
                    .build_themed(app, self.window, face, &self.theme, Role::FgMuted)
                    .map_err(|error| icon_error(self.window, error))?;
            }
        }

        Ok(face_box)
    }

    /// The wallet family's mark, as a badge on the lower-right corner of the face.
    ///
    /// The plate under the mark is the colour the menu resolves to, so the badge reads as a hole
    /// cut in the face rather than a sticker laid on it: the mark keeps its edges whatever the
    /// generated avatar puts behind them.
    fn wallet_badge(&self, app: &mut App, face_box: NodeId) -> Result<NodeId, EngineError> {
        // The badge is centred on the face's own lower-right edge rather than on the corner of
        // the box around it: the face is round, and a badge on the box's corner would float
        // beside the cheek. Half of it therefore falls outside that box.
        let offset = ACCOUNT_AVATAR_SIZE * 0.5 * (1.0 - std::f32::consts::FRAC_1_SQRT_2)
            - ACCOUNT_BADGE_SIZE * 0.5;
        let plate = self.theme.clone();
        let badge = NodeBuilder::boxed()
            .id(WALLET_BADGE_ID)
            .style(
                StyleBuilder::new()
                    .absolute()
                    .right(offset)
                    .bottom(offset)
                    .size(ACCOUNT_BADGE_SIZE, ACCOUNT_BADGE_SIZE)
                    .items_center()
                    .justify_center()
                    .build(),
            )
            .bind_style(move |cx| StylePatch {
                background: Some(plate.role(cx, Role::SurfaceRaised)),
                ..StylePatch::default()
            })
            .build(tree_mut(app, self.window)?, face_box)?;
        let brand = match self.account.wallet_kind() {
            AccountWalletKind::Ethereum => BrandKind::Ethereum,
            AccountWalletKind::Solana => BrandKind::Solana,
        };
        Brand::new(brand)
            .with_size(ACCOUNT_BADGE_LOGO_SIZE)
            .with_alt(wallet_kind_label(self.account.wallet_kind()))
            .build(app, self.window, badge)
            .map_err(|error| brand_error(self.window, error))?;

        Ok(badge)
    }

    /// The tier's badge.
    ///
    /// The label is **bound**: a tier the control reads after the menu opened moves the signal the
    /// badge reads, so the badge updates its text without the block being rebuilt. The initial
    /// value comes from the signal, so a menu opened while the query is in flight already says
    /// what it currently knows.
    fn plan(&self, app: &mut App, parent: NodeId) -> Result<NodeId, EngineError> {
        let wrapper = NodeBuilder::boxed()
            .id(PLAN_ID)
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .build(),
            )
            .build(tree_mut(app, self.window)?, parent)?;
        Badge::new(self.theme.clone())
            .solid()
            .small()
            .bind_label(self.signals.plan)
            .build(tree_mut(app, self.window)?, wrapper)?;

        Ok(wrapper)
    }

    /// The address row: the shortened address, its hint and the copy control at its right end.
    fn address_row(&self, app: &mut App, parent: NodeId) -> Result<Button, EngineError> {
        let text_color = self.theme.clone();
        let row = NodeBuilder::boxed()
            .id(ADDRESS_ROW_ID)
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .width_full()
                    .min_width_0()
                    .items_center()
                    .gap(4.0)
                    .build(),
            )
            .build(tree_mut(app, self.window)?, parent)?;
        // Both sides are given the same room rather than the address being padded by whatever
        // sits beside it, so the address holds the centre of the menu.
        NodeBuilder::boxed()
            .style(StyleBuilder::new().flex_1().build())
            .build(tree_mut(app, self.window)?, row)?;
        let mut address_style = TextStyle::new(UnifiedSize::Small.text_size());
        address_style.weight = 700;
        address_style.wrap = TextWrap::NoWrap;
        NodeBuilder::text(self.account.shortened_address().clone(), address_style)
            .id(ADDRESS_ID)
            .style(
                StyleBuilder::new()
                    .min_width_0()
                    .max_width(MENU_CONTENT_WIDTH * 0.5)
                    .build(),
            )
            .bind_style(move |cx| StylePatch {
                foreground: Some(text_color.role(cx, Role::FgDefault)),
                ..StylePatch::default()
            })
            .build(tree_mut(app, self.window)?, row)?;
        let right = NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .flex_1()
                    .flex_row()
                    .items_center()
                    .build(),
            )
            .build(tree_mut(app, self.window)?, row)?;
        self.copy_button(app, right)
    }

    /// The control that asks for the complete address to be copied.
    ///
    /// A click writes a request; the control takes it in its own `sync`, where it has the
    /// application in hand, and puts the address on the clipboard there. The value the request
    /// writes is also what a shell (and a test) reads as what was last offered.
    fn copy_button(&self, app: &mut App, parent: NodeId) -> Result<Button, EngineError> {
        let copy_label = t_smol!(&self.translate, "window.account.copy_wallet_address");
        let requests = self.signals.request;
        let button = Button::new(
            app,
            self.theme.clone(),
            format_smolstr!("{key}:copy", key = self.key),
        )
        .ghost()
        .small()
        .icon(IconKind::CopySimpleBold)
        .a11y_label(copy_label.clone())
        .tooltip_text(copy_label)
        .on_click(move |ctx| {
            ctx.write(&requests, Some(MenuRequest::CopyAddress));
            let window = ctx.window();
            let _ = ctx.app_mut().request_repaint(window);
        });
        button.build(app, self.window, parent)?;

        Ok(button)
    }

    /// One menu entry: a full-width ghost button that requests `request`.
    fn entry(
        &self,
        app: &mut App,
        parent: NodeId,
        suffix: &str,
        label: SmolStr,
        request: MenuRequest,
    ) -> Result<Button, EngineError> {
        let requests = self.signals.request;
        let button = Button::new(
            app,
            self.theme.clone(),
            format_smolstr!("{key}:{suffix}", key = self.key),
        )
        .ghost()
        .small()
        .label(label)
        .on_click(move |ctx| {
            ctx.write(&requests, Some(request));
            let window = ctx.window();
            let _ = ctx.app_mut().request_repaint(window);
        });
        button.build(app, self.window, parent)?;

        Ok(button)
    }

    /// The line between the account block and the entries.
    fn separator(&self, app: &mut App, parent: NodeId) -> Result<NodeId, EngineError> {
        let line = self.theme.clone();
        NodeBuilder::boxed()
            .style(StyleBuilder::new().width_full().height(1.0).build())
            .bind_style(move |cx| StylePatch {
                background: Some(line.role(cx, Role::LineDivider)),
                ..StylePatch::default()
            })
            .build(tree_mut(app, self.window)?, parent)
    }

    /// The shortened address node, which the complete-address hint hangs off.
    fn address_node(&self, app: &App, root: NodeId) -> Option<NodeId> {
        let tree = app.window(self.window)?.tree();
        find_debug(tree, root, ADDRESS_ID)
    }
}

/// The wallet family as the badge's alt text names it.
const fn wallet_kind_label(wallet_kind: AccountWalletKind) -> &'static str {
    match wallet_kind {
        AccountWalletKind::Ethereum => "Ethereum",
        AccountWalletKind::Solana => "Solana",
    }
}

/// The account's tier as the menu says it.
///
/// A tier the account service did not answer (or answered for an account that is gone) is said
/// with the stand-in sentence rather than left blank: a badge with no text reads as a badge with
/// no tier, which is not what "unknown" means.
pub(crate) fn plan_label(plan: Option<PlanCode>, translate: &Translate) -> SmolStr {
    match plan {
        Some(plan) => SmolStr::new_static(plan.display_name()),
        None => t_smol!(translate, "window.account.plan_unavailable"),
    }
}

/// The first node in `node`'s subtree whose debug id is `id`.
fn find_debug(tree: &NodeTree, node: NodeId, id: &str) -> Option<NodeId> {
    if tree.debug_id(node) == Some(id) {
        return Some(node);
    }
    for child in tree.get(node).ok()?.children() {
        if let Some(found) = find_debug(tree, *child, id) {
            return Some(found);
        }
    }
    None
}

fn tree_mut(app: &mut App, window: WindowId) -> Result<&mut NodeTree, EngineError> {
    Ok(app
        .window_mut(window)
        .ok_or(EngineError::UnknownWindow { window })?
        .tree_mut())
}

/// An icon's engine failure, as the engine's own error.
fn icon_error(window: WindowId, error: base_ui::kit::icon::IconError) -> EngineError {
    match error {
        base_ui::kit::icon::IconError::Node(error) => error,
        _ => EngineError::UnknownWindow { window },
    }
}

/// A brand logo's engine failure, as the engine's own error.
fn brand_error(window: WindowId, error: base_ui::kit::brand::BrandError) -> EngineError {
    match error {
        base_ui::kit::brand::BrandError::Node(error) => error,
        _ => EngineError::UnknownWindow { window },
    }
}

#[cfg(test)]
mod tests {
    use i18n::{language::codes::LanguageCode, t_smol};
    use kytrix_service_provider::auth::model::PlanCode;

    use super::plan_label;
    use crate::test_support::translate;

    /// The tier the menu says is the plan's own name, in every family.
    #[test]
    fn a_known_tier_should_be_named_by_the_plan() {
        let translate = translate(LanguageCode::En);
        for plan in PlanCode::ALL {
            assert_eq!(
                plan_label(Some(plan), &translate),
                plan.display_name(),
                "{plan:?} is named by the plan"
            );
        }
    }

    /// A tier nobody answered is said with the stand-in sentence, not left blank: a badge with no
    /// text reads as a badge with no tier, which is not what "unknown" means.
    #[test]
    fn an_unknown_tier_should_be_said_with_the_stand_in_sentence() {
        for language in [LanguageCode::En, LanguageCode::ZhCn] {
            let translate = translate(language);
            let label = plan_label(None, &translate);

            assert_eq!(label, t_smol!(translate, "window.account.plan_unavailable"));
            assert!(!label.is_empty(), "the stand-in sentence has copy");
        }
    }

    /// The menu is one fixed width, and its content leaves the panel's own padding out of it.
    #[test]
    fn the_menu_is_the_one_width_the_account_block_is_centred_on() {
        assert_eq!(super::MENU_WIDTH, 280.0);
        assert_eq!(
            super::MENU_CONTENT_WIDTH + 2.0 * super::PANEL_PADDING,
            super::MENU_WIDTH
        );
    }
}
