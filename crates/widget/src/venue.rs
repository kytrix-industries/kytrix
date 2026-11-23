use i18n::{t_smol, translate::Translate};
use market_participants_model::{
    display::lexical::scope_display_code, identifier::VenueId, listing::route::VenueRoute,
};
use smol_str::{SmolStr, format_smolstr};

pub trait VenueDisplay {
    fn name_key(&self) -> &'static str;

    fn asset_path(&self) -> &'static str;

    fn display_string(&self, translate: &Translate) -> SmolStr;
}

impl VenueDisplay for VenueId {
    fn name_key(&self) -> &'static str {
        match self {
            Self::Binance => "venue.binance",
            Self::Hyperliquid => "venue.hyperliquid",
            Self::Longbridge => "venue.longbridge",
            Self::Okx => "venue.okx",
            Self::Polymarket => "venue.polymarket",
        }
    }

    fn asset_path(&self) -> &'static str {
        match self {
            Self::Binance => ui_kit::kit::brand::BINANCE,
            Self::Hyperliquid => ui_kit::kit::brand::HYPERLIQUID,
            Self::Longbridge => ui_kit::kit::brand::LONGBRIDGE,
            Self::Okx => ui_kit::kit::brand::OKX,
            Self::Polymarket => ui_kit::kit::brand::POLYMARKET,
        }
    }

    fn display_string(&self, translate: &Translate) -> SmolStr {
        t_smol!(translate, self.name_key())
    }
}

pub fn route_display_string(route: &VenueRoute, translate: &Translate) -> SmolStr {
    format_smolstr!(
        "{}/{}",
        route.venue.display_string(translate),
        scope_display_code(&route.scope)
    )
}

pub fn joined_names(translate: &Translate, keys: &[&'static str]) -> SmolStr {
    let joined = keys
        .iter()
        .map(|key| t_smol!(translate, *key))
        .collect::<Vec<_>>()
        .join(", ");
    SmolStr::new(joined)
}

fn listed_names(translate: &Translate, keys: &[&'static str]) -> SmolStr {
    let listed = keys
        .iter()
        .map(|key| {
            let venue = venue_of_key(key);
            format_smolstr!(
                "![]({SCHEME}{}?size={LOGO_SIZE}) **{}**",
                venue.asset_path(),
                t_smol!(translate, *key)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    SmolStr::new(listed)
}

const SCHEME: &str = "file://";

const LOGO_SIZE: u16 = 16;

fn venue_of_key(key: &str) -> VenueId {
    VenueId::ALL
        .iter()
        .copied()
        .find(|venue| venue.name_key() == key)
        .unwrap_or_else(|| panic!("`{key}` is not the name key of a venue this build speaks"))
}

pub(crate) fn market_not_loaded_sentence(translate: &Translate, supported: &[VenueId]) -> SmolStr {
    let keys = supported
        .iter()
        .map(VenueDisplay::name_key)
        .collect::<Vec<_>>();
    t_smol!(
        translate,
        "common.market_not_loaded",
        markets = listed_names(translate, &keys)
    )
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use i18n::{language::codes::LanguageCode, translate::Translate};
    use market_participants_model::{
        identifier::{VenueId, VenueScopeId},
        listing::route::VenueRoute,
    };

    use super::{
        LOGO_SIZE, SCHEME, VenueDisplay, joined_names, listed_names, route_display_string,
        venue_of_key,
    };
    use crate::status::Notice;

    fn translate(language: LanguageCode) -> Translate {
        Translate::new(
            Rc::new(i18n::embedded::load().expect("the embedded locales load")),
            language,
        )
    }

    #[test]
    fn a_venue_is_named_by_the_catalogue() {
        for language in [LanguageCode::En, LanguageCode::ZhCn] {
            let translate = translate(language);
            for venue in VenueId::ALL {
                assert!(
                    !venue.display_string(&translate).is_empty(),
                    "{venue} has no name in {language}"
                );
            }
        }
    }

    #[test]
    fn a_venue_name_follows_the_language() {
        let en = translate(LanguageCode::En);
        assert_eq!(VenueId::Binance.display_string(&en), "Binance");
        assert_eq!(VenueId::Okx.display_string(&en), "OKX");
        assert_eq!(VenueId::Longbridge.display_string(&en), "Longbridge");

        let zh = translate(LanguageCode::ZhCn);
        assert_eq!(VenueId::Binance.display_string(&zh), "币安");
        assert_eq!(VenueId::Okx.display_string(&zh), "欧易");
        assert_eq!(VenueId::Longbridge.display_string(&zh), "长桥");
    }

    #[test]
    fn a_route_names_its_venue_and_its_scope_apart() {
        let route = VenueRoute::new(
            VenueId::Binance,
            VenueScopeId::new("usd-m-perp").expect("the fixture scope is canonical"),
        );

        assert_eq!(
            route_display_string(&route, &translate(LanguageCode::En)),
            "Binance/USD-M-PERP"
        );
        assert_eq!(
            route_display_string(&route, &translate(LanguageCode::ZhCn)),
            "币安/USD-M-PERP"
        );
    }

    #[test]
    fn joined_names_should_read_every_key_it_was_given() {
        assert_eq!(
            joined_names(
                &translate(LanguageCode::En),
                &["venue.binance", "venue.okx"]
            ),
            "Binance, OKX"
        );
        assert_eq!(
            joined_names(
                &translate(LanguageCode::ZhCn),
                &["venue.binance", "venue.okx"]
            ),
            "币安, 欧易"
        );
    }

    #[test]
    fn listed_names_should_put_every_key_on_a_line_of_its_own() {
        assert_eq!(
            listed_names(
                &translate(LanguageCode::En),
                &["venue.binance", "venue.okx", "venue.hyperliquid"]
            ),
            "![](file://brands/Binance/binance.png?size=16) **Binance**\n\
             ![](file://brands/OKX/okx.png?size=16) **OKX**\n\
             ![](file://brands/Hyperliquid/hyperliquid.png?size=16) **Hyperliquid**"
        );
        assert_eq!(
            listed_names(
                &translate(LanguageCode::ZhCn),
                &["venue.binance", "venue.okx", "venue.hyperliquid"]
            ),
            "![](file://brands/Binance/binance.png?size=16) **币安**\n\
             ![](file://brands/OKX/okx.png?size=16) **欧易**\n\
             ![](file://brands/Hyperliquid/hyperliquid.png?size=16) **Hyperliquid**"
        );
    }

    #[test]
    fn every_venue_should_state_the_logo_its_brand_draws() {
        for (venue, path) in [
            (VenueId::Binance, ui_kit::kit::brand::BINANCE),
            (VenueId::Okx, ui_kit::kit::brand::OKX),
            (VenueId::Hyperliquid, ui_kit::kit::brand::HYPERLIQUID),
            (VenueId::Longbridge, ui_kit::kit::brand::LONGBRIDGE),
        ] {
            assert_eq!(venue.asset_path(), path, "{venue} states its own logo");
            assert_eq!(
                venue_of_key(venue.name_key()),
                venue,
                "{venue} is the venue its own name key names"
            );
        }
    }

    #[test]
    fn a_market_notice_should_list_its_market_participants_in_both_languages() {
        for (language, names, logo) in [
            (
                LanguageCode::En,
                ["Binance", "OKX"],
                ui_kit::kit::brand::BINANCE,
            ),
            (
                LanguageCode::ZhCn,
                ["币安", "欧易"],
                ui_kit::kit::brand::BINANCE,
            ),
        ] {
            let notice = Notice::MarketNotLoaded {
                venues: vec![VenueId::Binance],
                supported: vec![VenueId::Binance, VenueId::Okx],
            };
            let translate = translate(language);
            assert!(
                notice.is_markdown(&translate),
                "the placeholder must draw this notice through the markdown component"
            );
            let notice = notice.resolve(&translate);

            assert!(
                notice.contains(&format!(
                    "![]({SCHEME}{logo}?size={LOGO_SIZE}) **{}**",
                    names[0]
                )) && notice.contains(&format!("**{}**", names[1]))
                    && notice.ends_with("</p>"),
                "{language} lists the market participants one per line, each its logo \
                 and its bold name: {notice:?}"
            );
            assert!(
                notice.contains("<kbd>![](file://icons/phosphoricons/bank.svg)</kbd>"),
                "{language} points at the icon-only market control: {notice}"
            );
            assert!(
                !notice.contains("venue.") && !notice.contains("%{"),
                "{language} leaves a key or a placeholder unresolved: {notice}"
            );
        }
    }

    #[test]
    fn every_venue_a_notice_may_list_should_render_in_both_languages() {
        let keys = VenueId::ALL
            .iter()
            .map(VenueDisplay::name_key)
            .collect::<Vec<_>>();

        for language in [LanguageCode::En, LanguageCode::ZhCn] {
            let listed = listed_names(&translate(language), &keys);
            for line in listed.lines() {
                assert!(
                    !line.trim().is_empty(),
                    "{language} renders a venue of this build as an empty line: {listed:?}"
                );
            }
        }
    }
}
