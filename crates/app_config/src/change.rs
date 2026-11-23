use market_participants_model::identifier::VenueId;
use smol_str::SmolStr;

use crate::{
    network::ServiceTarget,
    settings::{Appearance, Language},
    time_zone::DataTimeZone,
};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConfigChangeSet {
    pub appearance: Option<Appearance>,
    pub language: Option<Language>,
    pub data_time_zone: Option<DataTimeZone>,
    pub account_changed: bool,
    pub proxies: Vec<SmolStr>,
    pub proxy_default_changed: bool,
    pub proxy_applications: Vec<ServiceTarget>,
    pub venues: Vec<VenueId>,
}

impl ConfigChangeSet {
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
