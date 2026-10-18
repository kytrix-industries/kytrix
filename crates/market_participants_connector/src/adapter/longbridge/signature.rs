//! The signature the venue's HTTP surface presents (design §4.9).
//!
//! The venue's socket is authenticated by a one-time password, and that password is
//! fetched over HTTP from an endpoint that is signed rather than merely reached: the
//! live run measured `401 {"code":401001,"message":"token empty"}` for a request that
//! carried only the two routing headers, `403 {"code":403208,"message":"apikey empty"}`
//! once a token but no key was presented, and `403 {"code":403201,"message":"signature
//! invalid"}` when all three headers were presented under a wrong signature. What the
//! endpoint accepts is the algorithm below, stated exactly as it is computed here.
//!
//! The value this module answers is one declaration rather than four values a caller
//! assembles in order: the string to sign names the headers that were signed, and the
//! headers that travel carry that same list, so the two can never disagree.
//!
//! Only the socket-token request is signed. The Nasdaq symbol directory is served by
//! another host entirely and carries no venue header at all, and the socket's own
//! upgrade needs neither — the run measured both.

use hmac::{KeyInit as _, Mac as _};
use sha1::Digest as _;
use smol_str::{SmolStr, format_smolstr};

/// The message-authentication code every signature states.
const ALGORITHM: &str = "HMAC-SHA256";
/// The header the access token travels in, which is also the first name signed.
const AUTHORIZATION_HEADER: &str = "authorization";
/// The header the application key travels in.
const API_KEY_HEADER: &str = "x-api-key";
/// The header the request's own instant travels in.
const TIMESTAMP_HEADER: &str = "x-timestamp";
/// The header the signature itself travels in.
const SIGNATURE_HEADER: &str = "x-api-signature";
/// The header names that are signed, in the order the string to sign lists them.
///
/// The venue reads this list back out of [`SIGNATURE_HEADER`] as the signed-header
/// set, so it is stated once and used twice: as the middle field of the string to
/// sign, and as the list the signature header announces.
const SIGNED_HEADERS: &str = "authorization;x-api-key;x-timestamp";

/// One signed request, as the venue's own algorithm states it.
///
/// The four components are the request's identity as the signature covers it: the
/// method, the path, the query and the body. Everything else — the credentials, the
/// instant and the signature computed from them — belongs to one particular call, and
/// [`RequestSigning::sign`] answers that call's headers.
pub(super) struct RequestSigning<'a> {
    /// The HTTP method, spelled as the venue's own algorithm spells it (`GET`).
    method: &'a str,
    /// The request's path, which is the endpoint's own and carries no query.
    path: &'a str,
    /// The request's query string, or the empty string when it has none.
    query: &'a str,
    /// The request's body, whose SHA-1 is appended to the string to sign when it is
    /// present.
    body: Option<&'a [u8]>,
}

impl<'a> RequestSigning<'a> {
    /// States the identity of one request.
    ///
    /// The body is what decides which of the algorithm's two layouts applies, so it is
    /// a parameter rather than a second constructor: `None` is a request that signs
    /// five fields and a trailing bar, `Some` is one that also signs its body's SHA-1.
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

    /// Signs one call of this request with one credential set and one instant.
    ///
    /// The answer is every header the request presents: the signature and the three
    /// values it covers, in the venue's own spellings. The instant is a parameter
    /// rather than read from the clock here, because the string the signature is
    /// computed over has to state the same instant the header carries, and a test can
    /// pin the composition only if the instant is its input.
    pub(super) fn sign(
        &self,
        credentials: &crate::runtime::LongbridgeCredentialValues,
        timestamp: u64,
    ) -> Vec<(&'static str, SmolStr)> {
        // The request with no body signs the trailing bar and stops; a request with a
        // body appends that body's own digest after it, so the two layouts differ by
        // exactly the field §4.9 adds.
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

/// The lowercase hexadecimal SHA-1 digest of one message.
///
/// The digest is the middle of the string the HMAC is taken over rather than a value
/// the venue ever sees, so its spelling matters exactly as much: an uppercase digest
/// is a different message and therefore a different signature.
fn hex_sha1(message: &[u8]) -> SmolStr {
    let digest = sha1::Sha1::digest(message);
    SmolStr::from(const_hex::encode(digest))
}

/// The lowercase hexadecimal HMAC-SHA256 of one message under one secret.
///
/// The venue's key is the application secret, and the message is the algorithm's own
/// name and a bar in front of the digest above — the framing its client computes and
/// the framing the endpoint measured as accepted. The function is visible to the
/// venue's fixture, which recomputes a request's signature from the headers it
/// received, and to the adapter's own tests.
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

    /// The credentials one vector is signed with.
    ///
    /// They are test values rather than shapes: the composition of the string to sign
    /// is pinned by the digest each layout produces, and that digest is only a
    /// statement about this module if the values it was computed from are stated.
    fn credentials() -> LongbridgeCredentialValues {
        LongbridgeCredentialValues {
            app_key: SmolStr::new_static("test-app-key"),
            app_secret: SmolStr::new_static("test-app-secret"),
            access_token: SmolStr::new_static("test-access-token"),
        }
    }

    /// The instant every vector is signed at.
    const TIMESTAMP: u64 = 1_700_000_000;

    /// The signed values of the vector's credentials at [`TIMESTAMP`].
    const SIGNED_VALUES: &str =
        "authorization:test-access-token\nx-api-key:test-app-key\nx-timestamp:1700000000\n";

    /// The six fields of the string to sign are stated in the venue's own order, and a
    /// request with no body signs a trailing bar and nothing after it.
    ///
    /// This is the layout the live run verified against the venue: the token request
    /// carries no body, so the string is `GET|/v1/socket/token||values|headers|` — two
    /// empty bars where the query and body would be — and a layout that moved a field,
    /// dropped a bar or appended a body's digest would be a different digest.
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

        // The two layouts §4.9 distinguishes: a request with no body ends at the
        // trailing bar, and one with a body appends that body's own SHA-1 after it.
        // The digest is hexadecimal and carries no bar of its own, so the two layouts
        // differ by 40 characters and by nothing else — which is exactly the field the
        // algorithm adds, and why the suffix is stated by value rather than re-derived.
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

    /// The digest really is SHA-1's, checked against the one published vector that
    /// needs no context: FIPS 180-1's `abc`.
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

    /// The exact bytes SHA-1 hashes for the token request, as §4.9 states them.
    ///
    /// This is the message the whole signature rests on, spelled out rather than
    /// composed: `GET|/v1/socket/token||values|headers|` — the method, the path, an
    /// empty query, the three signed values, the signed-header list, and the trailing
    /// bar. Every assertion below compares against this literal, so a change to the
    /// composition reaches them all.
    const STRING_TO_SIGN: &str = "GET|/v1/socket/token||authorization:test-access-token\nx-api-key:test-app-key\n\
         x-timestamp:1700000000\n|authorization;x-api-key;x-timestamp|";

    /// The signature one known vector produces, computed by hand from §4.9's algorithm.
    ///
    /// The three steps are stated independently of the module that performs them: the
    /// string to sign is the literal above, the digest is that literal's SHA-1 in
    /// lowercase hex (pinned by value, and by [`the_digest_is_sha1_in_lowercase_hex`]
    /// against SHA-1's own published vectors), the message the HMAC is taken over is
    /// the algorithm's own name and a bar in front of that same digest, and the key is
    /// the application secret. The hexadecimal below is what the HMAC-SHA256 of those
    /// inputs is, so a change to the field order, to the bar count, to the algorithm's
    /// name, to the digest's case or to the HMAC framing changes it and fails the test.
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

    /// The four headers a signed request presents are the three values it covers and
    /// the signature over them, spelled in the venue's own header names.
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

    /// Two instants are two signatures over the same request: the instant is signed,
    /// so it cannot be dropped or reused from a previous call.
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

    /// A body is signed as its own SHA-1 after the trailing bar, and a query is signed
    /// in the field between the path and the values.
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
