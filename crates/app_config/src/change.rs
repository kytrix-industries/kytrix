//! What one published settings snapshot changed, domain by domain.

use market_participants_model::identifier::VenueId;
use smol_str::SmolStr;

use crate::{
    network::ServiceTarget,
    settings::{Appearance, Language},
    time_zone::DataTimeZone,
};

/// The configuration items one publish changed.
///
/// The set is computed once per publish from the previous and next snapshots.
/// It is deliberately a plain value: the GPUI events and the owner projections
/// are both derived from it, and it has no dependency on GPUI.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConfigChangeSet {
    /// The new appearance, when it changed.
    pub appearance: Option<Appearance>,
    /// The new language, when it changed.
    pub language: Option<Language>,
    /// The new display time zone, when it changed.
    pub data_time_zone: Option<DataTimeZone>,
    /// Whether the account or its avatar changed.
    pub account_changed: bool,
    /// The proxy catalog names that were added, removed or edited.
    pub proxies: Vec<SmolStr>,
    /// Whether the default route changed.
    pub proxy_default_changed: bool,
    /// The service targets whose application route was added, removed or edited.
    pub proxy_applications: Vec<ServiceTarget>,
    /// The venues whose settings were added, removed or edited.
    pub venues: Vec<VenueId>,
}

impl ConfigChangeSet {
    /// Reports whether this publish changed nothing the notification layer covers.
    pub fn is_empty(&self) -> bool {
        self.appearance.is_none()
            && self.language.is_none()
            && self.data_time_zone.is_none()
            && !self.account_changed
            && self.proxies.is_empty()
            && !self.proxy_default_changed
            && self.proxy_applications.is_empty()
            && self.venues.is_empty()
    }
}
