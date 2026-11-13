//! 形状缓存：`ShapeKey` + `Rc<ShapedText>` 的 LRU（条目数 + 字节双上限）。
//!
//! 设计见 `design/phase-05-text.md` §11。键**不含** `atlas_generation`（F-53）：图集换代只影响
//! 字形的 `AtlasTile`，不影响字形 id / advance / 基线 / 行数。

use std::{
    collections::VecDeque,
    hash::{Hash, Hasher},
};

use ahash::AHasher;

use super::{TextContent, shape::ShapedText};

/// 形状缓存键：文本哈希 + 样式哈希 + scale 位模式 + 字体栈版本。
///
/// 命中后仍要比较 `text`/`style`（防哈希碰撞），比较对象是缓存条目里的 `ShapedText`。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ShapeKey {
    text_hash: u64,
    text_len: u32,
    style_hash: u64,
    scale_bits: u32,
    font_stack_version: u32,
}

impl ShapeKey {
    /// 由内容 + 设备 scale + 字体栈版本构造。
    pub(crate) fn new(content: &TextContent, scale: f32, font_stack_version: u32) -> Self {
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
            scale_bits: scale.to_bits(),
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

/// 条目上限（§9.3 的「容量上限显式」）。
const ENTRY_LIMIT: usize = 256;
/// 字节上限（8 MiB）。
const BYTE_BUDGET: usize = 8 * 1024 * 1024;

impl ShapeCache {
    pub(crate) fn new() -> Self {
        Self {
            entries: ahash::HashMap::default(),
            order: VecDeque::with_capacity(ENTRY_LIMIT / 4),
            bytes: 0,
            byte_budget: BYTE_BUDGET,
        }
    }

    /// 取条目（命中即提升顺序，并校验文本/样式）。
    pub(crate) fn get(&mut self, key: &ShapeKey) -> Option<std::rc::Rc<ShapedText>> {
        let shaped = self.entries.get(key)?.shaped.clone();
        if let Some(index) = self.order.iter().position(|candidate| candidate == key) {
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
    fn shape_key_distinguishes_scale_and_stack_version() {
        let content = TextContent::new("hello", TextStyle::new(16.0));
        let base = ShapeKey::new(&content, 1.0, 1);

        assert_eq!(base, ShapeKey::new(&content, 1.0, 1));
        assert_ne!(
            base,
            ShapeKey::new(&content, 1.25, 1),
            "scale is part of the key"
        );
        assert_ne!(
            base,
            ShapeKey::new(&content, 1.0, 2),
            "stack version is part of the key"
        );
    }

    #[test]
    fn shape_key_distinguishes_text_and_style() {
        let style = TextStyle::new(16.0);
        let a = ShapeKey::new(&TextContent::new("ab", style.clone()), 1.0, 1);
        let b = ShapeKey::new(&TextContent::new("ba", style.clone()), 1.0, 1);
        let c = ShapeKey::new(
            &TextContent::new("ab", style.with_wrap(TextWrap::NoWrap)),
            1.0,
            1,
        );

        assert_ne!(a, b);
        assert_ne!(a, c);
    }
}
