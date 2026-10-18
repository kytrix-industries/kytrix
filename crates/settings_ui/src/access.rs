//! Value access: how one control reads and writes what it stands for.
//!
//! Every field of the settings carries both halves of its conversion with it, so a page is
//! a declaration rather than a match: rendering reads the value, and a control's change is
//! written back into it.
//!
//! The conversions are closures rather than plain function pointers, because a field built
//! from the settings has to carry the identity it stands for: one origin row is one scope of one
//! venue, one credential field is one field of one venue's shape, and a proxy line is one venue
//! or the set of Kytrix services it covers. Nothing is looked up from a field id at render time,
//! so a setting the page does not name can never be written to.
//!
//! Where that write lands is not this module's to decide: an access is given a [`Config`] by
//! whoever holds one, which while the settings dialog is open is the session the user is
//! configuring rather than the settings themselves — see [`crate::draft`]. What changed the
//! settings is only ever what the user confirmed.

use std::{rc::Rc, sync::Arc};

use app_config::{
    network::{ProxyAuthorizationConfig, ProxyConfig, ProxyRoute, ServiceTarget},
    settings::Config,
};
use gpui_kit::SharedString;
use market_participants_model::identifier::VenueId;
use smol_str::SmolStr;

/// A short text value as one control holds it.
pub(crate) type FieldValue = SmolStr;

/// Reads the value one field stands for.
pub(crate) type FieldGetter = Rc<dyn Fn(&Config) -> FieldValue>;
/// Writes the value one field stands for.
pub(crate) type FieldSetter = Rc<dyn Fn(&mut Config, &FieldValue)>;
/// Reads the value one reported field shows.
pub(crate) type InfoGetter = Rc<dyn Fn(&Config) -> SharedString>;
/// Reads the line every target behind one field is bound to.
pub(crate) type ProxyLineGetter = Rc<dyn Fn(&Config) -> FieldValue>;
/// Binds every target behind one field to a line.
pub(crate) type ProxyLineSetter = Rc<dyn Fn(&mut Config, &FieldValue)>;

/// Follow the default line.
pub(crate) const PROXY_INHERIT: &str = "@inherit";
/// Do not use a proxy.
pub(crate) const PROXY_DIRECT: &str = "@direct";
/// Use the operating system's proxy.
pub(crate) const PROXY_SYSTEM: &str = "@system";
/// The mixed value exists only as a rendered state. Nothing is ever stored as it: it says
/// that the targets behind one control do not agree, and choosing it would say the same
/// thing back.
pub(crate) const PROXY_MIXED: &str = "@mixed";

/// Reads the line one target is bound to, or the default line while it is unbound.
pub(crate) fn proxy_route_of(config: &Config, target: &ServiceTarget) -> FieldValue {
    proxy_route_name(config.proxy_applications.get(target))
}

/// The line one stored route names, as the control that offers it reads it.
fn proxy_route_name(route: Option<&ProxyRoute>) -> FieldValue {
    match route {
        None => SmolStr::new_static(PROXY_INHERIT),
        Some(ProxyRoute::Direct) => SmolStr::new_static(PROXY_DIRECT),
        Some(ProxyRoute::System) => SmolStr::new_static(PROXY_SYSTEM),
        Some(ProxyRoute::Named { name }) => name.clone(),
    }
}

/// The line a group of targets agrees on.
///
/// Targets that disagree are not one of the choices, so the answer is [`PROXY_MIXED`] and
/// the control offers it as the row that is currently selected.
pub(crate) fn proxy_route_of_group(
    config: &Config,
    targets: impl IntoIterator<Item = ServiceTarget>,
) -> FieldValue {
    let mut routes = targets
        .into_iter()
        .map(|target| proxy_route_of(config, &target));
    let Some(first) = routes.next() else {
        return SmolStr::new_static(PROXY_INHERIT);
    };
    if routes.all(|route| route == first) {
        first
    } else {
        SmolStr::new_static(PROXY_MIXED)
    }
}

/// Binds every target of a group to one line, or unbinds them when it is the default.
pub(crate) fn set_proxy_route_of_group(
    config: &mut Config,
    targets: impl IntoIterator<Item = ServiceTarget>,
    value: &FieldValue,
) {
    let Some(route) = decode_proxy_route(value) else {
        return;
    };

    for target in targets {
        match &route {
            None => {
                config.proxy_applications.remove(&target);
            }
            Some(route) => {
                config.proxy_applications.insert(target, route.clone());
            }
        }
    }
}

/// Decodes the line a value stands for.
///
/// A value that is not a reserved token is the name of a catalog entry. An empty or
/// reserved one that is not a token is nothing, which is what keeps a rendering state from
/// being written back as a route.
pub(crate) fn decode_proxy_route(value: &FieldValue) -> Option<Option<ProxyRoute>> {
    match value.as_str() {
        PROXY_INHERIT => Some(None),
        PROXY_DIRECT => Some(Some(ProxyRoute::Direct)),
        PROXY_SYSTEM => Some(Some(ProxyRoute::System)),
        name if !name.is_empty() && !app_config::settings::proxy_name_is_reserved(name) => {
            Some(Some(ProxyRoute::named(name)))
        }
        _ => None,
    }
}

/// The line one venue's own requests are bound to.
///
/// A venue is configured as a whole, so its line is the venue's own value rather than a set of
/// per-target overrides: what the row shows is the venue's own settings row. A venue that names
/// no line of its own follows the default, exactly as a Kytrix service without an override does,
/// and a venue this build does not speak holds no line at all — the row that would show one does
/// not exist either.
pub(crate) fn proxy_route_of_venue(config: &Config, venue: &VenueId) -> FieldValue {
    match config.venue(venue) {
        Some(settings) => proxy_route_name(settings.proxy().as_ref()),
        None => SmolStr::new_static(PROXY_INHERIT),
    }
}

/// Binds one venue to one line, or puts it back on the default line.
///
/// A venue names a line of its own or nothing at all: a value that stands for no line is what
/// makes the venue follow the default again, exactly as unbinding a group of services does. A
/// value that is not a line the settings can store is nothing to write, which is what keeps a
/// rendered state from being written back as a route.
pub(crate) fn set_proxy_route_of_venue(config: &mut Config, venue: &VenueId, value: &FieldValue) {
    let Some(route) = decode_proxy_route(value) else {
        return;
    };

    config.set_venue_proxy(venue, route);
}

/// The line a whole document asks for when no target overrides it.
pub(crate) fn proxy_default_of(config: &Config) -> FieldValue {
    match &config.proxy_default {
        ProxyRoute::Direct => SmolStr::new_static(PROXY_DIRECT),
        ProxyRoute::System => SmolStr::new_static(PROXY_SYSTEM),
        ProxyRoute::Named { name } => name.clone(),
    }
}

/// Sets the default line.
///
/// It is never inherited from anything, so a value that would mean "follow the default" is
/// nothing to write and leaves the stored one where it was.
pub(crate) fn set_proxy_default(config: &mut Config, value: &FieldValue) {
    let Some(route) = decode_proxy_route(value).flatten() else {
        return;
    };

    config.proxy_default = route;
}

/// One catalog entry as an add or edit form holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProxyDraft {
    pub name: FieldValue,
    pub addr: FieldValue,
    pub port: FieldValue,
    pub username: FieldValue,
    pub password: FieldValue,
    pub authenticated: bool,
    pub resolve_dns_at_remote: bool,
}

impl ProxyDraft {
    /// The form that adds a new entry.
    ///
    /// What it starts from is what a user who touches nothing records, so a new entry resolves
    /// names here: the mode that asks nothing of the proxy is the one to opt out of.
    pub fn empty() -> Self {
        Self {
            name: FieldValue::default(),
            addr: FieldValue::default(),
            port: FieldValue::default(),
            username: FieldValue::default(),
            password: FieldValue::default(),
            authenticated: false,
            resolve_dns_at_remote: false,
        }
    }

    /// The form that edits an existing entry, including the name it is stored under.
    pub fn from_proxy(name: &FieldValue, proxy: &ProxyConfig) -> Self {
        let (username, password, authenticated) = match &proxy.authorization {
            ProxyAuthorizationConfig::None => (FieldValue::default(), FieldValue::default(), false),
            ProxyAuthorizationConfig::UsernameAndPassword { username, password } => {
                (username.clone(), password.clone(), true)
            }
        };

        Self {
            name: name.clone(),
            addr: proxy.addr.clone(),
            port: FieldValue::new(proxy.port.to_string()),
            username,
            password,
            authenticated,
            resolve_dns_at_remote: proxy.resolve_dns_at_remote,
        }
    }
}

/// A target set a proxy-line field stands for, as the field and its closure hold it.
pub(crate) type TargetSet = Arc<[ServiceTarget]>;

#[cfg(test)]
mod tests {
    use app_config::{
        network::{
            KytrixServiceCapability, ProxyAuthorizationConfig, ProxyConfig, ProxyRoute,
            ServiceTarget,
        },
        settings::Config,
    };
    use market_participants_model::identifier::VenueId;

    use super::{
        FieldValue, PROXY_DIRECT, PROXY_INHERIT, PROXY_MIXED, ProxyDraft, decode_proxy_route,
        proxy_default_of, proxy_route_of, proxy_route_of_group, proxy_route_of_venue,
        set_proxy_default, set_proxy_route_of_group, set_proxy_route_of_venue,
    };

    fn target(capability: KytrixServiceCapability) -> ServiceTarget {
        ServiceTarget::KytrixService { capability }
    }

    fn local_proxy() -> ProxyConfig {
        ProxyConfig {
            addr: app_config::network::normalize_proxy_host("127.0.0.1").expect("a host"),
            port: 1080,
            authorization: ProxyAuthorizationConfig::None,
            resolve_dns_at_remote: true,
        }
    }

    #[test]
    fn an_unbound_target_should_report_the_default_line() {
        let config = Config::default();

        assert_eq!(
            proxy_route_of(&config, &target(KytrixServiceCapability::XMedia)),
            PROXY_INHERIT
        );
    }

    #[test]
    fn binding_a_group_should_reach_every_target_in_it() {
        let mut config = Config::default();
        let targets = [
            target(KytrixServiceCapability::XMedia),
            target(KytrixServiceCapability::News),
        ];

        set_proxy_route_of_group(&mut config, targets.clone(), &FieldValue::new_static("hk"));

        assert_eq!(config.proxy_applications.len(), 2);
        assert_eq!(proxy_route_of_group(&config, targets), "hk");
    }

    #[test]
    fn targets_that_disagree_should_report_a_mixed_group() {
        let mut config = Config::default();
        let targets = [
            target(KytrixServiceCapability::XMedia),
            target(KytrixServiceCapability::News),
        ];

        set_proxy_route_of_group(
            &mut config,
            [target(KytrixServiceCapability::XMedia)],
            &FieldValue::new_static("hk"),
        );

        assert_eq!(proxy_route_of_group(&config, targets), PROXY_MIXED);
    }

    #[test]
    fn unbinding_a_group_should_follow_the_default_again() {
        let mut config = Config::default();
        let targets = [target(KytrixServiceCapability::XMedia)];

        set_proxy_route_of_group(&mut config, targets.clone(), &FieldValue::new_static("hk"));
        set_proxy_route_of_group(&mut config, targets, &FieldValue::new_static(PROXY_INHERIT));

        assert!(config.proxy_applications.is_empty());
    }

    /// A mixed group is a rendering state, so it is never written back: what a control says
    /// the group currently is cannot become what the group is bound to.
    #[test]
    fn a_state_that_is_not_a_route_should_not_be_written() {
        let mut config = Config::default();

        for value in [PROXY_MIXED, "@none", ""] {
            set_proxy_route_of_group(
                &mut config,
                [target(KytrixServiceCapability::XMedia)],
                &FieldValue::new_static(value),
            );
            assert!(
                config.proxy_applications.is_empty(),
                "{value} must not become a binding"
            );
            assert_eq!(decode_proxy_route(&FieldValue::new_static(value)), None);
        }
    }

    #[test]
    fn a_named_default_should_round_trip() {
        let mut config = Config::default();

        set_proxy_default(&mut config, &FieldValue::new_static("hk"));
        assert_eq!(proxy_default_of(&config), "hk");

        set_proxy_default(&mut config, &FieldValue::new_static(PROXY_DIRECT));
        assert_eq!(proxy_default_of(&config), PROXY_DIRECT);
        assert_eq!(config.proxy_default, ProxyRoute::Direct);
    }

    /// The default line has nothing above it to inherit from, so a value that would say
    /// "follow the default" leaves the stored one where it was.
    #[test]
    fn a_default_line_should_not_be_set_to_inherit() {
        let mut config = Config::default();

        set_proxy_default(&mut config, &FieldValue::new_static(PROXY_INHERIT));

        assert_eq!(config.proxy_default, ProxyRoute::Direct);
    }

    /// A venue that names no line of its own reports the default one, and a value
    /// standing for no line puts it back there.
    #[test]
    fn a_venue_should_follow_the_default_line_until_it_is_bound() {
        let venue = VenueId::Binance;
        let mut config = Config::default();

        assert_eq!(proxy_route_of_venue(&config, &venue), PROXY_INHERIT);

        set_proxy_route_of_venue(&mut config, &venue, &FieldValue::new_static("hk"));
        assert_eq!(proxy_route_of_venue(&config, &venue), "hk");

        set_proxy_route_of_venue(&mut config, &venue, &FieldValue::new_static(PROXY_INHERIT));
        assert_eq!(proxy_route_of_venue(&config, &venue), PROXY_INHERIT);

        set_proxy_route_of_venue(&mut config, &venue, &FieldValue::new_static(PROXY_DIRECT));
        assert_eq!(proxy_route_of_venue(&config, &venue), PROXY_DIRECT);
    }

    /// A state that is not a route is not what a venue is bound to either, and a row
    /// the shape has no configuration for has no line to write.
    #[test]
    fn a_venue_should_not_take_a_value_that_is_not_a_line() {
        let venue = VenueId::Binance;
        let mut config = Config::default();
        let untouched = config.clone();

        set_proxy_route_of_venue(&mut config, &venue, &FieldValue::new_static(PROXY_MIXED));
        assert_eq!(
            config, untouched,
            "a rendered state must not become a binding"
        );

        // The shape covers every venue this build speaks, so the only way a venue
        // has no row is a configuration that was taken apart by hand.
        let mut without_a_row = config.clone();
        without_a_row.venues.remove(&venue);
        let before = without_a_row.clone();
        set_proxy_route_of_venue(&mut without_a_row, &venue, &FieldValue::new_static("hk"));
        assert_eq!(
            without_a_row, before,
            "a venue with no row has nowhere to write a line"
        );
    }

    #[test]
    fn an_edit_form_should_carry_the_entry_back_into_a_draft() {
        let proxy = ProxyConfig {
            authorization: ProxyAuthorizationConfig::UsernameAndPassword {
                username: FieldValue::new_static("user"),
                password: FieldValue::new_static("secret"),
            },
            resolve_dns_at_remote: false,
            ..local_proxy()
        };

        let draft = ProxyDraft::from_proxy(&FieldValue::new_static("hk"), &proxy);

        assert_eq!(
            draft,
            ProxyDraft {
                name: FieldValue::new_static("hk"),
                addr: FieldValue::new_static("127.0.0.1"),
                port: FieldValue::new_static("1080"),
                username: FieldValue::new_static("user"),
                password: FieldValue::new_static("secret"),
                authenticated: true,
                resolve_dns_at_remote: false,
            }
        );
        assert!(!ProxyDraft::empty().authenticated);
        assert!(
            !ProxyDraft::empty().resolve_dns_at_remote,
            "a new entry should resolve names here until the user chooses otherwise"
        );
    }
}
