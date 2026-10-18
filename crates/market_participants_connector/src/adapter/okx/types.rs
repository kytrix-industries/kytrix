use std::fmt;

use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct OkxArg {
    pub channel: SmolStr,
    /// The instrument a channel addresses, for the channels addressed by one.
    /// A field the channel does not use is sent as an explicit `null`, which is
    /// the shape OKX accepts for it.
    #[serde(rename = "instId")]
    pub inst_id: Option<SmolStr>,
    /// The instrument type a channel selects its data with; OKX refuses the
    /// subscription when a channel that needs it is sent without it.
    #[serde(rename = "instType")]
    pub inst_type: Option<SmolStr>,
}
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OkxLoginArg {
    #[serde(rename = "apiKey")]
    pub api_key: SmolStr,
    pub passphrase: SmolStr,
    pub timestamp: SmolStr,
    pub sign: SmolStr,
}
impl fmt::Debug for OkxLoginArg {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OkxLoginArg")
            .field("api_key", &"<redacted>")
            .field("passphrase", &"<redacted>")
            .field("timestamp", &self.timestamp)
            .field("sign", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OkxLoginMessage {
    pub op: SmolStr,
    pub args: Vec<OkxLoginArg>,
}
impl fmt::Debug for OkxLoginMessage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OkxLoginMessage")
            .field("op", &self.op)
            .field("args", &self.args)
            .finish()
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct OkxLoginAck {
    pub event: Option<SmolStr>,
    pub code: Option<SmolStr>,
    pub msg: Option<SmolStr>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct OkxResponse<T> {
    pub code: SmolStr,
    pub msg: Option<SmolStr>,
    pub data: Vec<T>,
}
/// The envelope OKX wraps a refused REST request in.
///
/// It carries no rows at all, which is what separates it from [`OkxResponse`]:
/// the failure is described by the code and the message alone.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct OkxErrorEnvelope {
    pub code: SmolStr,
    pub msg: Option<SmolStr>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct OkxOrderRequest {
    #[serde(rename = "instId")]
    pub inst_id: SmolStr,
    #[serde(rename = "tdMode")]
    pub td_mode: SmolStr,
    pub side: SmolStr,
    #[serde(rename = "ordType")]
    pub ord_type: SmolStr,
    pub sz: SmolStr,
    pub px: Option<SmolStr>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct OkxCancelOrderRequest {
    #[serde(rename = "instId")]
    pub inst_id: SmolStr,
    #[serde(rename = "ordId")]
    pub ord_id: SmolStr,
}

#[cfg(test)]
mod tests {
    use super::{OkxLoginArg, OkxLoginMessage};

    #[test]
    fn login_debug_redacts_credentials_and_signature() {
        let message = OkxLoginMessage {
            op: "login".into(),
            args: vec![OkxLoginArg {
                api_key: "api-secret".into(),
                passphrase: "pass-secret".into(),
                timestamp: "1".into(),
                sign: "signature-secret".into(),
            }],
        };
        let debug = format!("{message:?}");
        assert!(!debug.contains("api-secret"));
        assert!(!debug.contains("pass-secret"));
        assert!(!debug.contains("signature-secret"));
        assert!(debug.contains("<redacted>"));
    }
}
