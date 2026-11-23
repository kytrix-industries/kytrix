use std::fmt;

use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BinanceMarketCredentials {
    #[serde(rename = "api_key")]
    pub api_key: SmolStr,
    #[serde(rename = "private_key")]
    pub private_key: SmolStr,
    #[serde(rename = "passphrase")]
    pub passphrase: SmolStr,
}

impl BinanceMarketCredentials {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BinanceCredentialMarket {
    Spot,
    UsdMPerp,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OkxCredentials {
    #[serde(rename = "api_key")]
    pub api_key: SmolStr,
    #[serde(rename = "secret_key")]
    pub secret_key: SmolStr,
    #[serde(rename = "passphrase")]
    pub passphrase: SmolStr,
}

impl OkxCredentials {
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

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HyperliquidCredentials {
    #[serde(rename = "main_account_address")]
    pub main_account_address: SmolStr,
    #[serde(rename = "api_agent_private_key")]
    pub api_agent_private_key: SmolStr,
}

impl HyperliquidCredentials {
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

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LongbridgeCredentials {
    #[serde(rename = "app_key")]
    pub app_key: SmolStr,
    #[serde(rename = "app_secret")]
    pub app_secret: SmolStr,
    #[serde(rename = "access_token")]
    pub access_token: SmolStr,
}

impl LongbridgeCredentials {
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

    #[test]
    fn a_market_set_is_complete_without_its_optional_passphrase() {
        assert!(market_credentials("api-key", "private-key", "").is_complete());
        assert!(market_credentials("api-key", "private-key", "passphrase").is_complete());
        assert!(!market_credentials("", "private-key", "passphrase").is_complete());
        assert!(!market_credentials("api-key", "  ", "passphrase").is_complete());
        assert!(!market_credentials("", "", "").is_complete());
    }

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
