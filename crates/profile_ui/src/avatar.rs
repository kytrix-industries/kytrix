use dicebear_core::{Avatar, Style};
use smol_str::SmolStr;
use thiserror::Error;

const AVATAR_BORDER_RADIUS: u8 = 50;

#[derive(Debug, Error)]
pub(crate) enum AvatarError {
    #[error("the embedded DiceBear style is invalid")]
    Style(#[source] dicebear_core::Error),
    #[error("failed to draw the wallet avatar")]
    Draw(#[source] dicebear_core::Error),
    #[error("the wallet avatar task failed")]
    Task(#[source] tokio::task::JoinError),
}

pub(crate) async fn render_face(
    wallet: SmolStr,
    stored: Option<SmolStr>,
    runtime: tokio::runtime::Handle,
) -> Result<(SmolStr, SmolStr), AvatarError> {
    runtime
        .spawn_blocking(move || {
            let svg = face_svg(wallet.as_str(), stored.as_deref())?;

            Ok::<_, AvatarError>((wallet, svg))
        })
        .await
        .map_err(AvatarError::Task)?
}

pub(crate) fn face_svg(wallet: &str, stored: Option<&str>) -> Result<SmolStr, AvatarError> {
    match stored {
        Some(svg) => Ok(SmolStr::new(svg)),
        None => generate_avatar_svg(wallet).map(SmolStr::new),
    }
}

pub(crate) fn generate_avatar_svg(wallet_address: &str) -> Result<String, AvatarError> {
    let style = Style::from_str(dicebear_styles::GAZE).map_err(AvatarError::Style)?;
    let options = serde_json::json!({
        "seed": wallet_address,
        "borderRadius": AVATAR_BORDER_RADIUS,
    });
    let avatar = Avatar::new(&style, options).map_err(AvatarError::Draw)?;

    Ok(avatar.to_svg().to_owned())
}

#[cfg(test)]
mod tests {
    use smol_str::SmolStr;

    use super::{AVATAR_BORDER_RADIUS, AvatarError, face_svg, generate_avatar_svg, render_face};

    const WALLET: &str = "0x0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn an_avatar_should_be_deterministic_and_clipped_to_its_corner_radius() {
        let first = generate_avatar_svg(WALLET).expect("the embedded style should render");
        let second = generate_avatar_svg(WALLET).expect("the embedded style should render");

        assert_eq!(first, second);
        assert!(first.starts_with("<svg"), "{first}");
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

    #[test]
    fn a_stored_svg_should_be_kept_as_it_is() {
        let stored = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><rect width="10" height="10" fill="#ff0000"/></svg>"##;
        let svg = face_svg(WALLET, Some(stored)).expect("the stored face is kept");

        assert_eq!(svg, stored, "the stored document is what the settings keep");
        assert!(
            face_svg(WALLET, None)
                .expect("the embedded style should render")
                .starts_with("<svg")
        );
    }

    #[test]
    fn a_background_generation_should_return_the_address_it_was_generated_for() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("the test runtime");
        let (wallet, svg) = runtime
            .block_on(render_face(
                SmolStr::new(WALLET),
                None,
                runtime.handle().clone(),
            ))
            .expect("the embedded style should render");

        assert_eq!(wallet, WALLET);
        assert!(svg.starts_with("<svg"));
    }
}
