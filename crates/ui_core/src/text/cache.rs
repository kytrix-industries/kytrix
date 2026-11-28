use std::{
    collections::VecDeque,
    hash::{Hash, Hasher},
};

use ahash::AHasher;

use super::{TextContent, color_bits, shape::ShapedText};

const ENTRY_LIMIT: usize = 1024;
const BYTE_BUDGET: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ShapeKey {
    text_hash: u64,
    text_len: u32,
    style_hash: u64,
    font_stack_version: u32,
    parley_only: bool,
}

impl ShapeKey {
    pub(crate) fn new(content: &TextContent, font_stack_version: u32) -> Self {
        Self::with_parley_only(content, font_stack_version, false)
    }

    pub(crate) fn parley_only(content: &TextContent, font_stack_version: u32) -> Self {
        Self::with_parley_only(content, font_stack_version, true)
    }

    fn with_parley_only(content: &TextContent, font_stack_version: u32, parley_only: bool) -> Self {
        let mut text_hasher = AHasher::default();
        text_hasher.write(content.text.as_bytes());
        let mut style_hasher = AHasher::default();
        content.style.hash_into(&mut style_hasher);
        content.runs().len().hash(&mut style_hasher);
        for run in content.runs() {
            run.range.start.hash(&mut style_hasher);
            run.range.end.hash(&mut style_hasher);
            run.style.hash_into(&mut style_hasher);
            color_bits(run.style.color).hash(&mut style_hasher);
        }
        content.normalized_runs().hash(&mut style_hasher);
        for inline in content.inline_boxes.iter() {
            inline.id.hash(&mut style_hasher);
            inline.index.hash(&mut style_hasher);
            inline.size.width.value().to_bits().hash(&mut style_hasher);
            inline.size.height.value().to_bits().hash(&mut style_hasher);
            inline
                .baseline
                .map(|baseline| baseline.value().to_bits())
                .hash(&mut style_hasher);
            (inline.align as u8).hash(&mut style_hasher);
        }
        Self {
            text_hash: text_hasher.finish(),
            text_len: content.text.len() as u32,
            style_hash: style_hasher.finish(),
            font_stack_version,
            parley_only,
        }
    }
}

pub(crate) struct ShapeCache {
    entries: ahash::HashMap<ShapeKey, Entry>,
    order: VecDeque<ShapeKey>,
    bytes: usize,
    byte_budget: usize,
}

struct Entry {
    shaped: std::rc::Rc<ShapedText>,
    bytes: usize,
}

impl ShapeCache {
    pub(crate) fn new() -> Self {
        Self {
            entries: ahash::HashMap::default(),
            order: VecDeque::with_capacity(ENTRY_LIMIT / 4),
            bytes: 0,
            byte_budget: BYTE_BUDGET,
        }
    }

    pub(crate) fn get(&mut self, key: &ShapeKey) -> Option<std::rc::Rc<ShapedText>> {
        let shaped = self.entries.get(key)?.shaped.clone();
        if let Some(offset) = self
            .order
            .iter()
            .rev()
            .position(|candidate| candidate == key)
        {
            let index = self.order.len() - 1 - offset;
            self.order.remove(index);
        }
        self.order.push_back(*key);
        Some(shaped)
    }

    pub(crate) fn insert(&mut self, key: ShapeKey, shaped: std::rc::Rc<ShapedText>) {
        let bytes = shaped.approx_bytes();
        if let Some(old) = self.entries.insert(key, Entry { shaped, bytes }) {
            self.bytes = self.bytes.saturating_sub(old.bytes);
        } else {
            self.order.push_back(key);
        }
        self.bytes = self.bytes.saturating_add(bytes);
        while self.entries.len() > ENTRY_LIMIT || self.bytes > self.byte_budget {
            let Some(evicted) = self.order.pop_front() else {
                break;
            };
            if let Some(entry) = self.entries.remove(&evicted) {
                self.bytes = self.bytes.saturating_sub(entry.bytes);
            }
        }
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.bytes = 0;
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::ShapeKey;
    use crate::{
        math::px,
        text::{TextContent, TextStyle, TextWrap},
    };

    #[test]
    fn shape_key_distinguishes_stack_version() {
        let content = TextContent::new("hello", TextStyle::new(px(16.0)));
        let base = ShapeKey::new(&content, 1);

        assert_eq!(base, ShapeKey::new(&content, 1));
        assert_ne!(
            base,
            ShapeKey::new(&content, 2),
            "stack version is part of the key"
        );
    }

    #[test]
    fn shape_key_ignores_color_and_alignment() {
        use crate::{paint::color::Color, text::TextAlign};

        let base = TextContent::new("1,234.56", TextStyle::new(px(13.0)));
        let red = TextContent::new(
            "1,234.56",
            TextStyle::new(px(13.0)).with_color(Color::Srgb {
                r: 1.0,
                g: 0.0,
                b: 0.0,
                alpha: 1.0,
            }),
        );
        let centered = TextContent::new(
            "1,234.56",
            TextStyle::new(px(13.0)).with_alignment(TextAlign::Center),
        );

        assert_eq!(ShapeKey::new(&base, 1), ShapeKey::new(&red, 1));
        assert_eq!(ShapeKey::new(&base, 1), ShapeKey::new(&centered, 1));
        assert!(
            base.style.same_shaping_inputs(&red.style),
            "命中判据必须与键的等价关系一致"
        );
        assert!(base.style.same_shaping_inputs(&centered.style));
    }

    #[test]
    fn shape_key_distinguishes_new_shaping_inputs() {
        use crate::text::{TextFontVariation, TextLineBreak};

        let base = TextContent::new("1,234.56", TextStyle::new(px(13.0)));
        let locale = TextContent::new("1,234.56", TextStyle::new(px(13.0)).with_locale("zh-Hans"));
        let stretched = TextContent::new("1,234.56", TextStyle::new(px(13.0)).with_stretch(80));
        let spaced = TextContent::new(
            "1,234.56",
            TextStyle::new(px(13.0)).with_word_spacing(px(2.0)),
        );
        let line_break = TextContent::new(
            "1,234.56",
            TextStyle::new(px(13.0)).with_line_break(TextLineBreak::Loose),
        );
        let variations = TextContent::new(
            "1,234.56",
            TextStyle::new(px(13.0))
                .with_font_variations([TextFontVariation::new(*b"wght", 700.0)]),
        );

        let base_key = ShapeKey::new(&base, 1);
        for (name, content) in [
            ("locale", &locale),
            ("stretch", &stretched),
            ("word_spacing", &spaced),
            ("line_break", &line_break),
            ("font_variations", &variations),
        ] {
            assert_ne!(
                base_key,
                ShapeKey::new(content, 1),
                "{name} 必须参与形状缓存键"
            );
            assert!(
                !base.style.same_shaping_inputs(&content.style),
                "{name} 必须让命中判据失配"
            );
        }
    }

    #[test]
    fn shape_key_distinguishes_inline_boxes() {
        use crate::{
            math::{LogicalPx, LogicalSize, px},
            text::{InlineAlign, InlineBoxSpec},
        };

        let spec = |id: u64, index: usize, width: LogicalPx| InlineBoxSpec {
            id,
            index,
            size: LogicalSize::new(width, px(8.0)),
            baseline: None,
            align: InlineAlign::Baseline,
            paint: None,
        };
        let content = |boxes: &[InlineBoxSpec]| {
            TextContent::new("ab cd", TextStyle::new(px(16.0)))
                .with_inline_boxes(boxes.iter().copied())
        };

        let plain = ShapeKey::new(&TextContent::new("ab cd", TextStyle::new(px(16.0))), 1);
        let one = ShapeKey::new(&content(&[spec(1, 2, px(8.0))]), 1);
        let other_id = ShapeKey::new(&content(&[spec(2, 2, px(8.0))]), 1);
        let other_index = ShapeKey::new(&content(&[spec(1, 4, px(8.0))]), 1);
        let other_size = ShapeKey::new(&content(&[spec(1, 2, px(12.0))]), 1);
        let two = ShapeKey::new(&content(&[spec(1, 2, px(8.0)), spec(2, 4, px(8.0))]), 1);

        assert_ne!(plain, one, "有/无 inline_boxes 必须区分（变异 M5 的守卫）");
        assert_ne!(one, other_id, "id 必须参与键");
        assert_ne!(one, other_index, "index 必须参与键");
        assert_ne!(one, other_size, "size 必须参与键");
        assert_ne!(one, two, "行内盒序列必须参与键");
        assert_eq!(one, ShapeKey::new(&content(&[spec(1, 2, px(8.0))]), 1));
    }

    #[test]
    fn shape_key_distinguishes_runs() {
        use crate::{paint::color::Color, text::TextRun};

        let plain = TextContent::new("abcdef", TextStyle::new(px(16.0)));
        let one = plain.clone().with_runs([TextRun::new(
            0..3,
            TextStyle::new(px(16.0)).with_weight(700),
        )]);
        let other_range = plain.clone().with_runs([TextRun::new(
            1..4,
            TextStyle::new(px(16.0)).with_weight(700),
        )]);
        let other_style = plain.clone().with_runs([TextRun::new(
            0..3,
            TextStyle::new(px(16.0)).with_weight(800),
        )]);
        let other_size = plain
            .clone()
            .with_runs([TextRun::new(0..3, TextStyle::new(px(18.0)))]);
        let colored = plain.clone().with_runs([TextRun::new(
            0..3,
            TextStyle::new(px(16.0)).with_color(Color::WHITE),
        )]);
        let decorated = plain.clone().with_runs([TextRun::new(
            0..3,
            TextStyle::new(px(16.0)).with_underline(px(1.0), None),
        )]);
        let two = plain.clone().with_runs([
            TextRun::new(0..3, TextStyle::new(px(16.0)).with_weight(700)),
            TextRun::new(3..6, TextStyle::new(px(16.0)).with_weight(800)),
        ]);
        let same = plain.clone().with_runs([TextRun::new(
            0..3,
            TextStyle::new(px(16.0)).with_weight(700),
        )]);

        let base = ShapeKey::new(&plain, 1);
        let one_key = ShapeKey::new(&one, 1);
        assert_ne!(base, one_key, "有/无 runs 必须区分");
        for (name, content) in [
            ("range", &other_range),
            ("weight", &other_style),
            ("size", &other_size),
            ("color", &colored),
            ("decoration", &decorated),
            ("run count", &two),
        ] {
            assert_ne!(
                one_key,
                ShapeKey::new(content, 1),
                "{name} 必须参与形状缓存键"
            );
        }
        assert_eq!(one_key, ShapeKey::new(&same, 1));
        let shaped = crate::text::shape::ShapedText::blank_for_text_test(
            plain.text.clone(),
            plain.style.clone(),
        );
        assert!(
            shaped.same_shaping_inputs(&plain.style, plain.runs()),
            "空 runs 的快照必须与空 runs 的内容同源"
        );
        assert!(!shaped.same_shaping_inputs(&plain.style, one.runs()));
    }

    #[test]
    fn shape_key_distinguishes_decoration_sets_and_backgrounds() {
        use crate::{
            paint::color::Color,
            text::{TextBackground, TextRun},
        };

        let highlighted = TextBackground {
            color: Color::WHITE,
            corner_radius: px(2.0),
            padding_x: px(2.0),
        };
        let run = |style: TextStyle| {
            TextContent::new("abcdef", TextStyle::new(px(16.0)))
                .with_runs([TextRun::new(0..3, style)])
        };
        let underline = run(TextStyle::new(px(16.0)).with_underline(px(1.0), None));
        let strike = run(TextStyle::new(px(16.0)).with_strikethrough(px(1.0), None));
        let both = run(TextStyle::new(px(16.0))
            .with_underline(px(1.0), None)
            .with_strikethrough(px(1.0), None));
        let colored_line =
            run(TextStyle::new(px(16.0)).with_underline(px(1.0), Some(Color::WHITE)));
        let thicker_line = run(TextStyle::new(px(16.0)).with_underline(px(2.0), None));
        let background = run(TextStyle::new(px(16.0)).with_background(highlighted));
        let other_radius = run(TextStyle::new(px(16.0)).with_background(TextBackground {
            corner_radius: px(4.0),
            ..highlighted
        }));
        let other_padding = run(TextStyle::new(px(16.0)).with_background(TextBackground {
            padding_x: px(4.0),
            ..highlighted
        }));
        let other_fill = run(TextStyle::new(px(16.0)).with_background(TextBackground {
            color: Color::BLACK,
            ..highlighted
        }));

        let underline_key = ShapeKey::new(&underline, 1);
        for (name, content) in [
            ("strike", &strike),
            ("both", &both),
            ("line color", &colored_line),
            ("line thickness", &thicker_line),
            ("background", &background),
        ] {
            assert_ne!(
                underline_key,
                ShapeKey::new(content, 1),
                "{name} 必须参与形状缓存键"
            );
        }
        let background_key = ShapeKey::new(&background, 1);
        for (name, content) in [
            ("corner radius", &other_radius),
            ("padding", &other_padding),
            ("background fill", &other_fill),
        ] {
            assert_ne!(
                background_key,
                ShapeKey::new(content, 1),
                "只改背景{name}也必须换键（漏哈希该字段即红）"
            );
        }

        let plain = TextContent::new("abcdef", TextStyle::new(px(16.0)));
        let highlighted_base = TextContent::new(
            "abcdef",
            TextStyle::new(px(16.0)).with_background(highlighted),
        );
        assert!(
            !plain.style.same_shaping_inputs(&highlighted_base.style),
            "基础样式背景必须让命中判据失配"
        );
        assert_ne!(
            ShapeKey::new(&plain, 1),
            ShapeKey::new(&highlighted_base, 1)
        );
    }

    #[test]
    fn shape_key_distinguishes_base_style_decorations() {
        use crate::paint::color::Color;

        let plain = TextContent::new("abcdef", TextStyle::new(px(16.0)));
        let underline = TextContent::new(
            "abcdef",
            TextStyle::new(px(16.0)).with_underline(px(1.0), None),
        );
        let strike = TextContent::new(
            "abcdef",
            TextStyle::new(px(16.0)).with_strikethrough(px(1.0), None),
        );
        let both = TextContent::new(
            "abcdef",
            TextStyle::new(px(16.0))
                .with_underline(px(1.0), None)
                .with_strikethrough(px(1.0), None),
        );
        let colored_line = TextContent::new(
            "abcdef",
            TextStyle::new(px(16.0)).with_underline(px(1.0), Some(Color::WHITE)),
        );

        let plain_key = ShapeKey::new(&plain, 1);
        for (name, content) in [
            ("underline", &underline),
            ("strikethrough", &strike),
            ("both", &both),
            ("line color", &colored_line),
        ] {
            assert_ne!(
                plain_key,
                ShapeKey::new(content, 1),
                "只有基础样式{name}不同的文本也必须换形状缓存键"
            );
            assert!(
                !plain.style.same_shaping_inputs(&content.style),
                "基础样式{name}不同必须让命中判据失配"
            );
        }
        assert_ne!(
            ShapeKey::new(&underline, 1),
            ShapeKey::new(&both, 1),
            "基础样式的装饰集合不同也必须换键"
        );
    }

    #[test]
    fn shape_key_distinguishes_text_and_style() {
        let style = TextStyle::new(px(16.0));
        let a = ShapeKey::new(&TextContent::new("ab", style.clone()), 1);
        let b = ShapeKey::new(&TextContent::new("ba", style.clone()), 1);
        let c = ShapeKey::new(
            &TextContent::new("ab", style.with_wrap(TextWrap::NoWrap)),
            1,
        );

        assert_ne!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn shape_key_distinguishes_font_features() {
        use crate::text::TextFeature;

        let plain = TextContent::new("12345", TextStyle::new(px(16.0)));
        let tabular = TextContent::new(
            "12345",
            TextStyle::new(px(16.0)).with_font_features([TextFeature::TABULAR_NUMBERS]),
        );

        assert_ne!(
            ShapeKey::new(&plain, 1),
            ShapeKey::new(&tabular, 1),
            "font_features 必须参与缓存键"
        );
        let again = TextContent::new(
            "12345",
            TextStyle::new(px(16.0)).with_font_features([TextFeature::TABULAR_NUMBERS]),
        );
        assert_eq!(ShapeKey::new(&tabular, 1), ShapeKey::new(&again, 1));
    }
}
