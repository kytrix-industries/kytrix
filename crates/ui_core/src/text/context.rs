//! `TextContext`：进程级字体库 + 布局 scratch + 字体栈 + 形状缓存 + 诊断（§6）。
//!
//! **crate 私有**（F-54）：它持有 `parley::{FontContext, LayoutContext}`，公开即把 parley 拉进公开面。
//! 应用层的入口是 `App::{set_font_stack, font_stack, font_stack_version, reload_system_fonts, refresh_text,
//! text_diagnostics}`。
//!
//! # 字体栈解析只做一次（R12-2 / R12-3）
//!
//! 字体栈的健康状态与「parley 家族列表」**只依赖字体栈 + 系统字体集合**，与文本/字号/样式无关。
//! 旧实现在**每次形态缓存 miss**（= 每次字符串变化）时重建 `Vec<&str>`、跑一次 fontique 查询
//! （`set_families` + `first_available_font`，14 次家族查表 + 字体元数据加载），而解析结果被丢弃。
//! 现在：
//!
//! - 状态**懒求值一次**并缓存（`font_stack_status`，换栈/重载系统字体时置空）；
//! - 家族列表是**真缓存**（`family_list` / `custom_family_lists`）：命中路径只做一次
//!   `FontFamily::clone`（parley 的 `Cow` 深拷贝，API 限制），不再重建 14 个 `String`。
//!
//! # 数字快速路径（R12 最重要的性能项）
//!
//! [`DigitFastPath`] 在 miss 路径上先尝试「字符 → glyph id → advance」直拼（Blink
//! `SimpleLineLayout` 模型）；不适用时原样走 parley。命中率计入 `TextPassReport`。

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

/// 诊断环形缓冲上限。
const DIAGNOSTIC_LIMIT: usize = 64;

/// 配置期诊断：由配置决定、长期有效（当前只有字体栈）。字形诊断的洪峰不得把它们挤出
/// 诊断环 —— 否则「字体栈一个候选都没命中」这类启动期问题几帧后就不可观测（D-4）。
fn is_config_diagnostic(diagnostic: &TextDiagnostic) -> bool {
    matches!(diagnostic, TextDiagnostic::FontStackMiss { .. })
}

/// 家族名覆盖的缓存条目上限（超过即整表重建：覆盖家族名是极低频配置）。
const CUSTOM_FAMILY_LIMIT: usize = 16;

/// 省略号字形的缓存上限（每 `TextStyle` 一条）。
const ELLIPSIS_LIMIT: usize = 64;

/// 文本引擎（字体库 + 形状缓存）。**进程级一份**（`App` 持有，`Window` 不持有）。
pub(crate) struct TextContext {
    font_cx: parley::FontContext,
    layout_cx: parley::LayoutContext<ColorBrush>,
    font_stack: FontStack,
    /// 字体栈的**单调**版本（替换栈 / 重载系统字体都 +1；形状缓存键的一部分）。
    ///
    /// `FontStack::version` 是**内容**版本，替换栈时会从 1 重新开始，因此不能直接当缓存键。
    font_stack_version: u32,
    /// `FontStack` 的 parley 载体（**真缓存**：R12-3；换栈/重载系统字体时重建）。
    family_list: parley::FontFamily<'static>,
    /// 家族名覆盖的 parley 载体缓存（`TextStyle::family` 非 `None` 时用）。
    custom_family_lists: ahash::HashMap<SmolStr, parley::FontFamily<'static>>,
    cache: ShapeCache,
    /// 数字快速路径（Blink `SimpleLineLayout` 模型）。
    fast: DigitFastPath,
    /// 省略号字形（R12-9；按 `TextStyle` 的 shaping 输入缓存）。
    ellipsis: ahash::HashMap<u64, Rc<EllipsisGlyphs>>,
    /// 字体栈状态缓存（`None` = 尚未求值；R12-2）。
    font_stack_status: Option<FontStackStatus>,
    /// 字体栈状态真正被求值的次数（测试断言「不是每次 miss 都查」）。
    stack_resolutions: u32,
    diagnostics: SmallVec<[TextDiagnostic; 8]>,
    /// 字体代际（`set_font_stack` / `reload_system_fonts` 时 +1；字形缓存与图集的失效依据，B2）。
    font_epoch: u32,
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

    /// 换字体栈：版本 +1、字体代际 +1、清形状缓存 / 家族载体 / 快速路径表 / 省略号缓存。
    ///
    /// 返回**新**的 `font_stack_version`（调用方用它决定是否让所有文本节点失效）。
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

    /// 重载系统字体：**重建 `FontContext`**（R12-12）、字体代际 +1、清全部派生缓存。
    ///
    /// fontique 0.12 的 `Collection::load_system_fonts()` 在系统字体已加载后是 **no-op**
    /// （`collection/mod.rs`：`if self.inner.system.is_none()`），所以「重载」必须换一个新的
    /// `FontContext` 才会重新枚举系统字体。代价是一次全量扫描（Windows 上 10–80 ms 量级），
    /// **不要在热路径/每帧调用**。
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

    /// 数字快速路径开关（回退验证用）：切换后清形状缓存，保证两种模式的快照不混用。
    pub(crate) fn set_digit_fast_path(&mut self, enabled: bool) {
        if self.fast.enabled() == enabled {
            return;
        }
        self.fast.set_enabled(enabled);
        self.cache.clear();
    }

    /// 数字快速路径是否开启。
    pub(crate) fn digit_fast_path_enabled(&self) -> bool {
        self.fast.enabled()
    }

    /// 取走数字快速路径的统计（`App::refresh_text` 汇总进 `TextPassReport`）。
    pub(crate) fn take_fast_path_stats(&mut self) -> FastPathStats {
        self.fast.take_stats()
    }

    /// 当前字体栈的 parley 家族列表（含家族名覆盖；**缓存**的 `clone`）。
    ///
    /// 命中路径的唯一分配就是这次 `FontFamily::clone`：parley 的
    /// `StyleProperty::FontFamily` 要求 `'static` 持有，`Cow<'static, [_]>` 无法借出，
    /// 彻底消掉它需要 `StyleRunBuilder` 的样式表（parley 0.12 提供，属后续项）。
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

    /// 形状（命中缓存则复用 `Rc`）。第二个返回值 = 是否命中。
    pub(crate) fn shape_cached(&mut self, content: &TextContent) -> (Rc<ShapedText>, bool) {
        let key = ShapeKey::new(content, self.font_stack_version);
        if let Some(hit) = self.cache.get(&key)
            && hit.text() == content.text
            && hit.same_shaping_inputs(&content.style)
            && hit.inline_boxes() == content.inline_boxes.as_ref()
        {
            return (hit, true);
        }
        self.ensure_stack_status();
        let family = self.family_for(content.style.family());
        let font_identity = self.font_identity(content.style.family());
        // 数字快速路径：先试「字符 → glyph id → advance」直拼（R12 最重要的性能项）。
        if content.inline_boxes.is_empty()
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
        let shaped = Rc::new(ShapedText::shape(
            &mut self.font_cx,
            &mut self.layout_cx,
            &family,
            content,
            self.font_stack_version,
        ));
        self.cache.insert(key, Rc::clone(&shaped));
        (shaped, false)
    }

    /// 该样式的字体栈身份（快速路径验证缓存的键的一部分）。
    fn font_identity(&self, custom: Option<&SmolStr>) -> u64 {
        use core::hash::{Hash, Hasher};

        let mut hasher = ahash::AHasher::default();
        self.font_stack_version.hash(&mut hasher);
        custom.hash(&mut hasher);
        hasher.finish()
    }

    /// 省略号字形（R12-9）：按样式的 shaping 输入缓存。
    ///
    /// 省略号必须**参与 shaping**（advance 计入可用宽度）；这里 shape 一次 `…`（字体缺该字形时
    /// 退成 `...`），把 `(glyph id, advance)` 交给 `ShapedText::rasterize`。
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

    /// 字体栈的健康状态（N4）：懒求值一次并缓存（R12-2）。
    pub(crate) fn font_stack_status(&mut self) -> FontStackStatus {
        self.ensure_stack_status();
        self.font_stack_status
            .expect("ensure_stack_status always fills the cache")
    }

    /// 求值一次字体栈状态（每栈版本一次），并（若不健康）**每次使用都重压**那条配置诊断。
    ///
    /// 配置期诊断是「一次配置、长期有效」的：重压 + 去重（移到最新）保证它不会因为后续字形
    /// 诊断的洪峰而在诊断环里丢掉「最近诊断」的位置（D-4）。状态求值本身仍然每版本只跑一次
    /// （`stack_resolutions` 的断言不变）。
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

    /// 真正跑一次 fontique 查询（**只在**字体栈版本变化后的首次使用时调用）。
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

    /// 追加诊断：同一诊断在窗口内**只保留一条**（重复时移到最新），环仍有界（`DIAGNOSTIC_LIMIT`）。
    ///
    /// 「同一诊断」= [`TextDiagnostic`] 逐字段相等（kind + font/glyph/text_range/宽度…）。没有
    /// 去重时，每轮重排都会把同样的 `MissingGlyph`/`ColorGlyphMonochrome` 再压一遍，64 条环
    /// 几帧就被同类条目占满、启动期的 `FontStackMiss` 被挤出去（D-4）。
    ///
    /// 配置期诊断（[`TextDiagnostic::FontStackMiss`]）**常驻**：环满时优先淘汰最旧的非配置
    /// 诊断，字体栈问题不会因字形诊断洪峰而消失。
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
                // 去重 + 移到最新：同一诊断只出现一次，语义上随最近一次使用刷新。
                self.diagnostics.remove(index);
            } else if self.diagnostics.len() == DIAGNOSTIC_LIMIT {
                self.evict_oldest_diagnostic();
            }
            self.diagnostics.push(diagnostic);
        }
    }

    /// 淘汰最旧的**非配置期**诊断；全是配置期诊断时（理论上不存在）退化为丢最旧。
    ///
    /// `n <= DIAGNOSTIC_LIMIT` 的冷路径：线性扫描比维护 `HashSet` 索引更简单，而
    /// `TextDiagnostic` 含 `f32` 字段，做 `Hash/Eq` 反而要手写位级实现。
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

    /// 仅测试：字体栈状态被真正求值的次数（R12-2 的「不是每次 miss 都查」断言）。
    #[cfg(test)]
    pub(crate) fn stack_resolutions(&self) -> u32 {
        self.stack_resolutions
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

    /// R12-4：`scale` 不再是形状缓存键的一部分 —— DPI 变化不清形状缓存。
    ///
    /// 旧实现把 `scale_bits` 写进 `ShapeKey`，跨显示器拖动等于清空整个形状缓存 + 图集 +
    /// 整页重传。现在布局是逻辑坐标，scale 只在光栅期乘进去。
    #[test]
    fn scale_is_not_part_of_the_shape_key() {
        use crate::text::cache::ShapeKey;

        let content = content("scale");
        assert_eq!(
            ShapeKey::new(&content, 1),
            ShapeKey::new(&content, 1),
            "same content + stack version ⇒ same key"
        );
        // 形状缓存命中与设备 scale 无关：同一个上下文连续取两次必须命中（不存在 scale 入参）。
        let mut context = TextContext::new();
        if context.family_count() == 0 {
            eprintln!("skip: no system fonts");
            return;
        }
        let _ = context.shape_cached(&content);
        let (_, hit) = context.shape_cached(&content);
        assert!(hit);
    }

    /// R12-11：`color` / `alignment` 不进形状缓存键 —— 红/绿两态共享同一份快照。
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
            TextStyle::new(13.0).with_color(Color::Srgb {
                r: 1.0,
                g: 0.0,
                b: 0.0,
                alpha: 1.0,
            }),
        );
        let green = TextContent::new(
            "1,234.56",
            TextStyle::new(13.0).with_color(Color::Srgb {
                r: 0.0,
                g: 1.0,
                b: 0.0,
                alpha: 1.0,
            }),
        );
        let centered = TextContent::new(
            "1,234.56",
            TextStyle::new(13.0).with_alignment(TextAlign::Center),
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

    /// R12-2：字体栈解析不是「每次 miss 都查一次」—— 只在首次 / 换栈 / 重载后求值。
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

    /// R12-12：`reload_system_fonts` 必须真的重建字体集合（不是 no-op）。
    ///
    /// fontique 的 `Collection::load_system_fonts()` 在已加载后不做任何事，因此实现换成
    /// 「新建 `FontContext`」。可观测证据：系统字体家族数在新上下文里仍然 > 0，且字体代际与
    /// 栈版本都前进（派生缓存全部失效）。
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
        // 超出上限的 10 条是最旧的：环里应剩 id 10..=73（逐条不同 ⇒ 去重不改变本测试语义）。
        assert!(matches!(
            context.diagnostics().first(),
            Some(TextDiagnostic::InlineBoxOffBoundary { id, .. }) if *id as usize == 10
        ));
        context.clear_diagnostics();
        assert!(context.diagnostics().is_empty());
    }

    /// D-4：同一诊断（kind + font + glyph + text_range）在窗口内**只出现一次**；重复压入只把
    /// 它移到最新，环不增长。
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

    /// D-4：配置期诊断（字体栈）不被字形诊断洪峰淹没，且诊断环仍然有界（<= 64）。
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

    /// D-4：字体栈不健康时，每次走 shape/refresh 路径都**重压**那条配置诊断 ⇒ 它始终位于
    /// 最新位置；去重保证不会出现第二条。
    #[test]
    fn unhealthy_font_stack_repushes_its_diagnostic() {
        let mut context = TextContext::new();
        context.set_font_stack(FontStack::new(Vec::<&str>::new()));
        let _ = context.font_stack_status(); // 首次求值 + 压一条
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

    /// 快速路径开关：关闭后清形状缓存并回到 parley；重新打开再清一次。
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
