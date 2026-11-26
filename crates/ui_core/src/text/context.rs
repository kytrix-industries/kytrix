use std::rc::Rc;

use smallvec::SmallVec;
use smol_str::{SmolStr, format_smolstr};

use super::{
    FontStack, FontStackStatus, TextContent, TextDiagnostic, TextStyle,
    cache::{ShapeCache, ShapeKey},
    fast_path::{DigitFastPath, FastPathStats},
    shape::{self, EllipsisGlyphs, ShapedText},
};
use crate::paint::color::ColorBrush;

const DIAGNOSTIC_LIMIT: usize = 64;

fn is_config_diagnostic(diagnostic: &TextDiagnostic) -> bool {
    matches!(diagnostic, TextDiagnostic::FontStackMiss { .. })
}

const CUSTOM_FAMILY_LIMIT: usize = 16;

const ELLIPSIS_LIMIT: usize = 64;

pub(crate) struct TextContext {
    font_cx: parley::FontContext,
    layout_cx: parley::LayoutContext<ColorBrush>,
    font_stack: FontStack,
    font_stack_version: u32,
    family_list: parley::FontFamily<'static>,
    custom_family_lists: ahash::HashMap<SmolStr, parley::FontFamily<'static>>,
    cache: ShapeCache,
    fast: DigitFastPath,
    ellipsis: ahash::HashMap<u64, Rc<EllipsisGlyphs>>,
    font_stack_status: Option<FontStackStatus>,
    stack_resolutions: u32,
    diagnostics: SmallVec<[TextDiagnostic; 8]>,
    font_epoch: u32,
    #[cfg(all(test, feature = "test-support"))]
    shapes: u32,
}

impl TextContext {
    pub(crate) fn new() -> Self {
        let font_stack = FontStack::system_ui();
        Self {
            font_cx: parley::FontContext::new(),
            layout_cx: parley::LayoutContext::new(),
            family_list: shape::family_list(&font_stack, None),
            font_stack,
            font_stack_version: 1,
            custom_family_lists: ahash::HashMap::default(),
            cache: ShapeCache::new(),
            fast: DigitFastPath::new(),
            ellipsis: ahash::HashMap::default(),
            font_stack_status: None,
            stack_resolutions: 0,
            diagnostics: SmallVec::new(),
            font_epoch: 1,
            #[cfg(all(test, feature = "test-support"))]
            shapes: 0,
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

    pub(crate) fn set_font_stack(&mut self, stack: FontStack) -> u32 {
        self.font_stack = stack;
        self.family_list = shape::family_list(&self.font_stack, None);
        self.custom_family_lists.clear();
        self.font_stack_status = None;
        self.cache.clear();
        self.fast.reset();
        self.ellipsis.clear();
        self.clear_diagnostics();
        self.font_epoch = self.font_epoch.wrapping_add(1);
        self.font_stack_version = self.font_stack_version.wrapping_add(1);
        self.font_stack_version
    }

    pub(crate) fn reload_system_fonts(&mut self) -> u32 {
        self.font_cx = parley::FontContext::new();
        self.family_list = shape::family_list(&self.font_stack, None);
        self.custom_family_lists.clear();
        self.font_stack_status = None;
        self.cache.clear();
        self.fast.reset();
        self.ellipsis.clear();
        self.clear_diagnostics();
        self.font_epoch = self.font_epoch.wrapping_add(1);
        self.font_stack_version = self.font_stack_version.wrapping_add(1);
        self.font_stack_version
    }

    pub(crate) fn set_digit_fast_path(&mut self, enabled: bool) {
        if self.fast.enabled() == enabled {
            return;
        }
        self.fast.set_enabled(enabled);
        self.cache.clear();
    }

    pub(crate) fn digit_fast_path_enabled(&self) -> bool {
        self.fast.enabled()
    }

    pub(crate) fn take_fast_path_stats(&mut self) -> FastPathStats {
        self.fast.take_stats()
    }

    fn family_for(&mut self, custom: Option<&SmolStr>) -> parley::FontFamily<'static> {
        let Some(custom) = custom else {
            return self.family_list.clone();
        };
        if let Some(cached) = self.custom_family_lists.get(custom) {
            return cached.clone();
        }
        let list = shape::family_list(&self.font_stack, Some(custom.as_str()));
        if self.custom_family_lists.len() >= CUSTOM_FAMILY_LIMIT {
            self.custom_family_lists.clear();
        }
        self.custom_family_lists
            .insert(custom.clone(), list.clone());
        list
    }

    pub(crate) fn shape_cached(&mut self, content: &TextContent) -> (Rc<ShapedText>, bool) {
        self.shape_cached_impl(content, true)
    }

    pub(crate) fn shape_cached_editable(
        &mut self,
        content: &TextContent,
    ) -> (Rc<ShapedText>, bool) {
        self.shape_cached_impl(content, false)
    }

    fn shape_cached_impl(
        &mut self,
        content: &TextContent,
        allow_fast_path: bool,
    ) -> (Rc<ShapedText>, bool) {
        let key = if allow_fast_path {
            ShapeKey::new(content, self.font_stack_version)
        } else {
            ShapeKey::parley_only(content, self.font_stack_version)
        };
        if let Some(hit) = self.cache.get(&key)
            && hit.text() == content.text
            && hit.same_shaping_inputs(&content.style, content.runs())
            && hit.normalized_runs() == content.normalized_runs()
            && hit.inline_boxes() == content.inline_boxes.as_ref()
        {
            return (hit, true);
        }
        self.ensure_stack_status();
        let family = self.family_for(content.style.family());
        let font_identity = self.font_identity(content.style.family());
        if allow_fast_path
            && content.inline_boxes.is_empty()
            && content.runs().is_empty()
            && content.normalized_runs() == 0
            && content.style.decorations().is_empty()
            && content.style.background().is_none()
            && let Some(simple) = self.fast.try_shape(
                &mut self.font_cx,
                &mut self.layout_cx,
                &family,
                font_identity,
                &content.text,
                &content.style,
            )
        {
            let shaped = Rc::new(ShapedText::simple(content, simple, self.font_stack_version));
            self.cache.insert(key, Rc::clone(&shaped));
            return (shaped, false);
        }
        let run_families: Vec<(core::ops::Range<usize>, parley::FontFamily<'static>)> = content
            .runs()
            .iter()
            .filter(|run| run.style.family().is_some())
            .map(|run| {
                let run_family = self.family_for(run.style.family());
                (run.range.clone(), run_family)
            })
            .collect();
        #[cfg(all(test, feature = "test-support"))]
        {
            self.shapes += 1;
        }
        let shaped = Rc::new(ShapedText::shape(
            &mut self.font_cx,
            &mut self.layout_cx,
            &family,
            content,
            self.font_stack_version,
            &run_families,
        ));
        self.cache.insert(key, Rc::clone(&shaped));
        (shaped, false)
    }

    #[cfg(all(test, feature = "test-support"))]
    pub(crate) fn shape_count(&self) -> u32 {
        self.shapes
    }

    #[cfg(all(test, feature = "test-support"))]
    pub(crate) fn clear_shape_cache(&mut self) {
        self.cache.clear();
    }

    fn font_identity(&self, custom: Option<&SmolStr>) -> u64 {
        use core::hash::{Hash, Hasher};

        let mut hasher = ahash::AHasher::default();
        self.font_stack_version.hash(&mut hasher);
        custom.hash(&mut hasher);
        hasher.finish()
    }

    pub(crate) fn ellipsis_for(&mut self, style: &TextStyle) -> Option<Rc<EllipsisGlyphs>> {
        use core::hash::{Hash, Hasher};

        if style.overflow() != super::TextOverflow::Ellipsis {
            return None;
        }
        let mut hasher = ahash::AHasher::default();
        style.hash_into(&mut hasher);
        self.font_stack_version.hash(&mut hasher);
        let key = hasher.finish();
        if let Some(cached) = self.ellipsis.get(&key) {
            return Some(Rc::clone(cached));
        }
        let family = self.family_for(style.family());
        let glyphs =
            shape::measure_ellipsis(&mut self.font_cx, &mut self.layout_cx, &family, style)?;
        let glyphs = Rc::new(glyphs);
        if self.ellipsis.len() >= ELLIPSIS_LIMIT {
            self.ellipsis.clear();
        }
        self.ellipsis.insert(key, Rc::clone(&glyphs));
        Some(glyphs)
    }

    pub(crate) fn font_stack_status(&mut self) -> FontStackStatus {
        self.ensure_stack_status();
        self.font_stack_status
            .expect("ensure_stack_status always fills the cache")
    }

    fn ensure_stack_status(&mut self) {
        let status = match self.font_stack_status {
            Some(status) => status,
            None => {
                self.stack_resolutions = self.stack_resolutions.wrapping_add(1);
                let status = self.evaluate_stack_status();
                self.font_stack_status = Some(status);
                status
            }
        };
        if status == FontStackStatus::Ready {
            return;
        }
        let mut tried = String::new();
        for (index, family) in self.font_stack.families().enumerate() {
            if index > 0 {
                tried.push(',');
            }
            tried.push_str(family);
        }
        self.push_diagnostics(vec![TextDiagnostic::FontStackMiss {
            tried: format_smolstr!("{tried}"),
        }]);
    }

    fn evaluate_stack_status(&mut self) -> FontStackStatus {
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

    pub(crate) fn diagnostics(&self) -> &[TextDiagnostic] {
        &self.diagnostics
    }

    pub(crate) fn push_diagnostics(&mut self, mut extra: Vec<TextDiagnostic>) {
        if extra.is_empty() {
            return;
        }
        for diagnostic in extra.drain(..) {
            if let Some(index) = self
                .diagnostics
                .iter()
                .position(|existing| *existing == diagnostic)
            {
                self.diagnostics.remove(index);
            } else if self.diagnostics.len() == DIAGNOSTIC_LIMIT {
                self.evict_oldest_diagnostic();
            }
            self.diagnostics.push(diagnostic);
        }
    }

    fn evict_oldest_diagnostic(&mut self) {
        let Some(index) = self
            .diagnostics
            .iter()
            .position(|diagnostic| !is_config_diagnostic(diagnostic))
        else {
            self.diagnostics.remove(0);
            return;
        };
        self.diagnostics.remove(index);
    }

    pub(crate) fn clear_diagnostics(&mut self) {
        self.diagnostics.clear();
    }

    #[cfg(test)]
    pub(crate) fn cached_shapes(&self) -> usize {
        self.cache.len()
    }

    #[cfg(test)]
    pub(crate) fn cached_bytes(&self) -> usize {
        self.cache.bytes()
    }

    #[cfg(test)]
    pub(crate) fn stack_resolutions(&self) -> u32 {
        self.stack_resolutions
    }

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
    use crate::{
        math::px,
        text::{
            FontStack, FontStackStatus, TextAlign, TextContent, TextDiagnostic, TextStyle, TextWrap,
        },
    };

    fn content(text: &str) -> TextContent {
        TextContent::new(text, TextStyle::new(px(16.0)))
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
        let (first, first_hit) = context.shape_cached(&content);
        let (second, second_hit) = context.shape_cached(&content);

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
        let _ = context.shape_cached(&content("one"));
        let (_, hit) = context.shape_cached(&content("two"));

        assert!(!hit);
        assert_eq!(context.cached_shapes(), 2);
    }

    #[test]
    fn scale_is_not_part_of_the_shape_key() {
        use crate::text::cache::ShapeKey;

        let content = content("scale");
        assert_eq!(
            ShapeKey::new(&content, 1),
            ShapeKey::new(&content, 1),
            "same content + stack version ⇒ same key"
        );
        let mut context = TextContext::new();
        if context.family_count() == 0 {
            eprintln!("skip: no system fonts");
            return;
        }
        let _ = context.shape_cached(&content);
        let (_, hit) = context.shape_cached(&content);
        assert!(hit);
    }

    #[test]
    fn color_and_alignment_share_one_shape_snapshot() {
        use crate::{paint::color::Color, text::TextAlign};

        let mut context = TextContext::new();
        if context.family_count() == 0 {
            eprintln!("skip: no system fonts");
            return;
        }
        let red = TextContent::new(
            "1,234.56",
            TextStyle::new(px(13.0)).with_color(Color::Srgb {
                r: 1.0,
                g: 0.0,
                b: 0.0,
                alpha: 1.0,
            }),
        );
        let green = TextContent::new(
            "1,234.56",
            TextStyle::new(px(13.0)).with_color(Color::Srgb {
                r: 0.0,
                g: 1.0,
                b: 0.0,
                alpha: 1.0,
            }),
        );
        let centered = TextContent::new(
            "1,234.56",
            TextStyle::new(px(13.0)).with_alignment(TextAlign::Center),
        );
        let (first, _) = context.shape_cached(&red);
        let (second, second_hit) = context.shape_cached(&green);
        let (third, third_hit) = context.shape_cached(&centered);

        assert!(second_hit, "颜色不参与形状缓存键（红/绿共享一份快照）");
        assert!(third_hit, "对齐不参与形状缓存键");
        assert!(std::rc::Rc::ptr_eq(&first, &second));
        assert!(std::rc::Rc::ptr_eq(&first, &third));
        assert_eq!(context.cached_shapes(), 1);
    }

    #[test]
    fn break_word_numbers_bypass_the_fast_path_and_wrap() {
        let mut context = TextContext::new();
        if context.family_count() == 0 {
            eprintln!("skip: no system fonts");
            return;
        }
        let content = TextContent::new(
            "1234567890",
            TextStyle::new(px(12.0)).with_wrap(TextWrap::BreakWord),
        );
        let (shaped, _) = context.shape_cached(&content);
        assert!(
            !shaped.is_simple(),
            "BreakWord 数字文本在窄宽下必须能折行：不得命中单行快速路径"
        );
        let metrics = shaped.line_metrics(px(30.0), TextAlign::Start);
        assert!(
            metrics.line_count > 1,
            "窄宽 BreakWord 数字串必须折成多行：{metrics:?}"
        );

        let no_wrap = TextContent::new(
            "1234567890",
            TextStyle::new(px(12.0)).with_wrap(TextWrap::NoWrap),
        );
        let (shaped, _) = context.shape_cached(&no_wrap);
        assert!(
            shaped.is_simple(),
            "NoWrap 数字文本仍走单行快速路径（既有行为不变）"
        );
    }

    #[test]
    fn decorated_or_highlighted_numbers_bypass_the_fast_path() {
        use crate::{paint::color::Color, text::TextBackground};

        let mut context = TextContext::new();
        if context.family_count() == 0 {
            eprintln!("skip: no system fonts");
            return;
        }
        let highlight = TextBackground {
            color: Color::WHITE,
            corner_radius: px(2.0),
            padding_x: px(2.0),
        };
        let base = TextStyle::new(px(12.0)).with_wrap(TextWrap::NoWrap);
        let plain = TextContent::new("1234567890", base.clone());
        let highlighted = TextContent::new("1234567890", base.clone().with_background(highlight));
        let underlined = TextContent::new("1234567890", base.clone().with_underline(px(1.0), None));

        let (plain_shaped, plain_hit) = context.shape_cached(&plain);
        assert!(!plain_hit);
        assert!(plain_shaped.is_simple(), "无装饰的数字文本仍走单行快速路径");

        let invisible = TextContent::new(
            "1234567890",
            base.with_background(TextBackground {
                color: Color::TRANSPARENT,
                ..highlight
            }),
        );
        let (invisible_shaped, invisible_hit) = context.shape_cached(&invisible);
        assert!(
            invisible_hit,
            "不可见背景必须归一化为 None（与无背景共享键与判据）"
        );
        assert!(
            invisible_shaped.is_simple(),
            "不可见背景不得禁用数字单行快速路径"
        );

        let (highlighted_shaped, highlighted_hit) = context.shape_cached(&highlighted);
        assert!(!highlighted_hit, "背景必须进入形状缓存键");
        assert!(
            !highlighted_shaped.is_simple(),
            "带背景的文本必须走完整 shaping（快速路径不产背景几何）"
        );
        let (highlighted_again, highlighted_again_hit) = context.shape_cached(&highlighted);
        assert!(
            highlighted_again_hit,
            "同一带背景样式第二次必须命中形状缓存（键与判据必须同构）"
        );
        assert!(std::rc::Rc::ptr_eq(&highlighted_shaped, &highlighted_again));

        let (underlined_shaped, underlined_hit) = context.shape_cached(&underlined);
        assert!(!underlined_hit, "装饰必须进入形状缓存键");
        assert!(
            !underlined_shaped.is_simple(),
            "带装饰的文本必须走完整 shaping（快速路径不产装饰几何）"
        );
        let (underlined_again, underlined_again_hit) = context.shape_cached(&underlined);
        assert!(
            underlined_again_hit,
            "同一装饰集合第二次必须命中形状缓存（键与判据必须同构）"
        );
        assert!(std::rc::Rc::ptr_eq(&underlined_shaped, &underlined_again));
        assert_eq!(context.cached_shapes(), 3);
    }

    #[test]
    fn font_stack_is_resolved_once_per_version() {
        let mut context = TextContext::new();
        if context.family_count() == 0 {
            eprintln!("skip: no system fonts");
            return;
        }
        for index in 0..32 {
            let _ = context.shape_cached(&content(&format!("text {index}")));
        }
        assert_eq!(
            context.stack_resolutions(),
            1,
            "32 次 shape miss 只允许一次 fontique 查询"
        );
        let _ = context.font_stack_status();
        let _ = context.font_stack_status();
        assert_eq!(context.stack_resolutions(), 1, "读状态命中缓存，不重复查询");

        let _ = context.set_font_stack(FontStack::new(["Segoe UI", "Noto Sans"]));
        let _ = context.shape_cached(&content("after stack change"));
        assert_eq!(context.stack_resolutions(), 2, "换栈后重新求值一次");
    }

    #[test]
    fn font_stack_change_bumps_version_and_clears_the_cache() {
        let mut context = TextContext::new();
        if context.family_count() == 0 {
            eprintln!("skip: no system fonts");
            return;
        }
        let _ = context.shape_cached(&content("x"));
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
    fn reload_system_fonts_really_reloads() {
        let mut context = TextContext::new();
        let families_before = context.family_count();
        if families_before == 0 {
            eprintln!("skip: no system fonts");
            return;
        }
        let _ = context.shape_cached(&content("kept"));
        let before_stack = context.font_stack_version();
        let before_epoch = context.font_epoch();
        let resolutions_before = context.stack_resolutions();

        let version = context.reload_system_fonts();

        assert_eq!(version, before_stack + 1);
        assert!(context.font_epoch() > before_epoch);
        assert_eq!(context.cached_shapes(), 0, "重载必须清形状缓存");
        assert_eq!(
            context.stack_resolutions(),
            resolutions_before,
            "重载后状态缓存被清空，等下次使用再求值（不得新增解析）"
        );
        assert!(
            context.family_count() > 0,
            "重新枚举后系统字体仍然可用（真重载，不是 no-op）"
        );
        assert_eq!(context.font_stack_status(), FontStackStatus::Ready);
    }

    #[test]
    fn an_empty_font_stack_falls_back_to_generic_with_a_diagnostic() {
        let mut context = TextContext::new();
        context.set_font_stack(FontStack::new(Vec::<&str>::new()));
        assert_eq!(context.font_stack_status(), FontStackStatus::EmptyStack);

        let (_shaped, hit) = context.shape_cached(&content("x"));

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
        assert!(matches!(
            context.diagnostics().first(),
            Some(TextDiagnostic::InlineBoxOffBoundary { id, .. }) if *id as usize == 10
        ));
        context.clear_diagnostics();
        assert!(context.diagnostics().is_empty());
    }

    #[test]
    fn diagnostics_are_deduped() {
        let mut context = TextContext::new();
        let repeated = TextDiagnostic::MissingGlyph {
            font: smol_str::SmolStr::new_static("1234#0"),
            glyph: 0,
            text_range: 3..9,
        };
        for _ in 0..100 {
            context.push_diagnostics(vec![repeated.clone()]);
        }
        assert_eq!(
            context.diagnostics().len(),
            1,
            "同一诊断只保留一条：{:?}",
            context.diagnostics()
        );
        assert_eq!(context.diagnostics()[0], repeated);

        let other = TextDiagnostic::InlineBoxOffBoundary { id: 1, index: 2 };
        context.push_diagnostics(vec![other.clone()]);
        context.push_diagnostics(vec![repeated.clone()]);
        assert_eq!(
            context.diagnostics().len(),
            2,
            "不同诊断各自一条：{:?}",
            context.diagnostics()
        );
        assert_eq!(context.diagnostics()[0], other);
        assert_eq!(
            context.diagnostics().last(),
            Some(&repeated),
            "重复诊断必须移到最新：{:?}",
            context.diagnostics()
        );
    }

    #[test]
    fn font_stack_miss_survives_a_flood_of_glyph_diagnostics() {
        let mut context = TextContext::new();
        let miss = TextDiagnostic::FontStackMiss {
            tried: smol_str::SmolStr::new_static("Segoe UI,Noto Sans"),
        };
        context.push_diagnostics(vec![miss.clone()]);
        let flood: Vec<TextDiagnostic> = (0..100u32)
            .map(|glyph| TextDiagnostic::MissingGlyph {
                font: smol_str::format_smolstr!("{}#0", glyph),
                glyph,
                text_range: 0..1,
            })
            .collect();
        context.push_diagnostics(flood);

        assert_eq!(
            context.diagnostics().len(),
            super::DIAGNOSTIC_LIMIT,
            "诊断环仍有界：{:?}",
            context.diagnostics().len()
        );
        assert!(
            context
                .diagnostics()
                .iter()
                .any(|diagnostic| matches!(diagnostic, TextDiagnostic::FontStackMiss { .. })),
            "配置期诊断不得被字形诊断挤出环：{:?}",
            context.diagnostics()
        );
        assert_eq!(
            context.diagnostics()[0],
            miss,
            "常驻的配置期诊断是最旧条目（淘汰只针对非配置期诊断）"
        );
    }

    #[test]
    fn unhealthy_font_stack_repushes_its_diagnostic() {
        let mut context = TextContext::new();
        context.set_font_stack(FontStack::new(Vec::<&str>::new()));
        let _ = context.font_stack_status();
        context.push_diagnostics(vec![
            TextDiagnostic::InlineBoxOffBoundary { id: 7, index: 3 },
            TextDiagnostic::InlineBoxOffBoundary { id: 8, index: 5 },
        ]);
        assert!(matches!(
            context.diagnostics().first(),
            Some(TextDiagnostic::FontStackMiss { .. })
        ));
        assert!(matches!(
            context.diagnostics().last(),
            Some(TextDiagnostic::InlineBoxOffBoundary { id, .. }) if *id == 8
        ));

        let _ = context.shape_cached(&content("refresh"));

        assert!(
            matches!(
                context.diagnostics().last(),
                Some(TextDiagnostic::FontStackMiss { .. })
            ),
            "配置期诊断必须被重压到最新：{:?}",
            context.diagnostics()
        );
        assert!(
            matches!(
                context.diagnostics().first(),
                Some(TextDiagnostic::InlineBoxOffBoundary { id, .. }) if *id == 7
            ),
            "重压只移动配置期诊断，不改变其它诊断的相对顺序：{:?}",
            context.diagnostics()
        );
        assert_eq!(
            context
                .diagnostics()
                .iter()
                .filter(|diagnostic| matches!(diagnostic, TextDiagnostic::FontStackMiss { .. }))
                .count(),
            1,
            "重压 + 去重不能产生第二条：{:?}",
            context.diagnostics()
        );
    }

    #[test]
    fn digit_fast_path_switch_clears_the_cache() {
        let mut context = TextContext::new();
        if context.family_count() == 0 {
            eprintln!("skip: no system fonts");
            return;
        }
        assert!(context.digit_fast_path_enabled());
        let _ = context.shape_cached(&content("1,234.56"));
        assert_eq!(context.cached_shapes(), 1);

        context.set_digit_fast_path(false);
        assert!(!context.digit_fast_path_enabled());
        assert_eq!(context.cached_shapes(), 0, "切换开关必须清缓存");

        context.set_digit_fast_path(true);
        assert!(context.digit_fast_path_enabled());
        let _ = context.shape_cached(&content("1,234.56"));
        assert_eq!(context.cached_shapes(), 1);
    }
}
