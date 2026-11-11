//! The configure page: which proxy each network target is reached through.

use app_config::{
    network::{KytrixServiceCapability, ServiceTarget},
    settings::Config,
};
use gpui_kit::{AnyElement, App, IntoElement, SharedString};
use i18n::t;
use market_participants_connector::venue::definition::supported_venues;
use market_participants_model::identifier::VenueId;
use smol_str::SmolStr;
use widget::venue::VenueDisplay;

use crate::{
    layout::{self, PageArgs},
    pages::proxy::route::{
        PROXY_DIRECT, PROXY_INHERIT, PROXY_MIXED, PROXY_SYSTEM, proxy_default_of,
        proxy_route_of_group, proxy_route_of_venue, set_proxy_default, set_proxy_route_of_group,
        set_proxy_route_of_venue,
    },
    search::{FieldSearch, PageSearch, words},
};

/// The id of this page.
const PAGE: &str = "configure";

/// The Kytrix services the configure page reaches through one line.
const KYTRIX_CLOUD: &[ServiceTarget] = &[
    ServiceTarget::KytrixService {
        capability: KytrixServiceCapability::Account,
    },
    ServiceTarget::KytrixService {
        capability: KytrixServiceCapability::EthereumWallet,
    },
    ServiceTarget::KytrixService {
        capability: KytrixServiceCapability::SolanaWallet,
    },
    ServiceTarget::KytrixService {
        capability: KytrixServiceCapability::CryptocurrencyEtf,
    },
    ServiceTarget::KytrixService {
        capability: KytrixServiceCapability::News,
    },
    ServiceTarget::KytrixService {
        capability: KytrixServiceCapability::FinancialIndex,
    },
    ServiceTarget::KytrixService {
        capability: KytrixServiceCapability::X,
    },
    ServiceTarget::KytrixService {
        capability: KytrixServiceCapability::CoinMarketCapFearAndGreedIndex,
    },
    ServiceTarget::KytrixService {
        capability: KytrixServiceCapability::PentagonPizzaIndex,
    },
    ServiceTarget::KytrixService {
        capability: KytrixServiceCapability::FinancialCalendar,
    },
    ServiceTarget::KytrixService {
        capability: KytrixServiceCapability::GlobalShippingChokepoint,
    },
];

/// The application's own updates, which are fetched through their own line.
const APP_UPDATES: &[ServiceTarget] = &[
    ServiceTarget::KytrixService {
        capability: KytrixServiceCapability::UpdaterMetadata,
    },
    ServiceTarget::KytrixService {
        capability: KytrixServiceCapability::UpdaterDownload,
    },
];

/// The translation model downloads, which have a line of their own.
const AI_MODELS: &[ServiceTarget] = &[ServiceTarget::TranslationModelDownload];

/// The media service, which the Kytrix group covers beside its own services.
const X_MEDIA: &[ServiceTarget] = &[ServiceTarget::KytrixService {
    capability: KytrixServiceCapability::XMedia,
}];

/// The explanation one venue's row carries.
const fn venue_description_key(venue: VenueId) -> &'static str {
    match venue {
        VenueId::Binance => "settings_ui.proxies.binance_description",
        VenueId::Hyperliquid => "settings_ui.proxies.hyperliquid_description",
        VenueId::Longbridge => "settings_ui.proxies.longbridge_description",
        VenueId::Okx => "settings_ui.proxies.okx_description",
        VenueId::Polymarket => "settings_ui.proxies.polymarket_description",
    }
}

pub(crate) fn render(args: &mut PageArgs<'_, '_>) -> AnyElement {
    let mut groups = Vec::new();
    let mut default_rows = Vec::new();
    let mut venue_rows = Vec::new();
    let mut kytrix_rows = Vec::new();

    if args.filter.admits_field(PAGE, "proxy.default-line") {
        let label = t!(args.cx, "settings_ui.proxies.default_proxy");
        let selected = proxy_default_of(args.config);

        default_rows.push(proxy_line_row(
            args,
            "proxy.default-line",
            label,
            SharedString::default(),
            selected,
            true,
            set_proxy_default,
        ));
    }

    for venue in supported_venues() {
        let field_id = format!("{PAGE}.{}", venue.as_str());
        if !args.filter.admits_field(PAGE, &field_id) {
            continue;
        }

        let label = t!(args.cx, venue.name_key());
        let description = t!(args.cx, venue_description_key(venue));
        let selected = proxy_route_of_venue(args.config, &venue);

        venue_rows.push(proxy_line_row(
            args,
            &field_id,
            label,
            description,
            selected,
            false,
            move |config, value| set_proxy_route_of_venue(config, &venue, value),
        ));
    }

    for (id, label_key, description_key, targets) in [
        (
            "configure.kytrix.cloud",
            "settings_ui.proxies.kytrix_cloud",
            "settings_ui.proxies.kytrix_cloud_description",
            KYTRIX_CLOUD,
        ),
        (
            "configure.kytrix.updates",
            "settings_ui.proxies.app_updates",
            "settings_ui.proxies.app_updates_description",
            APP_UPDATES,
        ),
        (
            "configure.ai.models",
            "settings_ui.proxies.ai_model_downloads",
            "settings_ui.proxies.ai_model_downloads_description",
            AI_MODELS,
        ),
        (
            "configure.x.media",
            "settings_ui.proxies.x_post_media",
            "settings_ui.proxies.x_post_media_description",
            X_MEDIA,
        ),
    ] {
        if !args.filter.admits_field(PAGE, id) {
            continue;
        }

        let label = t!(args.cx, label_key);
        let description = t!(args.cx, description_key);
        let selected = proxy_route_of_group(args.config, targets.iter().cloned());

        kytrix_rows.push(proxy_line_row(
            args,
            id,
            label,
            description,
            selected,
            false,
            move |config, value| set_proxy_route_of_group(config, targets.iter().cloned(), value),
        ));
    }

    if !default_rows.is_empty() {
        groups.push(layout::section(None, "configure_0", default_rows, args.cx));
    }
    if !venue_rows.is_empty() {
        groups.push(layout::section(
            Some(t!(
                args.cx,
                "settings_ui.proxies.market_participants_section"
            )),
            "configure_1",
            venue_rows,
            args.cx,
        ));
    }
    if !kytrix_rows.is_empty() {
        groups.push(layout::section(
            Some(t!(args.cx, "settings_ui.proxies.kytrix_section")),
            "configure_2",
            kytrix_rows,
            args.cx,
        ));
    }

    layout::page(groups)
}

/// One proxy-line row: the control offers the entries of the catalog beside the built-in lines.
fn proxy_line_row(
    args: &mut PageArgs<'_, '_>,
    field_id: &str,
    label: SharedString,
    description: SharedString,
    selected: SmolStr,
    default_line: bool,
    apply: impl Fn(&mut Config, &str) + 'static,
) -> AnyElement {
    let choices = proxy_line_choices(args.config, selected.as_str(), default_line, args.cx);
    let select = layout::choice_select(
        layout::element_id("settings-proxy-line", field_id),
        Some(SharedString::from(selected.as_str())),
        choices,
        label.clone(),
        None,
        false,
        layout::on_select(args.handle, apply),
    );

    layout::row(
        field_id,
        label,
        description,
        select.into_any_element(),
        layout::CONTROL_WIDTH,
        args.cx,
    )
}

/// The choices one proxy-line control offers.
///
/// The entries of the catalog are read while the control is drawn, because the catalog is a
/// runtime value. The line the row holds is always one of the choices: a group whose targets
/// disagree, and an entry the catalog no longer carries, are the two lines a row can hold that no
/// other choice names, and both are offered as themselves — a control that showed anything else
/// would report a binding the settings do not have.
fn proxy_line_choices(
    config: &Config,
    selected: &str,
    default_line: bool,
    cx: &App,
) -> Vec<(SharedString, SharedString)> {
    let mut choices = Vec::with_capacity(config.proxies.len() + 4);

    if !default_line {
        choices.push((
            SharedString::new_static(PROXY_INHERIT),
            t!(cx, "settings_ui.proxies.inherit"),
        ));
    }
    choices.push((
        SharedString::new_static(PROXY_DIRECT),
        t!(cx, "settings_ui.proxies.no_proxy"),
    ));
    choices.push((
        SharedString::new_static(PROXY_SYSTEM),
        t!(cx, "settings_ui.proxies.system_proxy"),
    ));

    if selected == PROXY_MIXED {
        choices.push((
            SharedString::new_static(PROXY_MIXED),
            t!(cx, "settings_ui.proxies.mixed"),
        ));
    } else {
        let held_by_the_catalog = config.proxies.contains_key(selected);
        let built_in = selected == PROXY_DIRECT
            || selected == PROXY_SYSTEM
            || (!default_line && selected == PROXY_INHERIT);
        if !held_by_the_catalog && !built_in {
            choices.push((SharedString::from(selected), SharedString::from(selected)));
        }
    }

    for name in config.proxies.keys() {
        choices.push((
            SharedString::from(name.as_str()),
            SharedString::from(name.as_str()),
        ));
    }

    choices
}

pub(crate) fn search(cx: &App) -> PageSearch {
    let title = t!(cx, "settings_ui.proxies.configure");
    let default_label = t!(cx, "settings_ui.proxies.default_proxy");
    let kytrix_cloud = t!(cx, "settings_ui.proxies.kytrix_cloud");
    let kytrix_cloud_description = t!(cx, "settings_ui.proxies.kytrix_cloud_description");
    let app_updates = t!(cx, "settings_ui.proxies.app_updates");
    let app_updates_description = t!(cx, "settings_ui.proxies.app_updates_description");
    let ai_models = t!(cx, "settings_ui.proxies.ai_model_downloads");
    let ai_models_description = t!(cx, "settings_ui.proxies.ai_model_downloads_description");
    let x_media = t!(cx, "settings_ui.proxies.x_post_media");
    let x_media_description = t!(cx, "settings_ui.proxies.x_post_media_description");

    let mut fields = Vec::with_capacity(supported_venues().len() + 5);
    fields.push(FieldSearch {
        id: "proxy.default-line".into(),
        text: words(&[
            default_label.as_ref(),
            "default",
            "proxy",
            "system",
            "直连",
            "默认代理",
            "系统代理",
        ]),
    });
    for venue in supported_venues() {
        let label = t!(cx, venue.name_key());
        let description = t!(cx, venue_description_key(venue));
        let id = format!("{PAGE}.{}", venue.as_str());
        fields.push(FieldSearch {
            id: id.into(),
            text: words(&[
                label.as_ref(),
                description.as_ref(),
                venue.as_str(),
                "proxy",
                "venue",
                "market participant",
                "default",
                "inherit",
                "市场参与者",
                "代理",
                "默认代理",
            ]),
        });
    }
    fields.push(FieldSearch {
        id: "configure.kytrix.cloud".into(),
        text: words(&[
            kytrix_cloud.as_ref(),
            kytrix_cloud_description.as_ref(),
            "kytrix",
            "account",
            "cloud",
            "wallet",
            "news",
            "pizza",
            "calendar",
            "云服务",
            "账户",
        ]),
    });
    fields.push(FieldSearch {
        id: "configure.kytrix.updates".into(),
        text: words(&[
            app_updates.as_ref(),
            app_updates_description.as_ref(),
            "updater",
            "update",
            "更新",
        ]),
    });
    fields.push(FieldSearch {
        id: "configure.ai.models".into(),
        text: words(&[
            ai_models.as_ref(),
            ai_models_description.as_ref(),
            "ai",
            "model",
            "模型",
            "huggingface",
            "modelscope",
            "魔搭",
            "下载",
        ]),
    });
    fields.push(FieldSearch {
        id: "configure.x.media".into(),
        text: words(&[
            x_media.as_ref(),
            x_media_description.as_ref(),
            "x",
            "twitter",
            "post",
            "media",
            "帖子",
            "媒体",
        ]),
    });

    PageSearch {
        label: words(&[
            title.as_ref(),
            "proxy",
            "configure",
            "configuration",
            "routing",
            "behavior",
            "代理",
            "配置",
            "路由",
            "网络行为",
        ]),
        fields,
    }
}
