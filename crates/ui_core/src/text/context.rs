//! `TextContext`：进程级字体库 + 布局 scratch + 字体栈 + 形状缓存 + 诊断（§6）。
//!
//! **crate 私有**（F-54）：它持有 `parley::{FontContext, LayoutContext}`，公开即把 parley 拉进公开面。
//! 应用层的入口是 `App::{set_font_stack, font_stack, font_stack_version, reload_system_fonts, refresh_text,
//! text_diagnostics}`。

use std::rc::Rc;

use smallvec::SmallVec;

use super::{
    FontStack, FontStackStatus, TextContent, TextDiagnostic,
    cache::{ShapeCache, ShapeKey},
    shape::{self, ShapedText},
};
use crate::paint::color::ColorBrush;

/// 诊断环形缓冲上限。
const DIAGNOSTIC_LIMIT: usize = 64;

/// 文本引擎（字体库 + 形状缓存）。**进程级一份**（`App` 持有，`Window` 不持有）。
pub(crate) struct TextContext {
    font_cx: parley::FontContext,
    layout_cx: parley::LayoutContext<ColorBrush>,
    font_stack: FontStack,
    /// 字体栈的**单调**版本（替换栈 / 重载系统字体都 +1；形状缓存键的一部分）。
    ///
    /// `FontStack::version` 是**内容**版本，替换栈时会从 1 重新开始，因此不能直接当缓存键。
    font_stack_version: u32,
    /// `FontStack` 的 parley 载体（懒构建；`set_font_stack` 时置 `None`）。
    family_list: Option<parley::FontFamily<'static>>,
    cache: ShapeCache,
    diagnostics: SmallVec<[TextDiagnostic; 8]>,
    /// 字体代际（`set_font_stack` / `reload_system_fonts` 时 +1；字形缓存与图集的失效依据，B2）。
    font_epoch: u32,
}

impl TextContext {
    pub(crate) fn new() -> Self {
        Self {
            font_cx: parley::FontContext::new(),
            layout_cx: parley::LayoutContext::new(),
            font_stack: FontStack::system_ui(),
            font_stack_version: 1,
            family_list: None,
            cache: ShapeCache::new(),
            diagnostics: SmallVec::new(),
            font_epoch: 1,
        }
    }

    pub(crate) fn font_stack(&self) -> &FontStack {
        &self.font_stack
    }

    pub(crate) fn font_stack_version(&self) -> u32 {
        self.font_stack_version
    }

    pub(crate) fn font_epoch(&self) -> u32 {
        self.font_epoch
    }

    /// 换字体栈：版本 +1（内容变时）、字体代际 +1、清形状缓存 + 清字体系列缓存。
    ///
    /// 返回**新**的 `font_stack_version`（调用方用它决定是否让所有文本节点失效）。
    pub(crate) fn set_font_stack(&mut self, stack: FontStack) -> u32 {
        self.font_stack = stack;
        self.family_list = None;
        self.cache.clear();
        self.clear_diagnostics();
        self.font_epoch = self.font_epoch.wrapping_add(1);
        self.font_stack_version = self.font_stack_version.wrapping_add(1);
        self.font_stack_version
    }

    /// 重载系统字体（幂等）：字体代际 +1、清缓存；返回当前 `font_stack_version`。
    pub(crate) fn reload_system_fonts(&mut self) -> u32 {
        self.font_cx.collection.load_system_fonts();
        self.family_list = None;
        self.cache.clear();
        self.clear_diagnostics();
        self.font_epoch = self.font_epoch.wrapping_add(1);
        self.font_stack_version = self.font_stack_version.wrapping_add(1);
        self.font_stack_version
    }

    /// 形状（命中缓存则复用 `Rc`）。第二个返回值 = 是否命中。
    pub(crate) fn shape_cached(
        &mut self,
        content: &TextContent,
        scale: f32,
    ) -> (Rc<ShapedText>, bool) {
        let key = ShapeKey::new(content, scale, self.font_stack_version);
        if let Some(hit) = self.cache.get(&key)
            && hit.text() == content.text
            && hit.style() == &content.style
            && hit.inline_boxes() == content.inline_boxes.as_ref()
        {
            return (hit, true);
        }
        self.note_font_stack_status();
        if self.family_list.is_none() {
            self.family_list = Some(shape::family_list(&self.font_stack, None));
        }
        let family = shape::family_list(&self.font_stack, content.style.family.as_deref());
        let shaped = Rc::new(ShapedText::shape(
            &mut self.font_cx,
            &mut self.layout_cx,
            &family,
            content,
            scale,
            self.font_stack_version,
        ));
        self.cache.insert(key, Rc::clone(&shaped));
        (shaped, false)
    }

    /// 字体栈的健康状态（N4）。
    pub(crate) fn font_stack_status(&mut self) -> FontStackStatus {
        if self.font_stack.families().len() == 0 {
            return FontStackStatus::EmptyStack;
        }
        let parley::FontContext {
            collection,
            source_cache,
        } = &mut self.font_cx;
        let mut query = collection.query(source_cache);
        query.set_families(self.font_stack.families());
        if query.first_available_font().is_some() {
            FontStackStatus::Ready
        } else {
            FontStackStatus::NoMatchingFamily
        }
    }

    /// 每次 miss 检查一次字体栈；不可用时记 `FontStackMiss`（不静默，也不失败：generic 兜底）。
    fn note_font_stack_status(&mut self) {
        let families: Vec<&str> = self.font_stack.families().collect();
        let resolved = {
            let parley::FontContext {
                collection,
                source_cache,
            } = &mut self.font_cx;
            let mut query = collection.query(source_cache);
            query.set_families(families.iter().copied());
            query.first_available_font().is_some()
        };
        if !resolved {
            let tried = smol_str::format_smolstr!("{}", families.join(","));
            self.push_diagnostics(vec![TextDiagnostic::FontStackMiss { tried }]);
        }
    }

    pub(crate) fn diagnostics(&self) -> &[TextDiagnostic] {
        &self.diagnostics
    }

    /// 追加诊断（环形缓冲：超过 64 条丢最旧的）。
    pub(crate) fn push_diagnostics(&mut self, mut extra: Vec<TextDiagnostic>) {
        for diagnostic in extra.drain(..) {
            if self.diagnostics.len() == DIAGNOSTIC_LIMIT {
                self.diagnostics.remove(0);
            }
            self.diagnostics.push(diagnostic);
        }
    }

    pub(crate) fn clear_diagnostics(&mut self) {
        self.diagnostics.clear();
    }

    /// 仅测试：形状缓存条目数。
    #[cfg(test)]
    pub(crate) fn cached_shapes(&self) -> usize {
        self.cache.len()
    }

    /// 仅测试：形状缓存字节估算。
    #[cfg(test)]
    pub(crate) fn cached_bytes(&self) -> usize {
        self.cache.bytes()
    }

    /// 仅测试：系统字体家族数（0 = 宿主没有可用字体，测试应 skip）。
    #[cfg(test)]
    pub(crate) fn family_count(&mut self) -> usize {
        self.font_cx.collection.family_names().count()
    }
}

impl Default for TextContext {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::TextContext;
    use crate::text::{FontStack, FontStackStatus, TextContent, TextDiagnostic, TextStyle};

    fn content(text: &str) -> TextContent {
        TextContent::new(text, TextStyle::new(16.0))
    }

    #[test]
    fn system_fonts_are_enumerated_by_default() {
        let mut context = TextContext::new();
        let families = context.family_count();

        assert!(
            families > 0,
            "a desktop platform must expose at least one family"
        );
        assert!(context.font_stack_version() >= 1);
        assert!(context.font_epoch() >= 1);
    }

    #[test]
    fn shaping_hits_the_cache_for_identical_content() {
        let mut context = TextContext::new();
        if context.family_count() == 0 {
            eprintln!("skip: no system fonts");
            return;
        }
        let content = content("cache me");
        let (first, first_hit) = context.shape_cached(&content, 1.0);
        let (second, second_hit) = context.shape_cached(&content, 1.0);

        assert!(!first_hit);
        assert!(second_hit, "second identical shape must reuse the cache");
        assert!(std::rc::Rc::ptr_eq(&first, &second));
        assert_eq!(context.cached_shapes(), 1);
        assert!(context.cached_bytes() > 0);
    }

    #[test]
    fn changing_the_text_misses_the_cache() {
        let mut context = TextContext::new();
        if context.family_count() == 0 {
            eprintln!("skip: no system fonts");
            return;
        }
        let _ = context.shape_cached(&content("one"), 1.0);
        let (_, hit) = context.shape_cached(&content("two"), 1.0);

        assert!(!hit);
        assert_eq!(context.cached_shapes(), 2);
    }

    #[test]
    fn changing_the_scale_misses_the_cache() {
        let mut context = TextContext::new();
        if context.family_count() == 0 {
            eprintln!("skip: no system fonts");
            return;
        }
        let content = content("scale");
        let _ = context.shape_cached(&content, 1.0);
        let (_, hit) = context.shape_cached(&content, 1.25);

        assert!(!hit, "scale is part of the shape key");
    }

    #[test]
    fn font_stack_change_bumps_version_and_clears_the_cache() {
        let mut context = TextContext::new();
        if context.family_count() == 0 {
            eprintln!("skip: no system fonts");
            return;
        }
        let _ = context.shape_cached(&content("x"), 1.0);
        assert_eq!(context.cached_shapes(), 1);
        let before = context.font_stack_version();
        let before_epoch = context.font_epoch();

        let version = context.set_font_stack(FontStack::new(["Segoe UI", "Noto Sans"]));

        assert_eq!(version, before + 1);
        assert_eq!(context.cached_shapes(), 0, "shape cache must be dropped");
        assert!(
            context.font_epoch() > before_epoch,
            "glyph caches must be invalidated"
        );
    }

    #[test]
    fn reload_system_fonts_bumps_the_font_epoch() {
        let mut context = TextContext::new();
        let before = context.font_epoch();

        let _ = context.reload_system_fonts();

        assert!(context.font_epoch() > before);
    }

    #[test]
    fn an_empty_font_stack_falls_back_to_generic_with_a_diagnostic() {
        let mut context = TextContext::new();
        context.set_font_stack(FontStack::new(Vec::<&str>::new()));
        assert_eq!(context.font_stack_status(), FontStackStatus::EmptyStack);

        let (_shaped, hit) = context.shape_cached(&content("x"), 1.0);

        assert!(!hit, "the fallback shape must not come from the cache");
        assert!(
            context
                .diagnostics()
                .iter()
                .any(|diagnostic| matches!(diagnostic, TextDiagnostic::FontStackMiss { .. }))
        );
    }

    #[test]
    fn diagnostics_ring_keeps_the_newest_entries() {
        let mut context = TextContext::new();
        let batch: Vec<TextDiagnostic> = (0..super::DIAGNOSTIC_LIMIT + 10)
            .map(|index| TextDiagnostic::InlineBoxOffBoundary {
                id: index as u64,
                index,
            })
            .collect();

        context.push_diagnostics(batch);

        assert_eq!(context.diagnostics().len(), super::DIAGNOSTIC_LIMIT);
        assert!(matches!(
            context.diagnostics().last(),
            Some(TextDiagnostic::InlineBoxOffBoundary { id, .. }) if *id as usize == super::DIAGNOSTIC_LIMIT + 9
        ));
        context.clear_diagnostics();
        assert!(context.diagnostics().is_empty());
    }
}
