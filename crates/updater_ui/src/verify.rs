//! The minisign check a downloaded payload has to pass before anything installs it.

use base64::Engine as _;
use minisign_verify::{PublicKey, Signature};

use crate::error::SignatureError;

/// Verifies `payload` against the base64 signature published with it.
///
/// Both strings are base64 over minisign's own text — the public key is a
/// `minisign.pub` file, the signature is the `.sig` file a release publishes —
/// each encoded once more so they travel inside JSON. This is the updater
/// library's own check, which is private to it and which its installer expects to
/// have happened: the same two decodes, the same `minisign_verify` call, and the
/// same acceptance of legacy (non-prehashed) signatures, which is what the `true`
/// asks for.
pub fn verify(payload: &[u8], signature: &str, pubkey: &str) -> Result<(), SignatureError> {
    let key = base64::engine::general_purpose::STANDARD
        .decode(pubkey)
        .map_err(SignatureError::PublicKeyEncoding)?;
    let key = String::from_utf8(key).map_err(SignatureError::PublicKeyUtf8)?;
    let public_key = PublicKey::decode(&key).map_err(SignatureError::PublicKey)?;

    let text = base64::engine::general_purpose::STANDARD
        .decode(signature)
        .map_err(SignatureError::SignatureEncoding)?;
    let text = String::from_utf8(text).map_err(SignatureError::SignatureUtf8)?;
    let signature = Signature::decode(&text).map_err(SignatureError::Signature)?;

    public_key
        .verify(payload, &signature, true)
        .map_err(SignatureError::Verification)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::publish;

    /// What the sample signature was made over.
    const PAYLOAD: &[u8] = b"kytrix updater test payload\n";

    /// A minisign public key made for these tests, in the same form as [`publish::PUBKEY`].
    ///
    /// The key pair was generated once with Node's own Ed25519 (`crypto.generateKeyPairSync("ed25519")`,
    /// node v24) and a random 8-byte key id; the payload was signed in minisign's
    /// prehashed mode, which is the mode its default signer uses: the signature is
    /// over BLAKE2b-512 of the payload (`crypto.createHash("blake2b512")`), the
    /// algorithm bytes are `ED`, and the global signature is over the signature
    /// followed by the trusted comment after its `trusted comment: ` prefix. The
    /// four lines were then base64-encoded whole, which is what `latest.json`
    /// carries. The private half was discarded; only what is inlined here remains.
    const PUBKEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IEZBRkIxRDAxODQ4MjdEQzYKUlVUNit4MEJoSUo5eHNCMG5ncHhUdFNnVExmcUdENHNENlArNHZ6MHpnYzJQeHEyU2JPT0F1V1AK";

    /// The signature of [`PAYLOAD`] under [`PUBKEY`], in the form `latest.json` carries.
    const SIGNATURE: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIG1pbmlzaWduIHNlY3JldCBrZXkKUlVUNit4MEJoSUo5eHVDSmcyVzRORGI5SjU4blBObDRVU2pENHR3aUpIMGd6d2dZOVE3SDZZdThRMzlFMUdLRWJrdysxUTR0NzlhUU9JUDYvVzlDOUtHZ05HQ2VBNS94OUFFPQp0cnVzdGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNzYwMDAwMDAwCWZpbGU6a3l0cml4LXRlc3QtcGF5bG9hZAlwcmVoYXNoZWQKOEFoMDhTR2d3N0NVM1FkUWJyaXgzR3MrNEN6S0xna2YremlKSWtnb2hoekthNlVzSzJucFM5aXg0NUZxWW11Unh1b0FocVRBc3NBRmhKaHRYamdkQ1E9PQo=";

    #[test]
    fn the_payload_should_verify_against_the_signature_it_was_signed_with() {
        verify(PAYLOAD, SIGNATURE, PUBKEY).expect("the sample is its own signature");
    }

    #[test]
    fn a_payload_with_one_byte_changed_should_not_verify() {
        let mut tampered = PAYLOAD.to_vec();
        tampered[0] ^= 0x01;

        assert!(
            matches!(
                verify(&tampered, SIGNATURE, PUBKEY),
                Err(SignatureError::Verification(_))
            ),
            "a payload that is not the one that was signed is refused"
        );
    }

    #[test]
    fn a_signature_from_another_key_should_not_verify() {
        assert!(
            matches!(
                verify(PAYLOAD, SIGNATURE, publish::PUBKEY),
                Err(SignatureError::Verification(_))
            ),
            "the sample signature was not made by the release key"
        );
    }

    #[test]
    fn a_signature_that_is_not_base64_should_not_verify() {
        assert!(matches!(
            verify(PAYLOAD, "not base64!", PUBKEY),
            Err(SignatureError::SignatureEncoding(_))
        ));
    }

    /// The constant is the trust root of the whole update path, so what it holds is
    /// pinned here: it decodes, and it is the key the comment says it is.
    #[test]
    fn the_published_public_key_should_be_a_minisign_key() {
        let key = base64::engine::general_purpose::STANDARD
            .decode(publish::PUBKEY)
            .expect("the published key is base64");
        let key = String::from_utf8(key).expect("the published key is text");
        let key = PublicKey::decode(&key).expect("the published key is a minisign public key");

        assert_eq!(
            key.untrusted_comment(),
            Some("untrusted comment: minisign public key: C29675B22CEF8182"),
            "the key identifies itself as the one the release workflow signs with"
        );
    }

    /// What the release key's sample signature was made over, byte for byte.
    const RELEASE_PAYLOAD: &[u8] = b"kytrix updater trust root\n";

    /// The signature of [`RELEASE_PAYLOAD`] under [`publish::PUBKEY`].
    ///
    /// Made once with the repository's own signing key pair — the one
    /// `cargo packager signer sign` runs with in the release workflow — and encoded
    /// exactly as `latest.json` carries a signature. It is what ties the compiled-in
    /// key to the key the released packages are signed with: a build that ships any
    /// other key fails this test rather than every update on every machine.
    const RELEASE_SIGNATURE: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIGNhcmdvLXBhY2thZ2VyIHNlY3JldCBrZXkKUlVTQ2dlOHNzbldXd3BXc0UvNWh4QVhrMzY5Znd2dm5yYk8vMXZlVVdlcVNJa2JCM2FiVUp2MkpWK0JjbXNHZk1jcHBPWENRUGxkZXFCeGc3cllZVzBXd2xqR0hncXBSUlFjPQp0cnVzdGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNzkxODk5MjA1CWZpbGU6a3l0cml4LXRydXN0LXJvb3QtdmVjdG9yLmJpbgpjTUJCY2poNzdxTld5enZlU3Rsd3Z3QzRhb3BCU2FodUkxNUJPY3psbmtTOHp1RmxGdUc5WDhxOGkvWnF0SExwMUw3aDE0Ry9zakY0d0FXSk1RYUJCZz09Cg==";

    #[test]
    fn the_published_key_should_verify_a_signature_the_release_key_made() {
        verify(RELEASE_PAYLOAD, RELEASE_SIGNATURE, publish::PUBKEY)
            .expect("the compiled-in key is the one the released packages are signed with");
    }
}
