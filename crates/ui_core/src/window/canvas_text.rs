use std::rc::Rc;

use crate::{render::AtlasAllocator, text::ShapedLine};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CanvasTextKey {
    pub canvas_generation: u32,
    pub font_stack_version: u32,
    pub dpr_bits: u32,
    pub atlas_generation: u32,
    pub glyph_eviction_epoch: u32,
}

pub(crate) struct ResolvedCanvasText {
    pub key: CanvasTextKey,
    pub lines: Vec<Rc<ShapedLine>>,
    pub pending: bool,
}

pub(crate) fn line_is_live(line: &ShapedLine, atlas: &AtlasAllocator) -> bool {
    line.atlas_generation == atlas.generation()
        && line
            .glyph_runs()
            .all(|run| run.glyphs.iter().all(|glyph| atlas.is_alive(&glyph.tile)))
}

pub(crate) fn lines_are_live(lines: &[Rc<ShapedLine>], atlas: &AtlasAllocator) -> bool {
    lines.iter().all(|line| line_is_live(line, atlas))
}
