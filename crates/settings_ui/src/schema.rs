//! Every page and field the settings offer, written out.
//!
//! The static pages are declarations. The two proxy pages and the venue pages are built from
//! the build's static venue list and from the stored configuration — see
//! [`crate::entry::show_settings`]: a venue this build speaks has its line, its origins and its
//! credentials whether or not anything is loaded, because configuring them is what makes loading
//! a venue possible at all. The scope rows of one venue's page are the scopes its own stored
//! configuration names, and the origins of each are read and written there.
//!
//! A built page carries the identity its controls stand for: one proxy row is one venue, one
//! credential field is one field of the shape that venue's stored credentials hold, and one
//! origin row is one scope of one venue. Nothing is resolved from a field id while a control is
//! drawn, which is what keeps a setting from being written for a venue the page does not name.

use std::{rc::Rc, sync::Arc};

use app_config::{
    market_participants::{
        origins::{
            BinanceHttpOrigin, BinanceMarket, BinanceWebsocketOrigin, HyperliquidHttpOrigin,
            HyperliquidWebsocketOrigin, LongbridgeRegion, OkxRegion,
        },
        settings::{BinanceOrigins, VenueSettings},
    },
    network::{KytrixServiceCapability, ServiceTarget},
    settings::Config,
    time_zone::DataTimeZone,
};
use gpui_kit::SharedString;
use market_participants_model::{
    display::lexical::scope_display_code,
    identifier::{VenueId, VenueScopeId},
};
use smol_str::{SmolStr, format_smolstr};
use widget::venue::VenueDisplay;

use crate::{
    access::{
        FieldValue, TargetSet, proxy_default_of, proxy_route_of_group, proxy_route_of_venue,
        set_proxy_default, set_proxy_route_of_group, set_proxy_route_of_venue,
    },
    catalog::{
        FieldAccess, FieldDef, PageDef, PageTitle, SectionDef, SelectOptionDef, Text, TreeNodeDef,
        VenueView,
    },
};

const fn text(key: &'static str) -> Text {
    Text::new(key)
}

/// The tree and the pages of one venue list.
pub(crate) fn catalog(venues: &VenueView) -> (Vec<TreeNodeDef>, Vec<PageDef>) {
    let mut pages = vec![
        appearance_page(),
        time_zone_page(),
        management_page(),
        applications_page(venues),
    ];
    pages.extend(venue_pages(venues));
    pages.push(about_page());

    let mut tree = vec![
        TreeNodeDef::Group {
            id: "general",
            label: text("settings_ui.general.title"),
            keywords: &["general", "通用", "设置"],
            open: true,
            children: vec![
                TreeNodeDef::Page(SharedString::new_static("appearance")),
                TreeNodeDef::Page(SharedString::new_static("time-zone")),
            ],
        },
        TreeNodeDef::Group {
            id: "proxy",
            label: text("settings_ui.proxies.title"),
            keywords: &["proxy", "代理", "socks5", "网络"],
            open: true,
            children: vec![
                TreeNodeDef::Page(SharedString::new_static("proxy-management")),
                TreeNodeDef::Page(SharedString::new_static("proxy-applications")),
            ],
        },
        TreeNodeDef::Group {
            id: "market_participants",
            label: text("settings_ui.market_participants.title"),
            keywords: &[
                "market_participants",
                "venue",
                "市场参与者",
                "场所",
                "credentials",
                "凭据",
            ],
            open: true,
            children: pages
                .iter()
                .filter(|page| page.id.starts_with("market_participants."))
                .map(|page| TreeNodeDef::Page(page.id.clone()))
                .collect(),
        },
    ];
    tree.push(TreeNodeDef::Page(SharedString::new_static("about")));

    (tree, pages)
}

// --- published choices --------------------------------------------------------------------

/// A choice of one origin table, drawn as the address it stores.
///
/// A published choice is looked up by the option's id, which is the address itself, so
/// nothing here has to be kept in step with the enum it stands for: the id a row offers is
/// the value the settings store, and both come from the venue's own table.
macro_rules! origin_options {
    ($name:ident, $count:literal, [$($choice:expr),+ $(,)?]) => {
        /// The choices one origin row offers, in the order the venue publishes them.
        static $name: [SelectOptionDef; $count] = [
            $(SelectOptionDef {
                label: Text::Literal(SharedString::new_static($choice.as_str())),
                id: SmolStr::new_static($choice.as_str()),
            }),+
        ];
    };
}
origin_options!(
    BINANCE_SPOT_HTTP_OPTIONS,
    5,
    [
        BinanceHttpOrigin::Api,
        BinanceHttpOrigin::Api1,
        BinanceHttpOrigin::Api2,
        BinanceHttpOrigin::Api3,
        BinanceHttpOrigin::Api4,
    ]
);

origin_options!(
    BINANCE_USD_M_PERP_HTTP_OPTIONS,
    1,
    [BinanceHttpOrigin::FuturesApi]
);

origin_options!(
    BINANCE_SPOT_WEBSOCKET_OPTIONS,
    2,
    [
        BinanceWebsocketOrigin::Stream,
        BinanceWebsocketOrigin::StreamAlt,
    ]
);

origin_options!(
    BINANCE_USD_M_PERP_WEBSOCKET_OPTIONS,
    1,
    [BinanceWebsocketOrigin::FuturesStream]
);

origin_options!(HYPERLIQUID_HTTP_OPTIONS, 1, [HyperliquidHttpOrigin::Api]);

origin_options!(
    HYPERLIQUID_WEBSOCKET_OPTIONS,
    1,
    [HyperliquidWebsocketOrigin::Api]
);

/// The request addresses one Binance market may be reached on.
///
/// Each market offers its own: the venue's spot API publishes a primary host beside four
/// mirrors and its USDⓈ-M API one host, and a market reached on the other's host would be a
/// request to the wrong product.
const fn binance_http_options(market: BinanceMarket) -> &'static [SelectOptionDef] {
    match market {
        BinanceMarket::Spot => &BINANCE_SPOT_HTTP_OPTIONS,
        BinanceMarket::UsdMPerp => &BINANCE_USD_M_PERP_HTTP_OPTIONS,
    }
}

/// The stream addresses one Binance market may be reached on.
const fn binance_websocket_options(market: BinanceMarket) -> &'static [SelectOptionDef] {
    match market {
        BinanceMarket::Spot => &BINANCE_SPOT_WEBSOCKET_OPTIONS,
        BinanceMarket::UsdMPerp => &BINANCE_USD_M_PERP_WEBSOCKET_OPTIONS,
    }
}

/// The service regions an OKX account may belong to, drawn in the order the venue lists
/// them.
static OKX_REGION_OPTIONS: [SelectOptionDef; 3] = [
    SelectOptionDef::new(
        text("settings_ui.market_participants.service_region_global"),
        "global",
    ),
    SelectOptionDef::new(
        text("settings_ui.market_participants.service_region_us_au"),
        "us-au",
    ),
    SelectOptionDef::new(
        text("settings_ui.market_participants.service_region_eu"),
        "eu",
    ),
];

/// The service regions a Longbridge account may belong to, drawn in the order the venue lists
/// them.
static LONGBRIDGE_REGION_OPTIONS: [SelectOptionDef; 2] = [
    SelectOptionDef::new(
        text("settings_ui.market_participants.service_region_global"),
        "global",
    ),
    SelectOptionDef::new(
        text("settings_ui.market_participants.service_region_china"),
        "china",
    ),
];

// --- appearance -------------------------------------------------------------------------

static APPEARANCE_OPTIONS: [SelectOptionDef; 3] = [
    SelectOptionDef::new(text("settings_ui.appearance.light"), "Light"),
    SelectOptionDef::new(text("settings_ui.appearance.dark"), "Dark"),
    SelectOptionDef::new(text("settings_ui.appearance.system"), "System"),
];

static LANGUAGE_OPTIONS: [SelectOptionDef; 2] = [
    SelectOptionDef::new(text("settings_ui.appearance.language_zh_cn"), "zh-CN"),
    SelectOptionDef::new(text("settings_ui.appearance.language_en"), "en"),
];

/// The offsets the data time zone offers, west to east.
///
/// Each choice is drawn as the offset it stores, because that is what a UTC offset
/// is called in every language. The list is spelled out rather than derived so the
/// order the control draws is visible here; a test holds it to [`DataTimeZone::ALL`].
static DATA_TIME_ZONE_OPTIONS: [SelectOptionDef; DataTimeZone::COUNT] = [
    SelectOptionDef::literal("UTC-12", "-12"),
    SelectOptionDef::literal("UTC-11", "-11"),
    SelectOptionDef::literal("UTC-10", "-10"),
    SelectOptionDef::literal("UTC-9", "-9"),
    SelectOptionDef::literal("UTC-8", "-8"),
    SelectOptionDef::literal("UTC-7", "-7"),
    SelectOptionDef::literal("UTC-6", "-6"),
    SelectOptionDef::literal("UTC-5", "-5"),
    SelectOptionDef::literal("UTC-4", "-4"),
    SelectOptionDef::literal("UTC-3", "-3"),
    SelectOptionDef::literal("UTC-2", "-2"),
    SelectOptionDef::literal("UTC-1", "-1"),
    SelectOptionDef::literal("UTC+0", "0"),
    SelectOptionDef::literal("UTC+1", "1"),
    SelectOptionDef::literal("UTC+2", "2"),
    SelectOptionDef::literal("UTC+3", "3"),
    SelectOptionDef::literal("UTC+4", "4"),
    SelectOptionDef::literal("UTC+5", "5"),
    SelectOptionDef::literal("UTC+6", "6"),
    SelectOptionDef::literal("UTC+7", "7"),
    SelectOptionDef::literal("UTC+8", "8"),
    SelectOptionDef::literal("UTC+9", "9"),
    SelectOptionDef::literal("UTC+10", "10"),
    SelectOptionDef::literal("UTC+11", "11"),
    SelectOptionDef::literal("UTC+12", "12"),
    SelectOptionDef::literal("UTC+13", "13"),
    SelectOptionDef::literal("UTC+14", "14"),
];

fn time_zone_page() -> PageDef {
    PageDef {
        id: SharedString::new_static("time-zone"),
        title: PageTitle::Key(text("settings_ui.time_zone.title")),
        keywords: &[
            "time zone",
            "timezone",
            "utc",
            "offset",
            "时区",
            "时间",
            "数据时间",
        ],
        sections: vec![SectionDef::fields(
            None,
            false,
            vec![FieldDef {
                id: SharedString::new_static("time-zone.offset"),
                label: text("settings_ui.time_zone.title"),
                description: text("settings_ui.time_zone.description"),
                keywords: &["time zone", "timezone", "utc", "offset", "时区", "时间"],
                access: FieldAccess::Select {
                    options: &DATA_TIME_ZONE_OPTIONS,
                    get: Rc::new(time_zone_get),
                    set: Rc::new(time_zone_set),
                },
            }],
        )],
    }
}

fn time_zone_get(config: &Config) -> FieldValue {
    FieldValue::new(format_smolstr!("{}", config.data_time_zone.hours()).as_str())
}

fn time_zone_set(config: &mut Config, value: &FieldValue) {
    let Ok(hours) = value.as_str().parse::<i8>() else {
        return;
    };
    let Some(zone) = DataTimeZone::from_hours(hours) else {
        return;
    };
    config.data_time_zone = zone;
}

fn appearance_page() -> PageDef {
    PageDef {
        id: SharedString::new_static("appearance"),
        title: PageTitle::Key(text("settings_ui.appearance.title")),
        keywords: &["appearance", "style", "外观", "主题", "浅色", "深色"],
        sections: vec![SectionDef::fields(
            None,
            false,
            vec![
                FieldDef {
                    id: SharedString::new_static("appearance"),
                    label: text("settings_ui.appearance.mode"),
                    description: text("settings_ui.appearance.mode_description"),
                    keywords: &[
                        "appearance",
                        "style",
                        "浅色",
                        "深色",
                        "系统",
                        "主题",
                        "外观",
                    ],
                    access: FieldAccess::Select {
                        options: &APPEARANCE_OPTIONS,
                        get: Rc::new(appearance_get),
                        set: Rc::new(appearance_set),
                    },
                },
                FieldDef {
                    id: SharedString::new_static("language"),
                    label: text("settings_ui.appearance.language"),
                    description: text("settings_ui.appearance.language_description"),
                    keywords: &["language", "locale", "语言", "中文", "english", "英文"],
                    access: FieldAccess::Select {
                        options: &LANGUAGE_OPTIONS,
                        get: Rc::new(language_get),
                        set: Rc::new(language_set),
                    },
                },
            ],
        )],
    }
}

fn appearance_get(config: &Config) -> FieldValue {
    SmolStr::new_static(match config.appearance {
        app_config::settings::Appearance::Light => "Light",
        app_config::settings::Appearance::Dark => "Dark",
        app_config::settings::Appearance::System => "System",
    })
}

fn appearance_set(config: &mut Config, value: &FieldValue) {
    config.appearance = match value.as_str() {
        "Light" => app_config::settings::Appearance::Light,
        "Dark" => app_config::settings::Appearance::Dark,
        _ => app_config::settings::Appearance::System,
    };
}

fn language_get(config: &Config) -> FieldValue {
    SmolStr::new_static(config.language.locale())
}

fn language_set(config: &mut Config, value: &FieldValue) {
    config.language = match value.as_str() {
        "en" => app_config::settings::Language::En,
        _ => app_config::settings::Language::ZhCn,
    };
}

// --- about ------------------------------------------------------------------------------

fn about_page() -> PageDef {
    PageDef {
        id: SharedString::new_static("about"),
        title: PageTitle::Key(text("settings_ui.about.title")),
        keywords: &["about", "version", "关于", "版本"],
        sections: vec![SectionDef::fields(
            None,
            false,
            vec![FieldDef {
                id: SharedString::new_static("about.version"),
                label: text("settings_ui.about.version"),
                description: text("settings_ui.about.version_description"),
                keywords: &["about", "version", "关于", "版本"],
                access: FieldAccess::Info {
                    get: Rc::new(version_get),
                },
            }],
        )],
    }
}

fn version_get(_config: &Config) -> SharedString {
    SharedString::new_static(env!("CARGO_PKG_VERSION"))
}

// --- proxies ----------------------------------------------------------------------------

fn management_page() -> PageDef {
    PageDef {
        id: SharedString::new_static("proxy-management"),
        title: PageTitle::Key(text("settings_ui.proxies.management")),
        keywords: &["proxy", "management", "catalog", "socks5", "代理", "管理"],
        // The catalog is drawn by the proxy table, which is a view of its own rather than a
        // field: what the page declares here is that it holds nothing else.
        sections: Vec::new(),
    }
}

fn applications_page(venues: &VenueView) -> PageDef {
    PageDef {
        id: SharedString::new_static("proxy-applications"),
        title: PageTitle::Key(text("settings_ui.proxies.applications")),
        keywords: &[
            "proxy",
            "applications",
            "routing",
            "behavior",
            "代理",
            "应用",
            "路由",
            "网络行为",
        ],
        sections: application_sections(venues),
    }
}

/// The lines the applications page binds: the document-wide default, one row per venue this
/// build speaks, and the Kytrix services that are reached through a line of their own.
///
/// One row per venue rather than one per capability of one loaded scope: a venue is configured
/// as a whole, so what the page answers is "where do this venue's requests go", and the venue's
/// own origins are what its page adds to that answer.
fn application_sections(venues: &[VenueId]) -> Vec<SectionDef> {
    let mut sections = Vec::with_capacity(venues.len() + 3);
    sections.push(SectionDef::fields(
        Some(text("settings_ui.proxies.default_line")),
        false,
        vec![FieldDef {
            id: SharedString::new_static("proxy.default-line"),
            label: text("settings_ui.proxies.default_line"),
            description: text("settings_ui.proxies.default_line_description"),
            keywords: &[
                "default",
                "line",
                "proxy",
                "system",
                "直连",
                "默认线路",
                "系统代理",
            ],
            access: FieldAccess::ProxyLine {
                default_line: true,
                get: Rc::new(|config: &Config| proxy_default_of(config)),
                set: Rc::new(|config: &mut Config, value: &FieldValue| {
                    set_proxy_default(config, value)
                }),
            },
        }],
    ));

    for (index, venue) in venues.iter().enumerate() {
        sections.push(SectionDef::fields(
            Some(text("settings_ui.proxies.venue_section")),
            index != 0,
            vec![venue_proxy_field(venue)],
        ));
    }

    sections.push(SectionDef::fields(
        Some(text("settings_ui.proxies.kytrix_section")),
        true,
        vec![
            proxy_targets_field(
                "proxy-applications.kytrix.cloud",
                "settings_ui.proxies.kytrix_cloud",
                &["kytrix", "account", "wallet", "news", "x", "云服务"],
                KYTRIX_CLOUD,
            ),
            proxy_targets_field(
                "proxy-applications.kytrix.updates",
                "settings_ui.proxies.app_updates",
                &["updater", "update", "更新"],
                APP_UPDATES,
            ),
        ],
    ));
    sections.push(SectionDef::fields(
        Some(text("settings_ui.proxies.x_section")),
        true,
        vec![proxy_targets_field(
            "proxy-applications.x.media",
            "settings_ui.proxies.x_media_group",
            &["x", "twitter", "media", "媒体"],
            X_MEDIA,
        )],
    ));
    sections.push(SectionDef::fields(
        Some(text("settings_ui.proxies.public_feeds_section")),
        true,
        vec![proxy_targets_field(
            "proxy-applications.public-feeds.pizza-index",
            "settings_ui.proxies.pizza_index_group",
            &["pizza", "pizzint", "pentagon", "osint", "披萨", "五角大楼"],
            PUBLIC_FEEDS,
        )],
    ));

    sections
}

/// The line one venue's own requests are bound to.
///
/// The control offers the default line beside the lines a user can name: a venue that names none
/// follows the document's default, exactly as a group of Kytrix services without an override
/// does, and the venue's own settings row is where that is stored — nothing is named for a venue
/// that follows it.
fn venue_proxy_field(venue: &VenueId) -> FieldDef {
    let reader = *venue;
    let writer = *venue;

    FieldDef {
        id: SharedString::from(format!("proxy-applications.{}", venue.as_str())),
        label: venue_label(venue),
        description: text("settings_ui.proxies.venue_description"),
        keywords: field_keywords(vec![
            venue.as_str(),
            "proxy",
            "venue",
            "default",
            "inherit",
            "场所",
            "代理",
            "默认线路",
        ]),
        access: FieldAccess::ProxyLine {
            default_line: false,
            get: Rc::new(move |config: &Config| proxy_route_of_venue(config, &reader)),
            set: Rc::new(move |config: &mut Config, value: &FieldValue| {
                set_proxy_route_of_venue(config, &writer, value)
            }),
        },
    }
}

/// The proxy-line row of one group of Kytrix services.
fn proxy_targets_field(
    id: &'static str,
    label: &'static str,
    keywords: &'static [&'static str],
    targets: &'static [ServiceTarget],
) -> FieldDef {
    FieldDef {
        id: SharedString::new_static(id),
        label: text(label),
        description: text("settings_ui.proxies.group_description"),
        keywords,
        access: proxy_line_access(targets.to_vec(), false),
    }
}

/// The access of one proxy-line field over the Kytrix services it stands for.
///
/// The targets are captured rather than resolved from the field's id: which services one line
/// covers is a compile-time fact of this page, and a line written for a service nothing runs
/// would be a configuration entry the runtime cannot use.
fn proxy_line_access(
    targets: impl IntoIterator<Item = ServiceTarget>,
    default_line: bool,
) -> FieldAccess {
    let targets: TargetSet = targets.into_iter().collect();
    let reader: TargetSet = Arc::clone(&targets);
    let writer: TargetSet = Arc::clone(&targets);

    FieldAccess::ProxyLine {
        default_line,
        get: Rc::new(move |config: &Config| proxy_route_of_group(config, reader.iter().cloned())),
        set: Rc::new(move |config: &mut Config, value: &FieldValue| {
            set_proxy_route_of_group(config, writer.iter().cloned(), value)
        }),
    }
}

// --- venues -----------------------------------------------------------------------------

/// One page per venue this build serves.
///
/// The page is built from the venue's own identity rather than from what the settings
/// hold: a venue this build serves publishes the scopes it publishes, and which addresses
/// those scopes may be reached on is what the venue's documentation states. Nothing about
/// the page moves when a stored value does, which is what lets a user configure a venue
/// before anything of it is loaded.
fn venue_pages(venues: &VenueView) -> Vec<PageDef> {
    venues.iter().map(venue_page).collect()
}

/// The page of one venue.
///
/// What a page holds is what its venue's API has: Binance reaches two products at two
/// addresses and signs them with two key pairs, OKX belongs to one service region and
/// authenticates as a whole, HyperCore publishes one address of each kind, and Longbridge picks
/// one service site and authenticates with an app key, its secret and an access token. A venue
/// whose settings are of another shape is not one this build can draw, so the page is
/// chosen by the venue and the settings are read and written through the venue's own row.
///
/// The builder answers any venue identity, while the dialog draws the served list: a venue this
/// build serves nothing for would have a page here that nothing reaches, and the day its loader
/// lands is the day the same page is drawn. Every venue this build carries an identity for is
/// served, so the two halves agree — see [`crate::dialog::SettingsDialog::new`].
fn venue_page(venue: &VenueId) -> PageDef {
    PageDef {
        id: venue_page_id(venue),
        title: venue_title(venue),
        keywords: &[
            "market_participants",
            "venue",
            "credentials",
            "origin",
            "市场参与者",
            "场所",
            "凭据",
            "地址",
        ],
        sections: venue_sections(venue),
    }
}

/// The sections of one venue's page.
fn venue_sections(venue: &VenueId) -> Vec<SectionDef> {
    match venue {
        VenueId::Binance => binance_sections(venue),
        VenueId::Okx => okx_sections(venue),
        VenueId::Hyperliquid => hyperliquid_sections(venue),
        VenueId::Longbridge => longbridge_sections(venue),
    }
}

/// Binance's page: one section per market the venue publishes.
fn binance_sections(venue: &VenueId) -> Vec<SectionDef> {
    BinanceMarket::ALL
        .into_iter()
        .enumerate()
        .map(|(index, market)| {
            SectionDef::fields(
                Some(Text::literal(
                    scope_display_code(&scope_id(market.scope())).as_str(),
                )),
                index != 0,
                vec![
                    binance_http_field(venue, market),
                    binance_websocket_field(venue, market),
                    credential_field(venue, Some(market), CredentialField::BinanceApiKey),
                    credential_field(venue, Some(market), CredentialField::BinancePrivateKey),
                    credential_field(venue, Some(market), CredentialField::BinancePassphrase),
                ],
            )
        })
        .collect()
}

/// OKX's page: the region the account belongs to, beside its credentials.
///
/// The venue publishes one API per region, so the region is the whole of what an operator
/// chooses about where the venue is reached: both of its scopes move with it.
fn okx_sections(venue: &VenueId) -> Vec<SectionDef> {
    vec![
        SectionDef::fields(
            Some(text("settings_ui.market_participants.service_region")),
            false,
            vec![okx_region_field(venue)],
        ),
        SectionDef::fields(
            None,
            true,
            OKX_CREDENTIAL_FIELDS
                .iter()
                .map(|field| credential_field(venue, None, *field))
                .collect(),
        ),
    ]
}

/// HyperCore's page: one address of each kind, beside the API Agent's credentials.
fn hyperliquid_sections(venue: &VenueId) -> Vec<SectionDef> {
    vec![
        SectionDef::fields(
            Some(text("settings_ui.market_participants.origins")),
            false,
            vec![
                hyperliquid_http_field(venue),
                hyperliquid_websocket_field(venue),
            ],
        ),
        SectionDef::fields(
            None,
            true,
            HYPERLIQUID_CREDENTIAL_FIELDS
                .iter()
                .map(|field| credential_field(venue, None, *field))
                .collect(),
        ),
    ]
}

/// Longbridge's page: the region the account belongs to, beside its credentials.
///
/// The venue publishes one site per region, so the region is the whole of what an operator
/// chooses about where the venue is reached: its one scope moves with it.
fn longbridge_sections(venue: &VenueId) -> Vec<SectionDef> {
    vec![
        SectionDef::fields(
            Some(text("settings_ui.market_participants.service_region")),
            false,
            vec![longbridge_region_field(venue)],
        ),
        SectionDef::fields(
            None,
            true,
            LONGBRIDGE_CREDENTIAL_FIELDS
                .iter()
                .map(|field| credential_field(venue, None, *field))
                .collect(),
        ),
    ]
}

/// The id of one venue's page.
fn venue_page_id(venue: &VenueId) -> SharedString {
    SharedString::from(format!("market_participants.{}", venue.as_str()))
}

/// The name one venue is drawn under, both as its page's title and as its proxy row's label.
///
/// The name is the catalogue's, read through the one key a venue is named by: the page is
/// drawn in whatever language the window is showing, and the name it is drawn with is the
/// same one every other surface states.
fn venue_label(venue: &VenueId) -> Text {
    Text::Key(SharedString::new_static(venue.name_key()))
}

/// The title of one venue's page.
///
/// A venue is named by a catalogue key in every language, so the title is that key rather than a
/// name drawn as it is.
fn venue_title(venue: &VenueId) -> PageTitle {
    let Text::Key(key) = venue_label(venue) else {
        unreachable!("a venue is named by a catalogue key")
    };
    PageTitle::Key(Text::Key(key))
}

/// One venue's scope identity, as the request path spells it.
fn scope_id(scope: &str) -> VenueScopeId {
    VenueScopeId::new(scope).expect("a scope this build publishes is canonical")
}

// --- origins ------------------------------------------------------------------------------

/// Which of one scope's two addresses a row writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OriginKind {
    Http,
    Websocket,
}

impl OriginKind {
    /// The line under the row that names which address it writes.
    const fn label(self) -> Text {
        text(match self {
            Self::Http => "settings_ui.market_participants.http_origin",
            Self::Websocket => "settings_ui.market_participants.websocket_origin",
        })
    }
}

/// The row that chooses which published request address one Binance market is reached on.
fn binance_http_field(venue: &VenueId, market: BinanceMarket) -> FieldDef {
    let reader = *venue;
    let writer = *venue;

    FieldDef {
        id: SharedString::from(format!(
            "market_participants.{}.{}.http",
            venue.as_str(),
            market.scope()
        )),
        label: Text::literal(scope_display_code(&scope_id(market.scope())).as_str()),
        description: OriginKind::Http.label(),
        keywords: field_keywords(vec![
            venue.as_str(),
            market.scope(),
            "http",
            "origin",
            "地址",
        ]),
        access: FieldAccess::Select {
            options: binance_http_options(market),
            get: Rc::new(move |config: &Config| {
                let choice = binance_origins(config, &reader, market)
                    .map(|origins| origins.http)
                    .unwrap_or_else(|| market.http());
                FieldValue::new(choice.as_str())
            }),
            set: Rc::new(move |config: &mut Config, value: &FieldValue| {
                write_binance_origin(config, &writer, market, OriginKind::Http, value);
            }),
        },
    }
}

/// The row that chooses which published stream address one Binance market is reached on.
fn binance_websocket_field(venue: &VenueId, market: BinanceMarket) -> FieldDef {
    let reader = *venue;
    let writer = *venue;

    FieldDef {
        id: SharedString::from(format!(
            "market_participants.{}.{}.websocket",
            venue.as_str(),
            market.scope()
        )),
        label: Text::literal(scope_display_code(&scope_id(market.scope())).as_str()),
        description: OriginKind::Websocket.label(),
        keywords: field_keywords(vec![
            venue.as_str(),
            market.scope(),
            "websocket",
            "origin",
            "地址",
        ]),
        access: FieldAccess::Select {
            options: binance_websocket_options(market),
            get: Rc::new(move |config: &Config| {
                let choice = binance_origins(config, &reader, market)
                    .map(|origins| origins.websocket)
                    .unwrap_or_else(|| market.websocket());
                FieldValue::new(choice.as_str())
            }),
            set: Rc::new(move |config: &mut Config, value: &FieldValue| {
                write_binance_origin(config, &writer, market, OriginKind::Websocket, value);
            }),
        },
    }
}

/// The address choices one Binance market holds, or nothing while the venue is not
/// configured as Binance.
fn binance_origins(
    config: &Config,
    venue: &VenueId,
    market: BinanceMarket,
) -> Option<BinanceOrigins> {
    match config.venue(venue) {
        Some(VenueSettings::Binance(settings)) => settings.market_origins(market),
        _ => None,
    }
}

/// The row that chooses which service region an OKX account belongs to.
///
/// The venue publishes one API per region, so one row stands for the whole of where the
/// venue is reached: a region's two addresses are published together, and an operator
/// picks the region they registered in rather than a host.
fn okx_region_field(venue: &VenueId) -> FieldDef {
    let reader = *venue;
    let writer = *venue;

    FieldDef {
        id: SharedString::from(format!("market_participants.{}.region", venue.as_str())),
        label: text("settings_ui.market_participants.service_region"),
        description: text("settings_ui.market_participants.service_region_description"),
        keywords: &[
            "market_participants",
            "okx",
            "region",
            "origin",
            "服务地区",
            "市场参与者",
        ],
        access: FieldAccess::Select {
            options: &OKX_REGION_OPTIONS,
            get: Rc::new(move |config: &Config| {
                let region = match config.venue(&reader) {
                    Some(VenueSettings::Okx(settings)) => settings.region,
                    _ => OkxRegion::default(),
                };
                FieldValue::new(region.as_str())
            }),
            set: Rc::new(move |config: &mut Config, value: &FieldValue| {
                let Some(region) = OkxRegion::ALL
                    .iter()
                    .find(|region| region.as_str() == value.as_str())
                else {
                    return;
                };
                config.set_okx_region(&writer, *region);
            }),
        },
    }
}

/// The row that chooses which service region a Longbridge account belongs to.
///
/// The venue publishes one site per region, so one row stands for the whole of where the
/// venue is reached: a region's two addresses are published together, and an operator picks
/// the site their account was opened on rather than a host.
fn longbridge_region_field(venue: &VenueId) -> FieldDef {
    let reader = *venue;
    let writer = *venue;

    FieldDef {
        id: SharedString::from(format!("market_participants.{}.region", venue.as_str())),
        label: text("settings_ui.market_participants.service_region"),
        description: text("settings_ui.market_participants.service_region_description"),
        keywords: &[
            "market_participants",
            "longbridge",
            "region",
            "origin",
            "服务地区",
            "市场参与者",
        ],
        access: FieldAccess::Select {
            options: &LONGBRIDGE_REGION_OPTIONS,
            get: Rc::new(move |config: &Config| {
                let region = match config.venue(&reader) {
                    Some(VenueSettings::Longbridge(settings)) => settings.region,
                    _ => LongbridgeRegion::default(),
                };
                FieldValue::new(region.as_str())
            }),
            set: Rc::new(move |config: &mut Config, value: &FieldValue| {
                let Some(region) = LongbridgeRegion::ALL
                    .iter()
                    .find(|region| region.as_str() == value.as_str())
                else {
                    return;
                };
                config.set_longbridge_region(&writer, *region);
            }),
        },
    }
}

/// The row that chooses which published request address HyperCore is reached on.
fn hyperliquid_http_field(venue: &VenueId) -> FieldDef {
    let reader = *venue;
    let writer = *venue;

    FieldDef {
        id: SharedString::from(format!("market_participants.{}.http", venue.as_str())),
        label: text("settings_ui.market_participants.http_origin"),
        description: text("settings_ui.market_participants.hyperliquid_http_origin_description"),
        keywords: &[
            "market_participants",
            "hyperliquid",
            "http",
            "origin",
            "地址",
        ],
        access: FieldAccess::Select {
            options: &HYPERLIQUID_HTTP_OPTIONS,
            get: Rc::new(move |config: &Config| {
                let choice = match config.venue(&reader) {
                    Some(VenueSettings::Hyperliquid(settings)) => settings.http,
                    _ => HyperliquidHttpOrigin::default(),
                };
                FieldValue::new(choice.as_str())
            }),
            set: Rc::new(move |config: &mut Config, value: &FieldValue| {
                write_hyperliquid_origin(config, &writer, OriginKind::Http, value);
            }),
        },
    }
}

/// The row that chooses which published stream address HyperCore is reached on.
fn hyperliquid_websocket_field(venue: &VenueId) -> FieldDef {
    let reader = *venue;
    let writer = *venue;

    FieldDef {
        id: SharedString::from(format!("market_participants.{}.websocket", venue.as_str())),
        label: text("settings_ui.market_participants.websocket_origin"),
        description: text(
            "settings_ui.market_participants.hyperliquid_websocket_origin_description",
        ),
        keywords: &[
            "market_participants",
            "hyperliquid",
            "websocket",
            "origin",
            "地址",
        ],
        access: FieldAccess::Select {
            options: &HYPERLIQUID_WEBSOCKET_OPTIONS,
            get: Rc::new(move |config: &Config| {
                let choice = match config.venue(&reader) {
                    Some(VenueSettings::Hyperliquid(settings)) => settings.websocket,
                    _ => HyperliquidWebsocketOrigin::default(),
                };
                FieldValue::new(choice.as_str())
            }),
            set: Rc::new(move |config: &mut Config, value: &FieldValue| {
                write_hyperliquid_origin(config, &writer, OriginKind::Websocket, value);
            }),
        },
    }
}

/// Writes one Binance market's choice of one of its two addresses.
///
/// The pair is written as a whole — the other role is carried over unchanged — because a
/// market is reached at one pair of addresses and not at two values of different shapes. A
/// value no table publishes writes nothing: the choices are the venue's own, and a control
/// that offered one this build does not know is not a control it drew.
fn write_binance_origin(
    config: &mut Config,
    venue: &VenueId,
    market: BinanceMarket,
    kind: OriginKind,
    value: &FieldValue,
) {
    let Some(mut origins) = binance_origins(config, venue, market) else {
        return;
    };

    match kind {
        OriginKind::Http => {
            let Some(choice) = BinanceHttpOrigin::for_market(market)
                .iter()
                .find(|choice| choice.as_str() == value.as_str())
            else {
                return;
            };
            origins.http = *choice;
        }
        OriginKind::Websocket => {
            let Some(choice) = BinanceWebsocketOrigin::for_market(market)
                .iter()
                .find(|choice| choice.as_str() == value.as_str())
            else {
                return;
            };
            origins.websocket = *choice;
        }
    }

    config.set_binance_market_origins(venue, market, &origins);
}

/// Writes one of HyperCore's two address choices.
fn write_hyperliquid_origin(
    config: &mut Config,
    venue: &VenueId,
    kind: OriginKind,
    value: &FieldValue,
) {
    let Some(VenueSettings::Hyperliquid(settings)) = config.venue(venue) else {
        return;
    };
    let (mut http, mut websocket) = (settings.http, settings.websocket);

    match kind {
        OriginKind::Http => {
            let Some(choice) = HyperliquidHttpOrigin::from_url(value.as_str()) else {
                return;
            };
            http = choice;
        }
        OriginKind::Websocket => {
            let Some(choice) = HyperliquidWebsocketOrigin::from_url(value.as_str()) else {
                return;
            };
            websocket = choice;
        }
    }

    config.set_hyperliquid_origins(venue, http, websocket);
}

// --- credentials ------------------------------------------------------------------------

/// One value of one venue's credential set, as a page names it.
///
/// The name is the one the settings store the value under, so a field id, the row a user
/// types into and the value the runtime reads are one name rather than three.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CredentialField {
    BinanceApiKey,
    BinancePrivateKey,
    BinancePassphrase,
    OkxApiKey,
    OkxSecretKey,
    OkxPassphrase,
    MainAccountAddress,
    ApiAgentPrivateKey,
    LongbridgeAppKey,
    LongbridgeAppSecret,
    LongbridgeAccessToken,
}

impl CredentialField {
    /// The name the settings store this value under in one Binance market.
    ///
    /// Binance's two markets hold a set each, so a value's name carries the market it
    /// belongs to: `spot_api_key` and `usd_m_perp_api_key` are two values of one venue, and
    /// a name that did not say which would be a write the runtime could not place.
    const fn wire_name(self, market: BinanceMarket) -> &'static str {
        match (market, self) {
            (BinanceMarket::UsdMPerp, Self::BinanceApiKey) => "usd_m_perp_api_key",
            (BinanceMarket::UsdMPerp, Self::BinancePrivateKey) => "usd_m_perp_private_key",
            (BinanceMarket::UsdMPerp, Self::BinancePassphrase) => "usd_m_perp_passphrase",
            (_, Self::BinanceApiKey) => "spot_api_key",
            (_, Self::BinancePrivateKey) => "spot_private_key",
            (_, Self::BinancePassphrase) => "spot_passphrase",
            (_, Self::OkxApiKey) => "api_key",
            (_, Self::OkxSecretKey) => "secret_key",
            (_, Self::OkxPassphrase) => "passphrase",
            (_, Self::MainAccountAddress) => "main_account_address",
            (_, Self::ApiAgentPrivateKey) => "api_agent_private_key",
            (_, Self::LongbridgeAppKey) => "app_key",
            (_, Self::LongbridgeAppSecret) => "app_secret",
            (_, Self::LongbridgeAccessToken) => "access_token",
        }
    }

    /// The name this value is drawn under.
    const fn label(self) -> Text {
        text(match self {
            Self::BinanceApiKey | Self::OkxApiKey => "settings_ui.market_participants.api_key",
            Self::BinancePrivateKey => "settings_ui.market_participants.ed25519_private_key",
            Self::BinancePassphrase => "settings_ui.market_participants.ed25519_passphrase",
            Self::OkxSecretKey => "settings_ui.market_participants.secret_key",
            Self::OkxPassphrase => "settings_ui.market_participants.passphrase",
            Self::MainAccountAddress => "settings_ui.market_participants.main_account_address",
            Self::ApiAgentPrivateKey => "settings_ui.market_participants.api_agent_private_key",
            Self::LongbridgeAppKey => "settings_ui.market_participants.app_key",
            Self::LongbridgeAppSecret => "settings_ui.market_participants.app_secret",
            Self::LongbridgeAccessToken => "settings_ui.market_participants.access_token",
        })
    }

    /// The line under the row that says what the value is for.
    const fn description(self) -> Text {
        text(match self {
            Self::BinanceApiKey | Self::OkxApiKey => {
                "settings_ui.market_participants.api_key_description"
            }
            Self::BinancePrivateKey | Self::ApiAgentPrivateKey => {
                "settings_ui.market_participants.private_key_description"
            }
            Self::BinancePassphrase | Self::OkxPassphrase => {
                "settings_ui.market_participants.passphrase_description"
            }
            Self::OkxSecretKey => "settings_ui.market_participants.secret_key_description",
            Self::MainAccountAddress => "settings_ui.market_participants.main_account_description",
            Self::LongbridgeAppKey => "settings_ui.market_participants.app_key_description",
            Self::LongbridgeAppSecret => "settings_ui.market_participants.app_secret_description",
            Self::LongbridgeAccessToken => {
                "settings_ui.market_participants.access_token_description"
            }
        })
    }

    /// The hint a row shows while it is empty, if it has one.
    const fn placeholder(self) -> Option<Text> {
        Some(text(match self {
            Self::BinanceApiKey | Self::OkxApiKey => {
                "settings_ui.market_participants.enter_api_key"
            }
            Self::BinancePrivateKey | Self::ApiAgentPrivateKey => {
                "settings_ui.market_participants.enter_private_key"
            }
            Self::BinancePassphrase | Self::OkxPassphrase => {
                "settings_ui.market_participants.enter_passphrase"
            }
            Self::OkxSecretKey => "settings_ui.market_participants.enter_secret_key",
            Self::MainAccountAddress => return None,
            Self::LongbridgeAppKey => "settings_ui.market_participants.enter_app_key",
            Self::LongbridgeAppSecret => "settings_ui.market_participants.enter_app_secret",
            Self::LongbridgeAccessToken => "settings_ui.market_participants.enter_access_token",
        }))
    }

    /// Whether the value is drawn masked.
    ///
    /// A private key and a passphrase are secrets a shoulder must not read; an account address
    /// and an API key are identities the operator checks against the venue's own page, so they
    /// are drawn as they are. Longbridge's app key is that kind of identity, and the two values
    /// that sign with it are not.
    const fn password(self) -> bool {
        matches!(
            self,
            Self::BinancePrivateKey
                | Self::BinancePassphrase
                | Self::OkxSecretKey
                | Self::OkxPassphrase
                | Self::ApiAgentPrivateKey
                | Self::LongbridgeAppSecret
                | Self::LongbridgeAccessToken
        )
    }
}

/// The credential values OKX authenticates with, in the order its page draws them.
const OKX_CREDENTIAL_FIELDS: [CredentialField; 3] = [
    CredentialField::OkxApiKey,
    CredentialField::OkxSecretKey,
    CredentialField::OkxPassphrase,
];

/// The credential values HyperCore authenticates with, in the order its page draws them.
const HYPERLIQUID_CREDENTIAL_FIELDS: [CredentialField; 2] = [
    CredentialField::MainAccountAddress,
    CredentialField::ApiAgentPrivateKey,
];

/// The credential values Longbridge authenticates with, in the order its page draws them.
const LONGBRIDGE_CREDENTIAL_FIELDS: [CredentialField; 3] = [
    CredentialField::LongbridgeAppKey,
    CredentialField::LongbridgeAppSecret,
    CredentialField::LongbridgeAccessToken,
];

/// One credential row of one venue.
///
/// The value is read and written through the configuration's own accessor, keyed by the
/// name the settings store it under: which venue a value lands in, and whether the venue
/// can hold it at all, is the settings' answer rather than this page's, and a field the
/// venue's own shape does not have writes nothing.
///
/// `market` is the market whose value the row stands for: Binance holds one set per market,
/// so its rows carry the market in their id as well as in the name they write under, and a
/// row that named only the value would be one two markets share.
fn credential_field(
    venue: &VenueId,
    market: Option<BinanceMarket>,
    field: CredentialField,
) -> FieldDef {
    let reader = *venue;
    let writer = *venue;
    let wire_name = field.wire_name(market.unwrap_or(BinanceMarket::Spot));
    let id = match market {
        Some(market) => SharedString::from(format!(
            "market_participants.{}.{}.{wire_name}",
            venue.as_str(),
            market.scope()
        )),
        None => SharedString::from(format!(
            "market_participants.{}.{wire_name}",
            venue.as_str()
        )),
    };

    FieldDef {
        id,
        label: field.label(),
        description: field.description(),
        keywords: &["market_participants", "credential", "市场参与者", "凭据"],
        access: FieldAccess::Text {
            get: Rc::new(move |config: &Config| credential_value(config, &reader, wire_name)),
            set: Rc::new(move |config: &mut Config, value: &FieldValue| {
                config.set_venue_credential(&writer, wire_name, value);
            }),
            placeholder: field.placeholder(),
            password: field.password(),
        },
    }
}

/// Reads one credential value of one venue.
fn credential_value(config: &Config, venue: &VenueId, wire_name: &str) -> FieldValue {
    let Some(settings) = config.venue(venue) else {
        return FieldValue::default();
    };

    match settings {
        VenueSettings::Binance(binance) => {
            let credentials = binance
                .credentials
                .market(market_of_name(wire_name).credential_market());
            match wire_name {
                "spot_api_key" | "usd_m_perp_api_key" => FieldValue::new(credentials.api_key),
                "spot_private_key" | "usd_m_perp_private_key" => {
                    FieldValue::new(credentials.private_key)
                }
                "spot_passphrase" | "usd_m_perp_passphrase" => {
                    FieldValue::new(credentials.passphrase)
                }
                _ => FieldValue::default(),
            }
        }
        VenueSettings::Okx(okx) => match wire_name {
            "api_key" => okx.credentials.api_key.clone(),
            "secret_key" => okx.credentials.secret_key.clone(),
            "passphrase" => okx.credentials.passphrase.clone(),
            _ => FieldValue::default(),
        },
        VenueSettings::Hyperliquid(hyperliquid) => match wire_name {
            "main_account_address" => hyperliquid.credentials.main_account_address.clone(),
            "api_agent_private_key" => hyperliquid.credentials.api_agent_private_key.clone(),
            _ => FieldValue::default(),
        },
        VenueSettings::Longbridge(longbridge) => match wire_name {
            "app_key" => longbridge.credentials.app_key.clone(),
            "app_secret" => longbridge.credentials.app_secret.clone(),
            "access_token" => longbridge.credentials.access_token.clone(),
            _ => FieldValue::default(),
        },
    }
}

/// The Binance market one credential value's wire name belongs to.
fn market_of_name(wire_name: &str) -> BinanceMarket {
    match wire_name {
        "usd_m_perp_api_key" | "usd_m_perp_private_key" | "usd_m_perp_passphrase" => {
            BinanceMarket::UsdMPerp
        }
        _ => BinanceMarket::Spot,
    }
}

// --- names and registry lookups -----------------------------------------------------------

/// The Kytrix services the applications page reaches through one line.
const KYTRIX_CLOUD: &[ServiceTarget] = &[
    ServiceTarget::KytrixService {
        capability: KytrixServiceCapability::Account,
    },
    ServiceTarget::KytrixService {
        capability: KytrixServiceCapability::Wallet,
    },
    ServiceTarget::KytrixService {
        capability: KytrixServiceCapability::Etf,
    },
    ServiceTarget::KytrixService {
        capability: KytrixServiceCapability::News,
    },
    ServiceTarget::KytrixService {
        capability: KytrixServiceCapability::Indices,
    },
    ServiceTarget::KytrixService {
        capability: KytrixServiceCapability::X,
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

/// The media service, which the applications page groups on its own.
const X_MEDIA: &[ServiceTarget] = &[ServiceTarget::KytrixService {
    capability: KytrixServiceCapability::XMedia,
}];

/// The public feeds the application reads, which are grouped on their own because
/// none of them is an account service and none of them carries credentials.
const PUBLIC_FEEDS: &[ServiceTarget] = &[ServiceTarget::KytrixService {
    capability: KytrixServiceCapability::PentagonPizzaIndex,
}];

/// The words one built field is searched by.
///
/// Every word is already a static string — the identities and scope names a field is built from
/// are `&'static str` — so the slice is the only thing that has to outlive the catalog, and a
/// catalog is built once per settings session.
fn field_keywords(values: Vec<&'static str>) -> &'static [&'static str] {
    Box::leak(values.into_boxed_slice())
}

#[cfg(test)]
pub(crate) mod tests {
    use app_config::market_participants::credentials::LongbridgeCredentials;
    use market_participants_connector::venue::definition::supported_venues;

    use super::*;
    use crate::access::{PROXY_DIRECT, PROXY_INHERIT};

    /// The venues the fixture build speaks.
    pub(crate) fn venues() -> VenueView {
        supported_venues().into()
    }

    /// The settings the fixture build is described by.
    ///
    /// A fresh installation: every venue on the addresses its own documentation publishes and
    /// no credentials, which is what a page is drawn over.
    pub(crate) fn configured() -> Config {
        Config::default()
    }

    fn venue(name: &str) -> VenueId {
        VenueId::from_name(name).expect("the fixture venue is canonical")
    }

    fn catalog_of() -> crate::catalog::Catalog {
        crate::catalog::catalog(&venues())
    }

    fn page<'a>(catalog: &'a crate::catalog::Catalog, id: &str) -> &'a PageDef {
        catalog
            .page(id)
            .unwrap_or_else(|| panic!("{id} should be built"))
    }

    fn field<'a>(page: &'a PageDef, id: &str) -> &'a FieldDef {
        page.sections
            .iter()
            .flat_map(|section| &section.fields)
            .find(|field| field.id == id)
            .unwrap_or_else(|| panic!("{id} should be offered"))
    }

    /// Every id a page offers.
    fn field_ids(page: &PageDef) -> Vec<&str> {
        page.sections
            .iter()
            .flat_map(|section| &section.fields)
            .map(|field| field.id.as_ref())
            .collect()
    }

    /// The name each section of one page is drawn under.
    ///
    /// A section's title is either a message key or a name drawn as it is, so this is what the
    /// page names its groups by rather than what a reader sees them rendered as.
    fn section_titles(page: &PageDef) -> Vec<&str> {
        page.sections
            .iter()
            .map(|section| {
                match section
                    .title
                    .as_ref()
                    .expect("the page titles its sections")
                {
                    Text::Literal(name) => name.as_ref(),
                    Text::Key(key) => key.as_ref(),
                }
            })
            .collect()
    }

    /// The section one field is drawn under.
    fn section_of<'a>(page: &'a PageDef, id: &str) -> &'a str {
        page.sections
            .iter()
            .find(|section| section.fields.iter().any(|field| field.id == id))
            .and_then(|section| section.title.as_ref())
            .map_or_else(
                || panic!("{id} should be drawn under a named section"),
                |title| match title {
                    Text::Literal(name) => name.as_ref(),
                    Text::Key(key) => key.as_ref(),
                },
            )
    }

    /// Every option id one select field declares.
    fn option_ids(page: &PageDef, id: &str) -> Vec<&'static str> {
        match &field(page, id).access {
            FieldAccess::Select { options, .. } => {
                options.iter().map(|option| option.id.as_ref()).collect()
            }
            _ => panic!("{id} is a select"),
        }
    }

    /// Every key the built pages declare is a key the catalogue translates.
    ///
    /// A key no locale defines renders as an empty string, which is a label that is simply not
    /// there rather than a failure anyone sees. The pages a build generates — one per venue, with
    /// the rows its own API has — are what makes this worth asking: a label comes with the code
    /// that adds the setting, and the text for it is written somewhere else.
    #[test]
    fn every_key_the_catalog_declares_should_translate() {
        let i18n = i18n::global::embedded::load().expect("embedded locales should load");
        let catalog = catalog_of();

        let mut keys = Vec::new();
        for node in &catalog.tree {
            if let TreeNodeDef::Group { label, .. } = node {
                keys.extend(label.key().map(str::to_owned));
            }
        }
        for page in &catalog.pages {
            if let PageTitle::Key(key) = &page.title {
                keys.extend(key.key().map(str::to_owned));
            }
            for section in &page.sections {
                keys.extend(
                    section
                        .title
                        .iter()
                        .filter_map(|title| title.key().map(str::to_owned)),
                );
                for field in &section.fields {
                    keys.extend(field.label.key().map(str::to_owned));
                    keys.extend(field.description.key().map(str::to_owned));
                    match &field.access {
                        FieldAccess::Text { placeholder, .. } => keys.extend(
                            placeholder
                                .iter()
                                .filter_map(|placeholder| placeholder.key().map(str::to_owned)),
                        ),
                        FieldAccess::Select { options, .. } => keys.extend(
                            options
                                .iter()
                                .filter_map(|option| option.label.key().map(str::to_owned)),
                        ),
                        FieldAccess::Info { .. } | FieldAccess::ProxyLine { .. } => {}
                    }
                }
            }
        }

        assert!(!keys.is_empty(), "the catalog should declare something");

        for language in i18n.languages() {
            for key in &keys {
                assert!(
                    i18n.try_translate(language, key)
                        .is_some_and(|text| !text.is_empty()),
                    "{key} is declared by the catalog but not defined in {language}"
                );
            }
        }
    }

    /// The attribute a source file opens its tests with.
    const TEST_MODULE_ATTRIBUTE: &str = concat!("#[cfg", "(test)]");

    /// The module declarations that make that attribute a *test module* rather than a gate on
    /// one item.
    const TEST_MODULE_SHAPES: [&str; 4] =
        ["mod ", "pub mod ", "pub(crate) mod ", "pub(super) mod "];

    /// Every source of this crate, as the file it was written in beside its text.
    ///
    /// The list is spelled out because a source that declares a label but is not read here would
    /// silently escape both halves of the guard below: its keys would never be looked for in a
    /// locale, and a locale's key would look like an orphan the crate does not draw. Nothing ties
    /// it to the directory, so a source added to this crate has to be added here too — which is
    /// what [`every_settings_source_should_be_read_by_the_key_guard`] is for.
    const SETTINGS_SOURCES: [(&str, &str); 12] = [
        ("access.rs", include_str!("access.rs")),
        ("appearance.rs", include_str!("appearance.rs")),
        ("catalog.rs", include_str!("catalog.rs")),
        ("dialog.rs", include_str!("dialog.rs")),
        ("draft.rs", include_str!("draft.rs")),
        ("editor.rs", include_str!("editor.rs")),
        ("entry.rs", include_str!("entry.rs")),
        ("lib.rs", include_str!("lib.rs")),
        ("page.rs", include_str!("page.rs")),
        ("schema.rs", include_str!("schema.rs")),
        ("search.rs", include_str!("search.rs")),
        ("table.rs", include_str!("table.rs")),
    ];

    /// The half of one source that names the settings: everything above the test module it
    /// declares, or the whole file when it declares none.
    ///
    /// The slice is taken at the test *module* rather than at the first `#[cfg(test)]`, because
    /// the sources of this crate gate accessors a test drives with that attribute in the middle
    /// of their production half — `catalog.rs` is entirely production and has one — and a slice
    /// taken there would leave everything after it unread.
    fn production_half(source: &'static str) -> &'static str {
        let mut offset = 0usize;

        while let Some(attribute) = source[offset..].find(TEST_MODULE_ATTRIBUTE) {
            offset += attribute + TEST_MODULE_ATTRIBUTE.len();
            let declaration = source[offset..].trim_start();
            if TEST_MODULE_SHAPES
                .iter()
                .any(|shape| declaration.starts_with(shape))
            {
                return &source[..offset - TEST_MODULE_ATTRIBUTE.len()];
            }
        }

        source
    }

    /// Every double-quoted literal the half of a source that names the settings writes.
    ///
    /// The quotes are paired by parity, so the halves have to be cut at the right place: a
    /// literal that falls in a source's *test* half is a key only a test names, and one that
    /// falls in its production half is a key the settings draw.
    fn declared_literals() -> impl Iterator<Item = &'static str> {
        SETTINGS_SOURCES
            .iter()
            .flat_map(|(_, source)| production_half(source).split('"').skip(1).step_by(2))
    }

    /// Every statement key the sources of this crate name.
    ///
    /// A literal beginning with the prefix is a key: everything else a source writes in quotes
    /// is a route, an identity, a proxy line or a word somebody chose, and a key is the one
    /// spelling this crate's locale files and its sources have to agree on.
    fn declared_keys() -> Vec<&'static str> {
        declared_literals()
            .filter(|literal| {
                literal
                    .strip_prefix("settings_ui.")
                    .is_some_and(|key| !key.is_empty())
            })
            .collect()
    }

    /// The key guard reads every source of this crate, each one from its own half, and that half
    /// ends at a test *module* rather than at any test attribute.
    ///
    /// The list of sources is written out by hand, so this is what holds it to the crate: the
    /// directory is what a source is, and a source that is not in the list escapes both halves of
    /// the guard — its keys would never be looked for in a locale, and a locale's key would look
    /// like text nothing draws.
    ///
    /// The other half of the check is what the slice is taken at. `catalog.rs` gates an accessor
    /// with a test attribute and declares no test module, so a slice taken at the first attribute
    /// would leave most of that file unread; a source sliced at the wrong place would silently
    /// stop checking the keys below the cut. Both are stated here, and the second is checked
    /// against the shape the sources actually write.
    #[test]
    fn every_settings_source_should_be_read_by_the_key_guard() {
        let declared: Vec<&str> = SETTINGS_SOURCES.iter().map(|(name, _)| *name).collect();
        let mut present: Vec<String> =
            std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/src"))
                .expect("the crate's own source directory is readable")
                .map(|entry| entry.expect("a directory entry is readable").file_name())
                .filter_map(|name| {
                    let name = name.to_string_lossy();
                    if name.ends_with(".rs") {
                        Some(name.into_owned())
                    } else {
                        None
                    }
                })
                .collect();
        present.sort();

        assert_eq!(
            declared
                .iter()
                .map(|name| (*name).to_owned())
                .collect::<Vec<_>>(),
            present,
            "the guard reads the crate's sources, and the crate's sources are these"
        );

        for (name, source) in SETTINGS_SOURCES {
            let attributes = source.matches(TEST_MODULE_ATTRIBUTE).count();
            let opens_a_module = TEST_MODULE_SHAPES
                .iter()
                .filter(|shape| source.contains(&format!("{TEST_MODULE_ATTRIBUTE}\n{shape}")))
                .count();
            assert!(
                opens_a_module <= 1,
                "{name} opens {opens_a_module} test modules, so the half read from it is not the \
                 one that names the settings"
            );
            assert!(
                !production_half(source)
                    .lines()
                    .any(|line| line.trim_end() == TEST_MODULE_ATTRIBUTE),
                "{name}'s production half ends before its test attribute — it has {attributes}"
            );
        }

        // `catalog.rs` is the source the split exists for: it gates an accessor with a test
        // attribute and declares no test module, so a slice taken at the attribute would leave
        // most of the file unread — and the keys below the cut uncheckable.
        let catalog = SETTINGS_SOURCES
            .iter()
            .find(|(name, _)| *name == "catalog.rs")
            .map(|(_, source)| *source)
            .expect("the guard reads catalog.rs");
        assert_eq!(
            production_half(catalog).len(),
            catalog.len(),
            "a source with no test module is read whole"
        );
    }

    /// The half of a source the guard reads ends where its test module begins, and nowhere else.
    #[test]
    fn the_guard_should_read_a_source_up_to_its_test_module() {
        for (name, declaration) in [
            ("a private test module", "mod tests {"),
            ("a crate-visible one", "pub(crate) mod tests {"),
            ("a public one", "pub mod tests {"),
            ("a parent-visible one", "pub(super) mod tests {"),
        ] {
            let source: &'static str = Box::leak(
                format!(
                    "fn drawn() {{ text(\"settings_ui.drawn\"); }}\n\
                     {TEST_MODULE_ATTRIBUTE}\n{declaration}\n\
                     fn named_by_a_test() {{ text(\"settings_ui.not_drawn\"); }}\n"
                )
                .into_boxed_str(),
            );

            assert_eq!(
                production_half(source),
                "fn drawn() { text(\"settings_ui.drawn\"); }\n",
                "{name} ends the production half where it begins"
            );
        }

        // An attribute that gates one item is not a test module: the guard reads past it, which
        // is what keeps `catalog.rs`'s accessor from hiding the rest of that file.
        let gated: &'static str = Box::leak(
            format!(
                "impl Catalog {{\n    {TEST_MODULE_ATTRIBUTE}\n    fn key(&self) {{}}\n}}\n\
                 fn below() {{ text(\"settings_ui.below_the_gate\"); }}\n"
            )
            .into_boxed_str(),
        );
        assert_eq!(
            production_half(gated),
            gated,
            "an accessor gated for tests does not end the production half"
        );
        assert!(
            declared_literals_of(gated).any(|literal| literal == "settings_ui.below_the_gate"),
            "and a key written below such a gate is still read"
        );
    }

    /// Every literal one source's production half writes, as the guard reads them.
    fn declared_literals_of(source: &'static str) -> impl Iterator<Item = &'static str> {
        production_half(source).split('"').skip(1).step_by(2)
    }

    /// Every key the settings of this crate name is one every locale defines.
    ///
    /// A key no locale defines renders as an empty label rather than failing. Asking the built
    /// pages covers what the fixture build declares, which is not the whole of what the settings
    /// name: a heading or a page that no venue reaches today is declared all the same, and would
    /// be drawn the day one is — so the keys are read out of the declarations themselves,
    /// wherever in the crate they are written.
    #[test]
    fn every_key_the_settings_name_should_be_defined_in_every_locale() {
        let i18n = i18n::global::embedded::load().expect("embedded locales should load");
        let keys = declared_keys();

        assert!(
            keys.len() > 50,
            "the settings should name their labels, and {} was read out of them",
            keys.len()
        );

        for language in i18n.languages() {
            let defined = i18n
                .keys(language)
                .expect("an embedded language exposes its keys")
                .collect::<Vec<_>>();

            for key in &keys {
                assert!(
                    defined.contains(key),
                    "{key} is named by the settings but {language} does not define it"
                );
                assert!(
                    i18n.try_translate(language, key)
                        .is_some_and(|text| !text.is_empty()),
                    "{key} is named by the settings but renders empty in {language}"
                );
            }
        }
    }

    /// Every key a locale defines for the settings is one the settings draw.
    ///
    /// The other half of the guard above, and the one that catches a key left behind by a page
    /// that no longer exists: an orphan is not a blank label anyone notices, it is text that sits
    /// in every locale until somebody reuses the name for something else. What counts as drawn is
    /// every key the built pages declare — a page, a heading, a label, a description, a
    /// placeholder or a choice — beside every key the crate's own sources name, because the proxy
    /// table and the editor draw their labels without going through a page.
    #[test]
    fn every_settings_key_is_drawn_and_every_drawn_key_exists() {
        let i18n = i18n::global::embedded::load().expect("embedded locales should load");
        let catalog = catalog_of();
        let mut drawn = declared_keys()
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();

        for node in &catalog.tree {
            if let TreeNodeDef::Group { label, .. } = node {
                drawn.extend(label.key().map(str::to_owned));
            }
        }
        for page in &catalog.pages {
            if let PageTitle::Key(key) = &page.title {
                drawn.extend(key.key().map(str::to_owned));
            }
            for section in &page.sections {
                drawn.extend(
                    section
                        .title
                        .iter()
                        .filter_map(|title| title.key().map(str::to_owned)),
                );
                for field in &section.fields {
                    drawn.extend(field.label.key().map(str::to_owned));
                    drawn.extend(field.description.key().map(str::to_owned));
                    match &field.access {
                        FieldAccess::Text { placeholder, .. } => drawn.extend(
                            placeholder
                                .iter()
                                .filter_map(|placeholder| placeholder.key().map(str::to_owned)),
                        ),
                        FieldAccess::Select { options, .. } => drawn.extend(
                            options
                                .iter()
                                .filter_map(|option| option.label.key().map(str::to_owned)),
                        ),
                        FieldAccess::Info { .. } | FieldAccess::ProxyLine { .. } => {}
                    }
                }
            }
        }

        for language in i18n.languages() {
            let defined = i18n
                .keys(language)
                .expect("an embedded language exposes its keys")
                .filter(|key| key.starts_with("settings_ui."))
                .collect::<Vec<_>>();

            for key in &defined {
                assert!(
                    drawn.iter().any(|drawn| drawn == key),
                    "{key} is defined in {language} but nothing in the settings draws it"
                );
            }
        }
    }

    /// A select field is drawn with one of the choices it declares: a stored value no choice
    /// names is a control that draws nothing at all, which is a setting the user cannot read back
    /// whatever the settings hold.
    ///
    /// The values a select can hold are the variants of the type it stores, so every one of them
    /// is put through the field's own reader here: a choice set that drifted from the type — an
    /// option id spelled differently from the value the settings store — is caught rather than
    /// drawn as a blank control.
    #[test]
    fn every_select_should_draw_a_choice_it_declares() {
        let mut light = Config::default();
        light.appearance = app_config::settings::Appearance::Light;
        light.language = app_config::settings::Language::En;
        let mut dark = Config::default();
        dark.appearance = app_config::settings::Appearance::Dark;
        let configs = [Config::default(), light, dark];
        let catalog = catalog_of();
        let mut checked = 0;

        for config in &configs {
            for page in &catalog.pages {
                for section in &page.sections {
                    for field in &section.fields {
                        let FieldAccess::Select { options, .. } = field.access else {
                            continue;
                        };
                        let current = field.access.current(config);

                        assert!(
                            options.iter().any(|option| option.id == current),
                            "{} reads {current:?}, which is not one of the choices it declares",
                            field.id
                        );
                        checked += 1;
                    }
                }
            }
        }

        assert!(checked > 0, "the settings should declare a select");
    }

    /// Every proxy line finds the venues or targets it stands for in its own access, so a field
    /// whose access does not carry them is a row that neither shows the stored line nor writes the
    /// chosen one: the control takes the choice and the settings never see it.
    #[test]
    fn every_proxy_line_should_read_back_the_line_it_writes() {
        let line = FieldValue::new_static("local");
        let mut checked = 0;
        for page in catalog_of().pages {
            for section in &page.sections {
                for field in &section.fields {
                    let FieldAccess::ProxyLine { .. } = field.access else {
                        continue;
                    };
                    let mut config = Config::default();

                    field.access.write(&mut config, &line);

                    assert_eq!(
                        field.access.current(&config),
                        line,
                        "{} does not resolve the venues or targets it stands for",
                        field.id
                    );
                    checked += 1;
                }
            }
        }
        assert!(checked > 0, "the proxy pages should declare lines");
    }

    /// A venue's own line is the venue's, and the row that draws it offers the default line
    /// beside the lines a user can name: the row reads back the line it wrote, and the document's
    /// default line is a row of its own that leaves the venue where it is.
    #[test]
    fn a_venue_line_should_be_written_where_the_row_reads_it() {
        let catalog = catalog_of();
        let binance = venue("binance");
        let row = field(
            page(&catalog, "proxy-applications"),
            &format!("proxy-applications.{}", binance.as_str()),
        );

        let FieldAccess::ProxyLine {
            default_line: venue_default_line,
            ..
        } = row.access
        else {
            panic!("a venue row is a proxy line");
        };
        assert!(
            !venue_default_line,
            "a venue row is one of the rows that may follow the default line"
        );

        let mut config = Config::default();
        let line = FieldValue::new_static("local");
        row.access.write(&mut config, &line);

        assert_eq!(row.access.current(&config), line);
        assert_eq!(
            config
                .venue(&binance)
                .expect("the fixture settings name every venue this build speaks")
                .proxy(),
            Some(app_config::network::ProxyRoute::named("local")),
            "the row wrote the venue's own line"
        );

        // The document-wide default is a row of its own, and setting it leaves the venue alone.
        let default_row = field(page(&catalog, "proxy-applications"), "proxy.default-line");
        default_row
            .access
            .write(&mut config, &FieldValue::new_static("other"));

        assert_eq!(
            config
                .venue(&binance)
                .expect("the fixture settings name every venue this build speaks")
                .proxy(),
            Some(app_config::network::ProxyRoute::named("local")),
            "the default line is not the venue's line"
        );

        // Giving the line up is what puts the venue back on the default line.
        row.access
            .write(&mut config, &FieldValue::new_static(PROXY_INHERIT));

        assert_eq!(row.access.current(&config), PROXY_INHERIT);
        assert_eq!(
            config
                .venue(&binance)
                .expect("the fixture settings name every venue this build speaks")
                .proxy(),
            None,
            "a venue that follows the default line names none of its own"
        );
    }

    /// Every venue this build speaks starts on the default line, and the default line itself
    /// starts as no proxy at all: what a fresh installation stores is a venue with no line of its
    /// own beside a default line that is "do not use a proxy".
    ///
    /// The rows that may name a line of their own are offered that choice with nothing in the
    /// catalog, because following the default line is a line the settings can hold whether or not
    /// an entry has been added: a row that could not be set would be one the user has to fill the
    /// catalog in before touching.
    #[test]
    fn a_fresh_installation_should_follow_a_default_line_that_is_no_proxy() {
        let config = configured();
        let catalog = catalog_of();
        let applications = page(&catalog, "proxy-applications");
        assert!(
            config.proxies.is_empty(),
            "the fixture installation has an empty proxy catalog"
        );

        for venue in supported_venues() {
            let row = field(
                applications,
                &format!("proxy-applications.{}", venue.as_str()),
            );

            assert!(
                matches!(
                    row.access,
                    FieldAccess::ProxyLine {
                        default_line: false,
                        ..
                    }
                ),
                "{} is a row that may follow the default line",
                venue.as_str()
            );
            assert_eq!(
                row.access.current(&config),
                PROXY_INHERIT,
                "{} follows the default line until it is given one",
                venue.as_str()
            );
        }

        for id in [
            "proxy-applications.kytrix.cloud",
            "proxy-applications.kytrix.updates",
            "proxy-applications.x.media",
            "proxy-applications.public-feeds.pizza-index",
        ] {
            let row = field(applications, id);
            assert!(
                matches!(
                    row.access,
                    FieldAccess::ProxyLine {
                        default_line: false,
                        ..
                    }
                ),
                "{id} is a row that may follow the default line"
            );
            assert_eq!(
                row.access.current(&config),
                PROXY_INHERIT,
                "{id} follows the default line while it names none of its own"
            );
        }

        assert_eq!(
            field(applications, "proxy.default-line")
                .access
                .current(&config),
            PROXY_DIRECT,
            "and the default line is no proxy"
        );
    }

    /// One page per venue this build speaks, and the page offers the fields that venue's API has.
    #[test]
    fn one_venue_should_have_one_page() {
        let catalog = catalog_of();

        assert_eq!(
            catalog
                .pages
                .iter()
                .filter(|page| page.id.as_ref().starts_with("market_participants."))
                .count(),
            supported_venues().len(),
            "one page per venue this build speaks"
        );

        for venue in supported_venues() {
            let page = page(&catalog, &format!("market_participants.{}", venue.as_str()));
            assert!(
                !page.sections.is_empty(),
                "{} should offer its own fields",
                venue.as_str()
            );
        }
    }

    /// The display preferences live under one general group, and its entries are the
    /// appearance page and the data time zone page — the appearance page is not a
    /// top-level entry of its own any more.
    #[test]
    fn the_general_group_should_hold_the_display_settings() {
        let catalog = catalog_of();
        let general = catalog
            .tree
            .iter()
            .find(|node| node.id() == "general")
            .expect("the settings should offer a general group");
        let TreeNodeDef::Group { children, .. } = general else {
            panic!("general is a group");
        };

        assert_eq!(
            children.iter().map(TreeNodeDef::id).collect::<Vec<_>>(),
            ["appearance", "time-zone"],
            "the group holds the appearance and the data time zone pages"
        );
        assert!(
            !catalog.tree.iter().any(|node| node.id() == "appearance"),
            "the appearance page is offered under the group rather than at the top"
        );
        assert!(
            catalog.page("time-zone").is_some(),
            "the time zone page is built"
        );
    }

    /// The data time zone offers every offset the setting can store, west to east, and
    /// each choice is drawn as the offset it stores.
    ///
    /// The list is spelled out in this module, so this is what holds it to the type:
    /// an option the setting cannot hold would be a choice that writes nothing, and an
    /// offset missing from the list would be one a user cannot choose.
    #[test]
    fn the_data_time_zone_should_offer_every_offset_it_can_store() {
        let catalog = catalog_of();
        let field = field(page(&catalog, "time-zone"), "time-zone.offset");
        let FieldAccess::Select { options, .. } = &field.access else {
            panic!("the data time zone is a select");
        };

        assert_eq!(options.len(), DataTimeZone::COUNT);

        let mut config = Config::default();
        let mut zones = Vec::with_capacity(options.len());
        for option in options.iter() {
            let hours = option
                .id
                .as_str()
                .parse::<i8>()
                .expect("an offset is stored as a number of hours");
            let zone = DataTimeZone::from_hours(hours)
                .expect("every option is an offset the setting offers");
            let Text::Literal(label) = &option.label else {
                panic!("a UTC offset is drawn as it is");
            };

            assert_eq!(label.as_ref(), zone.label().as_str());
            field.access.write(&mut config, &option.id);
            assert_eq!(config.data_time_zone, zone);
            assert_eq!(field.access.current(&config), option.id);
            zones.push(zone);
        }

        assert_eq!(zones, DataTimeZone::ALL, "the choices are the whole range");
    }

    /// A value the control never offers is not a value this setting can hold: a
    /// write that names one leaves the stored offset exactly as it was.
    #[test]
    fn the_data_time_zone_should_refuse_a_value_it_does_not_offer() {
        let catalog = catalog_of();
        let field = field(page(&catalog, "time-zone"), "time-zone.offset");
        let mut config = Config::default();

        for value in ["15", "-13", "not-a-number", ""] {
            field.access.write(&mut config, &SmolStr::new_static(value));
        }

        assert_eq!(config.data_time_zone, DataTimeZone::UTC);
    }

    /// Binance's page is its two markets, each with the addresses it is reached on and the
    /// credential set it signs with.
    ///
    /// What the rows are drawn *under* is part of the setting: two markets' rows in one
    /// undivided list would be a page where the operator cannot tell which key signs what, so
    /// the section each row belongs to is asserted beside the rows.
    #[test]
    fn binances_page_should_be_its_two_markets() {
        let catalog = catalog_of();
        let binance = page(&catalog, "market_participants.binance");

        assert_eq!(
            section_titles(binance),
            ["SPOT", "USD-M-PERP"],
            "each market is a section of its own, named by the scope it routes"
        );
        assert_eq!(
            field_ids(binance),
            [
                "market_participants.binance.spot.http",
                "market_participants.binance.spot.websocket",
                "market_participants.binance.spot.spot_api_key",
                "market_participants.binance.spot.spot_private_key",
                "market_participants.binance.spot.spot_passphrase",
                "market_participants.binance.usd-m-perp.http",
                "market_participants.binance.usd-m-perp.websocket",
                "market_participants.binance.usd-m-perp.usd_m_perp_api_key",
                "market_participants.binance.usd-m-perp.usd_m_perp_private_key",
                "market_participants.binance.usd-m-perp.usd_m_perp_passphrase",
            ],
            "the page names each market's own addresses and its own credential values"
        );
        assert_eq!(
            section_of(
                binance,
                "market_participants.binance.usd-m-perp.usd_m_perp_api_key"
            ),
            "USD-M-PERP",
            "a perpetual credential row is drawn under the perpetual market"
        );
        assert_eq!(
            section_of(binance, "market_participants.binance.spot.spot_api_key"),
            "SPOT",
            "and a spot credential row under the spot market"
        );

        assert_eq!(
            option_ids(binance, "market_participants.binance.spot.http"),
            [
                "https://api.binance.com",
                "https://api1.binance.com",
                "https://api2.binance.com",
                "https://api3.binance.com",
                "https://api4.binance.com",
            ],
            "the spot market offers the venue's primary host beside its mirrors"
        );
        assert_eq!(
            option_ids(binance, "market_participants.binance.spot.websocket"),
            [
                "wss://stream.binance.com:9443",
                "wss://stream.binance.com:443",
            ],
        );
        assert_eq!(
            option_ids(binance, "market_participants.binance.usd-m-perp.http"),
            ["https://fapi.binance.com"],
            "the USD-M market offers the one host its venue publishes"
        );
        assert_eq!(
            option_ids(binance, "market_participants.binance.usd-m-perp.websocket"),
            ["wss://fstream.binance.com"]
        );
    }

    /// OKX's page is the region its account belongs to beside the one credential set the venue
    /// authenticates as a whole with: no address row exists, because the region is what decides
    /// where the venue is reached.
    #[test]
    fn okxs_page_should_be_a_region_and_one_credential_set() {
        let catalog = catalog_of();
        let okx = page(&catalog, "market_participants.okx");

        assert_eq!(
            field_ids(okx),
            [
                "market_participants.okx.region",
                "market_participants.okx.api_key",
                "market_participants.okx.secret_key",
                "market_participants.okx.passphrase",
            ],
            "the venue configures a region rather than an address"
        );
        assert_eq!(
            option_ids(okx, "market_participants.okx.region"),
            ["global", "us-au", "eu"]
        );
    }

    /// HyperCore's page is one address of each kind beside the API Agent's credentials, and each
    /// of the two address rows offers the one address its venue publishes.
    #[test]
    fn hypercores_page_should_be_one_address_of_each_kind() {
        let catalog = catalog_of();
        let hyperliquid = page(&catalog, "market_participants.hyperliquid");

        assert_eq!(
            field_ids(hyperliquid),
            [
                "market_participants.hyperliquid.http",
                "market_participants.hyperliquid.websocket",
                "market_participants.hyperliquid.main_account_address",
                "market_participants.hyperliquid.api_agent_private_key",
            ]
        );
        assert_eq!(
            option_ids(hyperliquid, "market_participants.hyperliquid.http"),
            ["https://api.hyperliquid.xyz/"]
        );
        assert_eq!(
            option_ids(hyperliquid, "market_participants.hyperliquid.websocket"),
            ["wss://api.hyperliquid.xyz/ws"]
        );
    }

    /// Longbridge's page is the region its account belongs to beside the three values the venue
    /// authenticates with.
    ///
    /// The page is built from the venue's own identity rather than from the served list, so this
    /// is where the rows no served venue reaches today are read: a `wire_name` a credential row
    /// writes under, a region choice the select draws, or an accessor that read another venue's
    /// row is caught here rather than the day the venue is wired. The region is asserted the way
    /// a request would meet it — through the settings' own origins — so the choice a user makes
    /// is the pair the venue is dialled on.
    #[test]
    fn longbridges_page_should_be_a_region_and_one_credential_set() {
        let longbridge = venue("longbridge");
        let page = venue_page(&longbridge);
        let mut config = configured();

        assert_eq!(
            field_ids(&page),
            [
                "market_participants.longbridge.region",
                "market_participants.longbridge.app_key",
                "market_participants.longbridge.app_secret",
                "market_participants.longbridge.access_token",
            ],
            "the venue configures a region rather than an address"
        );

        assert_eq!(
            option_ids(&page, "market_participants.longbridge.region"),
            ["global", "china"],
            "the venue publishes one site per region"
        );
        assert_eq!(
            field(&page, "market_participants.longbridge.region")
                .access
                .current(&config),
            "global",
            "a fresh installation speaks to the international site"
        );

        field(&page, "market_participants.longbridge.region")
            .access
            .write(&mut config, &FieldValue::new_static("china"));

        let settings = config
            .venue(&longbridge)
            .expect("longbridge is configured by the built-in table");
        assert_eq!(
            settings
                .origins("us")
                .map(|origins| origins.http.as_str().to_owned()),
            Some("https://openapi.longbridge.cn/".to_owned()),
            "the region moved the site the venue is reached on"
        );
        assert_eq!(
            settings
                .origins("us")
                .map(|origins| origins.websocket.as_str().to_owned()),
            Some("wss://openapi-quote.longbridge.cn/v2".to_owned()),
            "and its streams moved with it"
        );

        for (id, value) in [
            ("market_participants.longbridge.app_key", "app-key"),
            ("market_participants.longbridge.app_secret", "app-secret"),
            (
                "market_participants.longbridge.access_token",
                "access-token",
            ),
        ] {
            field(&page, id)
                .access
                .write(&mut config, &FieldValue::new_static(value));
            assert_eq!(
                field(&page, id).access.current(&config).as_str(),
                value,
                "{id} reads back the value it wrote"
            );
        }

        let Some(VenueSettings::Longbridge(settings)) = config.venue(&longbridge) else {
            panic!("longbridge is configured as Longbridge");
        };
        assert_eq!(
            settings.credentials,
            LongbridgeCredentials {
                app_key: SmolStr::new_static("app-key"),
                app_secret: SmolStr::new_static("app-secret"),
                access_token: SmolStr::new_static("access-token"),
            },
            "each row landed in the venue's own credential set rather than another venue's"
        );

        for (id, masked) in [
            ("market_participants.longbridge.app_key", false),
            ("market_participants.longbridge.app_secret", true),
            ("market_participants.longbridge.access_token", true),
        ] {
            let FieldAccess::Text { password, .. } = field(&page, id).access else {
                panic!("{id} is a text row");
            };
            assert_eq!(password, masked, "{id}");
        }
    }

    /// An address row writes the choice it is given into the scope it stands for, and a market's
    /// pair is written as a whole: the other role, the other market and the other venue are left
    /// exactly as they were.
    #[test]
    fn an_origin_row_should_be_written_where_it_reads_it() {
        let catalog = catalog_of();
        let binance = page(&catalog, "market_participants.binance");
        let mut config = configured();
        let before = config
            .venue(&venue("binance"))
            .expect("binance is configured")
            .clone();

        field(binance, "market_participants.binance.spot.http")
            .access
            .write(
                &mut config,
                &FieldValue::new_static("https://api3.binance.com"),
            );

        let settings = config
            .venue(&venue("binance"))
            .expect("binance is configured");
        assert_eq!(
            settings
                .origins("spot")
                .map(|origins| origins.http.as_str().to_owned()),
            Some("https://api3.binance.com/".to_owned())
        );
        assert_eq!(
            settings
                .origins("spot")
                .map(|origins| origins.websocket.as_str().to_owned()),
            before
                .origins("spot")
                .map(|origins| origins.websocket.as_str().to_owned()),
            "the market keeps the other address of its own pair"
        );
        assert_eq!(
            settings
                .origins("usd-m-perp")
                .map(|origins| origins.http.as_str().to_owned()),
            before
                .origins("usd-m-perp")
                .map(|origins| origins.http.as_str().to_owned()),
            "another market is not touched"
        );
        assert_eq!(
            field(binance, "market_participants.binance.spot.http")
                .access
                .current(&config)
                .as_str(),
            "https://api3.binance.com"
        );

        // A value no table publishes is refused: the market keeps what it had.
        field(binance, "market_participants.binance.spot.http")
            .access
            .write(&mut config, &FieldValue::new_static("https://spot.example"));
        assert_eq!(
            config
                .venue(&venue("binance"))
                .expect("binance is configured")
                .origins("spot")
                .map(|origins| origins.http.as_str().to_owned()),
            Some("https://api3.binance.com/".to_owned()),
            "an address no documentation publishes is nothing a request could use"
        );

        // And the other venue's rows are still its own.
        field(binance, "market_participants.binance.spot.http")
            .access
            .write(
                &mut config,
                &FieldValue::new_static("https://api.binance.com"),
            );
        assert_eq!(
            config
                .venue(&venue("binance"))
                .expect("binance is configured")
                .origins("spot")
                .map(|origins| origins.http.as_str().to_owned()),
            Some("https://api.binance.com/".to_owned())
        );
    }

    /// The region row moves every scope of the venue, because the venue publishes one API per
    /// region rather than one per API product.
    #[test]
    fn the_region_row_should_move_every_scope_of_its_venue() {
        let catalog = catalog_of();
        let okx = page(&catalog, "market_participants.okx");
        let mut config = configured();

        assert_eq!(
            field(okx, "market_participants.okx.region")
                .access
                .current(&config),
            "global",
            "a fresh installation speaks to the international API"
        );

        field(okx, "market_participants.okx.region")
            .access
            .write(&mut config, &FieldValue::new_static("eu"));

        let settings = config.venue(&venue("okx")).expect("okx is configured");
        for scope in ["spot", "swap"] {
            assert_eq!(
                settings
                    .origins(scope)
                    .map(|origins| origins.http.as_str().to_owned()),
                Some("https://eea.okx.com/".to_owned()),
                "OKX/{scope} moved with the region"
            );
        }

        // A value that is not a region this build speaks writes nothing.
        field(okx, "market_participants.okx.region")
            .access
            .write(&mut config, &FieldValue::new_static("us"));
        assert_eq!(
            field(okx, "market_participants.okx.region")
                .access
                .current(&config),
            "eu"
        );
    }

    /// A credential row writes the value it stands for and nothing else: the venue's other values
    /// and every other venue are left exactly as they were.
    #[test]
    fn a_credential_row_should_be_written_where_it_reads_it() {
        let catalog = catalog_of();
        let binance = page(&catalog, "market_participants.binance");
        let okx = page(&catalog, "market_participants.okx");
        let mut config = configured();

        field(binance, "market_participants.binance.spot.spot_api_key")
            .access
            .write(&mut config, &FieldValue::new_static("spot-key"));
        field(
            binance,
            "market_participants.binance.usd-m-perp.usd_m_perp_api_key",
        )
        .access
        .write(&mut config, &FieldValue::new_static("perp-key"));

        assert_eq!(
            field(binance, "market_participants.binance.spot.spot_api_key")
                .access
                .current(&config)
                .as_str(),
            "spot-key",
            "the field reads the value it wrote"
        );
        assert_eq!(
            field(
                binance,
                "market_participants.binance.usd-m-perp.usd_m_perp_api_key"
            )
            .access
            .current(&config)
            .as_str(),
            "perp-key",
            "each market holds its own value"
        );
        assert_eq!(
            field(
                binance,
                "market_participants.binance.usd-m-perp.usd_m_perp_private_key"
            )
            .access
            .current(&config)
            .as_str(),
            "",
            "and writing one value of a market does not fill in its others"
        );

        // A row of another venue writes into its own row, not into Binance's.
        field(okx, "market_participants.okx.api_key")
            .access
            .write(&mut config, &FieldValue::new_static("okx-key"));
        assert_eq!(
            field(binance, "market_participants.binance.spot.spot_api_key")
                .access
                .current(&config)
                .as_str(),
            "spot-key",
            "the Binance value is untouched by an OKX row"
        );
        assert_eq!(
            field(okx, "market_participants.okx.api_key")
                .access
                .current(&config)
                .as_str(),
            "okx-key"
        );
    }

    /// A credential row is drawn masked unless the value is an API key, which is the one value of
    /// a credential set that is an identity rather than a secret.
    #[test]
    fn a_credential_row_should_be_drawn_masked_unless_it_is_a_key() {
        let catalog = catalog_of();

        for (page_id, field_id, password) in [
            (
                "market_participants.binance",
                "market_participants.binance.spot.spot_api_key",
                false,
            ),
            (
                "market_participants.binance",
                "market_participants.binance.spot.spot_private_key",
                true,
            ),
            (
                "market_participants.binance",
                "market_participants.binance.spot.spot_passphrase",
                true,
            ),
            (
                "market_participants.okx",
                "market_participants.okx.api_key",
                false,
            ),
            (
                "market_participants.okx",
                "market_participants.okx.secret_key",
                true,
            ),
            (
                "market_participants.okx",
                "market_participants.okx.passphrase",
                true,
            ),
            (
                "market_participants.hyperliquid",
                "market_participants.hyperliquid.main_account_address",
                false,
            ),
            (
                "market_participants.hyperliquid",
                "market_participants.hyperliquid.api_agent_private_key",
                true,
            ),
            (
                "market_participants.longbridge",
                "market_participants.longbridge.app_key",
                false,
            ),
            (
                "market_participants.longbridge",
                "market_participants.longbridge.app_secret",
                true,
            ),
            (
                "market_participants.longbridge",
                "market_participants.longbridge.access_token",
                true,
            ),
        ] {
            let page = page(&catalog, page_id);
            let FieldAccess::Text {
                password: masked, ..
            } = field(page, field_id).access
            else {
                panic!("{field_id} is a text row");
            };
            assert_eq!(masked, password, "{field_id}");
        }
    }
}
