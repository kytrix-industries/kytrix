use base64::{Engine, engine::general_purpose::STANDARD};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use smol_str::SmolStr;
type HmacSha256 = Hmac<Sha256>;
#[derive(Clone)]
pub struct OkxSigner {
    secret: SmolStr,
}

impl std::fmt::Debug for OkxSigner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OkxSigner")
            .field("secret", &"<redacted>")
            .finish()
    }
}
impl OkxSigner {
    pub fn new(secret: impl Into<SmolStr>) -> Self {
        Self {
            secret: secret.into(),
        }
    }
    pub fn sign(&self, payload: &str) -> SmolStr {
        let mut mac = HmacSha256::new_from_slice(self.secret.as_bytes())
            .expect("HMAC accepts every key length");
        mac.update(payload.as_bytes());
        STANDARD.encode(mac.finalize().into_bytes()).into()
    }

    /// The `OK-ACCESS-SIGN` value of one signed REST request.
    ///
    /// OKX signs the concatenation of the timestamp, the method and the request
    /// path together with its query, and nothing else: a body is signed only by
    /// the requests that carry one.
    pub fn request_signature(
        &self,
        timestamp: &str,
        method: &str,
        path_and_query: &str,
    ) -> SmolStr {
        self.sign(&format!("{timestamp}{method}{path_and_query}"))
    }

    pub fn login_signature(&self, timestamp: &str) -> SmolStr {
        self.sign(&format!("{timestamp}GET/users/self/verify"))
    }
    pub fn login_message(
        &self,
        api_key: impl Into<SmolStr>,
        passphrase: impl Into<SmolStr>,
        timestamp: impl Into<SmolStr>,
    ) -> super::types::OkxLoginMessage {
        let timestamp = timestamp.into();
        super::types::OkxLoginMessage {
            op: "login".into(),
            args: vec![super::types::OkxLoginArg {
                api_key: api_key.into(),
                passphrase: passphrase.into(),
                sign: self.login_signature(&timestamp),
                timestamp,
            }],
        }
    }
    pub fn login_acknowledged(
        &self,
        ack: &super::types::OkxLoginAck,
        route: &market_participants_model::listing::route::VenueRoute,
    ) -> Result<(), crate::error::MarketParticipantError> {
        let event = ack.event.as_deref().ok_or_else(|| {
            crate::error::MarketParticipantError::AuthenticationFailed {
                route: route.clone(),
                account: None,
                source: crate::error::AuthError::ResponseMalformed,
            }
        })?;
        let code = ack.code.as_deref().ok_or_else(|| {
            crate::error::MarketParticipantError::AuthenticationFailed {
                route: route.clone(),
                account: None,
                source: crate::error::AuthError::ResponseMalformed,
            }
        })?;
        if event != "login" || code != "0" {
            return Err(crate::error::MarketParticipantError::AuthenticationFailed {
                route: route.clone(),
                account: None,
                source: crate::error::AuthError::InvalidCredentials,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use hmac::{Hmac, KeyInit, Mac};
    use market_participants_model::{
        identifier::{VenueId, VenueScopeId},
        listing::route::VenueRoute,
    };
    use sha2::Sha256;

    use super::OkxSigner;
    use crate::adapter::okx::OkxLoginAck;

    type HmacSha256 = Hmac<Sha256>;

    /// The route whose credential is being authenticated.
    fn route() -> VenueRoute {
        VenueRoute::new(
            VenueId::Okx,
            VenueScopeId::new("spot").expect("the scope id is canonical"),
        )
    }

    #[test]
    fn okx_login_message_uses_documented_hmac_payload() {
        let signer = OkxSigner::new("secret");
        let timestamp = "1597026383085";
        let mut mac = HmacSha256::new_from_slice(b"secret").unwrap();
        mac.update(format!("{timestamp}GET/users/self/verify").as_bytes());
        let expected = STANDARD.encode(mac.finalize().into_bytes());
        let message = signer.login_message("api-key", "passphrase", timestamp);
        assert_eq!(message.op, "login");
        assert_eq!(message.args[0].api_key, "api-key");
        assert_eq!(message.args[0].passphrase, "passphrase");
        assert_eq!(message.args[0].timestamp, timestamp);
        assert_eq!(message.args[0].sign, expected);
    }

    /// A signed REST request signs the timestamp, the method and the path with
    /// its query, which is what OKX verifies server-side.
    #[test]
    fn okx_request_signature_covers_timestamp_method_and_path() {
        let signer = OkxSigner::new("secret");
        let signature = signer.request_signature("1700000000000", "GET", "/api/v5/account/balance");

        let mut mac = HmacSha256::new_from_slice(b"secret").unwrap();
        mac.update(b"1700000000000GET/api/v5/account/balance");
        let expected = STANDARD.encode(mac.finalize().into_bytes());
        assert_eq!(signature, expected);
    }

    #[test]
    fn okx_login_acknowledgment_requires_login_and_success_code() {
        let signer = OkxSigner::new("secret");
        assert!(
            signer
                .login_acknowledged(
                    &OkxLoginAck {
                        event: Some("login".into()),
                        code: Some("0".into()),
                        msg: None,
                    },
                    &route(),
                )
                .is_ok()
        );
        assert!(
            signer
                .login_acknowledged(
                    &OkxLoginAck {
                        event: Some("error".into()),
                        code: Some("60009".into()),
                        msg: Some("login failed".into()),
                    },
                    &route(),
                )
                .is_err()
        );
    }

    #[test]
    fn okx_login_acknowledgment_rejects_missing_fields() {
        let signer = OkxSigner::new("secret");
        let error = signer
            .login_acknowledged(
                &OkxLoginAck {
                    event: None,
                    code: None,
                    msg: None,
                },
                &route(),
            )
            .expect_err("an empty acknowledgement must not authenticate");
        assert!(matches!(
            error,
            crate::error::MarketParticipantError::AuthenticationFailed {
                source: crate::error::AuthError::ResponseMalformed,
                ..
            }
        ));
    }

    /// A refusal names the route that was being authenticated, so a caller can
    /// tell which route's credential failed.
    #[test]
    fn okx_login_refusal_names_the_authenticated_target() {
        let signer = OkxSigner::new("secret");
        let route = route();
        let error = signer
            .login_acknowledged(
                &OkxLoginAck {
                    event: Some("error".into()),
                    code: Some("60009".into()),
                    msg: Some("login failed".into()),
                },
                &route,
            )
            .expect_err("a refused login must not authenticate");

        assert!(matches!(
            error,
            crate::error::MarketParticipantError::AuthenticationFailed {
                route: failed,
                source: crate::error::AuthError::InvalidCredentials,
                ..
            } if failed == route
        ));
    }

    #[test]
    fn okx_signer_debug_redacts_secret() {
        let signer = OkxSigner::new("secret-value");
        let debug = format!("{signer:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("secret-value"));
    }
}
