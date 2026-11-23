use hmac::{KeyInit as _, Mac as _};
use sha1::Digest as _;
use smol_str::{SmolStr, format_smolstr};

const ALGORITHM: &str = "HMAC-SHA256";
const AUTHORIZATION_HEADER: &str = "authorization";
const API_KEY_HEADER: &str = "x-api-key";
const TIMESTAMP_HEADER: &str = "x-timestamp";
const SIGNATURE_HEADER: &str = "x-api-signature";
const SIGNED_HEADERS: &str = "authorization;x-api-key;x-timestamp";

pub(super) struct RequestSigning<'a> {
    method: &'a str,
    path: &'a str,
    query: &'a str,
    body: Option<&'a [u8]>,
}

impl<'a> RequestSigning<'a> {
    pub(super) const fn new(
        method: &'a str,
        path: &'a str,
        query: &'a str,
        body: Option<&'a [u8]>,
    ) -> Self {
        Self {
            method,
            path,
            query,
            body,
        }
    }

    pub(super) fn sign(
        &self,
        credentials: &crate::runtime::LongbridgeCredentialValues,
        timestamp: u64,
    ) -> Vec<(&'static str, SmolStr)> {
        let body_digest = self.body.map(hex_sha1).unwrap_or_default();
        let signed_values = format_smolstr!(
            "{AUTHORIZATION_HEADER}:{}\n{API_KEY_HEADER}:{}\n{TIMESTAMP_HEADER}:{timestamp}\n",
            credentials.access_token,
            credentials.app_key
        );
        let signature = hex_hmac_sha256(
            &credentials.app_secret,
            &format_smolstr!(
                "{ALGORITHM}|{}",
                hex_sha1(
                    format_smolstr!(
                        "{}|{}|{}|{signed_values}|{SIGNED_HEADERS}|{body_digest}",
                        self.method,
                        self.path,
                        self.query
                    )
                    .as_bytes()
                )
            ),
        );
        vec![
            (AUTHORIZATION_HEADER, credentials.access_token.clone()),
            (API_KEY_HEADER, credentials.app_key.clone()),
            (TIMESTAMP_HEADER, format_smolstr!("{timestamp}")),
            (
                SIGNATURE_HEADER,
                format_smolstr!(
                    "{ALGORITHM} SignedHeaders={SIGNED_HEADERS}, Signature={signature}"
                ),
            ),
        ]
    }
}

fn hex_sha1(message: &[u8]) -> SmolStr {
    let digest = sha1::Sha1::digest(message);
    SmolStr::from(const_hex::encode(digest))
}

pub(super) fn hex_hmac_sha256(secret: &str, message: &str) -> SmolStr {
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes())
        .expect("an HMAC key of any length");
    mac.update(message.as_bytes());
    SmolStr::from(const_hex::encode(mac.finalize().into_bytes()))
}

#[cfg(test)]
mod tests {
    use smol_str::SmolStr;

    use super::{RequestSigning, hex_hmac_sha256, hex_sha1};
    use crate::runtime::LongbridgeCredentialValues;

    fn credentials() -> LongbridgeCredentialValues {
        LongbridgeCredentialValues {
            app_key: SmolStr::new_static("test-app-key"),
            app_secret: SmolStr::new_static("test-app-secret"),
            access_token: SmolStr::new_static("test-access-token"),
        }
    }

    const TIMESTAMP: u64 = 1_700_000_000;

    const SIGNED_VALUES: &str =
        "authorization:test-access-token\nx-api-key:test-app-key\nx-timestamp:1700000000\n";

    #[test]
    fn the_string_to_sign_is_the_six_fields_in_the_venues_own_order() {
        let composed =
            format!("GET|/v1/socket/token||{SIGNED_VALUES}|authorization;x-api-key;x-timestamp|");
        assert_eq!(
            composed, STRING_TO_SIGN,
            "the four stated components compose the literal string to sign"
        );
        assert_eq!(
            composed.split('|').collect::<Vec<_>>(),
            vec![
                "GET",
                "/v1/socket/token",
                "",
                SIGNED_VALUES,
                "authorization;x-api-key;x-timestamp",
                "",
            ],
            "six fields: the method, the path, the empty query, the values, the headers, \
             and the empty field the trailing bar closes"
        );

        let with_body = format!("{composed}{}", hex_sha1(b""));
        assert_eq!(
            composed.matches('|').count(),
            with_body.matches('|').count(),
            "the body's digest is appended after the trailing bar rather than folded in"
        );
        assert_eq!(
            with_body.len(),
            composed.len() + 40,
            "and what is appended is one SHA-1 in hexadecimal: {}",
            hex_sha1(b"")
        );
        assert_ne!(
            hex_sha1(composed.as_bytes()),
            hex_sha1(with_body.as_bytes()),
            "so a request with a body never signs the layout of one without"
        );
    }

    #[test]
    fn the_digest_is_sha1_in_lowercase_hex() {
        assert_eq!(
            hex_sha1(b"abc"),
            "a9993e364706816aba3e25717850c26c9cd0d89d",
            "SHA-1 of `abc` is the standard's own vector"
        );
        assert_eq!(
            hex_sha1(b""),
            "da39a3ee5e6b4b0d3255bfef95601890afd80709",
            "and the empty message has the one digest SHA-1 states for it"
        );
    }

    const STRING_TO_SIGN: &str = "GET|/v1/socket/token||authorization:test-access-token\nx-api-key:test-app-key\n\
         x-timestamp:1700000000\n|authorization;x-api-key;x-timestamp|";

    #[test]
    fn the_signature_of_the_known_vector_is_pinned_by_value() {
        let digest = hex_sha1(STRING_TO_SIGN.as_bytes());
        assert_eq!(
            digest, "6897581b5b8ad65da86259b5bdadd39e4fc84e22",
            "SHA-1 of the stated string to sign"
        );
        assert_eq!(digest.len(), 40, "SHA-1 is 20 bytes of hex: {digest}");
        assert!(
            digest
                .chars()
                .all(|digit| digit.is_ascii_digit() || digit.is_ascii_lowercase()),
            "the digest the venue reads back is lowercase hexadecimal: {digest}"
        );

        let signatures = RequestSigning::new("GET", "/v1/socket/token", "", None)
            .sign(&credentials(), TIMESTAMP);
        let presented = &signatures[3].1;
        assert_eq!(
            presented,
            "HMAC-SHA256 SignedHeaders=authorization;x-api-key;x-timestamp, Signature=\
             a2591ebb77eafa31ebb5d6982735a974edd6f59181bf314592ec8539042f159e",
            "the signature header is the algorithm's own line, and the signature is \
             HMAC-SHA256(app_secret, \"HMAC-SHA256|\" + the digest above) in lowercase hex"
        );
        assert_eq!(
            hex_hmac_sha256(&credentials().app_secret, &format!("HMAC-SHA256|{digest}")),
            "a2591ebb77eafa31ebb5d6982735a974edd6f59181bf314592ec8539042f159e",
            "which is the same hexadecimal computed from the digest alone"
        );
    }

    #[test]
    fn the_signed_request_presents_the_three_values_and_their_signature() {
        let presented = RequestSigning::new("GET", "/v1/socket/token", "", None)
            .sign(&credentials(), TIMESTAMP);
        let names: Vec<&str> = presented.iter().map(|(name, _)| *name).collect();
        assert_eq!(
            names,
            vec![
                "authorization",
                "x-api-key",
                "x-timestamp",
                "x-api-signature"
            ],
            "the signature covers three headers and is the fourth"
        );
        assert_eq!(presented[0].1, "test-access-token", "the token is bare");
        assert_eq!(presented[1].1, "test-app-key");
        assert_eq!(presented[2].1, "1700000000", "the instant is Unix seconds");
        let signature = &presented[3].1;
        assert!(
            signature.starts_with(
                "HMAC-SHA256 SignedHeaders=authorization;x-api-key;x-timestamp, Signature="
            ),
            "the header line is the algorithm's own: {signature}"
        );
        let hex = signature
            .rsplit_once("Signature=")
            .expect("the line states its signature")
            .1;
        assert_eq!(hex.len(), 64, "SHA-256 is 32 bytes of hex");
        assert!(
            hex.chars()
                .all(|digit| digit.is_ascii_digit() || digit.is_ascii_lowercase()),
            "the signature is lowercase hexadecimal"
        );
    }

    #[test]
    fn two_instants_are_two_signatures() {
        let signing = RequestSigning::new("GET", "/v1/socket/token", "", None);
        let first = signing.sign(&credentials(), TIMESTAMP);
        let second = signing.sign(&credentials(), TIMESTAMP + 1);
        assert_ne!(
            first[3].1, second[3].1,
            "the signature covers the instant it travels with"
        );
        assert_eq!(first[2].1, "1700000000");
        assert_eq!(second[2].1, "1700000001");
    }

    #[test]
    fn a_body_and_a_query_change_the_signature() {
        let without = RequestSigning::new("GET", "/v1/socket/token", "", None)
            .sign(&credentials(), TIMESTAMP);
        let with_query = RequestSigning::new("GET", "/v1/socket/token", "a=1", None)
            .sign(&credentials(), TIMESTAMP);
        let with_body = RequestSigning::new("GET", "/v1/socket/token", "", Some(b"{}"))
            .sign(&credentials(), TIMESTAMP);

        assert_ne!(without[3].1, with_query[3].1, "the query is signed");
        assert_ne!(without[3].1, with_body[3].1, "the body is signed");
        assert_ne!(with_query[3].1, with_body[3].1);
    }
}
