use app_config::{
    network::{KytrixServiceCapability, ServiceTarget},
    settings::Config,
};
use i18n::translate::Translate;
use market_participants_connector::venue::definition::supported_venues;
use market_participants_model::identifier::VenueId;
use smol_str::SmolStr;
use ui_core::{app::App, error::EngineError, event::Event, node::id::NodeId, window::WindowId};
use ui_kit::{
    kit::select::{Select, SelectItem},
    theme::handle::ThemeHandle,
};
use widget::venue::VenueDisplay;

use crate::{
    host::SettingsHost,
    layout,
    pages::{
        page::{PageBuild, PageView},
        proxy::route::{
            PROXY_DIRECT, PROXY_INHERIT, PROXY_MIXED, PROXY_SYSTEM, proxy_default_of,
            proxy_route_of_group, proxy_route_of_venue, set_proxy_default,
            set_proxy_route_of_group, set_proxy_route_of_venue,
        },
    },
    search::{FieldSearch, PageSearch, words},
    session::SettingsChange,
};

const PAGE: &str = "configure";

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

const APP_UPDATES: &[ServiceTarget] = &[
    ServiceTarget::KytrixService {
        capability: KytrixServiceCapability::UpdaterMetadata,
    },
    ServiceTarget::KytrixService {
        capability: KytrixServiceCapability::UpdaterDownload,
    },
];

const AI_MODELS: &[ServiceTarget] = &[ServiceTarget::TranslationModelDownload];

const X_MEDIA: &[ServiceTarget] = &[ServiceTarget::KytrixService {
    capability: KytrixServiceCapability::XMedia,
}];

const KYTRIX_GROUPS: [(&str, &str, &str, &[ServiceTarget]); 4] = [
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
];

const fn venue_field_id(venue: VenueId) -> &'static str {
    match venue {
        VenueId::Binance => "configure.binance",
        VenueId::Hyperliquid => "configure.hyperliquid",
        VenueId::Longbridge => "configure.longbridge",
        VenueId::Okx => "configure.okx",
        VenueId::Polymarket => "configure.polymarket",
    }
}

const fn venue_description_key(venue: VenueId) -> &'static str {
    match venue {
        VenueId::Binance => "settings_ui.proxies.binance_description",
        VenueId::Hyperliquid => "settings_ui.proxies.hyperliquid_description",
        VenueId::Longbridge => "settings_ui.proxies.longbridge_description",
        VenueId::Okx => "settings_ui.proxies.okx_description",
        VenueId::Polymarket => "settings_ui.proxies.polymarket_description",
    }
}

#[derive(Clone, Copy)]
enum LineKind {
    Default,
    Venue(VenueId),
    Group(&'static [ServiceTarget]),
}

impl LineKind {
    fn default_line(self) -> bool {
        matches!(self, Self::Default)
    }

    fn value(self, config: &Config) -> SmolStr {
        match self {
            Self::Default => proxy_default_of(config),
            Self::Venue(venue) => proxy_route_of_venue(config, &venue),
            Self::Group(targets) => proxy_route_of_group(config, targets.iter().cloned()),
        }
    }

    fn apply(self, config: &mut Config, value: &str) {
        match self {
            Self::Default => set_proxy_default(config, value),
            Self::Venue(venue) => set_proxy_route_of_venue(config, &venue, value),
            Self::Group(targets) => {
                set_proxy_route_of_group(config, targets.iter().cloned(), value);
            }
        }
    }

    fn choices(self, config: &Config, selected: &str, translate: &Translate) -> Vec<SelectItem> {
        let mut choices = Vec::with_capacity(config.proxies.len() + 4);

        if !self.default_line() {
            choices.push(SelectItem::new(
                SmolStr::new_static(PROXY_INHERIT),
                i18n::t_smol!(translate, "settings_ui.proxies.inherit"),
            ));
        }
        choices.push(SelectItem::new(
            SmolStr::new_static(PROXY_DIRECT),
            i18n::t_smol!(translate, "settings_ui.proxies.no_proxy"),
        ));
        choices.push(SelectItem::new(
            SmolStr::new_static(PROXY_SYSTEM),
            i18n::t_smol!(translate, "settings_ui.proxies.system_proxy"),
        ));

        if selected == PROXY_MIXED {
            choices.push(SelectItem::new(
                SmolStr::new_static(PROXY_MIXED),
                i18n::t_smol!(translate, "settings_ui.proxies.mixed"),
            ));
        } else {
            let held_by_the_catalog = config.proxies.contains_key(selected);
            let built_in = selected == PROXY_DIRECT
                || selected == PROXY_SYSTEM
                || (!self.default_line() && selected == PROXY_INHERIT);
            if !held_by_the_catalog && !built_in {
                let name = SmolStr::new(selected);
                choices.push(SelectItem::new(name.clone(), name));
            }
        }

        for name in config.proxies.keys() {
            choices.push(SelectItem::new(name.clone(), name.clone()));
        }

        choices
    }
}

struct ProxyLine {
    kind: LineKind,
    select: Select,
    items: Vec<SelectItem>,
}

pub(crate) struct ConfigurePage {
    host: SettingsHost,
    rows: Vec<ProxyLine>,
}

struct RowSpec {
    id: &'static str,
    kind: LineKind,
    label: SmolStr,
    description: SmolStr,
}

impl ConfigurePage {
    pub(crate) fn new(host: SettingsHost, _key: &str) -> Self {
        Self {
            host,
            rows: Vec::new(),
        }
    }

    fn row(
        &mut self,
        app: &mut App,
        window: WindowId,
        theme: &ThemeHandle,
        spec: RowSpec,
        config: &Config,
        translate: &Translate,
        cx: &PageBuild<'_>,
    ) -> layout::RowFn {
        let selected = spec.kind.value(config);
        let items = spec.kind.choices(config, selected.as_str(), translate);

        let host = self.host.clone();
        let select = layout::choice_select(
            app,
            &self.host,
            cx.unique_key(spec.id),
            selected.as_str(),
            items
                .iter()
                .map(|item| (item.id.clone(), item.label.clone())),
            spec.label.clone(),
            move |app, value| {
                let value = SmolStr::new(value);
                let kind = spec.kind;
                let _ = host.write(
                    app,
                    window,
                    SettingsChange::edit(move |config| kind.apply(config, value.as_str())),
                );
            },
        );

        let control = select.clone();
        let row = layout::row(
            theme,
            spec.id,
            spec.label,
            spec.description,
            move |app, window, parent| {
                control.mount(app, window, parent)?;
                Ok(())
            },
        );
        self.rows.push(ProxyLine {
            kind: spec.kind,
            select,
            items,
        });

        row
    }
}

impl PageView for ConfigurePage {
    fn page_id(&self) -> &'static str {
        PAGE
    }

    fn build(
        &mut self,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
        cx: &PageBuild<'_>,
    ) -> Result<(), EngineError> {
        let theme = self.host.theme_handle();
        let translate = self.host.translate().clone();
        let config = self.host.config(app, |config| config.clone());

        let mut default_rows = Vec::new();
        if cx.filter.admits_field(PAGE, "proxy.default-line") {
            default_rows.push(self.row(
                app,
                window,
                &theme,
                RowSpec {
                    id: "proxy.default-line",
                    kind: LineKind::Default,
                    label: i18n::t_smol!(translate, "settings_ui.proxies.default_proxy"),
                    description: SmolStr::default(),
                },
                &config,
                &translate,
                cx,
            ));
        }

        let mut venue_rows = Vec::new();
        for venue in supported_venues() {
            let field_id = venue_field_id(venue);
            if !cx.filter.admits_field(PAGE, field_id) {
                continue;
            }
            venue_rows.push(self.row(
                app,
                window,
                &theme,
                RowSpec {
                    id: field_id,
                    kind: LineKind::Venue(venue),
                    label: i18n::t_smol!(translate, venue.name_key()),
                    description: i18n::t_smol!(translate, venue_description_key(venue)),
                },
                &config,
                &translate,
                cx,
            ));
        }

        let mut kytrix_rows = Vec::new();
        for (field_id, label_key, description_key, targets) in KYTRIX_GROUPS {
            if !cx.filter.admits_field(PAGE, field_id) {
                continue;
            }
            kytrix_rows.push(self.row(
                app,
                window,
                &theme,
                RowSpec {
                    id: field_id,
                    kind: LineKind::Group(targets),
                    label: i18n::t_smol!(translate, label_key),
                    description: i18n::t_smol!(translate, description_key),
                },
                &config,
                &translate,
                cx,
            ));
        }

        layout::section(
            app,
            window,
            parent,
            &theme,
            "configure_0",
            None,
            default_rows,
        )?;
        layout::section(
            app,
            window,
            parent,
            &theme,
            "configure_1",
            Some(i18n::t_smol!(
                translate,
                "settings_ui.proxies.market_participants_section"
            )),
            venue_rows,
        )?;
        layout::section(
            app,
            window,
            parent,
            &theme,
            "configure_2",
            Some(i18n::t_smol!(
                translate,
                "settings_ui.proxies.kytrix_section"
            )),
            kytrix_rows,
        )?;

        Ok(())
    }

    fn sync(&mut self, app: &mut App) -> Result<(), EngineError> {
        let translate = self.host.translate().clone();
        let rows = &self.rows;
        let desired = self.host.config(app, |config| {
            rows.iter()
                .map(|row| {
                    let value = row.kind.value(config);
                    let items = row.kind.choices(config, value.as_str(), &translate);
                    (value, items)
                })
                .collect::<Vec<_>>()
        });

        for (row, (value, items)) in self.rows.iter_mut().zip(desired) {
            let items_changed = row.items != items;
            if items_changed {
                row.select.set_items(app, items.clone())?;
                row.items = items;
            }
            let chosen = row.select.chosen(app);
            let matches = chosen.len() == 1 && chosen[0] == value;
            if items_changed || !matches {
                row.select.set_chosen(app, [value.as_str()], false);
            }
        }

        Ok(())
    }

    fn dispatch(&mut self, app: &mut App, event: &Event) -> bool {
        let mut consumed = false;
        for row in &self.rows {
            consumed |= row.select.dispatch(app, event);
        }
        consumed
    }

    fn release(&mut self, app: &mut App) {
        for row in self.rows.drain(..) {
            row.select.release(app);
        }
    }
}

pub(crate) fn search(translate: &Translate) -> PageSearch {
    let title = i18n::t_smol!(translate, "settings_ui.proxies.configure");
    let default_label = i18n::t_smol!(translate, "settings_ui.proxies.default_proxy");
    let kytrix_cloud = i18n::t_smol!(translate, "settings_ui.proxies.kytrix_cloud");
    let kytrix_cloud_description =
        i18n::t_smol!(translate, "settings_ui.proxies.kytrix_cloud_description");
    let app_updates = i18n::t_smol!(translate, "settings_ui.proxies.app_updates");
    let app_updates_description =
        i18n::t_smol!(translate, "settings_ui.proxies.app_updates_description");
    let ai_models = i18n::t_smol!(translate, "settings_ui.proxies.ai_model_downloads");
    let ai_models_description = i18n::t_smol!(
        translate,
        "settings_ui.proxies.ai_model_downloads_description"
    );
    let x_media = i18n::t_smol!(translate, "settings_ui.proxies.x_post_media");
    let x_media_description =
        i18n::t_smol!(translate, "settings_ui.proxies.x_post_media_description");

    let mut fields = Vec::with_capacity(supported_venues().len() + 5);
    fields.push(FieldSearch {
        id: SmolStr::new_static("proxy.default-line"),
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
        let label = i18n::t_smol!(translate, venue.name_key());
        let description = i18n::t_smol!(translate, venue_description_key(venue));
        fields.push(FieldSearch {
            id: SmolStr::new_static(venue_field_id(venue)),
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
        id: SmolStr::new_static("configure.kytrix.cloud"),
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
        id: SmolStr::new_static("configure.kytrix.updates"),
        text: words(&[
            app_updates.as_ref(),
            app_updates_description.as_ref(),
            "updater",
            "update",
            "更新",
        ]),
    });
    fields.push(FieldSearch {
        id: SmolStr::new_static("configure.ai.models"),
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
        id: SmolStr::new_static("configure.x.media"),
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
