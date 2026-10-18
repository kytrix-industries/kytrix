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
