//! 形状缓存：`ShapeKey` + `Rc<ShapedText>` 的 LRU（条目数 + 字节双上限）。
//!
//! 设计见 `design/phase-05-text.md` §11。键**不含** `atlas_generation`（F-53）：图集换代只影响
//! 字形的 `AtlasTile`，不影响字形 id / advance / 基线 / 行数。
//!
//! 键也**不含水平亚像素相位**（P2-2）：相位是 `rasterize` 阶段由最终布局坐标派生的量化值
//! （`raster::split_subpixel_x`），不是 shape 的输入。同一条 `ShapedText` + 同一内容宽重排出的
//! 字形位置逐位相同 ⇒ 相位相同；`layout_text` 只在内容宽或图集代际变化时重建 `ShapedLine`，
//! 相位随该快照一起失效。

use std::{
    collections::VecDeque,
    hash::{Hash, Hasher},
};

use ahash::AHasher;

use super::{TextContent, shape::ShapedText};

/// 条目上限（§9.3 的「容量上限显式」）。
///
/// **从 256 提到 1024**（P1-2.3）：窗口里不同文本超过 256 条时，LRU 在两次全量扫描之间必然
/// 逐出，`Rc::ptr_eq` 永不命中 → `set_text_shaped` 清空 `line/broken` → `layout_text` 全量
/// 重排 + 重光栅。行情表 / 成交明细这类「几百个标签同时存活」的界面会稳定踩到这个上限。
///
/// 1024 条配合 16 MiB 字节预算：典型 12–16 px 标签约 1–3 KiB/条，1024 条在预算内。
const ENTRY_LIMIT: usize = 1024;
/// 字节上限（16 MiB）。
///
/// 与 `ENTRY_LIMIT` 同时提高：`approx_bytes` 修正为按字形计数后，长文本条目会占用更多预算，
/// 8 MiB 在多窗口 + CJK 场景下会过早逐出。
const BYTE_BUDGET: usize = 16 * 1024 * 1024;

/// 形状缓存键：文本哈希 + 样式哈希 + 字体栈版本。
///
/// **不含** `scale`（R12-4：逻辑坐标 shaping，DPI 变化不再清缓存）、**不含** `color` /
/// `alignment`（R12-11：两者不影响 glyph id / advance / 断行）。
///
/// 命中后仍要比较 `text`/`style`（防哈希碰撞），比较对象是缓存条目里的 `ShapedText`；
/// 等价关系必须与键一致 —— 见 `TextStyle::same_shaping_inputs`。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ShapeKey {
    text_hash: u64,
    text_len: u32,
    style_hash: u64,
    font_stack_version: u32,
}

impl ShapeKey {
    /// 由内容 + 字体栈版本构造。
    pub(crate) fn new(content: &TextContent, font_stack_version: u32) -> Self {
        let mut text_hasher = AHasher::default();
        text_hasher.write(content.text.as_bytes());
        let mut style_hasher = AHasher::default();
        content.style.hash_into(&mut style_hasher);
        for inline in content.inline_boxes.iter() {
            inline.id.hash(&mut style_hasher);
            inline.index.hash(&mut style_hasher);
            inline.size.width.to_bits().hash(&mut style_hasher);
            inline.size.height.to_bits().hash(&mut style_hasher);
            inline.baseline.map(f32::to_bits).hash(&mut style_hasher);
            (inline.align as u8).hash(&mut style_hasher);
        }
        Self {
            text_hash: text_hasher.finish(),
            text_len: content.text.len() as u32,
            style_hash: style_hasher.finish(),
            font_stack_version,
        }
    }
}

/// 形状缓存（LRU：命中即提升到队尾，超限从队首逐出）。
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

    /// 取条目（命中即提升顺序）。
    ///
    /// LRU 提升用「从队尾线性查找」而不是全表扫描：命中的条目大概率就在队尾附近（刚用过），
    /// 因此平均移动距离很小；`position` 从队首扫会在 1024 条时每次命中都走 1000 步。
    pub(crate) fn get(&mut self, key: &ShapeKey) -> Option<std::rc::Rc<ShapedText>> {
        let shaped = self.entries.get(key)?.shaped.clone();
        // 队尾是「最近使用」，从后往前找能让命中项很快被发现。
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

    /// 插入（超限逐出）。
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

    /// 清空（字体栈/系统字体变化时，与 `font_stack_version += 1` 同一事务）。
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.bytes = 0;
    }

    /// 仅测试：条目数。
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// 仅测试：字节估算。
    #[cfg(test)]
    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::ShapeKey;
    use crate::text::{TextContent, TextStyle, TextWrap};

    #[test]
    fn shape_key_distinguishes_stack_version() {
        let content = TextContent::new("hello", TextStyle::new(16.0));
        let base = ShapeKey::new(&content, 1);

        assert_eq!(base, ShapeKey::new(&content, 1));
        assert_ne!(
            base,
            ShapeKey::new(&content, 2),
            "stack version is part of the key"
        );
    }

    /// R12-11：颜色与对齐不是 shaping 输入 ⇒ 不进缓存键（红/绿两态共享一份快照）。
    #[test]
    fn shape_key_ignores_color_and_alignment() {
        use crate::{paint::color::Color, text::TextAlign};

        let base = TextContent::new("1,234.56", TextStyle::new(13.0));
        let red = TextContent::new(
            "1,234.56",
            TextStyle::new(13.0).with_color(Color::Srgb {
                r: 1.0,
                g: 0.0,
                b: 0.0,
                alpha: 1.0,
            }),
        );
        let centered = TextContent::new(
            "1,234.56",
            TextStyle::new(13.0).with_alignment(TextAlign::Center),
        );

        assert_eq!(ShapeKey::new(&base, 1), ShapeKey::new(&red, 1));
        assert_eq!(ShapeKey::new(&base, 1), ShapeKey::new(&centered, 1));
        assert!(
            base.style.same_shaping_inputs(&red.style),
            "命中判据必须与键的等价关系一致"
        );
        assert!(base.style.same_shaping_inputs(&centered.style));
    }

    /// R12-10：新增的 shaping 输入必须参与缓存键（否则历史条目会被静默误命中）。
    #[test]
    fn shape_key_distinguishes_new_shaping_inputs() {
        use crate::text::{TextFontVariation, TextLineBreak};

        let base = TextContent::new("1,234.56", TextStyle::new(13.0));
        let locale = TextContent::new("1,234.56", TextStyle::new(13.0).with_locale("zh-Hans"));
        let stretched = TextContent::new("1,234.56", TextStyle::new(13.0).with_stretch(80));
        let spaced = TextContent::new("1,234.56", TextStyle::new(13.0).with_word_spacing(2.0));
        let line_break = TextContent::new(
            "1,234.56",
            TextStyle::new(13.0).with_line_break(TextLineBreak::Loose),
        );
        let variations = TextContent::new(
            "1,234.56",
            TextStyle::new(13.0).with_font_variations([TextFontVariation::new(*b"wght", 700.0)]),
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
    fn shape_key_distinguishes_text_and_style() {
        let style = TextStyle::new(16.0);
        let a = ShapeKey::new(&TextContent::new("ab", style.clone()), 1);
        let b = ShapeKey::new(&TextContent::new("ba", style.clone()), 1);
        let c = ShapeKey::new(
            &TextContent::new("ab", style.with_wrap(TextWrap::NoWrap)),
            1,
        );

        assert_ne!(a, b);
        assert_ne!(a, c);
    }

    /// P1-2.3：OpenType 特性必须参与缓存键。
    ///
    /// `tnum` 会改变每个数字字形的 advance，从而改变整段文本的宽度与断行。若它不在键里，
    /// 「先 shape 了比例数字、再请求等宽数字」会命中旧条目，价格列依旧抖动 —— 这是静默错误。
    #[test]
    fn shape_key_distinguishes_font_features() {
        use crate::text::TextFeature;

        let plain = TextContent::new("12345", TextStyle::new(16.0));
        let tabular = TextContent::new(
            "12345",
            TextStyle::new(16.0).with_font_features([TextFeature::TABULAR_NUMBERS]),
        );

        assert_ne!(
            ShapeKey::new(&plain, 1),
            ShapeKey::new(&tabular, 1),
            "font_features 必须参与缓存键"
        );
        // 同样的特性集 ⇒ 同一个键（顺序与内容都相同）。
        let again = TextContent::new(
            "12345",
            TextStyle::new(16.0).with_font_features([TextFeature::TABULAR_NUMBERS]),
        );
        assert_eq!(ShapeKey::new(&tabular, 1), ShapeKey::new(&again, 1));
    }
}
