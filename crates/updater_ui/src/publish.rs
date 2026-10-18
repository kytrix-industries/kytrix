//! The build-time facts the updater trusts.

/// The version this build reports, which is the application version.
///
/// Cargo writes it into every crate from `[workspace.package]`, so this is the
/// version the release was tagged with and the one a newer release is compared to.
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Where the update metadata is read from.
///
/// GitHub serves `releases/latest/download/<asset>` as a redirect to the asset of
/// the newest published release, which is what makes this a stable name for a
/// per-release artifact.
pub const METADATA_ENDPOINT: &str =
    "https://github.com/kytrix-industries/kytrix/releases/latest/download/latest.json";

/// The release page, for the platforms that cannot install an update in place.
pub const RELEASE_PAGE: &str = "https://github.com/kytrix-industries/kytrix/releases/latest";

/// The cargo-packager minisign public key every update package is verified with.
///
/// It is the public half of the key pair `cargo packager signer generate` made for
/// this repository, as a `minisign.pub` file: the private half is
/// `CARGO_PACKAGER_SIGN_PRIVATE_KEY` in the repository's Actions secrets and never
/// leaves the release workflow.
///
/// Rotating the key means replacing this constant, and the release that ships the
/// new key must keep accepting the old one for as long as builds signed with it
/// are still being updated from: a build that only knows the new key refuses every
/// package the old key signed, which is an update that fails verification on every
/// machine still running it.
pub const PUBKEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IEMyOTY3NUIyMkNFRjgxODIKUldTQ2dlOHNzbldXd2hvbk0vRzdsTEtpSHp0YlVlRTluckNVakdHTDhzdXI0Slc0TWFkRTlQWGoK";
