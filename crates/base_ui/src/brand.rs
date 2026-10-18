use gpui_kit::{
    App, Img, IntoElement, RenderOnce, StyleRefinement, Styled, Window, base::StyledExt, img,
};

/// The asset path of Ethereum's mark, relative to `crates/base_ui/assets`.
///
/// The path is stated once and read by both readers of a brand: the [`Brand`] element
/// below, which draws it, and anything that has to *name* the logo rather than draw it —
/// the placeholder sentence of a widget that lists markets writes the same path into an
/// inline image. One path per brand is what keeps those two from drifting apart.
pub const ETHEREUM: &str = "brands/Ethereum/colored@72px.png";

/// The asset path of Solana's mark.
pub const SOLANA: &str = "brands/Solana/colored@72px.png";

/// The asset path of Binance's mark.
pub const BINANCE: &str = "brands/Binance/binance.png";

/// The asset path of OKX's mark.
pub const OKX: &str = "brands/OKX/okx.png";

/// The asset path of Hyperliquid's mark.
pub const HYPERLIQUID: &str = "brands/Hyperliquid/hyperliquid.png";

/// The asset path of Longbridge's mark.
///
/// The file is a placeholder standing in for the venue's own mark, which is acquired
/// and checked for the right to use it before a release carries it; the path is the
/// one the real asset keeps either way.
pub const LONGBRIDGE: &str = "brands/Longbridge/longbridge.png";

#[derive(IntoElement)]
pub struct Brand {
    base: Img,
    style: StyleRefinement,
}

impl Brand {
    pub fn ethereum() -> Self {
        Self {
            base: img(ETHEREUM),
            style: StyleRefinement::default(),
        }
    }

    pub fn solana() -> Self {
        Self {
            base: img(SOLANA),
            style: StyleRefinement::default(),
        }
    }

    pub fn binance() -> Self {
        Self {
            base: img(BINANCE),
            style: StyleRefinement::default(),
        }
    }

    pub fn okx() -> Self {
        Self {
            base: img(OKX),
            style: StyleRefinement::default(),
        }
    }

    pub fn hyperliquid() -> Self {
        Self {
            base: img(HYPERLIQUID),
            style: StyleRefinement::default(),
        }
    }

    pub fn longbridge() -> Self {
        Self {
            base: img(LONGBRIDGE),
            style: StyleRefinement::default(),
        }
    }
}

impl Styled for Brand {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for Brand {
    fn render(self, _window: &mut Window, _: &mut App) -> impl IntoElement {
        let Self { base, style } = self;

        base.refine_style(&style)
    }
}
