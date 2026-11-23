use bytes::Bytes;
use dicebear_core::{Avatar, Style};
use smol_str::SmolStr;
use thiserror::Error;

const AVATAR_BORDER_RADIUS: u8 = 50;

const MATTE_RGB: u8 = 255;

#[derive(Clone, Debug)]
pub(crate) struct Face {
    pub svg: String,
    pub png: Bytes,
}

impl Face {
    #[must_use]
    pub(crate) fn svg_smol(&self) -> SmolStr {
        SmolStr::new(self.svg.as_str())
    }
}

#[derive(Debug, Error)]
pub(crate) enum AvatarError {
    #[error("the embedded DiceBear style is invalid")]
    Style(#[source] dicebear_core::Error),
    #[error("failed to draw the wallet avatar")]
    Draw(#[source] dicebear_core::Error),
    #[error("failed to parse the wallet avatar SVG")]
    Parse,
    #[error("failed to rasterize the wallet avatar")]
    Rasterize,
    #[error("failed to encode the wallet avatar as a PNG")]
    Encode(#[source] image::ImageError),
    #[error("the wallet avatar task failed")]
    Task(#[source] tokio::task::JoinError),
}

pub(crate) async fn render_face(
    wallet: SmolStr,
    stored: Option<SmolStr>,
    side: u32,
    runtime: tokio::runtime::Handle,
) -> Result<(SmolStr, Face), AvatarError> {
    runtime
        .spawn_blocking(move || {
            let face = render_face_blocking(wallet.as_str(), stored.as_deref(), side)?;

            Ok::<_, AvatarError>((wallet, face))
        })
        .await
        .map_err(AvatarError::Task)?
}

pub(crate) fn render_face_blocking(
    wallet: &str,
    stored: Option<&str>,
    side: u32,
) -> Result<Face, AvatarError> {
    let svg = match stored {
        Some(svg) => svg.to_owned(),
        None => generate_avatar_svg(wallet)?,
    };
    let png = rasterize_avatar_svg(&svg, side)?;

    Ok(Face { svg, png })
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

pub(crate) fn rasterize_avatar_svg(svg: &str, side: u32) -> Result<Bytes, AvatarError> {
    if side == 0 {
        return Err(AvatarError::Rasterize);
    }
    let tree =
        usvg::Tree::from_str(svg, &usvg::Options::default()).map_err(|_| AvatarError::Parse)?;
    let canvas = tree.size();
    let longest = canvas.width().max(canvas.height());
    if !longest.is_finite() || longest <= 0.0 {
        return Err(AvatarError::Rasterize);
    }
    let scale = side as f32 / longest;
    let origin = resvg::tiny_skia::Transform::from_scale(scale, scale).post_translate(
        (side as f32 - canvas.width() * scale) * 0.5,
        (side as f32 - canvas.height() * scale) * 0.5,
    );
    let mut pixmap = resvg::tiny_skia::Pixmap::new(side, side).ok_or(AvatarError::Rasterize)?;
    resvg::render(&tree, origin, &mut pixmap.as_mut());

    let mut rgba = Vec::with_capacity((side * side * 4) as usize);
    for pixel in pixmap.pixels() {
        let alpha = pixel.alpha();
        let (red, green, blue) = if alpha == 0 {
            (MATTE_RGB, MATTE_RGB, MATTE_RGB)
        } else {
            let unmultiply = |component: u8| -> u8 {
                let alpha = u32::from(alpha);
                ((u32::from(component) * 255 + alpha / 2) / alpha) as u8
            };
            (
                unmultiply(pixel.red()),
                unmultiply(pixel.green()),
                unmultiply(pixel.blue()),
            )
        };
        rgba.extend_from_slice(&[red, green, blue, alpha]);
    }

    let mut png = Vec::with_capacity(rgba.len() / 2);
    let encoder = image::codecs::png::PngEncoder::new(&mut png);
    image::ImageEncoder::write_image(encoder, &rgba, side, side, image::ExtendedColorType::Rgba8)
        .map_err(AvatarError::Encode)?;

    Ok(Bytes::from(png))
}

#[cfg(test)]
mod tests {
    use image::ImageDecoder as _;

    use super::{
        AVATAR_BORDER_RADIUS, AvatarError, generate_avatar_svg, rasterize_avatar_svg, render_face,
        render_face_blocking,
    };

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
    fn a_generated_avatar_should_rasterize_to_a_png_of_the_drawn_size() {
        let svg = generate_avatar_svg(WALLET).expect("the embedded style should render");
        let png = rasterize_avatar_svg(&svg, 56).expect("the face should rasterize");

        assert!(png.starts_with(b"\x89PNG"), "the face is a PNG");
        let decoder = image::codecs::png::PngDecoder::new(std::io::Cursor::new(png))
            .expect("the face decodes as a PNG");
        assert_eq!(
            (
                decoder.total_bytes(),
                image::ImageDecoder::dimensions(&decoder)
            ),
            (56 * 56 * 4, (56, 56)),
            "the face is a 56 px straight-alpha RGBA raster"
        );
    }

    #[test]
    fn a_stored_svg_should_be_rasterized_as_it_is() {
        let stored = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><rect width="10" height="10" fill="#ff0000"/></svg>"##;
        let face = render_face_blocking(WALLET, Some(stored), 8).expect("the stored face renders");

        assert_eq!(
            face.svg, stored,
            "the stored document is what the settings keep"
        );
        assert!(face.png.starts_with(b"\x89PNG"));
    }

    #[test]
    fn a_background_generation_should_return_the_address_it_was_generated_for() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("the test runtime");
        let (wallet, face) = runtime
            .block_on(render_face(
                WALLET.into(),
                None,
                56,
                runtime.handle().clone(),
            ))
            .expect("the embedded style should render");

        assert_eq!(wallet, WALLET);
        assert!(face.svg.starts_with("<svg"));
        assert!(face.png.starts_with(b"\x89PNG"));
    }
}
