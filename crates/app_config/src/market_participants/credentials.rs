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
//! A venue — or one of Binance's markets — holding no set is *unconfigured*: the
//! settings store nothing for it rather than an empty set, and such a request fails
//! closed. A stored set that is missing a required value is rejected, so a half-filled
//! set can never sign a request.

use std::fmt;

use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

/// One market's Binance credential set.
///
/// The three values are one credential: an API key without its private key signs
/// nothing, and neither is stored apart from the passphrase its key was created with.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BinanceMarketCredentials {
    /// The market API's Ed25519 API key.
    #[serde(rename = "api_key")]
    pub api_key: SmolStr,
    /// The market API's Ed25519 private key.
    #[serde(rename = "private_key")]
    pub private_key: SmolStr,
    /// The market API's optional Ed25519 private-key passphrase.
    #[serde(rename = "passphrase")]
    pub passphrase: SmolStr,
}

impl BinanceMarketCredentials {
    /// Reports whether the two required values are present.
    ///
    /// The passphrase is optional: Binance creates a key with or without one, so it
    /// is part of the credential only when the operator's key has one.
    pub fn is_complete(&self) -> bool {
        !self.api_key.trim().is_empty() && !self.private_key.trim().is_empty()
    }
}

impl fmt::Debug for BinanceMarketCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BinanceMarketCredentials")
            .field("api_key", &"<redacted>")
            .field("private_key", &"<redacted>")
            .field("passphrase", &"<redacted>")
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

/// OKX API credentials.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HyperliquidCredentials {
    /// Main account address whose state and orders the API Agent controls.
    #[serde(rename = "main_account_address")]
    pub main_account_address: SmolStr,
    /// API Agent secp256k1 private key.
    #[serde(rename = "api_agent_private_key")]
    pub api_agent_private_key: SmolStr,
}

impl HyperliquidCredentials {
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
/// set that holds fewer than all three signs nothing.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn market_credentials(
        api_key: &str,
        private_key: &str,
        passphrase: &str,
    ) -> BinanceMarketCredentials {
        BinanceMarketCredentials {
            api_key: SmolStr::new(api_key),
            private_key: SmolStr::new(private_key),
            passphrase: SmolStr::new(passphrase),
        }
    }

    /// A market's credential set is the three values it signs with, and a market the
    /// operator configured none of stores nothing at all rather than an empty set.
    #[test]
    fn a_market_set_should_encode_the_values_it_signs_with() {
        let stored = market_credentials("api-key", "private-key", "passphrase");
        let wire = json!({
            "api_key": "api-key",
            "private_key": "private-key",
            "passphrase": "passphrase",
        });

        assert_eq!(
            serde_json::to_value(Some(&stored)).expect("a stored set serializes"),
            wire
        );
        assert_eq!(
            serde_json::to_value(Option::<&BinanceMarketCredentials>::None)
                .expect("an unconfigured market serializes"),
            serde_json::Value::Null,
            "a market nothing was typed into stores nothing"
        );
        assert_eq!(
            serde_json::from_value::<Option<BinanceMarketCredentials>>(wire)
                .expect("a stored set decodes"),
            Some(stored)
        );
        assert_eq!(
            serde_json::from_value::<Option<BinanceMarketCredentials>>(serde_json::Value::Null)
                .expect("nothing decodes"),
            None
        );
    }

    /// The two required values are what makes a Binance market's set complete; the
    /// passphrase is optional, and a value of only whitespace is no value at all.
    #[test]
    fn a_market_set_is_complete_without_its_optional_passphrase() {
        assert!(market_credentials("api-key", "private-key", "").is_complete());
        assert!(market_credentials("api-key", "private-key", "passphrase").is_complete());
        assert!(!market_credentials("", "private-key", "passphrase").is_complete());
        assert!(!market_credentials("api-key", "  ", "passphrase").is_complete());
        assert!(!market_credentials("", "", "").is_complete());
    }

    /// Every other venue's set is complete only when every one of its values is present.
    #[test]
    fn every_venue_set_is_complete_only_when_every_value_is_present() {
        let okx = OkxCredentials {
            api_key: SmolStr::new_static("api-key"),
            secret_key: SmolStr::new_static("secret-key"),
            passphrase: SmolStr::new_static("passphrase"),
        };
        let hyperliquid = HyperliquidCredentials {
            main_account_address: SmolStr::new_static("0xmain"),
            api_agent_private_key: SmolStr::new_static("agent-private"),
        };
        let longbridge = LongbridgeCredentials {
            app_key: SmolStr::new_static("app-key"),
            app_secret: SmolStr::new_static("app-secret"),
            access_token: SmolStr::new_static("access-token"),
        };

        assert!(
            okx.is_complete() && hyperliquid.is_complete() && longbridge.is_complete(),
            "a set holding every value it signs with is complete"
        );
        assert!(
            !OkxCredentials {
                secret_key: SmolStr::default(),
                ..okx
            }
            .is_complete(),
            "a set missing a value is not complete"
        );
        assert!(
            !HyperliquidCredentials {
                api_agent_private_key: SmolStr::default(),
                ..hyperliquid
            }
            .is_complete(),
            "a set missing a value is not complete"
        );
        assert!(
            !LongbridgeCredentials {
                access_token: SmolStr::default(),
                ..longbridge
            }
            .is_complete(),
            "a set missing a value is not complete"
        );
    }

    /// Each set stores the field names its venue signs with, and decodes back into the
    /// same values.
    #[test]
    fn sets_should_encode_the_fields_they_sign_with() {
        let cases = [
            (
                serde_json::to_value(BinanceMarketCredentials {
                    api_key: SmolStr::new_static("api-key"),
                    private_key: SmolStr::new_static("private-key"),
                    passphrase: SmolStr::new_static("passphrase"),
                })
                .expect("a Binance set serializes"),
                json!({
                    "api_key": "api-key",
                    "private_key": "private-key",
                    "passphrase": "passphrase",
                }),
            ),
            (
                serde_json::to_value(OkxCredentials {
                    api_key: SmolStr::new_static("okx-key"),
                    secret_key: SmolStr::new_static("okx-secret"),
                    passphrase: SmolStr::new_static("okx-passphrase"),
                })
                .expect("an OKX set serializes"),
                json!({
                    "api_key": "okx-key",
                    "secret_key": "okx-secret",
                    "passphrase": "okx-passphrase",
                }),
            ),
            (
                serde_json::to_value(HyperliquidCredentials {
                    main_account_address: SmolStr::new_static("0xmain"),
                    api_agent_private_key: SmolStr::new_static("agent-private"),
                })
                .expect("a HyperCore set serializes"),
                json!({
                    "main_account_address": "0xmain",
                    "api_agent_private_key": "agent-private",
                }),
            ),
            (
                serde_json::to_value(LongbridgeCredentials {
                    app_key: SmolStr::new_static("app-key"),
                    app_secret: SmolStr::new_static("app-secret"),
                    access_token: SmolStr::new_static("access-token"),
                })
                .expect("a Longbridge set serializes"),
                json!({
                    "app_key": "app-key",
                    "app_secret": "app-secret",
                    "access_token": "access-token",
                }),
            ),
        ];

        for (encoded, wire) in cases {
            assert_eq!(encoded, wire);
        }

        assert_eq!(
            serde_json::from_value::<OkxCredentials>(
                json!({ "api_key": "okx-key", "secret_key": "okx-secret", "passphrase": "pass" })
            )
            .expect("an OKX set decodes"),
            OkxCredentials {
                api_key: SmolStr::new_static("okx-key"),
                secret_key: SmolStr::new_static("okx-secret"),
                passphrase: SmolStr::new_static("pass"),
            }
        );
    }

    /// The shape this module replaced is not one a file can carry: the six values a
    /// Binance row used to hold under one set are not a market's set, and a set that is
    /// missing a value it signs with is refused rather than decoded incomplete.
    #[test]
    fn a_superseded_credential_shape_should_not_decode() {
        for removed in [
            json!({
                "spot_api_key": "key",
                "spot_private_key": "private",
                "spot_passphrase": "",
                "usd_m_perp_api_key": "key",
                "usd_m_perp_private_key": "private",
                "usd_m_perp_passphrase": "",
            }),
            json!({ "kind": "binance", "api_key": "key", "private_key": "private" }),
            json!({ "api_key": "key" }),
            json!({}),
        ] {
            let decoded = serde_json::from_value::<BinanceMarketCredentials>(removed.clone());
            if let Ok(decoded) = decoded {
                panic!("{removed} decoded into {decoded:?}");
            }
        }
    }

    #[test]
    fn debug_output_should_redact_every_market_participant_secret() {
        let binance = BinanceMarketCredentials {
            api_key: SmolStr::new_static("binance-api-key"),
            private_key: SmolStr::new_static("binance-private-key"),
            passphrase: SmolStr::new_static("binance-passphrase"),
        };
        let okx = OkxCredentials {
            api_key: SmolStr::new_static("okx-api-key"),
            secret_key: SmolStr::new_static("okx-secret-key"),
            passphrase: SmolStr::new_static("okx-passphrase"),
        };
        let hyperliquid = HyperliquidCredentials {
            main_account_address: SmolStr::new_static("main-account"),
            api_agent_private_key: SmolStr::new_static("agent-private-key"),
        };
        let longbridge = LongbridgeCredentials {
            app_key: SmolStr::new_static("longbridge-app-key"),
            app_secret: SmolStr::new_static("longbridge-app-secret"),
            access_token: SmolStr::new_static("longbridge-access-token"),
        };

        let output = format!("{binance:?} {okx:?} {hyperliquid:?} {longbridge:?}");

        assert!(
            [
                "binance-api-key",
                "binance-private-key",
                "binance-passphrase",
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
