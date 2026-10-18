//! The avatar a signed-in wallet is drawn as.
//!
//! It is generated rather than uploaded: the wallet address is the seed, so the same
//! account always gets the same face, on every machine and without a request. The
//! result is an SVG, which is what the settings persist and what the menu draws.
//!
//! Generation is CPU work, so it never runs on the frame thread: the caller hands the
//! address to a blocking task and gets the pair `(address, svg)` back, which is what
//! lets it tell a result that still belongs to the current account from one that
//! arrived after the user signed out.

use dicebear_core::{Avatar, Style};
use smol_str::SmolStr;
use thiserror::Error;
use widget_tokio_runtime::runtime::Runtime;

/// Corner radius of the generated face, in the style's own coordinate space.
const AVATAR_BORDER_RADIUS: u8 = 50;

/// A generated avatar could not be produced.
#[derive(Debug, Error)]
pub(crate) enum AvatarError {
    #[error("the embedded DiceBear style is invalid")]
    Style(#[source] dicebear_core::Error),
    #[error("failed to render the wallet avatar")]
    Render(#[source] dicebear_core::Error),
    #[error("the wallet avatar task failed")]
    Task(#[source] tokio::task::JoinError),
}

/// Generates the avatar of `wallet_address` on a blocking thread.
///
/// The address is returned with its avatar so the caller can check that the account
/// it was generated for is still the signed-in one before drawing or storing it.
///
/// The runtime is what the blocking thread comes from: the caller is a menu drawn
/// from a GPUI task, which has no Tokio context to hand a blocking pool.
pub(crate) async fn generate_avatar(
    wallet_address: SmolStr,
    runtime: Runtime,
) -> Result<(SmolStr, String), AvatarError> {
    let seed = wallet_address.clone();
    runtime
        .blocking(move || generate_avatar_svg(&seed).map(|svg| (wallet_address, svg)))
        .await
        .map_err(AvatarError::Task)?
}

/// Renders the avatar SVG for a wallet address.
///
/// The style is parsed here rather than held in application state: it is a 64 KiB
/// embedded document, this runs once per sign-in on a blocking thread, and a parsed
/// style that outlived its only use would be state to keep consistent for nothing.
fn generate_avatar_svg(wallet_address: &str) -> Result<String, AvatarError> {
    let style = Style::from_str(dicebear_styles::GAZE).map_err(AvatarError::Style)?;
    // DiceBear takes its options as a JSON document, so this is the one place in the
    // crate where a dynamic JSON value is the interface rather than a shortcut.
    let options = serde_json::json!({
        "seed": wallet_address,
        "borderRadius": AVATAR_BORDER_RADIUS,
    });
    let avatar = Avatar::new(&style, options).map_err(AvatarError::Render)?;

    Ok(avatar.to_svg().to_owned())
}

#[cfg(test)]
mod tests {
    use widget_tokio_runtime::runtime::Runtime;

    use super::{AVATAR_BORDER_RADIUS, AvatarError, generate_avatar, generate_avatar_svg};

    const WALLET: &str = "0x0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn an_avatar_should_be_deterministic_and_clipped_to_its_corner_radius() {
        let first = generate_avatar_svg(WALLET).expect("the embedded style should render");
        let second = generate_avatar_svg(WALLET).expect("the embedded style should render");

        assert_eq!(first, second);
        assert!(first.starts_with("<svg"), "{first}");
        // The style draws into a 100-unit box, so the radius asked for comes back in those
        // units: half of the box, which is what makes the face a circle.
        let radius = AVATAR_BORDER_RADIUS;
        assert!(
            first.contains("<clipPath")
                && first.contains(&format!(r#"rx="{radius}" ry="{radius}""#)),
            "{first}"
        );
    }

    #[test]
    fn different_wallets_should_get_different_avatars() {
        let other = "0x0123456789abcdef0123456789abcdef01234568";

        assert_ne!(
            generate_avatar_svg(WALLET).expect("the embedded style should render"),
            generate_avatar_svg(other).expect("the embedded style should render"),
        );
    }

    #[test]
    fn a_failed_style_is_a_typed_error() {
        let error = AvatarError::Style(dicebear_core::Error::Validation("no".to_owned()));

        assert!(error.to_string().contains("style"));
    }

    #[tokio::test]
    async fn a_background_generation_should_return_the_address_it_was_generated_for() {
        let (wallet, svg) = generate_avatar(WALLET.into(), Runtime::for_test())
            .await
            .expect("the embedded style should render");

        assert_eq!(wallet, WALLET);
        assert!(svg.starts_with("<svg"));
    }
}
