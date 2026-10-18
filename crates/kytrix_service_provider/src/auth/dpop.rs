use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use p256::{
    ecdsa::{Signature, SigningKey, signature::Signer as _},
    elliptic_curve::Generate as _,
};
use rand::TryRng as _;
use serde::Serialize;
use sha2::{Digest as _, Sha256};

use super::{AuthError, timestamp};

#[derive(Clone)]
pub struct DpopKey(SigningKey);

#[derive(Serialize)]
struct DpopHeader<'a> {
    typ: &'static str,
    alg: &'static str,
    jwk: Jwk<'a>,
}

#[derive(Serialize)]
struct Jwk<'a> {
    kty: &'static str,
    crv: &'static str,
    x: &'a str,
    y: &'a str,
}

#[derive(Serialize)]
struct DpopClaims<'a> {
    jti: String,
    htm: &'a str,
    htu: &'a str,
    iat: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    ath: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    nonce: Option<&'a str>,
}

impl DpopKey {
    pub(super) const fn from_signing_key(key: SigningKey) -> Self {
        Self(key)
    }

    const fn signing_key(&self) -> &SigningKey {
        &self.0
    }

    pub fn generate() -> Result<Self, AuthError> {
        SigningKey::try_generate()
            .map(DpopKey::from_signing_key)
            .map_err(|_| AuthError::SecureRandom)
    }

    pub fn thumbprint(&self) -> String {
        let (x, y) = self.coordinates();
        URL_SAFE_NO_PAD.encode(Sha256::digest(format!(
            r#"{{"crv":"P-256","kty":"EC","x":"{x}","y":"{y}"}}"#
        )))
    }

    pub(super) fn proof(
        &self,
        method: &str,
        uri: &str,
        access_token: Option<&str>,
        nonce: Option<&str>,
    ) -> Result<String, AuthError> {
        let (x, y) = self.coordinates();
        let header = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&DpopHeader {
                typ: "dpop+jwt",
                alg: "ES256",
                jwk: Jwk {
                    kty: "EC",
                    crv: "P-256",
                    x: &x,
                    y: &y,
                },
            })
            .map_err(|_| AuthError::InvalidResponse)?,
        );
        let claims = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&DpopClaims {
                jti: random_token()?,
                htm: method,
                htu: uri,
                iat: timestamp()?,
                ath: access_token.map(|token| URL_SAFE_NO_PAD.encode(Sha256::digest(token))),
                nonce,
            })
            .map_err(|_| AuthError::InvalidResponse)?,
        );
        let signing_input = format!("{header}.{claims}");
        let signature: Signature = self.signing_key().sign(signing_input.as_bytes());
        Ok(format!(
            "{signing_input}.{}",
            URL_SAFE_NO_PAD.encode(signature.to_bytes())
        ))
    }

    pub(super) fn coordinates(&self) -> (String, String) {
        let point = self.signing_key().verifying_key().to_sec1_point(false);
        let bytes = point.as_ref();
        debug_assert_eq!(bytes.len(), 65);
        (
            URL_SAFE_NO_PAD.encode(&bytes[1..33]),
            URL_SAFE_NO_PAD.encode(&bytes[33..65]),
        )
    }

    pub(super) fn encode_private(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.signing_key().to_bytes())
    }

    pub(super) fn decode_private(value: &str) -> Result<Self, AuthError> {
        let bytes = URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|_| AuthError::InvalidCredential)?;
        SigningKey::from_slice(&bytes)
            .map(DpopKey::from_signing_key)
            .map_err(|_| AuthError::InvalidCredential)
    }
}

fn random_token() -> Result<String, AuthError> {
    let mut bytes = [0_u8; 32];
    rand::rngs::SysRng
        .try_fill_bytes(&mut bytes)
        .map_err(|_| AuthError::SecureRandom)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

#[cfg(test)]
mod tests {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use p256::ecdsa::{Signature, SigningKey, signature::Verifier as _};
    use sha2::Sha256;

    use super::*;

    #[test]
    fn dpop_proof_is_es256_and_binds_method_uri_and_access_token() {
        let key = DpopKey::from_signing_key(SigningKey::generate());
        let proof = key
            .proof("GET", "https://api.kytrix.xyz/", Some("access-token"), None)
            .unwrap();
        let parts = proof.split('.').collect::<Vec<_>>();
        assert_eq!(parts.len(), 3);
        let header: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap();
        let claims: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(header["typ"], "dpop+jwt");
        assert_eq!(header["alg"], "ES256");
        assert_eq!(claims["htm"], "GET");
        assert_eq!(claims["htu"], "https://api.kytrix.xyz/");
        assert_eq!(
            claims["ath"],
            URL_SAFE_NO_PAD.encode(Sha256::digest(b"access-token"))
        );
        let signature = Signature::from_slice(&URL_SAFE_NO_PAD.decode(parts[2]).unwrap()).unwrap();
        key.0
            .verifying_key()
            .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
            .unwrap();
    }

    #[test]
    fn thumbprint_uses_the_rfc_7638_member_order() {
        let key = DpopKey::from_signing_key(SigningKey::from_slice(&[1_u8; 32]).unwrap());
        let (x, y) = key.coordinates();
        let expected = URL_SAFE_NO_PAD.encode(Sha256::digest(format!(
            r#"{{"crv":"P-256","kty":"EC","x":"{x}","y":"{y}"}}"#
        )));
        assert_eq!(key.thumbprint(), expected);
    }
}
