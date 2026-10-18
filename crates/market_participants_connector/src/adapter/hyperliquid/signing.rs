use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use alloy_primitives::{Address, B256, keccak256};
use alloy_sol_types::{SolStruct, eip712_domain, sol};
use k256::ecdsa::SigningKey;
use market_participants_model::identifier::VenueId;
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

use crate::error::MarketParticipantError;

sol! { struct Agent { string source; bytes32 connectionId; } }

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Eip712Signature {
    pub r: B256,
    pub s: B256,
    pub v: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedAction<T> {
    pub action: T,
    pub nonce: u64,
    pub signature: Eip712Signature,
}

#[derive(Clone)]
pub struct Eip712Signer {
    private_key: SmolStr,
}

impl std::fmt::Debug for Eip712Signer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Eip712Signer")
            .field("private_key", &"<redacted>")
            .finish()
    }
}

impl Eip712Signer {
    pub fn new(private_key: impl Into<SmolStr>) -> Self {
        Self {
            private_key: private_key.into(),
        }
    }
    pub fn sign_agent(
        &self,
        source: &str,
        connection_id: B256,
    ) -> Result<Eip712Signature, MarketParticipantError> {
        self.sign_digest(agent_digest(source, connection_id))
    }
    pub fn attach<T: Serialize>(
        &self,
        action: T,
        nonce: u64,
    ) -> Result<SignedAction<T>, MarketParticipantError> {
        let mut encoded = rmp_serde::to_vec_named(&action).map_err(|_| {
            MarketParticipantError::ProtocolError {
                operation: "encode Hyperliquid action".into(),
                reason: "action could not be encoded for signing".into(),
                raw_data: None,
            }
        })?;
        encoded.extend(nonce.to_be_bytes());
        encoded.push(0);
        let signature = self.sign_agent("a", keccak256(encoded))?;
        Ok(SignedAction {
            action,
            nonce,
            signature,
        })
    }
    fn sign_digest(&self, digest: B256) -> Result<Eip712Signature, MarketParticipantError> {
        let key_bytes =
            const_hex::decode_to_array::<_, 32>(self.private_key.trim().trim_start_matches("0x"))
                .map_err(|_| MarketParticipantError::UnusableCredentials {
                venue: VenueId::Hyperliquid,
            })?;
        let signing_key = SigningKey::from_bytes((&key_bytes).into()).map_err(|_| {
            MarketParticipantError::UnusableCredentials {
                venue: VenueId::Hyperliquid,
            }
        })?;
        let (signature, recovery_id) = signing_key.sign_prehash_recoverable(digest.as_slice());
        let bytes = signature.to_bytes();
        Ok(Eip712Signature {
            r: B256::from_slice(&bytes[..32]),
            s: B256::from_slice(&bytes[32..]),
            v: 27 + u64::from(recovery_id.to_byte()),
        })
    }
}

fn agent_digest(source: &str, connection_id: B256) -> B256 {
    let domain = eip712_domain! { name: "Exchange", version: "1", chain_id: 1337, verifying_contract: Address::ZERO, };
    let agent = Agent {
        source: source.to_owned(),
        connectionId: connection_id,
    };
    let mut input = [0_u8; 66];
    input[..2].copy_from_slice(&[0x19, 0x01]);
    input[2..34].copy_from_slice(domain.hash_struct().as_slice());
    input[34..].copy_from_slice(agent.eip712_hash_struct().as_slice());
    keccak256(input)
}

pub struct NonceGenerator {
    next: AtomicU64,
}
impl Default for NonceGenerator {
    fn default() -> Self {
        Self::new()
    }
}
impl NonceGenerator {
    pub const fn new() -> Self {
        Self {
            next: AtomicU64::new(0),
        }
    }
    pub fn next(&self) -> Result<u64, MarketParticipantError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| {
                MarketParticipantError::Internal("system clock is before Unix epoch".into())
            })?
            .as_millis();
        let now = u64::try_from(now)
            .map_err(|_| MarketParticipantError::Internal("nonce overflow".into()))?;
        let mut previous = self.next.load(Ordering::Acquire);
        loop {
            let candidate = now.max(previous.saturating_add(1));
            match self.next.compare_exchange_weak(
                previous,
                candidate,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(candidate),
                Err(observed) => previous = observed,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde::Serialize;

    use super::*;
    use crate::adapter::hyperliquid::{
        HyperliquidLimitOrderType, HyperliquidOrder, HyperliquidOrderAction, HyperliquidOrderType,
    };

    const TEST_PRIVATE_KEY: &str =
        "0000000000000000000000000000000000000000000000000000000000000001";

    #[derive(Serialize)]
    #[serde(tag = "type")]
    enum HyperliquidCancelAction {
        #[serde(rename = "cancel")]
        Cancel { cancels: Vec<HyperliquidCancel> },
    }

    #[derive(Serialize)]
    struct HyperliquidCancel {
        #[serde(rename = "a")]
        asset: u32,
        #[serde(rename = "o")]
        oid: u64,
    }

    #[test]
    fn hyperliquid_attach_signs_the_typed_action_and_nonce() {
        let signer = Eip712Signer::new(TEST_PRIVATE_KEY);
        let action = HyperliquidOrderAction {
            r#type: "order".into(),
            orders: vec![HyperliquidOrder {
                asset: 1,
                is_buy: true,
                sz: "0.1".into(),
                limit_px: "100".into(),
                order_type: HyperliquidOrderType::Limit(HyperliquidLimitOrderType {
                    time_in_force: "Gtc".into(),
                }),
                reduce_only: false,
            }],
            grouping: "na".into(),
        };

        let signed = signer
            .attach(action.clone(), 1_583_838)
            .expect("typed action should be signable");
        let repeated = signer
            .attach(action.clone(), 1_583_838)
            .expect("same action should produce a deterministic signature");
        let changed_nonce = signer
            .attach(action.clone(), 1_583_839)
            .expect("changed nonce should remain signable");

        assert_eq!(signed.action, action);
        assert_eq!(signed.nonce, 1_583_838);
        assert_eq!(signed.signature, repeated.signature);
        assert_ne!(signed.signature, changed_nonce.signature);
        assert!(matches!(signed.signature.v, 27 | 28));
        let wire = serde_json::to_string(&signed).expect("signed action should serialize");
        assert!(wire.contains("\"action\""));
        assert!(wire.contains("\"nonce\":1583838"));
        assert!(wire.contains("\"signature\""));
    }

    #[test]
    fn hyperliquid_order_action_uses_documented_wire_fields() {
        let action = HyperliquidOrderAction {
            r#type: "order".into(),
            orders: vec![HyperliquidOrder {
                asset: 4,
                is_buy: false,
                sz: "0.25".into(),
                limit_px: "101.5".into(),
                order_type: HyperliquidOrderType::Limit(HyperliquidLimitOrderType {
                    time_in_force: "Ioc".into(),
                }),
                reduce_only: true,
            }],
            grouping: "na".into(),
        };
        let wire = serde_json::to_string(&action).expect("order action should serialize");
        assert!(wire.contains("\"a\":4"));
        assert!(wire.contains("\"b\":false"));
        assert!(wire.contains("\"s\":\"0.25\""));
        assert!(wire.contains("\"p\":\"101.5\""));
        assert!(wire.contains("\"t\":{\"limit\":{\"tif\":\"Ioc\"}}"));
        assert!(wire.contains("\"r\":true"));
        assert!(wire.contains("\"grouping\":\"na\""));
    }

    #[test]
    fn hyperliquid_attach_matches_the_documented_cancel_vector() {
        let signer =
            Eip712Signer::new("e908f86dbb4d55ac876378565aafeabc187f6690f046459397b17d9b9a19688e");
        let signed = signer
            .attach(
                HyperliquidCancelAction::Cancel {
                    cancels: vec![HyperliquidCancel {
                        asset: 1,
                        oid: 82_382,
                    }],
                },
                1_583_838,
            )
            .expect("cancel action should be signable");
        let mut encoded = [0_u8; 65];
        encoded[..32].copy_from_slice(signed.signature.r.as_slice());
        encoded[32..64].copy_from_slice(signed.signature.s.as_slice());
        encoded[64] = u8::try_from(signed.signature.v).unwrap();
        assert_eq!(
            const_hex::encode_prefixed(encoded),
            "0x02f76cc5b16e0810152fa0e14e7b219f49c361e3325f771544c6f54e157bf9fa17ed0afc11a98596be85d5cd9f86600aad515337318f7ab346e5ccc1b03425d51b"
        );
    }

    #[test]
    fn eip712_signer_debug_redacts_private_key() {
        let signer = Eip712Signer::new(TEST_PRIVATE_KEY);
        let debug = format!("{signer:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains(TEST_PRIVATE_KEY));
    }
}
