//! The credentials each venue's authenticated requests sign with.
//!
//! A credential set is a value of exactly one venue's shape, and the shape is
//! written here rather than derived from a venue name: what a venue signs with is a
//! property of the venue's API, and the settings hold the typed value the adapters
//! read.
//!
//! Binance is the one venue whose credentials are split: the spot API and the USDⓈ-M
//! futures API are two products with two key pairs, so a Binance row carries one set
//! per market and an authenticated request signs with the set of its own scope. Every
//! other venue authenticates its whole surface with one set.
//!
//! An empty set is *unconfigured* rather than invalid: it is what every venue starts
//! as, and such a request fails closed. Anything between empty and complete is
//! rejected, so a half-filled set can never sign a request.

use std::fmt;

use market_participants_model::identifier::VenueId;
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

use crate::market_participants::error::MarketParticipantsConfigError;

/// The credentials of one venue.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum VenueCredentials {
    #[serde(rename = "binance")]
    Binance(BinanceCredentials),
    #[serde(rename = "okx")]
    Okx(OkxCredentials),
    #[serde(rename = "hyperliquid")]
    Hyperliquid(HyperliquidCredentials),
    #[serde(rename = "longbridge")]
    Longbridge(LongbridgeCredentials),
}

/// Binance's credentials, one set per market it publishes.
///
/// The two sets are stored apart because the venue's two products are configured
/// apart: an operator grants the spot API one key and the USDⓈ-M futures API another,
/// and a signature made for one is refused by the other.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BinanceCredentials {
    /// The spot API's Ed25519 API key.
    #[serde(rename = "spot_api_key")]
    pub spot_api_key: SmolStr,
    /// The spot API's Ed25519 private key.
    #[serde(rename = "spot_private_key")]
    pub spot_private_key: SmolStr,
    /// The spot API's optional Ed25519 private-key passphrase.
    #[serde(rename = "spot_passphrase")]
    pub spot_passphrase: SmolStr,
    /// The USDⓈ-M futures API's Ed25519 API key.
    #[serde(rename = "usd_m_perp_api_key")]
    pub usd_m_perp_api_key: SmolStr,
    /// The USDⓈ-M futures API's Ed25519 private key.
    #[serde(rename = "usd_m_perp_private_key")]
    pub usd_m_perp_private_key: SmolStr,
    /// The USDⓈ-M futures API's optional Ed25519 private-key passphrase.
    #[serde(rename = "usd_m_perp_passphrase")]
    pub usd_m_perp_passphrase: SmolStr,
}

impl BinanceCredentials {
    /// Returns the credential set of one market.
    pub fn market(&self, market: BinanceCredentialMarket) -> BinanceMarketCredentials<'_> {
        match market {
            BinanceCredentialMarket::Spot => BinanceMarketCredentials {
                api_key: &self.spot_api_key,
                private_key: &self.spot_private_key,
                passphrase: &self.spot_passphrase,
            },
            BinanceCredentialMarket::UsdMPerp => BinanceMarketCredentials {
                api_key: &self.usd_m_perp_api_key,
                private_key: &self.usd_m_perp_private_key,
                passphrase: &self.usd_m_perp_passphrase,
            },
        }
    }

    /// Replaces the credential set of one market.
    pub fn set_market(
        &mut self,
        market: BinanceCredentialMarket,
        credentials: BinanceMarketCredentials<'_>,
    ) {
        let (api_key, private_key, passphrase) = (
            SmolStr::new(credentials.api_key),
            SmolStr::new(credentials.private_key),
            SmolStr::new(credentials.passphrase),
        );
        match market {
            BinanceCredentialMarket::Spot => {
                self.spot_api_key = api_key;
                self.spot_private_key = private_key;
                self.spot_passphrase = passphrase;
            }
            BinanceCredentialMarket::UsdMPerp => {
                self.usd_m_perp_api_key = api_key;
                self.usd_m_perp_private_key = private_key;
                self.usd_m_perp_passphrase = passphrase;
            }
        }
    }
}

impl fmt::Debug for BinanceCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BinanceCredentials")
            .field("spot", &"<redacted>")
            .field("usd_m_perp", &"<redacted>")
            .finish()
    }
}

/// Which of Binance's two markets one credential set belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BinanceCredentialMarket {
    /// The spot API.
    Spot,
    /// The USDⓈ-M futures API.
    UsdMPerp,
}

/// One market's Binance credential set.
///
/// The three values are borrowed together because they are one credential: an API
/// key without its private key signs nothing, and neither is stored apart from the
/// passphrase its key was created with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BinanceMarketCredentials<'a> {
    pub api_key: &'a str,
    pub private_key: &'a str,
    pub passphrase: &'a str,
}

impl BinanceMarketCredentials<'_> {
    /// Reports whether every one of the three values is blank.
    pub fn is_empty(&self) -> bool {
        self.api_key.trim().is_empty()
            && self.private_key.trim().is_empty()
            && self.passphrase.trim().is_empty()
    }

    /// Reports whether the two required values are present.
    ///
    /// The passphrase is optional: Binance creates a key with or without one, so it
    /// is part of the credential only when the operator's key has one.
    pub fn is_complete(&self) -> bool {
        !self.api_key.trim().is_empty() && !self.private_key.trim().is_empty()
    }

    /// Reports whether this market's credentials are complete, empty, or only partly
    /// filled.
    pub fn validate(
        &self,
        venue: VenueId,
        scope: Option<&str>,
    ) -> Result<bool, MarketParticipantsConfigError> {
        if self.is_empty() {
            Ok(false)
        } else if self.is_complete() {
            Ok(true)
        } else {
            Err(MarketParticipantsConfigError::IncompleteCredentials {
                venue,
                scope: scope.map(SmolStr::new),
            })
        }
    }
}

/// OKX API credentials.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OkxCredentials {
    /// OKX API key.
    #[serde(rename = "api_key")]
    pub api_key: SmolStr,
    /// OKX secret key.
    #[serde(rename = "secret_key")]
    pub secret_key: SmolStr,
    /// OKX API passphrase.
    #[serde(rename = "passphrase")]
    pub passphrase: SmolStr,
}

impl OkxCredentials {
    /// Reports whether every value is blank.
    pub fn is_empty(&self) -> bool {
        self.api_key.trim().is_empty()
            && self.secret_key.trim().is_empty()
            && self.passphrase.trim().is_empty()
    }

    /// Reports whether every value is present.
    pub fn is_complete(&self) -> bool {
        !self.api_key.trim().is_empty()
            && !self.secret_key.trim().is_empty()
            && !self.passphrase.trim().is_empty()
    }
}

impl fmt::Debug for OkxCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OkxCredentials")
            .field("api_key", &"<redacted>")
            .field("secret_key", &"<redacted>")
            .field("passphrase", &"<redacted>")
            .finish()
    }
}

/// Hyperliquid main-account address and API Agent key.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HyperliquidCredentials {
    /// Main account address whose state and orders the API Agent controls.
    #[serde(rename = "main_account_address")]
    pub main_account_address: SmolStr,
    /// API Agent secp256k1 private key.
    #[serde(rename = "api_agent_private_key")]
    pub api_agent_private_key: SmolStr,
}

impl HyperliquidCredentials {
    /// Reports whether every value is blank.
    pub fn is_empty(&self) -> bool {
        self.main_account_address.trim().is_empty() && self.api_agent_private_key.trim().is_empty()
    }

    /// Reports whether every value is present.
    pub fn is_complete(&self) -> bool {
        !self.main_account_address.trim().is_empty()
            && !self.api_agent_private_key.trim().is_empty()
    }
}

impl fmt::Debug for HyperliquidCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HyperliquidCredentials")
            .field("main_account_address", &"<redacted>")
            .field("api_agent_private_key", &"<redacted>")
            .finish()
    }
}

/// Longbridge OpenAPI credentials.
///
/// The three values are one credential: the access token is what an authenticated socket
/// presents, and it is the app key and secret the token is issued and re-signed with, so a
/// row that holds fewer than all three signs nothing.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LongbridgeCredentials {
    /// The OpenAPI application key.
    #[serde(rename = "app_key")]
    pub app_key: SmolStr,
    /// The OpenAPI application secret.
    #[serde(rename = "app_secret")]
    pub app_secret: SmolStr,
    /// The access token issued for the application.
    #[serde(rename = "access_token")]
    pub access_token: SmolStr,
}

impl LongbridgeCredentials {
    /// Reports whether every value is blank.
    pub fn is_empty(&self) -> bool {
        self.app_key.trim().is_empty()
            && self.app_secret.trim().is_empty()
            && self.access_token.trim().is_empty()
    }

    /// Reports whether every value is present.
    pub fn is_complete(&self) -> bool {
        !self.app_key.trim().is_empty()
            && !self.app_secret.trim().is_empty()
            && !self.access_token.trim().is_empty()
    }
}

impl fmt::Debug for LongbridgeCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LongbridgeCredentials")
            .field("app_key", &"<redacted>")
            .field("app_secret", &"<redacted>")
            .field("access_token", &"<redacted>")
            .finish()
    }
}

impl VenueCredentials {
    /// Reports whether one market's credential set is complete, empty, or only
    /// partly filled.
    ///
    /// `scope` is the scope whose authenticated requests would sign with `market`; it
    /// is carried so that a half-filled set is reported against the market the user
    /// has to finish rather than against the venue as a whole.
    pub fn validate_market(
        &self,
        venue: VenueId,
        market: BinanceCredentialMarket,
        scope: Option<&str>,
    ) -> Result<bool, MarketParticipantsConfigError> {
        let Self::Binance(credentials) = self else {
            return Err(MarketParticipantsConfigError::MismatchedShape {
                venue,
                scope: scope.map(SmolStr::new),
            });
        };
        credentials.market(market).validate(venue, scope)
    }

    /// Reports whether this whole-venue set is complete, empty, or only partly
    /// filled.
    pub fn validate_venue(&self, venue: VenueId) -> Result<bool, MarketParticipantsConfigError> {
        let (empty, complete) = match self {
            Self::Okx(credentials) => (credentials.is_empty(), credentials.is_complete()),
            Self::Hyperliquid(credentials) => (credentials.is_empty(), credentials.is_complete()),
            Self::Longbridge(credentials) => (credentials.is_empty(), credentials.is_complete()),
            Self::Binance(_) => {
                return Err(MarketParticipantsConfigError::MismatchedShape { venue, scope: None });
            }
        };
        if empty {
            Ok(false)
        } else if complete {
            Ok(true)
        } else {
            Err(MarketParticipantsConfigError::IncompleteCredentials { venue, scope: None })
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn venue(name: &str) -> VenueId {
        VenueId::from_name(name).expect("the venue id should be canonical")
    }

    #[test]
    fn default_credentials_should_be_unconfigured() {
        let binance = VenueCredentials::Binance(BinanceCredentials::default());
        let profile = venue("binance");

        assert_eq!(
            (
                binance.validate_market(profile, BinanceCredentialMarket::Spot, Some("spot")),
                binance.validate_market(
                    profile,
                    BinanceCredentialMarket::UsdMPerp,
                    Some("usd-m-perp")
                ),
                VenueCredentials::Okx(OkxCredentials::default()).validate_venue(venue("okx")),
                VenueCredentials::Hyperliquid(HyperliquidCredentials::default())
                    .validate_venue(venue("hyperliquid")),
            ),
            (Ok(false), Ok(false), Ok(false), Ok(false))
        );
        assert_eq!(
            VenueCredentials::Longbridge(LongbridgeCredentials::default())
                .validate_venue(venue("longbridge")),
            Ok(false),
            "a venue nothing was typed into is unconfigured, not invalid"
        );
    }

    #[test]
    fn complete_credentials_should_be_configured_without_trimming_values() {
        let profile = venue("binance");
        let binance = VenueCredentials::Binance(BinanceCredentials {
            spot_api_key: SmolStr::new_static(" api-key "),
            spot_private_key: SmolStr::new_static(" private-key "),
            spot_passphrase: SmolStr::new_static("   "),
            usd_m_perp_api_key: SmolStr::new_static(" perp-key "),
            usd_m_perp_private_key: SmolStr::new_static(" perp-private "),
            usd_m_perp_passphrase: SmolStr::new_static("perp-pass"),
        });
        let okx = VenueCredentials::Okx(OkxCredentials {
            api_key: SmolStr::new_static(" api-key "),
            secret_key: SmolStr::new_static(" secret-key "),
            passphrase: SmolStr::new_static(" passphrase "),
        });
        let hyperliquid = VenueCredentials::Hyperliquid(HyperliquidCredentials {
            main_account_address: SmolStr::new_static(" address "),
            api_agent_private_key: SmolStr::new_static(" private-key "),
        });
        let longbridge = VenueCredentials::Longbridge(LongbridgeCredentials {
            app_key: SmolStr::new_static(" app-key "),
            app_secret: SmolStr::new_static(" app-secret "),
            access_token: SmolStr::new_static(" access-token "),
        });

        assert_eq!(
            (
                binance.validate_market(profile, BinanceCredentialMarket::Spot, Some("spot")),
                binance.validate_market(
                    profile,
                    BinanceCredentialMarket::UsdMPerp,
                    Some("usd-m-perp")
                ),
                okx.validate_venue(venue("okx")),
                hyperliquid.validate_venue(venue("hyperliquid")),
                longbridge.validate_venue(venue("longbridge")),
            ),
            (Ok(true), Ok(true), Ok(true), Ok(true), Ok(true)),
            "a credential set is complete without trimming any of its values"
        );
    }

    #[test]
    fn partial_credentials_should_be_refused_for_every_shape() {
        let profile = venue("okx");
        let binance = VenueCredentials::Binance(BinanceCredentials {
            spot_passphrase: SmolStr::new_static("passphrase-only"),
            ..BinanceCredentials::default()
        });
        let okx = VenueCredentials::Okx(OkxCredentials {
            api_key: SmolStr::new_static("api-key-only"),
            ..OkxCredentials::default()
        });
        let hyperliquid = VenueCredentials::Hyperliquid(HyperliquidCredentials {
            main_account_address: SmolStr::new_static("address-only"),
            ..HyperliquidCredentials::default()
        });
        let longbridge = VenueCredentials::Longbridge(LongbridgeCredentials {
            app_key: SmolStr::new_static("app-key-only"),
            ..LongbridgeCredentials::default()
        });

        // The refusal names the venue and the market the profile was validated for,
        // whatever shape the profile holds: a venue can carry another venue's shape,
        // and it is the market the user has to finish.
        assert_eq!(
            (
                binance.validate_market(profile, BinanceCredentialMarket::Spot, Some("spot")),
                okx.validate_venue(profile),
                hyperliquid.validate_venue(profile),
                longbridge.validate_venue(profile),
            ),
            (
                Err(MarketParticipantsConfigError::IncompleteCredentials {
                    venue: profile,
                    scope: Some(SmolStr::new_static("spot")),
                }),
                Err(MarketParticipantsConfigError::IncompleteCredentials {
                    venue: profile,
                    scope: None,
                }),
                Err(MarketParticipantsConfigError::IncompleteCredentials {
                    venue: profile,
                    scope: None,
                }),
                Err(MarketParticipantsConfigError::IncompleteCredentials {
                    venue: profile,
                    scope: None,
                }),
            )
        );
    }

    /// A credential set is a value of exactly one venue's shape: presenting Binance's
    /// keys to a venue that authenticates as a whole — or the other way round — is
    /// not a half-filled set to finish but the wrong shape entirely.
    #[test]
    fn a_profile_of_another_shape_should_be_refused_as_a_mismatch() {
        let binance = VenueCredentials::Binance(BinanceCredentials::default());
        let okx = VenueCredentials::Okx(OkxCredentials::default());

        assert_eq!(
            binance.validate_venue(venue("okx")),
            Err(MarketParticipantsConfigError::MismatchedShape {
                venue: venue("okx"),
                scope: None,
            })
        );
        assert_eq!(
            okx.validate_market(venue("okx"), BinanceCredentialMarket::Spot, Some("spot")),
            Err(MarketParticipantsConfigError::MismatchedShape {
                venue: venue("okx"),
                scope: Some(SmolStr::new_static("spot")),
            })
        );
    }

    #[test]
    fn profiles_should_encode_the_venue_they_authenticate() {
        let cases = [
            (
                VenueCredentials::Binance(BinanceCredentials {
                    spot_api_key: SmolStr::new_static("spot-key"),
                    spot_private_key: SmolStr::new_static("spot-private"),
                    spot_passphrase: SmolStr::new_static("spot-passphrase"),
                    usd_m_perp_api_key: SmolStr::new_static("perp-key"),
                    usd_m_perp_private_key: SmolStr::new_static("perp-private"),
                    usd_m_perp_passphrase: SmolStr::new_static("perp-passphrase"),
                }),
                json!({
                    "kind": "binance",
                    "spot_api_key": "spot-key",
                    "spot_private_key": "spot-private",
                    "spot_passphrase": "spot-passphrase",
                    "usd_m_perp_api_key": "perp-key",
                    "usd_m_perp_private_key": "perp-private",
                    "usd_m_perp_passphrase": "perp-passphrase",
                }),
            ),
            (
                VenueCredentials::Okx(OkxCredentials {
                    api_key: SmolStr::new_static("okx-key"),
                    secret_key: SmolStr::new_static("okx-secret"),
                    passphrase: SmolStr::new_static("okx-passphrase"),
                }),
                json!({
                    "kind": "okx",
                    "api_key": "okx-key",
                    "secret_key": "okx-secret",
                    "passphrase": "okx-passphrase",
                }),
            ),
            (
                VenueCredentials::Hyperliquid(HyperliquidCredentials {
                    main_account_address: SmolStr::new_static("0xmain"),
                    api_agent_private_key: SmolStr::new_static("agent-private"),
                }),
                json!({
                    "kind": "hyperliquid",
                    "main_account_address": "0xmain",
                    "api_agent_private_key": "agent-private",
                }),
            ),
            (
                VenueCredentials::Longbridge(LongbridgeCredentials {
                    app_key: SmolStr::new_static("app-key"),
                    app_secret: SmolStr::new_static("app-secret"),
                    access_token: SmolStr::new_static("access-token"),
                }),
                json!({
                    "kind": "longbridge",
                    "app_key": "app-key",
                    "app_secret": "app-secret",
                    "access_token": "access-token",
                }),
            ),
        ];

        for (profile, wire) in cases {
            let encoded = serde_json::to_value(&profile).expect("a profile should serialize");
            assert_eq!(encoded, wire);
            assert_eq!(
                serde_json::from_value::<VenueCredentials>(wire)
                    .expect("a profile should deserialize"),
                profile
            );
        }
    }

    /// The shape this module replaced is not one a file can carry: a venue-wide
    /// Binance key and a per-market split of another venue's credentials are both
    /// refused rather than decoded into the nearest match.
    #[test]
    fn a_superseded_credential_shape_should_not_decode() {
        for removed in [
            json!({
                "kind": "binance",
                "ed_25519_api_key": "key",
                "ed_25519_private_key": "private",
                "ed_25519_passphrase": "",
            }),
            json!({ "kind": "binance", "spot": {}, "usd_m_perp": {} }),
            json!({ "kind": "binance" }),
            json!({ "kind": "okx" }),
            json!({ "kind": "kraken", "api_key": "key" }),
            json!({ "kind": "spot", "api_key": "key" }),
        ] {
            let decoded = serde_json::from_value::<VenueCredentials>(removed.clone());
            if let Ok(decoded) = decoded {
                panic!("{removed} decoded into {decoded:?}");
            }
        }
    }

    #[test]
    fn debug_output_should_redact_every_market_participant_secret() {
        let binance = VenueCredentials::Binance(BinanceCredentials {
            spot_api_key: SmolStr::new_static("binance-spot-api-key"),
            spot_private_key: SmolStr::new_static("binance-spot-private-key"),
            spot_passphrase: SmolStr::new_static("binance-spot-passphrase"),
            usd_m_perp_api_key: SmolStr::new_static("binance-perp-api-key"),
            usd_m_perp_private_key: SmolStr::new_static("binance-perp-private-key"),
            usd_m_perp_passphrase: SmolStr::new_static("binance-perp-passphrase"),
        });
        let okx = VenueCredentials::Okx(OkxCredentials {
            api_key: SmolStr::new_static("okx-api-key"),
            secret_key: SmolStr::new_static("okx-secret-key"),
            passphrase: SmolStr::new_static("okx-passphrase"),
        });
        let hyperliquid = VenueCredentials::Hyperliquid(HyperliquidCredentials {
            main_account_address: SmolStr::new_static("main-account"),
            api_agent_private_key: SmolStr::new_static("agent-private-key"),
        });
        let longbridge = VenueCredentials::Longbridge(LongbridgeCredentials {
            app_key: SmolStr::new_static("longbridge-app-key"),
            app_secret: SmolStr::new_static("longbridge-app-secret"),
            access_token: SmolStr::new_static("longbridge-access-token"),
        });

        let output = format!("{binance:?} {okx:?} {hyperliquid:?} {longbridge:?}");

        assert!(
            [
                "binance-spot-api-key",
                "binance-spot-private-key",
                "binance-spot-passphrase",
                "binance-perp-api-key",
                "binance-perp-private-key",
                "binance-perp-passphrase",
                "okx-api-key",
                "okx-secret-key",
                "okx-passphrase",
                "main-account",
                "agent-private-key",
                "longbridge-app-key",
                "longbridge-app-secret",
                "longbridge-access-token",
            ]
            .iter()
            .all(|secret| !output.contains(secret))
        );
    }
}
