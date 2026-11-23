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

pub(crate) const MENU_WIDTH: f32 = 280.0;

const PANEL_PADDING: f32 = 4.0;

const MENU_CONTENT_WIDTH: f32 = MENU_WIDTH - 2.0 * PANEL_PADDING;

pub(crate) const ACCOUNT_AVATAR_SIZE: f32 = 56.0;

const ACCOUNT_BADGE_SIZE: f32 = 20.0;

const ACCOUNT_BADGE_LOGO_SIZE: f32 = 14.0;

const ACCOUNT_BLOCK_GAP: f32 = 6.0;

const ACCOUNT_BLOCK_PADDING: f32 = 8.0;

const ACCOUNT_BLOCK_ID: &str = "TEST_PROFILE_ACCOUNT";

const ACCOUNT_FACE_ID: &str = "TEST_PROFILE_ACCOUNT_AVATAR";

const WALLET_BADGE_ID: &str = "TEST_PROFILE_BRAND";

const PLAN_ID: &str = "TEST_PROFILE_PLAN";

const ADDRESS_ROW_ID: &str = "TEST_PROFILE_ACCOUNT_ADDRESS_ROW";

const ADDRESS_ID: &str = "TEST_PROFILE_ADDRESS";

const MENU_ROOT_ID: &str = "TEST_PROFILE_MENU";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MenuRequest {
    Trigger,
    CopyAddress,
    OpenAccount,
    SignOut,
    CancelSignIn,
    ConfirmSignOut,
}

pub(crate) struct MenuSignals {
    pub request: Signal<Option<MenuRequest>>,
    pub copied: Signal<Option<SmolStr>>,
    pub plan: Signal<SmolStr>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MenuShape {
    pub language: LanguageCode,
    pub face: Option<u64>,
}

pub(crate) struct MenuContent {
    pub root: NodeId,
    pub open_account: Button,
    pub sign_out: Button,
    pub copy: Button,
    pub address_hint: Tooltip,
    pub shape: MenuShape,
}

impl MenuContent {
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

    pub(crate) fn sync(&self, app: &mut App) -> Result<(), EngineError> {
        self.open_account.sync(app)?;
        self.sign_out.sync(app)?;
        self.copy.sync(app)?;
        self.address_hint.sync(app);

        Ok(())
    }

    pub(crate) fn release(self, app: &mut App) {
        self.address_hint.release(app);
        self.copy.release(app);
        self.open_account.release(app);
        self.sign_out.release(app);
    }
}

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

    fn wallet_badge(&self, app: &mut App, face_box: NodeId) -> Result<NodeId, EngineError> {
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

    fn address_node(&self, app: &App, root: NodeId) -> Option<NodeId> {
        let tree = app.window(self.window)?.tree();
        find_debug(tree, root, ADDRESS_ID)
    }
}

const fn wallet_kind_label(wallet_kind: AccountWalletKind) -> &'static str {
    match wallet_kind {
        AccountWalletKind::Ethereum => "Ethereum",
        AccountWalletKind::Solana => "Solana",
    }
}

pub(crate) fn plan_label(plan: Option<PlanCode>, translate: &Translate) -> SmolStr {
    match plan {
        Some(plan) => SmolStr::new_static(plan.display_name()),
        None => t_smol!(translate, "window.account.plan_unavailable"),
    }
}

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

fn icon_error(window: WindowId, error: base_ui::kit::icon::IconError) -> EngineError {
    match error {
        base_ui::kit::icon::IconError::Node(error) => error,
        _ => EngineError::UnknownWindow { window },
    }
}

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

    #[test]
    fn an_unknown_tier_should_be_said_with_the_stand_in_sentence() {
        for language in [LanguageCode::En, LanguageCode::ZhCn] {
            let translate = translate(language);
            let label = plan_label(None, &translate);

            assert_eq!(label, t_smol!(translate, "window.account.plan_unavailable"));
            assert!(!label.is_empty(), "the stand-in sentence has copy");
        }
    }

    #[test]
    fn the_menu_is_the_one_width_the_account_block_is_centred_on() {
        assert_eq!(super::MENU_WIDTH, 280.0);
        assert_eq!(
            super::MENU_CONTENT_WIDTH + 2.0 * super::PANEL_PADDING,
            super::MENU_WIDTH
        );
    }
}
