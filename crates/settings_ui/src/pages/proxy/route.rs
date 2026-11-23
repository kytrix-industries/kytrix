use app_config::{
    network::{ProxyRoute, ServiceTarget},
    settings::Config,
};
use market_participants_model::identifier::VenueId;
use smol_str::SmolStr;

pub(crate) const PROXY_INHERIT: &str = "@inherit";
pub(crate) const PROXY_DIRECT: &str = "@direct";
pub(crate) const PROXY_SYSTEM: &str = "@system";
pub(crate) const PROXY_MIXED: &str = "@mixed";

pub(crate) fn proxy_route_of(config: &Config, target: &ServiceTarget) -> SmolStr {
    proxy_route_name(config.proxy_applications.get(target))
}

pub(crate) fn proxy_route_name(route: Option<&ProxyRoute>) -> SmolStr {
    match route {
        None => SmolStr::new_static(PROXY_INHERIT),
        Some(ProxyRoute::Direct) => SmolStr::new_static(PROXY_DIRECT),
        Some(ProxyRoute::System) => SmolStr::new_static(PROXY_SYSTEM),
        Some(ProxyRoute::Named { name }) => name.clone(),
    }
}

pub(crate) fn proxy_route_of_group(
    config: &Config,
    targets: impl IntoIterator<Item = ServiceTarget>,
) -> SmolStr {
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

pub(crate) fn set_proxy_route_of_group(
    config: &mut Config,
    targets: impl IntoIterator<Item = ServiceTarget>,
    value: &str,
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

pub(crate) fn decode_proxy_route(value: &str) -> Option<Option<ProxyRoute>> {
    match value {
        PROXY_INHERIT => Some(None),
        PROXY_DIRECT => Some(Some(ProxyRoute::Direct)),
        PROXY_SYSTEM => Some(Some(ProxyRoute::System)),
        name if !name.is_empty() && !app_config::settings::proxy_name_is_reserved(name) => {
            Some(Some(ProxyRoute::named(name)))
        }
        _ => None,
    }
}

pub(crate) fn proxy_route_of_venue(config: &Config, venue: &VenueId) -> SmolStr {
    match config.venue(venue) {
        Some(settings) => proxy_route_name(settings.proxy().as_ref()),
        None => SmolStr::new_static(PROXY_INHERIT),
    }
}

pub(crate) fn set_proxy_route_of_venue(config: &mut Config, venue: &VenueId, value: &str) {
    let Some(route) = decode_proxy_route(value) else {
        return;
    };

    config.set_venue_proxy(venue, route);
}

pub(crate) fn proxy_default_of(config: &Config) -> SmolStr {
    match &config.proxy_default {
        ProxyRoute::Direct => SmolStr::new_static(PROXY_DIRECT),
        ProxyRoute::System => SmolStr::new_static(PROXY_SYSTEM),
        ProxyRoute::Named { name } => name.clone(),
    }
}

pub(crate) fn set_proxy_default(config: &mut Config, value: &str) {
    let Some(route) = decode_proxy_route(value).flatten() else {
        return;
    };

    config.proxy_default = route;
}

#[cfg(test)]
mod tests {
    use app_config::{
        network::{KytrixServiceCapability, ProxyRoute},
        settings::Config,
    };
    use market_participants_model::identifier::VenueId;

    use super::{
        PROXY_DIRECT, PROXY_INHERIT, PROXY_MIXED, decode_proxy_route, proxy_default_of,
        proxy_route_of, proxy_route_of_group, proxy_route_of_venue, set_proxy_default,
        set_proxy_route_of_group, set_proxy_route_of_venue,
    };

    fn target(capability: KytrixServiceCapability) -> app_config::network::ServiceTarget {
        app_config::network::ServiceTarget::KytrixService { capability }
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

        set_proxy_route_of_group(&mut config, targets.iter().cloned(), "hk");

        assert_eq!(config.proxy_applications.len(), 2);
        assert_eq!(proxy_route_of_group(&config, targets.iter().cloned()), "hk");
    }

    #[test]
    fn targets_that_disagree_should_report_a_mixed_group() {
        let mut config = Config::default();
        let targets = [
            target(KytrixServiceCapability::XMedia),
            target(KytrixServiceCapability::News),
        ];

        set_proxy_route_of_group(&mut config, [target(KytrixServiceCapability::XMedia)], "hk");

        assert_eq!(proxy_route_of_group(&config, targets), PROXY_MIXED);
    }

    #[test]
    fn unbinding_a_group_should_follow_the_default_again() {
        let mut config = Config::default();
        let targets = [target(KytrixServiceCapability::XMedia)];

        set_proxy_route_of_group(&mut config, targets.iter().cloned(), "hk");
        set_proxy_route_of_group(&mut config, targets.iter().cloned(), PROXY_INHERIT);

        assert!(config.proxy_applications.is_empty());
    }

    #[test]
    fn a_state_that_is_not_a_route_should_not_be_written() {
        let mut config = Config::default();

        for value in [PROXY_MIXED, "@none", ""] {
            set_proxy_route_of_group(
                &mut config,
                [target(KytrixServiceCapability::XMedia)],
                value,
            );
            assert!(
                config.proxy_applications.is_empty(),
                "{value} must not become a binding"
            );
            assert_eq!(decode_proxy_route(value), None);
        }
    }

    #[test]
    fn a_named_default_should_round_trip() {
        let mut config = Config::default();

        set_proxy_default(&mut config, "hk");
        assert_eq!(proxy_default_of(&config), "hk");

        set_proxy_default(&mut config, PROXY_DIRECT);
        assert_eq!(proxy_default_of(&config), PROXY_DIRECT);
        assert_eq!(config.proxy_default, ProxyRoute::Direct);
    }

    #[test]
    fn a_default_line_should_not_be_set_to_inherit() {
        let mut config = Config::default();

        set_proxy_default(&mut config, PROXY_INHERIT);

        assert_eq!(config.proxy_default, ProxyRoute::Direct);
    }

    #[test]
    fn a_venue_should_follow_the_default_line_until_it_is_bound() {
        let venue = VenueId::Binance;
        let mut config = Config::default();

        assert_eq!(proxy_route_of_venue(&config, &venue), PROXY_INHERIT);

        set_proxy_route_of_venue(&mut config, &venue, "hk");
        assert_eq!(proxy_route_of_venue(&config, &venue), "hk");

        set_proxy_route_of_venue(&mut config, &venue, PROXY_INHERIT);
        assert_eq!(proxy_route_of_venue(&config, &venue), PROXY_INHERIT);

        set_proxy_route_of_venue(&mut config, &venue, PROXY_DIRECT);
        assert_eq!(proxy_route_of_venue(&config, &venue), PROXY_DIRECT);
    }

    #[test]
    fn a_venue_should_not_take_a_value_that_is_not_a_line() {
        let venue = VenueId::Binance;
        let mut config = Config::default();
        let untouched = config.clone();

        set_proxy_route_of_venue(&mut config, &venue, PROXY_MIXED);
        assert_eq!(
            config, untouched,
            "a rendered state must not become a binding"
        );

        let mut without_a_row = config.clone();
        without_a_row.venues.remove(&venue);
        let before = without_a_row.clone();
        set_proxy_route_of_venue(&mut without_a_row, &venue, "hk");
        assert_eq!(
            without_a_row, before,
            "a venue with no row has nowhere to write a line"
        );
    }
}
