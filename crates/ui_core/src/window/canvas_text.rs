use std::rc::Rc;

use crate::{
    paint::atlas::{AtlasKind, AtlasTile},
    render::AtlasAllocator,
    text::ShapedLine,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CanvasTextKey {
    pub canvas_generation: u32,
    pub font_stack_version: u32,
    pub dpr_bits: u32,
    pub atlas_generation: u32,
    pub color_atlas_generation: u32,
    pub glyph_eviction_epoch: u32,
}

pub(crate) struct ResolvedCanvasText {
    pub key: CanvasTextKey,
    pub lines: Vec<Rc<ShapedLine>>,
    pub pending: bool,
}

#[derive(Clone, Copy)]
pub(crate) struct GlyphAtlases<'a> {
    pub glyph: &'a AtlasAllocator,
    pub color: &'a AtlasAllocator,
}

impl GlyphAtlases<'_> {
    pub(crate) fn tile_is_alive(&self, tile: &AtlasTile) -> bool {
        match tile.atlas {
            AtlasKind::ColorGlyph => self.color.is_alive(tile),
            _ => self.glyph.is_alive(tile),
        }
    }

    pub(crate) fn line_is_live(&self, line: &ShapedLine) -> bool {
        line.atlas_generation == self.glyph.generation()
            && line.color_atlas_generation == self.color.generation()
            && line.glyph_runs().all(|run| {
                run.glyphs
                    .iter()
                    .all(|glyph| self.tile_is_alive(&glyph.tile))
            })
    }

    pub(crate) fn lines_are_live(&self, lines: &[Rc<ShapedLine>]) -> bool {
        lines.iter().all(|line| self.line_is_live(line))
    }
}

pub(crate) fn lines_are_live(lines: &[Rc<ShapedLine>], atlases: GlyphAtlases<'_>) -> bool {
    atlases.lines_are_live(lines)
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use super::GlyphAtlases;
    use crate::{
        math::{DevicePixelRatio, LogicalPoint, LogicalSize, px},
        paint::{
            atlas::{AtlasKind, AtlasTile},
            color::Color,
            primitive::PositionedGlyph,
        },
        render::AtlasAllocator,
        text::{ShapedGlyphRun, ShapedLine, ShapedTextItem, ShapedTextLine},
    };

    fn line_with_tile(tile: AtlasTile, glyph_generation: u32, color_generation: u32) -> ShapedLine {
        ShapedLine {
            text: smol_str::SmolStr::new_static("ab"),
            lines: vec![ShapedTextLine {
                text_range: 0..2,
                baseline: px(12.0),
                line_height: px(16.0),
                advance: px(20.0),
                items: vec![ShapedTextItem::GlyphRun(ShapedGlyphRun {
                    color: Color::BLACK,
                    text_range: 0..2,
                    glyphs: vec![PositionedGlyph {
                        tile,
                        position: LogicalPoint::ZERO,
                        size: LogicalSize::new(px(8.0), px(8.0)),
                    }],
                })],
                decorations: Vec::new(),
            }],
            width: px(20.0),
            height: px(16.0),
            first_baseline: Some(px(12.0)),
            atlas_generation: glyph_generation,
            color_atlas_generation: color_generation,
            font_stack_version: 1,
            scale: DevicePixelRatio::ONE,
        }
    }

    fn live(atlases: GlyphAtlases<'_>, line: &ShapedLine) -> bool {
        atlases.line_is_live(line)
    }

    #[test]
    fn a_color_atlas_generation_change_alone_breaks_line_liveness() {
        let mut glyph = AtlasAllocator::with_page(AtlasKind::Glyph, (64, 64));
        let mut color = AtlasAllocator::with_page(AtlasKind::ColorGlyph, (64, 64));
        let allocation = glyph.allocate((8, 8)).expect("tile fits");
        let line = line_with_tile(allocation.tile, glyph.generation(), color.generation());
        assert!(live(
            GlyphAtlases {
                glyph: &glyph,
                color: &color
            },
            &line
        ));

        assert!(color.grow(), "彩色页扩容换代");
        assert!(
            !live(
                GlyphAtlases {
                    glyph: &glyph,
                    color: &color
                },
                &line
            ),
            "仅彩色代际变化也必须判定行不可用"
        );

        let mut updated = line;
        updated.color_atlas_generation = color.generation();
        assert!(
            live(
                GlyphAtlases {
                    glyph: &glyph,
                    color: &color
                },
                &updated
            ),
            "更新彩色代际后行重新可用（证明比较项是彩色代际而非恒假）"
        );
    }

    #[test]
    fn a_glyph_atlas_generation_change_alone_breaks_line_liveness() {
        let mut glyph = AtlasAllocator::with_page(AtlasKind::Glyph, (64, 64));
        let color = AtlasAllocator::with_page(AtlasKind::ColorGlyph, (64, 64));
        let allocation = glyph.allocate((8, 8)).expect("tile fits");
        let line = line_with_tile(allocation.tile, glyph.generation(), color.generation());
        assert!(live(
            GlyphAtlases {
                glyph: &glyph,
                color: &color
            },
            &line
        ));

        let old_generation = glyph.generation();
        assert!(glyph.grow(), "单色页扩容换代");
        let reallocated = glyph.allocate((8, 8)).expect("扩容后的页仍有空位");
        let stale = line_with_tile(reallocated.tile, old_generation, color.generation());
        assert!(
            !live(
                GlyphAtlases {
                    glyph: &glyph,
                    color: &color
                },
                &stale
            ),
            "tile 仍存活但单色代际过期时，行也必须不可用"
        );

        let relaid = line_with_tile(reallocated.tile, glyph.generation(), color.generation());
        assert!(
            live(
                GlyphAtlases {
                    glyph: &glyph,
                    color: &color
                },
                &relaid
            ),
            "换新代际 + 新 tile 后行重新可用（证明比较项是单色代际而非恒假）"
        );
    }

    #[test]
    fn lines_are_live_checks_every_line() {
        let glyph = AtlasAllocator::with_page(AtlasKind::Glyph, (64, 64));
        let color = AtlasAllocator::with_page(AtlasKind::ColorGlyph, (64, 64));
        let atlases = GlyphAtlases {
            glyph: &glyph,
            color: &color,
        };
        let live_line = line_with_tile(
            crate::paint::atlas::AtlasTile {
                atlas: AtlasKind::Glyph,
                page: 0,
                u0: 0.0,
                v0: 0.0,
                u1: 0.5,
                v1: 0.5,
                generation: glyph.generation(),
                slot: 0,
                slot_epoch: 0,
            },
            glyph.generation(),
            color.generation(),
        );
        let mut stale = live_line.clone();
        stale.color_atlas_generation = color.generation().wrapping_add(1);
        assert!(!atlases.lines_are_live(&[Rc::new(live_line), Rc::new(stale)]));
    }
}
