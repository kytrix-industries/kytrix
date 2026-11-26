use std::io::Cursor;

use smol_str::SmolStr;
use ui_core::math::{LogicalSize, px};

use super::MarkdownAssets;

const SCHEME: &str = "file:";
const SIZE_KEY: &str = "size";
const TYPE_KEY: &str = "type";
const TYPE_IMAGE: &str = "img";
const TYPE_SVG: &str = "svg";
const SVG_EXTENSION: &str = "svg";
const DEFAULT_IMAGE_SIZE: f32 = 16.;
const MIN_IMAGE_SIZE: f32 = 1.;
const MAX_IMAGE_SIZE: f32 = 256.;
const MIN_INTRINSIC_SIZE: f32 = 1.;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(super) enum InlineImageFormat {
    Image,
    Svg,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct InlineImageSource {
    pub format: InlineImageFormat,
    pub path: SmolStr,
    pub size: Option<LogicalSize>,
}

impl InlineImageSource {
    pub(super) fn parse(uri: &str) -> Option<Self> {
        let reference = uri.strip_prefix(SCHEME)?;
        let reference = reference.strip_prefix("//").unwrap_or(reference);
        let (path, query) = match reference.split_once('?') {
            Some((path, query)) => (path, Some(query)),
            None => (reference, None),
        };
        if path.is_empty() {
            return None;
        }

        let mut format = None;
        let mut size = None;
        let mut seen_size = false;
        if let Some(query) = query {
            for segment in query.split('&') {
                if segment.is_empty() {
                    continue;
                }
                let (key, value) = segment.split_once('=')?;
                let value = unquote(value)?;
                match key {
                    TYPE_KEY => {
                        if format.is_some() {
                            return None;
                        }
                        format = Some(match value {
                            TYPE_IMAGE => InlineImageFormat::Image,
                            TYPE_SVG => InlineImageFormat::Svg,
                            _ => return None,
                        });
                    }
                    SIZE_KEY => {
                        if seen_size {
                            return None;
                        }
                        seen_size = true;
                        size = parse_size(value);
                    }
                    _ => {}
                }
            }
        }

        Some(Self {
            format: format.unwrap_or_else(|| format_of(path)),
            path: SmolStr::new(path),
            size,
        })
    }

    pub(super) fn resolve(&self, assets: &dyn MarkdownAssets) -> LogicalSize {
        if let Some(size) = self.size {
            return size;
        }
        let intrinsic = match self.format {
            InlineImageFormat::Svg => None,
            InlineImageFormat::Image => assets
                .load(&self.path)
                .as_deref()
                .and_then(intrinsic_size)
                .filter(|size| *size != LogicalSize::ZERO),
        }
        .unwrap_or(LogicalSize::new(
            px(DEFAULT_IMAGE_SIZE),
            px(DEFAULT_IMAGE_SIZE),
        ));

        LogicalSize::new(
            intrinsic.width.max(px(MIN_INTRINSIC_SIZE)),
            intrinsic.height.max(px(MIN_INTRINSIC_SIZE)),
        )
    }
}

fn format_of(path: &str) -> InlineImageFormat {
    if extension_of(path).as_deref() == Some(SVG_EXTENSION) {
        InlineImageFormat::Svg
    } else {
        InlineImageFormat::Image
    }
}

fn extension_of(path: &str) -> Option<String> {
    let (_, extension) = path.rsplit_once('.')?;
    if extension.is_empty() || extension.contains('/') {
        return None;
    }
    Some(extension.to_ascii_lowercase())
}

fn unquote(value: &str) -> Option<&str> {
    match value.strip_prefix('"') {
        Some(inner) => inner.strip_suffix('"').filter(|inner| !inner.contains('"')),
        None => (!value.contains('"')).then_some(value),
    }
}

fn parse_size(value: &str) -> Option<LogicalSize> {
    let (width, height) = match value.split_once(',') {
        Some((width, height)) => (width, height),
        None => (value, value),
    };
    if !is_plain_number(width) || !is_plain_number(height) {
        return None;
    }
    let (Ok(width), Ok(height)) = (width.parse::<f32>(), height.parse::<f32>()) else {
        return None;
    };

    Some(LogicalSize::new(
        px(width.clamp(MIN_IMAGE_SIZE, MAX_IMAGE_SIZE)),
        px(height.clamp(MIN_IMAGE_SIZE, MAX_IMAGE_SIZE)),
    ))
}

fn is_plain_number(value: &str) -> bool {
    let mut dots = 0;
    !value.is_empty()
        && value.bytes().all(|byte| match byte {
            b'0'..=b'9' => true,
            b'.' => {
                dots += 1;
                dots == 1
            }
            _ => false,
        })
}

fn intrinsic_size(bytes: &[u8]) -> Option<LogicalSize> {
    let (width, height) = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()?;

    Some(LogicalSize::new(px(width as f32), px(height as f32)))
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use smol_str::SmolStr;
    use ui_core::math::{LogicalSize, px};

    use super::{InlineImageFormat, InlineImageSource};
    use crate::kit::markdown::MarkdownAssets;

    struct EmbeddedAssets;

    impl MarkdownAssets for EmbeddedAssets {
        fn load(&self, path: &str) -> Option<Cow<'static, [u8]>> {
            if path.is_empty() {
                return None;
            }
            crate::kit::assets::bytes(path).map(Cow::Borrowed)
        }
    }

    fn parsed(uri: &str) -> (SmolStr, InlineImageFormat, Option<LogicalSize>) {
        let source = InlineImageSource::parse(uri)
            .unwrap_or_else(|| panic!("{uri} should be a usable image URI"));
        (source.path, source.format, source.size)
    }

    fn parsed_as(uri: &str, path: &'static str, format: InlineImageFormat) {
        assert_eq!(
            parsed(uri),
            (SmolStr::new_static(path), format, None),
            "{uri} should parse to that path and format"
        );
    }

    fn parsed_size(uri: &str, expected: LogicalSize) {
        let (_, _, size) = parsed(uri);
        assert_eq!(
            size,
            Some(expected),
            "{uri} should carry that explicit size"
        );
    }

    #[test]
    fn the_scheme_is_file_and_the_slashes_are_optional() {
        parsed_as(
            "file://brands/Binance/binance.png",
            "brands/Binance/binance.png",
            InlineImageFormat::Image,
        );
        parsed_as(
            "file:brands/Binance/binance.png",
            "brands/Binance/binance.png",
            InlineImageFormat::Image,
        );
    }

    #[test]
    fn anything_that_is_not_a_file_uri_is_rejected() {
        for uri in [
            "icon:Graph",
            "https://e.com/a.png",
            "brands/Binance/binance.png",
            "FILE://brands/Binance/binance.png",
            "file://",
        ] {
            assert_eq!(
                InlineImageSource::parse(uri),
                None,
                "{uri} should not be an image of this component"
            );
        }
    }

    #[test]
    fn a_single_slash_is_part_of_the_path() {
        parsed_as(
            "file:/brands/Binance/binance.png",
            "/brands/Binance/binance.png",
            InlineImageFormat::Image,
        );
    }

    #[test]
    fn the_asset_path_is_taken_verbatim() {
        parsed_as(
            "file://brands/Solana/colored@72px.png",
            "brands/Solana/colored@72px.png",
            InlineImageFormat::Image,
        );
        parsed_as(
            "file://icons/phosphoricons/graph.svg",
            "icons/phosphoricons/graph.svg",
            InlineImageFormat::Svg,
        );
        parsed_as(
            "file://中文/图.png",
            "中文/图.png",
            InlineImageFormat::Image,
        );
        parsed_as("file://a/b#c.png", "a/b#c.png", InlineImageFormat::Image);
        parsed_as("file://a/b&c.png", "a/b&c.png", InlineImageFormat::Image);
        parsed_as("file://a/b\"c.png", "a/b\"c.png", InlineImageFormat::Image);
    }

    #[test]
    fn a_path_cannot_escape_the_embedded_assets() {
        for uri in [
            "file://../../Cargo.toml",
            "file://..%2f..%2fCargo.toml",
            "file://C:/Windows/win.ini",
            "file:///etc/passwd",
            "file://..\\..\\Cargo.toml",
        ] {
            let source = InlineImageSource::parse(uri)
                .unwrap_or_else(|| panic!("{uri} should be a usable image URI"));
            let default = LogicalSize::new(px(16.), px(16.));

            assert_eq!(
                EmbeddedAssets.load(&source.path),
                None,
                "{uri} should not name anything the asset source can read"
            );
            assert_eq!(
                source.resolve(&EmbeddedAssets),
                default,
                "{uri} should fall back to the default size"
            );
        }
    }

    #[test]
    fn the_format_follows_the_extension_when_type_is_absent() {
        for uri in ["file://a/b.svg", "file://a/b.SVG", "file://b.svg"] {
            assert_eq!(
                InlineImageSource::parse(uri).map(|source| source.format),
                Some(InlineImageFormat::Svg),
                "{uri} should be drawn as an svg"
            );
        }
        for uri in [
            "file://a/b.png",
            "file://a/b",
            "file://a/b.svga",
            "file://a.svg/b",
        ] {
            assert_eq!(
                InlineImageSource::parse(uri).map(|source| source.format),
                Some(InlineImageFormat::Image),
                "{uri} should be drawn as a bitmap"
            );
        }
    }

    #[test]
    fn type_overrides_the_extension_and_may_be_quoted() {
        for (uri, expected) in [
            ("file://a/b.png?type=svg", InlineImageFormat::Svg),
            ("file://a/b.png?type=\"svg\"", InlineImageFormat::Svg),
            ("file://a/b.svg?type=img", InlineImageFormat::Image),
            ("file://a/b.svg?type=\"img\"", InlineImageFormat::Image),
        ] {
            assert_eq!(
                InlineImageSource::parse(uri).map(|source| source.format),
                Some(expected),
                "{uri} should take its format from `type`"
            );
        }
    }

    #[test]
    fn an_unusable_type_is_rejected() {
        for uri in [
            "file://a/b.png?type=ico",
            "file://a/b.png?type=",
            "file://a/b.png?type=IMG",
            "file://a/b.png?type=img&type=svg",
            "file://a/b.png?type=\"img",
            "file://a/b.png?type=img\"",
        ] {
            assert_eq!(
                InlineImageSource::parse(uri),
                None,
                "{uri} should be rejected whole"
            );
        }
    }

    #[test]
    fn size_takes_one_number_for_both_edges_or_two_for_width_and_height() {
        parsed_size("file://a/b.png?size=24", LogicalSize::new(px(24.), px(24.)));
        parsed_size(
            "file://a/b.png?size=24,48",
            LogicalSize::new(px(24.), px(48.)),
        );
        parsed_size(
            "file://a/b.png?size=3.5",
            LogicalSize::new(px(3.5), px(3.5)),
        );
        parsed_size(
            "file://a/b.png?size=24.5,48.25",
            LogicalSize::new(px(24.5), px(48.25)),
        );
        parsed_size(
            "file://a/b.png?size=00024",
            LogicalSize::new(px(24.), px(24.)),
        );
    }

    #[test]
    fn size_may_be_quoted_and_lives_anywhere_in_the_query() {
        parsed_size(
            "file://a/b.png?size=\"24,48\"",
            LogicalSize::new(px(24.), px(48.)),
        );
        parsed_size(
            "file://a/b.png?size=24,48&type=img",
            LogicalSize::new(px(24.), px(48.)),
        );
        parsed_size(
            "file://a/b.png?type=img&size=24,48",
            LogicalSize::new(px(24.), px(48.)),
        );
    }

    #[test]
    fn an_explicit_size_is_clamped() {
        parsed_size("file://a/b.png?size=0", LogicalSize::new(px(1.), px(1.)));
        parsed_size("file://a/b.png?size=0.5", LogicalSize::new(px(1.), px(1.)));
        parsed_size("file://a/b.png?size=.5", LogicalSize::new(px(1.), px(1.)));
        parsed_size("file://a/b.png?size=0,0", LogicalSize::new(px(1.), px(1.)));
        parsed_size("file://a/b.png?size=1", LogicalSize::new(px(1.), px(1.)));
        parsed_size(
            "file://a/b.png?size=256",
            LogicalSize::new(px(256.), px(256.)),
        );
        parsed_size(
            "file://a/b.png?size=9999",
            LogicalSize::new(px(256.), px(256.)),
        );
        parsed_size(
            "file://a/b.png?size=256.5",
            LogicalSize::new(px(256.), px(256.)),
        );
        parsed_size(
            "file://a/b.png?size=0,9999",
            LogicalSize::new(px(1.), px(256.)),
        );
        parsed_size(
            "file://a/b.png?size=9999,0",
            LogicalSize::new(px(256.), px(1.)),
        );
    }

    #[test]
    fn an_absurdly_long_number_is_clamped_instead_of_overflowing() {
        let uri = format!("file://a/b.png?size={}", "9".repeat(400));

        parsed_size(&uri, LogicalSize::new(px(256.), px(256.)));
    }

    #[test]
    fn an_unusable_size_falls_back_to_the_intrinsic_one() {
        for value in [
            "", "abc", "-3", "+3", "1e3", "1e9", "inf", "NaN", " 20", "20px", "0x10", "1_0", ".",
            ",", ",24", "1.2.3", "20,",
        ] {
            let uri = format!("file://a/b.png?size={value}");

            assert_eq!(
                parsed(&uri).2,
                None,
                "{uri} should fall back to the resource's own size"
            );
        }
        assert_eq!(
            InlineImageSource::parse("file://a/b.png?size=16&size=20"),
            None,
            "two contradicting sizes should be rejected whole"
        );
    }

    #[test]
    fn unknown_and_empty_query_segments_are_ignored() {
        parsed_as(
            "file://a/b.png?color=red",
            "a/b.png",
            InlineImageFormat::Image,
        );
        parsed_size(
            "file://a/b.png?color=red&size=24",
            LogicalSize::new(px(24.), px(24.)),
        );
        parsed_size(
            "file://a/b.png?&&size=24&&",
            LogicalSize::new(px(24.), px(24.)),
        );
        parsed_as("file://a/b.png?", "a/b.png", InlineImageFormat::Image);
    }

    #[test]
    fn a_query_segment_without_a_value_is_rejected() {
        for uri in [
            "file://a/b.png?size",
            "file://a/b.png?type",
            "file://a/b.png?color",
            "file://a?b/c.png",
        ] {
            assert_eq!(
                InlineImageSource::parse(uri),
                None,
                "{uri} should be rejected whole"
            );
        }
    }

    #[test]
    fn a_missing_size_resolves_to_the_asset_size() {
        let resolve = |uri: &str| {
            InlineImageSource::parse(uri)
                .expect("the fixture URI should be usable")
                .resolve(&EmbeddedAssets)
        };

        assert_eq!(
            resolve("file://brands/Binance/binance.png"),
            LogicalSize::new(px(128.), px(128.))
        );
        assert_eq!(
            resolve("file://brands/Ethereum/colored@72px.png"),
            LogicalSize::new(px(44.), px(72.))
        );
        assert_eq!(
            resolve("file://brands/Solana/colored@72px.png"),
            LogicalSize::new(px(81.), px(72.))
        );
        assert_eq!(
            resolve("file://brands/Hyperliquid/hyperliquid.png"),
            LogicalSize::new(px(180.), px(180.))
        );
        assert_eq!(
            resolve("file://brands/OKX/okx.png"),
            LogicalSize::new(px(200.), px(200.))
        );
        assert_eq!(
            resolve("file://nope/nothing.png"),
            LogicalSize::new(px(16.), px(16.))
        );
        assert_eq!(
            resolve("file://icons/phosphoricons/graph.svg"),
            LogicalSize::new(px(16.), px(16.))
        );
    }

    #[test]
    fn an_explicit_size_wins_over_the_asset_size() {
        let resolve = |uri: &str| {
            InlineImageSource::parse(uri)
                .expect("the fixture URI should be usable")
                .resolve(&EmbeddedAssets)
        };

        assert_eq!(
            resolve("file://brands/Binance/binance.png?size=24,40"),
            LogicalSize::new(px(24.), px(40.))
        );
        assert_eq!(
            resolve("file://nope/nothing.png?size=40"),
            LogicalSize::new(px(40.), px(40.))
        );
    }

    #[test]
    fn a_resource_that_is_not_a_bitmap_falls_back_to_the_default_size() {
        let resolved = InlineImageSource::parse("file://icons/phosphoricons/graph.svg?type=img")
            .expect("the fixture URI should be usable")
            .resolve(&EmbeddedAssets);
        let default = LogicalSize::new(px(16.), px(16.));

        assert_eq!(resolved, default);
    }

    #[test]
    fn an_unreadable_asset_falls_back_to_the_default_size() {
        struct SvgOnly;

        impl MarkdownAssets for SvgOnly {
            fn load(&self, path: &str) -> Option<Cow<'static, [u8]>> {
                (path == "icons/phosphoricons/graph.svg")
                    .then_some(Cow::Borrowed(b"<svg/>" as &[u8]))
            }
        }

        let source = InlineImageSource::parse("file://icons/phosphoricons/graph.svg")
            .expect("the fixture URI should be usable");
        let default = LogicalSize::new(px(16.), px(16.));

        assert_eq!(source.resolve(&SvgOnly), default);
        assert!(SvgOnly.load(&source.path).is_some(), "the asset exists");
    }

    #[test]
    fn an_empty_path_names_no_asset() {
        assert_eq!(
            EmbeddedAssets.load(""),
            None,
            "an empty path should not name any asset"
        );
    }
}
