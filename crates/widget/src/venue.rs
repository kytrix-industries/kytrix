//! How a venue is named in the interface.
//!
//! This lives beside `LocalizedError`, which is the workspace's other spelling of
//! "this domain value, in the user's language": a venue's name is a fact about the
//! venue that only the interface can state, so the layer that owns the interface's
//! text owns the name, and the primitive layer below it stays free of business types.
//!
//! A venue is a closed identity of this build, and the name a reader sees is a
//! message of the catalogue rather than a spelling of that identity: the
//! canonical text — `binance` — is what identity digests, catalog source ids,
//! persisted keys and route revisions are built from, and it is never what a
//! reader sees. Every surface that states a venue therefore reads its name here,
//! in the language the window is showing, instead of upper-casing the identity
//! or carrying a spelling of its own.
//!
//! A scope is not named the same way. It is a protocol namespace rather than a
//! brand — `spot`, `usd-m-perp`, and the `perp-dex/<name>` scopes a venue
//! deploys at runtime — so it has one form in every language, and a route states
//! its venue in the reader's language and its scope in the protocol's.

use gpui_kit::{App, SharedString};
use i18n::t;
use market_participants_model::{
    display::lexical::scope_display_code, identifier::VenueId, listing::route::VenueRoute,
};
use smol_str::format_smolstr;

/// A value that names a venue in the language the interface is showing.
pub trait VenueDisplay {
    /// The catalogue key this venue's name is read from.
    ///
    /// A caller that states the name in a sentence of its own — the caption that names
    /// the markets the force-order table is reading, say — passes this key to
    /// [`joined_names`], or resolves it with `t!`, rather than spelling the name out, so
    /// the name is read in whatever language is showing when the sentence is reported.
    fn name_key(&self) -> &'static str;

    /// The asset path of this venue's mark, relative to `crates/base_ui/assets`.
    ///
    /// A venue's logo is a fact about the venue rather than about the language it is read
    /// in, so it is stated here beside the name and read by the one sentence that draws it
    /// (through [`listed_names`]). The paths themselves live in `base_ui::brand`, which is
    /// what [`base_ui::brand::Brand`] draws from: a logo has one path, however it is used.
    fn asset_path(&self) -> &'static str;

    /// The name, in the language the interface is showing.
    fn display_string(&self, cx: &App) -> SharedString;
}

impl VenueDisplay for VenueId {
    fn name_key(&self) -> &'static str {
        match self {
            Self::Binance => "venue.binance",
            Self::Hyperliquid => "venue.hyperliquid",
            Self::Longbridge => "venue.longbridge",
            Self::Okx => "venue.okx",
        }
    }

    fn asset_path(&self) -> &'static str {
        match self {
            Self::Binance => base_ui::brand::BINANCE,
            Self::Hyperliquid => base_ui::brand::HYPERLIQUID,
            Self::Longbridge => base_ui::brand::LONGBRIDGE,
            Self::Okx => base_ui::brand::OKX,
        }
    }

    fn display_string(&self, cx: &App) -> SharedString {
        t!(cx, self.name_key())
    }
}

/// Renders a route the way a reader reads it: the venue's name in the reader's
/// language, and the scope in the protocol's own upper case.
///
/// The canonical lower-case `VenueRoute::Display` — `binance/spot` — is not
/// changed by this and stays with machine identity, logs and explicitly marked
/// diagnostics.
pub fn route_display_string(route: &VenueRoute, cx: &App) -> SharedString {
    SharedString::new(format_smolstr!(
        "{}/{}",
        route.venue.display_string(cx),
        scope_display_code(&route.scope)
    ))
}

/// Joins catalogue keys into the one value a sentence interpolates.
///
/// The separator is punctuation this module states rather than a phrase any locale
/// translates, so a sentence that carries a list of names binds this one value instead of
/// spelling the names into its own text. A caller that states several names in a sentence
/// of its own — the force-order table's caption is one — joins them the same way.
///
/// The names are read here, in the language showing now: the caller hands the result to a
/// message it is resolving in that same language, so the two agree.
pub fn joined_names(cx: &App, keys: &[&'static str]) -> SharedString {
    let joined = keys
        .iter()
        .map(|key| t!(cx, *key))
        .collect::<Vec<_>>()
        .join(", ");
    SharedString::from(joined)
}

/// Lists catalogue keys one per line, each as its venue's logo and its name in bold.
///
/// This is [`joined_names`] for the one sentence that states its names as a list rather
/// than inside its own prose: `common.market_not_loaded` ends in a colon and leaves the
/// rest of the paragraph to the venues, so what is bound is a block that reads as one
/// market participant per line — the newlines are line breaks to the markdown component, which is
/// what makes the block centred and counted rather than wrapped into a sentence.
///
/// A line is markdown rather than a name: each one writes the venue's own logo as an
/// inline image at [`LOGO_SIZE`] and sets the name in bold, so a reader scanning the list
/// recognises the market participant before reading it. The mark is written before the name and
/// inside the same line, which is what keeps the two together when the block wraps. The
/// logo is drawn from the catalogue's mark of the venue and the name from the catalogue's
/// name of it, both read here, in the language showing now.
fn listed_names(cx: &App, keys: &[&'static str]) -> SharedString {
    let listed = keys
        .iter()
        .map(|key| {
            let venue = venue_of_key(key);
            format_smolstr!(
                "![]({SCHEME}{}?size={LOGO_SIZE}) **{}**",
                venue.asset_path(),
                t!(cx, *key)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    SharedString::from(listed)
}

/// The markdown scheme an inline image of a venue's logo is written with.
///
/// It is the `file:` URI the markdown component reads its own assets through, and the one
/// [`LOGO_SIZE`] is written beside; the path between them is the venue's.
const SCHEME: &str = "file://";

/// The edge of a listed venue's logo, in pixels.
///
/// One number rather than a pair: every mark in `crates/base_ui/assets/brands` is square,
/// and the inline image takes a single number as both edges. It is written into the
/// sentence rather than derived from the asset, because the assets differ in size — OKX's
/// is 200×200 and Binance's 128×128 — and a list wants all of them the same height.
const LOGO_SIZE: u16 = 16;

/// The venue a catalogue key names.
///
/// The keys this module lists are [`VenueDisplay::name_key`]s, so the mapping back to the
/// venue is stated once, here, beside the trait that produced them.
fn venue_of_key(key: &str) -> VenueId {
    VenueId::ALL
        .iter()
        .copied()
        .find(|venue| venue.name_key() == key)
        .unwrap_or_else(|| panic!("`{key}` is not the name key of a venue this build speaks"))
}

/// The control the catalogue's "load the market first" sentence points at.
///
/// The sentence and the control it names are one fact of the product —
/// `common.market_not_loaded` spells that control's icon and its `%{control}` — so
/// the correspondence lives here rather than at each of the call sites that report
/// the notice.
const MARKET_CONTROL: &str = "window.market_participants.title";

/// Renders the whole sentence that asks the user to load the metadata of a market first.
///
/// The sentence states what has to happen and where the control that does it is; the venues
/// it ends with are the ones the reporting widget can read at all, so a reader who loads any
/// of them gets a pane that works. Both arguments are supplied here, in the language showing
/// now, because both of them are punctuation of this module's own kind: the venues through
/// [`listed_names`] — each line its logo and its name — and the control through the key it is
/// itself labelled with, so the control a reader is pointed at reads as the control they can
/// see.
///
/// `supported` is the widget's own answer rather than anything derived here: which markets a
/// widget may read is a fact about that widget's capability, and this layer is below the one
/// that knows capabilities. The markets the widget waits for are deliberately not read: the
/// sentence is the same whichever of them is missing, and the report's identity is the
/// widget's business rather than the sentence's.
pub(crate) fn market_not_loaded_sentence(cx: &App, supported: &[VenueId]) -> SharedString {
    let keys = supported
        .iter()
        .map(VenueDisplay::name_key)
        .collect::<Vec<_>>();
    t!(
        cx,
        "common.market_not_loaded",
        markets = listed_names(cx, &keys),
        control = t!(cx, MARKET_CONTROL)
    )
}

#[cfg(test)]
mod tests {
    use gpui_kit::SharedString;
    use i18n::language::codes::LanguageCode;
    use market_participants_model::{
        identifier::{VenueId, VenueScopeId},
        listing::route::VenueRoute,
    };

    use super::{
        LOGO_SIZE, SCHEME, VenueDisplay, joined_names, listed_names, route_display_string,
        venue_of_key,
    };
    use crate::status::Notice;

    /// Every venue of this build is named by the catalogue in every language it
    /// ships, so a name is a translated word rather than the identity spelled
    /// out.
    #[gpui_kit::test]
    fn a_venue_is_named_by_the_catalogue(cx: &mut gpui_kit::TestAppContext) {
        cx.update(|cx| {
            i18n::global::embedded::install(cx).expect("the embedded locales should load");

            for language in [LanguageCode::En, LanguageCode::ZhCn] {
                cx.set_global(language);
                for venue in VenueId::ALL {
                    assert!(
                        !venue.display_string(cx).is_empty(),
                        "{venue} has no name in {language}"
                    );
                }
            }
        });
    }

    /// The two languages really name one venue differently, which is the whole
    /// reason the name is not derived from the identity.
    #[gpui_kit::test]
    fn a_venue_name_follows_the_language(cx: &mut gpui_kit::TestAppContext) {
        cx.update(|cx| {
            i18n::global::embedded::install(cx).expect("the embedded locales should load");

            cx.set_global(LanguageCode::En);
            assert_eq!(VenueId::Binance.display_string(cx).as_ref(), "Binance");
            assert_eq!(VenueId::Okx.display_string(cx).as_ref(), "OKX");
            assert_eq!(
                VenueId::Longbridge.display_string(cx).as_ref(),
                "Longbridge"
            );

            cx.set_global(LanguageCode::ZhCn);
            assert_eq!(VenueId::Binance.display_string(cx).as_ref(), "币安");
            assert_eq!(VenueId::Okx.display_string(cx).as_ref(), "欧易");
            assert_eq!(VenueId::Longbridge.display_string(cx).as_ref(), "长桥");
        });
    }

    /// A route states its venue in the reader's language and its scope in the
    /// protocol's: the scope names a namespace, not a brand.
    #[gpui_kit::test]
    fn a_route_names_its_venue_and_its_scope_apart(cx: &mut gpui_kit::TestAppContext) {
        cx.update(|cx| {
            i18n::global::embedded::install(cx).expect("the embedded locales should load");
            let route = VenueRoute::new(
                VenueId::Binance,
                VenueScopeId::new("usd-m-perp").expect("the fixture scope is canonical"),
            );

            cx.set_global(LanguageCode::En);
            assert_eq!(
                route_display_string(&route, cx),
                SharedString::new_static("Binance/USD-M-PERP")
            );

            cx.set_global(LanguageCode::ZhCn);
            assert_eq!(
                route_display_string(&route, cx),
                SharedString::new_static("币安/USD-M-PERP")
            );
        });
    }

    /// The value a sentence interpolates is the names joined by `, `, read in the language
    /// showing when it is asked for.
    #[gpui_kit::test]
    fn joined_names_should_read_every_key_it_was_given(cx: &mut gpui_kit::TestAppContext) {
        cx.update(|cx| {
            i18n::global::embedded::install(cx).expect("the embedded locales should load");

            cx.set_global(LanguageCode::En);
            assert_eq!(
                joined_names(cx, &["venue.binance", "venue.okx"]).as_ref(),
                "Binance, OKX"
            );

            cx.set_global(LanguageCode::ZhCn);
            assert_eq!(
                joined_names(cx, &["venue.binance", "venue.okx"]).as_ref(),
                "币安, 欧易"
            );
        });
    }

    /// The list the "load a market's metadata first" sentence ends with is one market participant per
    /// line — the separators are line breaks rather than the `, ` [`joined_names`] states,
    /// because that sentence states its names as a list instead of inside its own prose.
    ///
    /// A line is not a bare name: it is the venue's logo drawn at [`LOGO_SIZE`] followed by
    /// its name in bold, which is what a reader of a list of market participants is looking for. The
    /// logo comes from the venue's own asset path and the name from the language showing when
    /// they are asked for, like every other value this module supplies.
    #[gpui_kit::test]
    fn listed_names_should_put_every_key_on_a_line_of_its_own(cx: &mut gpui_kit::TestAppContext) {
        cx.update(|cx| {
            i18n::global::embedded::install(cx).expect("the embedded locales should load");

            cx.set_global(LanguageCode::En);
            assert_eq!(
                listed_names(cx, &["venue.binance", "venue.okx", "venue.hyperliquid"]).as_ref(),
                "![](file://brands/Binance/binance.png?size=16) **Binance**\n\
                 ![](file://brands/OKX/okx.png?size=16) **OKX**\n\
                 ![](file://brands/Hyperliquid/hyperliquid.png?size=16) **Hyperliquid**"
            );

            cx.set_global(LanguageCode::ZhCn);
            assert_eq!(
                listed_names(cx, &["venue.binance", "venue.okx", "venue.hyperliquid"]).as_ref(),
                "![](file://brands/Binance/binance.png?size=16) **币安**\n\
                 ![](file://brands/OKX/okx.png?size=16) **欧易**\n\
                 ![](file://brands/Hyperliquid/hyperliquid.png?size=16) **Hyperliquid**"
            );
        });
    }

    /// Every venue of this build states a logo, and it is the mark `base_ui::brand` draws for
    /// it: the element and the sentence read one path per brand, so the logo in a list is the
    /// logo the rest of the interface shows rather than a second copy of the file name.
    #[gpui_kit::test]
    fn every_venue_should_state_the_logo_its_brand_draws(cx: &mut gpui_kit::TestAppContext) {
        cx.update(|cx| {
            i18n::global::embedded::install(cx).expect("the embedded locales should load");

            for (venue, path) in [
                (VenueId::Binance, base_ui::brand::BINANCE),
                (VenueId::Okx, base_ui::brand::OKX),
                (VenueId::Hyperliquid, base_ui::brand::HYPERLIQUID),
                (VenueId::Longbridge, base_ui::brand::LONGBRIDGE),
            ] {
                assert_eq!(venue.asset_path(), path, "{venue} states its own logo");
                assert_eq!(
                    venue_of_key(venue.name_key()),
                    venue,
                    "{venue} is the venue its own name key names"
                );
            }
        });
    }

    /// The notice a pane shows while a market's metadata is not loaded is the one message
    /// of the product that asks the user to load one, and it reads out of the catalogue the
    /// product ships, in both of its languages.
    ///
    /// The fixture is the shape this module has to handle; this is the copy the user
    /// actually gets against it — the market participants the sentence ends with, one per
    /// line, and the control through the key the control itself is labelled with, with the
    /// control's icon kept in the sentence. Both locales write that message, so rendering it
    /// is the only guard against one of its `%{…}` being renamed out from under the call
    /// sites.
    #[gpui_kit::test]
    fn a_market_notice_should_list_its_market_participants_and_name_its_control_in_both_languages(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        cx.update(|cx| {
            i18n::global::embedded::install(cx).expect("the embedded locales should load");

            for (language, names, logo, control) in [
                ("en", ["Binance", "OKX"], base_ui::brand::BINANCE, "Markets"),
                ("zh-CN", ["币安", "欧易"], base_ui::brand::BINANCE, "市场"),
            ] {
                cx.set_global(match language {
                    "en" => LanguageCode::En,
                    _ => LanguageCode::ZhCn,
                });
                let notice = Notice::MarketNotLoaded {
                    venues: vec![VenueId::Binance],
                    supported: vec![VenueId::Binance, VenueId::Okx],
                }
                .resolve(cx)
                .to_string();

                assert!(
                    notice.contains(&format!(
                        "![]({SCHEME}{logo}?size={LOGO_SIZE}) **{}**",
                        names[0]
                    )) && notice.contains(&format!("**{}**", names[1]))
                        && notice.ends_with("</p>"),
                    "{language} lists the market participants one per line, each its logo and its bold name: \
                     {notice:?}"
                );
                assert!(notice.contains(control), "{language}: {notice}");
                assert!(
                    notice.contains("file://icons/phosphoricons/graph.svg"),
                    "{language}: {notice}"
                );
                assert!(
                    !notice.contains("venue.") && !notice.contains("%{"),
                    "{language} leaves a key or a placeholder unresolved: {notice}"
                );
            }
        });
    }

    /// Every venue this build speaks is named by the catalogue in both of its languages, so
    /// a list a widget builds out of the definition table renders as translated names rather
    /// than as empty lines — which is what a reader of the notice would otherwise get, since
    /// the widget names the venues of its whole capability rather than one it chose.
    #[gpui_kit::test]
    fn every_venue_a_notice_may_list_should_render_in_both_languages(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        cx.update(|cx| {
            i18n::global::embedded::install(cx).expect("the embedded locales should load");

            let keys = VenueId::ALL
                .iter()
                .map(VenueDisplay::name_key)
                .collect::<Vec<_>>();

            for language in [LanguageCode::En, LanguageCode::ZhCn] {
                cx.set_global(language);
                let listed = listed_names(cx, &keys);

                for line in listed.lines() {
                    assert!(
                        !line.trim().is_empty(),
                        "{language} renders a venue of this build as an empty line: {listed:?}"
                    );
                }
            }
        });
    }
}
