//! markdown 图片的 `file:` URI：语法、资源解析与固有尺寸。
//!
//! 一段行内图片写成 `![alt](file://{asset_path}?type={img|svg}&size={size})`，
//! 其中：
//!
//! - `file:` 是 scheme（大小写**敏感**，只认小写的 `file:`；按 URL 的通行写法，
//!   `file:` 后面允许写 `//`，写了就跳过它）。`file://` 之外的 scheme（`https:`、`icon:`）
//!   与相对路径一律不是本组件的图片，整条 URI 作废。
//! - `{asset_path}` 是 [`AssetSource::load`] 收到的那一串路径，也就是相对
//!   `crates/base_ui/assets` 的路径（`brands/Binance/binance.png`、
//!   `icons/phosphoricons/graph.svg`）。这里**不做任何前缀剥离**：写什么就去资源里找什么。
//! - `type` 决定用哪种画法：`img` 走 gpui 的 `img()`（位图，自身带尺寸），
//!   `svg` 走 gpui 的 `svg()`（按主题文字色染色的单色矢量图，自身没有尺寸）。
//!   两种取值都可以写成裸值或带双引号（`type=img` / `type="img"`）。省略 `type` 时按
//!   [`asset_path`](InlineImageSource::path) 的扩展名判断：`.svg` 是 `svg`，其余是 `img`。
//! - `size` 是绘制尺寸，两种写法：单个数字表示宽高都是它，`width,height` 表示宽度与高度
//!   各自取值。都可以带双引号。不写 `size` 时用资源自身的尺寸（见
//!   [`InlineImageSource::resolve`]）。**尺寸与画法互不影响**：`type` 只决定用哪个元素画，
//!   没写 `size` 时两者都按资源自身的尺寸取值。
//!
//! query 里未知的键（`color=red`）、空段（`&&`、结尾的 `&`）一律忽略：它们既不影响
//! 解析结果，也不会让整条 URI 作废。同名的 `type` 或 `size` 出现两次（`size=16&size=20`）
//! 则整条 URI 作废——两个互相矛盾的值不猜哪个算数；反过来，一段 query 里没有 `=`（`?size`）
//! 是写法不合法，整条 URI 同样作废。
//!
//! **尺寸**：显式写出的边长解析失败（`abc`、`-3`、`1e3`、`20px`、`1.2.3`、`20,`）时回退到
//! 「没写 size」，于是接着按资源自身的尺寸取值；解析成功后 clamp 到
//! [`MIN_IMAGE_SIZE`]..=[`MAX_IMAGE_SIZE`]（0 变 1、9999 变 256）。资源自身的尺寸**不** clamp：
//! 一张 512×512 的图就该是 512×512 那么大（与超宽图片一样溢出，见 `inline_flow` 的已知限制）。
//! 找不到资源、或者资源不是能读出尺寸的位图（例如 `svg`）时用 [`DEFAULT_IMAGE_SIZE`]。

use std::io::Cursor;

use gpui_kit::{AssetSource, Pixels, Size, px, size};

/// 图片 URI 的 scheme。
const SCHEME: &str = "file:";
/// `size` 查询键。
const SIZE_KEY: &str = "size";
/// `type` 查询键。
const TYPE_KEY: &str = "type";
/// `type` 的两个取值。
const TYPE_IMAGE: &str = "img";
const TYPE_SVG: &str = "svg";
/// 图片的扩展名（不含点，小写）。判 `type` 与读固有尺寸都用它。
const SVG_EXTENSION: &str = "svg";
/// 没有固有尺寸可用时（缺资源、svg、读不出尺寸的字节）的边长。
const DEFAULT_IMAGE_SIZE: f32 = 16.;
/// 显式写出的边长允许的最小值。
const MIN_IMAGE_SIZE: f32 = 1.;
/// 显式写出的边长允许的最大值。
const MAX_IMAGE_SIZE: f32 = 256.;
/// 资源自身尺寸为 0（畸形图片）时不画任何东西，也就不能占位。
const MIN_INTRINSIC_SIZE: f32 = 1.;

/// 行内图片的画法。
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum InlineImageFormat {
    /// 位图：交给 gpui 的 `img()` 解码绘制。
    Image,
    /// 单色矢量图：交给 gpui 的 `svg()` 按文字色染色绘制。
    Svg,
}

/// 一条合法 `file:` URI 解析出来的东西：画什么、画多大。
///
/// `size` 为 `None` 表示原文没有写 `size`（或者写出的大小用不了），最终尺寸要等
/// [`resolve`](Self::resolve) 拿到资源之后才知道。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InlineImageSource {
    pub path: String,
    pub format: InlineImageFormat,
    pub size: Option<Size<Pixels>>,
}

impl InlineImageSource {
    /// 解析一条 `file:` URI；不合法（不是 `file:`、没有路径、query 里有重复键或没有 `=` 的段）
    /// 时返回 `None`。
    pub fn parse(uri: &str) -> Option<Self> {
        let reference = uri.strip_prefix(SCHEME)?;
        // `file://x` 与 `file:x` 等价：`//` 是 URL 里 file scheme 的习惯写法，这里让过去。
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
            path: path.to_string(),
            size,
        })
    }

    /// 这条图片最终画多大：原文写了 `size` 就是它；没写（或写坏了）时用资源自身的尺寸。
    ///
    /// 资源自身的尺寸来自资源的**文件头**（只有位图有），svg 与读不出来的字节都用
    /// [`DEFAULT_IMAGE_SIZE`]。只有显式写出的边长才 clamp，见模块文档。
    pub fn resolve(&self, assets: &dyn AssetSource) -> Size<Pixels> {
        if let Some(size) = self.size {
            return size;
        }
        let bytes = assets.load(&self.path).ok().flatten();
        let intrinsic = bytes
            .as_deref()
            .and_then(intrinsic_size)
            .filter(|size| size.width > Pixels::ZERO && size.height > Pixels::ZERO)
            .unwrap_or_else(|| square(DEFAULT_IMAGE_SIZE));

        size(
            intrinsic.width.max(px(MIN_INTRINSIC_SIZE)),
            intrinsic.height.max(px(MIN_INTRINSIC_SIZE)),
        )
    }
}

/// 按扩展名判断画法：`.svg` 是矢量图，其余一律按位图（包括没有扩展名的路径）。
fn format_of(path: &str) -> InlineImageFormat {
    if extension_of(path).as_deref() == Some(SVG_EXTENSION) {
        InlineImageFormat::Svg
    } else {
        InlineImageFormat::Image
    }
}

/// 路径的扩展名，小写、不含点；没有扩展名时为 `None`。
///
/// 只看最后一个 `.` 之后那一段，而且那一段必须落在路径最后（`a.png/x` 没有扩展名）。
/// 扩展名区分大小写不敏感（`.PNG` 与 `.png` 一样）。
fn extension_of(path: &str) -> Option<String> {
    let (_, extension) = path.rsplit_once('.')?;
    if extension.is_empty() || extension.contains('/') {
        return None;
    }
    Some(extension.to_ascii_lowercase())
}

/// 去掉一层成对的双引号；引号不成对、或者值里还有别的引号时为 `None`（这一段写法不合法）。
///
/// 这里只接受**双**引号：markdown 的链接目标本身可以用 `<…>` 包起来，再掺进单引号会让
/// 调用点很难看出哪一层是谁的。
fn unquote(value: &str) -> Option<&str> {
    match value.strip_prefix('"') {
        Some(inner) => inner.strip_suffix('"').filter(|inner| !inner.contains('"')),
        None => (!value.contains('"')).then_some(value),
    }
}

/// 解析 `size` 的值：`{边长}` 或 `{宽},{高}`。
///
/// 返回 `None` 表示"这个值用不了"，按没写 `size` 处理——写成 `width,`（少一个数）、
/// `1.2.3`、`abc` 都是这种。逗号多于一个时按同一个规则落空。
fn parse_size(value: &str) -> Option<Size<Pixels>> {
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

    Some(size(
        px(width.clamp(MIN_IMAGE_SIZE, MAX_IMAGE_SIZE)),
        px(height.clamp(MIN_IMAGE_SIZE, MAX_IMAGE_SIZE)),
    ))
}

/// 一个"普通数字"：非空，且只由 ASCII 数字与至多一个 `.` 组成。
///
/// 先做字符集检查再交给 `parse`，于是 `-3`、`+3`、`1e3`、`inf`、`NaN`、` 20` 都不会被接受。
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

/// 位图字节流文件头里的宽高；不是能读出尺寸的位图时为 `None`。
///
/// 按字节流的魔数判断格式，再让 `image` 只读文件头（`into_dimensions` 不解码像素），
/// 所以一张 512×512 的图也只读开头那几个字节。魔数认不出来（例如 svg 的 XML 文本）时
/// 返回 `None`，调用方按没有固有尺寸处理。
fn intrinsic_size(bytes: &[u8]) -> Option<Size<Pixels>> {
    let (width, height) = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()?;

    Some(size(px(width as f32), px(height as f32)))
}

/// 边长都是 `side` 的正方形尺寸。
fn square(side: f32) -> Size<Pixels> {
    size(px(side), px(side))
}

#[cfg(test)]
mod tests {
    use gpui_kit::{AssetSource as _, Pixels, Size, px, size};

    use super::{InlineImageFormat, InlineImageSource};
    use crate::assets::Assets;

    /// 一条解析成功的 URI：路径、画法与显式尺寸。
    fn parsed(uri: &str) -> (String, InlineImageFormat, Option<Size<Pixels>>) {
        let source = InlineImageSource::parse(uri)
            .unwrap_or_else(|| panic!("{uri} should be a usable image URI"));
        (source.path, source.format, source.size)
    }

    /// 只关心路径与画法时的解析断言。
    fn parsed_as(uri: &str, path: &str, format: InlineImageFormat) {
        assert_eq!(
            parsed(uri),
            (path.to_string(), format, None),
            "{uri} should parse to that path and format"
        );
    }

    /// 显式尺寸的断言。
    fn parsed_size(uri: &str, expected: Size<Pixels>) {
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

    /// 只有 scheme 之后紧跟的那一对斜杠会被跳过（`file://a` 与 `file:a` 等价）；写一个斜杠
    /// 不等于写了两个，那一个斜杠是路径的一部分。
    #[test]
    fn a_single_slash_is_part_of_the_path() {
        parsed_as(
            "file:/brands/Binance/binance.png",
            "/brands/Binance/binance.png",
            InlineImageFormat::Image,
        );
    }

    /// 路径里的字符一律原样保留（只有 `?` 在前面就被切掉了）：`//` 只在 scheme 之后那一次被
    /// 跳过，路径中间的斜杠、点、`@` 与中文都照原样交给资源源（真资源名里带 `@` 的
    /// `colored@72px.png` 就是例子）。
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
        // `#`（URL 里的 fragment）在本组件支持的写法里没有位置，所以它和别的字符一样是路径的
        // 一部分：这样的路径在资源里找不到，图片按默认尺寸画不出来，而不是被悄悄截短。
        parsed_as("file://a/b#c.png", "a/b#c.png", InlineImageFormat::Image);
        parsed_as("file://a/b&c.png", "a/b&c.png", InlineImageFormat::Image);
        parsed_as("file://a/b\"c.png", "a/b\"c.png", InlineImageFormat::Image);
    }

    /// 资源路径走不出内嵌资源：`..`、盘符、前导斜杠与反斜杠都只是资源名的一部分，内嵌资源里
    /// 没有它们，于是既读不到任何文件、也不会 panic，只按默认尺寸留一个空盒子。
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
            let default = size(px(16.), px(16.));

            assert_eq!(
                Assets
                    .load(&source.path)
                    .expect("the asset source should answer"),
                None,
                "{uri} should not name anything the asset source can read"
            );
            assert_eq!(
                source.resolve(&Assets),
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
        parsed_size("file://a/b.png?size=24", size(px(24.), px(24.)));
        parsed_size("file://a/b.png?size=24,48", size(px(24.), px(48.)));
        parsed_size("file://a/b.png?size=3.5", size(px(3.5), px(3.5)));
        parsed_size("file://a/b.png?size=24.5,48.25", size(px(24.5), px(48.25)));
        // 前导零也是数字：写成 `00024` 与 `24` 是同一个尺寸。
        parsed_size("file://a/b.png?size=00024", size(px(24.), px(24.)));
    }

    #[test]
    fn size_may_be_quoted_and_lives_anywhere_in_the_query() {
        parsed_size("file://a/b.png?size=\"24,48\"", size(px(24.), px(48.)));
        parsed_size("file://a/b.png?size=24,48&type=img", size(px(24.), px(48.)));
        parsed_size("file://a/b.png?type=img&size=24,48", size(px(24.), px(48.)));
    }

    /// clamp 的两端与边界值：`1` 与 `256` 原样通过，越界的分别落到两端。
    ///
    /// 两个边界值各自独立：`0,0` 两条边都夹到 1。`.5` 是一个**能解析**的数（先导点属于
    /// [`is_plain_number`] 认的写法），所以它夹到 1，而不是像 `1.2.3` 那样落回"没写 size"——
    /// 两者在尺寸上碰巧可能一样（资源自身是 40×30），在"写坏了"与"写得太小"之间却不同。
    #[test]
    fn an_explicit_size_is_clamped() {
        parsed_size("file://a/b.png?size=0", size(px(1.), px(1.)));
        parsed_size("file://a/b.png?size=0.5", size(px(1.), px(1.)));
        parsed_size("file://a/b.png?size=.5", size(px(1.), px(1.)));
        parsed_size("file://a/b.png?size=0,0", size(px(1.), px(1.)));
        parsed_size("file://a/b.png?size=1", size(px(1.), px(1.)));
        parsed_size("file://a/b.png?size=256", size(px(256.), px(256.)));
        parsed_size("file://a/b.png?size=9999", size(px(256.), px(256.)));
        parsed_size("file://a/b.png?size=256.5", size(px(256.), px(256.)));
        parsed_size("file://a/b.png?size=0,9999", size(px(1.), px(256.)));
        parsed_size("file://a/b.png?size=9999,0", size(px(256.), px(1.)));
    }

    /// 位数多到 `f32` 装不下时 `parse` 交出来的是无穷大，clamp 之后仍然是上限：没有溢出、
    /// 也没有 panic（`size` 的字符集检查已经挡住了 `NaN` / `inf` 这些字面量）。
    #[test]
    fn an_absurdly_long_number_is_clamped_instead_of_overflowing() {
        let uri = format!("file://a/b.png?size={}", "9".repeat(400));

        parsed_size(&uri, size(px(256.), px(256.)));
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
        parsed_size("file://a/b.png?color=red&size=24", size(px(24.), px(24.)));
        parsed_size("file://a/b.png?&&size=24&&", size(px(24.), px(24.)));
        parsed_as("file://a/b.png?", "a/b.png", InlineImageFormat::Image);
    }

    #[test]
    fn a_query_segment_without_a_value_is_rejected() {
        for uri in [
            "file://a/b.png?size",
            "file://a/b.png?type",
            "file://a/b.png?color",
            // 路径里的 `?` 打不出来：`?` 之后的一切都是 query，而这一段没有 `=`，整条 URI 作废。
            // 这比"截到 `?` 之前"安全——截出来的 `a` 可能是另一个真实资源。
            "file://a?b/c.png",
        ] {
            assert_eq!(
                InlineImageSource::parse(uri),
                None,
                "{uri} should be rejected whole"
            );
        }
    }

    /// 没写 `size` 时用资源自身的尺寸：真资源读出真实宽高，找不到的资源与没有固有尺寸的
    /// 资源（svg）都用默认边长。
    #[test]
    fn a_missing_size_resolves_to_the_asset_size() {
        let resolve = |uri: &str| {
            InlineImageSource::parse(uri)
                .expect("the fixture URI should be usable")
                .resolve(&Assets)
        };

        assert_eq!(
            resolve("file://brands/Binance/binance.png"),
            size(px(128.), px(128.))
        );
        assert_eq!(
            resolve("file://brands/Ethereum/colored@72px.png"),
            size(px(44.), px(72.))
        );
        // 示例文档里那几张 `brands/`：这张数也是那份文档对读者说的数。
        assert_eq!(
            resolve("file://brands/Solana/colored@72px.png"),
            size(px(81.), px(72.))
        );
        assert_eq!(
            resolve("file://brands/Hyperliquid/hyperliquid.png"),
            size(px(180.), px(180.))
        );
        assert_eq!(
            resolve("file://brands/OKX/okx.png"),
            size(px(200.), px(200.))
        );
        assert_eq!(resolve("file://nope/nothing.png"), size(px(16.), px(16.)));
        assert_eq!(
            resolve("file://icons/phosphoricons/graph.svg"),
            size(px(16.), px(16.))
        );
    }

    /// 显式写出的 `size` 与资源无关：资源不存在也照样用它，连资源都不必读。
    #[test]
    fn an_explicit_size_wins_over_the_asset_size() {
        let resolve = |uri: &str| {
            InlineImageSource::parse(uri)
                .expect("the fixture URI should be usable")
                .resolve(&Assets)
        };

        assert_eq!(
            resolve("file://brands/Binance/binance.png?size=24,40"),
            size(px(24.), px(40.))
        );
        assert_eq!(
            resolve("file://nope/nothing.png?size=40"),
            size(px(40.), px(40.))
        );
    }

    /// 资源存在但不是位图（扩展名骗人）时不能 panic，按没有固有尺寸处理。
    #[test]
    fn a_resource_that_is_not_a_bitmap_falls_back_to_the_default_size() {
        let resolved = InlineImageSource::parse("file://icons/phosphoricons/graph.svg?type=img")
            .expect("the fixture URI should be usable")
            .resolve(&Assets);
        let default = size(px(16.), px(16.));

        assert_eq!(resolved, default);
    }

    /// 资源本身在，但它不是能读出尺寸的位图（svg 的 XML 文本）：`image` 读不出尺寸也不能
    /// panic，按没有固有尺寸处理。
    #[test]
    fn an_unreadable_asset_falls_back_to_the_default_size() {
        let source = InlineImageSource::parse("file://icons/phosphoricons/graph.svg")
            .expect("the fixture URI should be usable");
        let default = size(px(16.), px(16.));

        assert_eq!(source.resolve(&Assets), default);
        assert!(
            Assets
                .load(&source.path)
                .expect("the asset exists")
                .is_some()
        );
    }
}
