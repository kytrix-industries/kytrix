//! The avatar a signed-in wallet is drawn as.
//!
//! It is generated rather than uploaded: the wallet address is the seed, so the same account
//! always gets the same face, on every machine and without a request. The result is an SVG,
//! which is what the settings persist.
//!
//! # Why there is a raster step
//!
//! The engine's image nodes decode **raster** formats only (`crates/ui_core/src/resources.rs`:
//! PNG / JPEG / WebP — it deliberately does not carry an SVG renderer), so the SVG is rasterized
//! here on a blocking thread and encoded as a PNG before it reaches a node. The face is drawn at
//! `56 px × DPI` (the account block's own size), which is the only size the menu draws it at:
//! rasterizing on the frame thread would put a layout-time document parse in the paint path.
//!
//! # Why it is a background job
//!
//! Generation and rasterization are CPU work, so neither runs on the frame thread: the caller
//! hands the address to a blocking task on the application runtime and gets the pair
//! `(address, face)` back, which is what lets it tell a result that still belongs to the current
//! account from one that arrived after the user signed out.
//!
//! # Errors
//!
//! [`AvatarError`] is a **diagnostic** type: a face that cannot be produced is logged and the
//! menu draws its default glyph, because a missing picture is not something a user can act on
//! and must not become an alert. Its `Display` text is for logs only — no user-visible copy is
//! built from it (compare [`crate::oauth::LoginError`], which does carry a sentence).

use bytes::Bytes;
use dicebear_core::{Avatar, Style};
use smol_str::SmolStr;
use thiserror::Error;

/// Corner radius of the generated face, in the style's own coordinate space.
const AVATAR_BORDER_RADIUS: u8 = 50;

/// RGB written to fully transparent pixels of a rasterized face.
///
/// The engine scales straight-alpha pixels before premultiplying, so a transparent pixel whose
/// RGB is zero bleeds a dark ring into every edge of the face. Writing the matte colour white
/// (which is what the settings surface behind the face resolves to in the light appearance and
/// what the generated styles draw their background from) keeps that ring invisible.
const MATTE_RGB: u8 = 255;

/// The face one wallet is drawn with: the SVG the settings persist, and its rasterized PNG.
#[derive(Clone, Debug)]
pub(crate) struct Face {
    /// The generated SVG, exactly as the settings keep it.
    pub svg: String,
    /// The rasterized PNG the engine image node decodes.
    pub png: Bytes,
}

impl Face {
    /// The SVG as the settings hand it back.
    #[must_use]
    pub(crate) fn svg_smol(&self) -> SmolStr {
        SmolStr::new(self.svg.as_str())
    }
}

/// A generated avatar could not be produced.
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

/// Renders the avatar of `wallet` on a blocking thread.
///
/// `stored` is the SVG the settings already hold: a face that was generated before is only
/// rasterized again, and a wallet without one gets a freshly generated SVG. The address is
/// returned with its face so the caller can check that the account it was for is still the
/// signed-in one before drawing or storing it.
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

/// Generates and rasterizes one wallet's face (blocking; see [`render_face`]).
///
/// # Errors
/// See [`AvatarError`]: a malformed embedded style, a style that refuses the seed, an SVG that
/// does not parse or rasterize, and a PNG that cannot be encoded.
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

/// Renders the avatar SVG for a wallet address.
///
/// The style is parsed here rather than held in application state: it is a 64 KiB embedded
/// document, this runs once per sign-in on a blocking thread, and a parsed style that outlived
/// its only use would be state to keep consistent for nothing.
///
/// # Errors
/// [`AvatarError::Style`] when the embedded style document is invalid (a build fault) and
/// [`AvatarError::Draw`] when the style refuses the seed or the options.
pub(crate) fn generate_avatar_svg(wallet_address: &str) -> Result<String, AvatarError> {
    let style = Style::from_str(dicebear_styles::GAZE).map_err(AvatarError::Style)?;
    // DiceBear takes its options as a JSON document, so this is the one place in the crate where
    // a dynamic JSON value is the interface rather than a shortcut (`AGENTS.md` §2.1 exemption).
    let options = serde_json::json!({
        "seed": wallet_address,
        "borderRadius": AVATAR_BORDER_RADIUS,
    });
    let avatar = Avatar::new(&style, options).map_err(AvatarError::Draw)?;

    Ok(avatar.to_svg().to_owned())
}

/// Rasterizes an avatar SVG into a `side × side` straight-alpha PNG.
///
/// The face is fitted into the square by its longer side and centred; the styles draw on a square
/// canvas, so this is an exact fit, and a document that does not declare one is letterboxed
/// rather than stretched.
///
/// # Errors
/// [`AvatarError::Parse`] when the document is not a readable SVG, [`AvatarError::Rasterize`]
/// when the pixel buffer cannot be allocated or a `side` of zero is asked for, and
/// [`AvatarError::Encode`] when the PNG encoder refuses the buffer.
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

    // tiny-skia hands back premultiplied RGBA; PNG wants straight alpha. Undoing the multiply
    // here (rather than telling the decoder about it) keeps the engine's image path a plain
    // straight-alpha PNG consumer.
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

    /// The engine's image nodes decode raster formats only, so the generated face has to reach
    /// them as a PNG of the size the account block draws — this is the step that turns the SVG
    /// into that PNG.
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

    /// A stored SVG is rasterized without being generated again: the settings' copy of the face
    /// is the one the menu draws.
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
        // The control hands its work to the injected runtime and awaits the blocking pool on it:
        // the test owns a runtime of its own and drives exactly that, rather than reading a handle
        // out of whatever context happens to be current.
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
