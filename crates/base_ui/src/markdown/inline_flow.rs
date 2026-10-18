//! 块内行内流动：把一个标题或一个段落里的文字与行内图片排进同一条行流。
//!
//! GPUI 的 `StyledText` 只能画一整段统一行高的纯文字；折行器只提供
//! [`LineFragment::element`] 这一层把固定尺寸盒子插进行流的原语，没有更高层的行内元素支持，
//! 所以折行与绘制都由这里的自绘 [`Element`] 完成：用文字系统塑形、用折行器折行、
//! 自己把文字、链接下划线与图片画到窗口上。
//!
//! 行内图片（`![alt](file://…)`，语法见 [`super::source`]）分两种画法：`img` 交给 gpui 的
//! `img()` 解码位图，`svg` 交给 gpui 的 `svg()` 按文字色染成单色矢量图。两者都**只吃尺寸**，
//! 所以尺寸在解析阶段就定下来（写没写 `size` 都会定下来），这里只按那个尺寸摆盒子：折行按宽，
//! 行高按高，行盒里垂直居中。
//!
//! 布局分六步：[`item_spans`] 建立逻辑索引空间、[`wrap_fragments`] 生成折行片段、
//! [`hard_line_ranges`] 按 `'\n'` 切硬换行、[`wrap_ranges`] 折行（含"塑形宽度复核 +
//! 收紧预算"的回退循环）、[`place_lines`] 定位、[`align_lines`] 按段落的对齐方式偏移整行。
//!
//! 段落对齐（`<p align="…">` 标签对给出的 [`ParagraphAlign`]，见 [`align_lines`]）是**整行**
//! 的水平偏移：一行内容在块自己的宽度里靠左 / 居中 / 靠右，行内一切（文字、图片、kbd 框、
//! 链接下划线、链接命中盒）的相对位置都不变。折行后的每一行各自对齐（与 CSS `text-align`
//! 一致），内容比块还宽时偏移为 0，溢出留在右侧——与本组件其它超宽内容（超宽图片、拆不开的
//! 长词）的处理一致。对齐宽度取本行**可见**内容（行尾悬挂的空白不算，见 [`place_lines`]）。
//!
//! # 已知限制
//!
//! 1. 文本不可选中、不可复制（需求 D9）。
//! 2. 没有 a11y 角色与朗读标签（不实现 [`Element::a11y_role`]）；要朗读文本的调用方自己拿
//!    `MarkdownState::plain_text()`，要原文的拿 `MarkdownState::source()`。
//! 3. hover 下划线由绘制时的指针位置现算（不再读 `MarkdownState::hovered_link`）：指针不再
//!    压在链接上时，下一次重绘就会让下划线消失，而不是"保持最后一次的值"一直画着。例外是
//!    鼠标移出窗口：平台不再送鼠标事件、指针位置仍停在窗口内最后的位置，此时要等指针再次
//!    移动才会更新。
//! 4. 图片在行盒里**垂直居中**，不参与基线对齐（不实现基线对齐那套机制）。
//! 5. 单行只有一张超宽图片（宽度大于容器）时该行溢出，与 gpui 自身对超长单词的处理一致。
//! 6. 同一份 `MarkdownState` 被多处渲染时，`MarkdownState::hovered_link` 只反映"最近一次 hover
//!    信号"（可能被另一个实例清成 `None`）；绘制本身每个实例各自正确。
//! 7. 按下记录 `MarkdownState::pressed` 是文档级单槽：只有某次手势的 `MouseUp` 没有落到本块
//!    （被更外层的监听器提前 `stop_propagation`，或指针已经在块外抬起）时才会残留。残留会被本块
//!    之后的任意一次"本块之外"的 `MouseDown` 作废，`MouseUp` 也要求按下点落在本块几何内，所以它
//!    不会被别的实例（同一份 `MarkdownState`、同名 `LinkId`）当成点击依据。
//! 8. **不折叠空白**：源码里的连续空格会被逐个绘制、照样占宽度，所以一段很长的空格仍会让文字
//!    多折一行。这不是缺陷，是忠实渲染：折行点之后的空白**悬挂**在上一行行尾，既不丢字符也不
//!    另起一行（见 [`wrap_ranges`]）。硬换行之后的行首是例外：那里若是一整段超宽空白（可达的
//!    是 `U+00A0` / `U+3000` 这类不被 CommonMark 当行尾空白剥掉、`char::is_whitespace()` 又为真
//!    的字符），悬挂会把它整段扩到该行末尾，于是它独占一个行盒。
//! 9. **kbd 内容不参与折行**：一段 `<kbd>` 内容（见 [`KbdId`]）整体是一个不可拆分的单元——
//!    当前行放不下时整段移到下一行，比整行还宽时溢出（与超宽图片一致），绝不拆到两行上。
//!    段内的**显式**换行（`SoftBreak` / `LineBreak`）仍然换行：此时整段按行拆成几个框，每行各画
//!    一个闭合的框。框是一圈 1px 圆角边框，颜色取主题的 `roles.line.divider`，上下与左右
//!    的内边距、圆角半径都按本块字号的倍率取。
//! 10. **右对齐（居中时则要行尾空白宽于右侧剩余空间）时，行尾空白会落到块右边缘之外**：对齐
//!     宽度不含行尾空白（见 [`place_lines`]），而它的位置跟着整行一起偏移。右对齐一定越出：
//!     可见内容的右端就贴在块右边缘上；居中时越出量是"行尾空白宽 − 右侧剩余空间
//!     `(available - content_width) / 2`"，所以常见的一两个空格通常仍在块内。空白本身不可见，
//!     看得见的只有链接下划线在行尾多出来的那一小段，以及命中盒不可达的那一小条（指针在块外
//!     时 `link_at` 根本不参与）。要消掉它得按片段再记一份可见宽度，而受影响的只有"行尾带空白
//!     的链接片段"这一种情况，为一段 1px 下划线不值得，所以留着它。

use std::{cell::RefCell, ops::Range, rc::Rc, sync::Arc};

use gpui_kit::{
    AnyElement, App, AvailableSpace, BorderStyle, Bounds, CursorStyle, DispatchPhase, Element,
    ElementId, Font, FontStyle, FontWeight, GlobalElementId, Hitbox, HitboxBehavior, Hsla,
    ImageSource, IndentAdjustment, InspectorElementId, IntoElement, LayoutId, LineFragment,
    LineWrapperHandle, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point,
    Resource, ShapedLine, SharedString, Size, Style, Styled, TextAlign, TextRun, WeakEntity,
    Window, fill, img, outline, point, px, size, svg,
};

use super::{
    document::{KbdId, LinkId, MarkdownPiece, ParagraphAlign},
    source::InlineImageFormat,
    view::{MarkdownState, PressedLink},
};
use crate::style::theme::ThemeExt;

/// 逻辑索引空间里一张行内图片占据的字节数。
const IMAGE_LEN: usize = 1;
/// hover 时链接下划线的粗细。
const UNDERLINE_THICKNESS: Pixels = px(1.);
/// 标题字重。
const HEADING_WEIGHT: FontWeight = FontWeight::BOLD;
/// kbd 框的内边距（上下与左右同值）：相对本块字号的倍率。
const KBD_PADDING: f32 = 0.2;
/// kbd 框的圆角半径：相对本块字号的倍率。
const KBD_RADIUS: f32 = 0.2;

/// 折行片段的一个原子。
enum FlowItem {
    Text(TextItem),
    InlineImage(InlineImageItem),
}

impl FlowItem {
    /// 这个原子属于哪一段 kbd 内容；不在 `<kbd>` 里时为 `None`。
    fn kbd(&self) -> Option<KbdId> {
        match self {
            FlowItem::Text(item) => item.kbd,
            FlowItem::InlineImage(item) => item.kbd,
        }
    }
}

struct TextItem {
    text: SharedString,
    weight: FontWeight,
    style: FontStyle,
    color: Hsla,
    link: Option<LinkId>,
    kbd: Option<KbdId>,
}

impl TextItem {
    /// 这一段的字体：本块默认字体加上它自己的字重与字形。
    fn font(&self, default: &Font) -> Font {
        Font {
            weight: self.weight,
            style: self.style,
            ..default.clone()
        }
    }
}

/// 一张行内图片。`size` 是解析阶段就定下来的绘制尺寸（见 [`MarkdownPiece::InlineImage`]），
/// 所以这个元素不再自己量尺寸：折行按 `size.width` 摆它，定位按 `size` 画它。
struct InlineImageItem {
    size: Size<Pixels>,
    link: Option<LinkId>,
    kbd: Option<KbdId>,
}

/// 一张行内图片要画的那个元素。
///
/// 两种画法各自走 gpui 自己的元素：位图交给 `img()` 解码，矢量图交给 `svg()` 按文字色染成
/// 单色。两者都只吃尺寸，所以尺寸一律显式给——`svg()` 尤其需要：没有固有尺寸的它不给宽高就
/// 什么都不画。
///
/// **位图一律按内嵌资源构造**，而不是把路径当字符串交给 `img()`：`ImageSource` 从字符串构造时
/// 会拿 `url::Url` 问一次，能被解析成 URL 的路径（`http://…` 这种形状）会变成 `Resource::Uri`，
/// 于是 `img()` 的元素去发一次网络请求。而这里按契约（见 [`MarkdownPiece::InlineImage`]）拿到的
/// 就是 `AssetSource::load` 收到的那串路径，只有资源源一种解释。这与 `svg()` 一致：它同样只查
/// 资源源。
///
/// `svg()` 只在窗口有文字色时才画得出来，所以颜色显式给：链接里用链接色，其余用本块的文字色。
fn inline_image_element(
    path: SharedString,
    format: InlineImageFormat,
    size: Size<Pixels>,
    link: Option<LinkId>,
    link_color: Hsla,
    text_color: Hsla,
) -> AnyElement {
    match format {
        InlineImageFormat::Image => img(embedded_image_source(path))
            .w(size.width)
            .h(size.height)
            .into_any_element(),
        InlineImageFormat::Svg => svg()
            .path(path)
            .text_color(if link.is_some() {
                link_color
            } else {
                text_color
            })
            .w(size.width)
            .h(size.height)
            .into_any_element(),
    }
}

/// 一个位图资源的来源：永远是**内嵌资源**，绝不是 URI。
///
/// `img()` 接受 `impl Into<ImageSource>`，而 `ImageSource` 的 `From<&str>` / `From<SharedString>`
/// 会拿 `url::Url` 问一次：能被解析成 URL 的路径（`http://…` 这种形状）会变成
/// `Resource::Uri`，它的加载器真去发一次网络请求（gpui 的 `ImageAssetLoader::load`）。这里的
/// 路径按契约（见 [`MarkdownPiece::InlineImage`]）就是 `AssetSource::load` 收到的那一串，
/// 所以只有资源源一种解释——这与 `svg()` 一致，它同样只查资源源。
pub(super) fn embedded_image_source(path: SharedString) -> ImageSource {
    ImageSource::Resource(Resource::Embedded(path))
}

/// 一块（一个标题或一个段落）的行内流动元素。
pub(super) struct TextFlow {
    id: ElementId,
    items: Rc<[FlowItem]>,
    /// 与 `items` 中的 `InlineImage` 一一对应，顺序一致。
    images: Vec<AnyElement>,
    /// 本块字号（继承字号 × 标题倍率），在 [`TextFlow::new`] 里解析成像素。
    font_size: Pixels,
    /// 本块行高（继承行高 × 标题倍率）。
    line_height: Pixels,
    /// 本块默认字体：标题是 BOLD，段落取继承的字重与字形。
    default_font: Font,
    /// 本块的水平对齐：标题恒为左对齐，段落取自 IR。
    align: ParagraphAlign,
    /// 整篇文档的链接表。
    links: Arc<[SharedString]>,
    entity: WeakEntity<MarkdownState>,
    /// 折行结果在测量阶段写入，prepaint / paint / 鼠标回调共用。
    layout: Rc<RefCell<Option<FlowLayout>>>,
}

struct FlowLayout {
    wrap_width: Option<Pixels>,
    size: Size<Pixels>,
    lines: Vec<FlowLine>,
}

struct FlowLine {
    top: Pixels,
    height: Pixels,
    /// 本行**可见**内容的宽度：行尾空白不占宽度（见 [`trim_trailing_whitespace`]），所以它是
    /// 本行画出来（含 kbd 框的边框与内边距）的右边缘；整行只有空白时为 0（对齐跳过它，见
    /// [`align_lines`]）。段落对齐拿它当行宽。
    content_width: Pixels,
    fragments: Vec<PlacedFragment>,
    /// 本行里的 kbd 框：一行可以有多段 kbd 内容（相邻的两对标签各算一段）。
    kbd_boxes: Vec<KbdBox>,
}

/// 一个 kbd 框在本行里的几何，全部相对于本行行盒的左上角。
struct KbdBox {
    left: Pixels,
    width: Pixels,
    /// 框的高度：内容高（段内文字字形的 `ascent + descent` 与图片高的较大者）加上上下内边距。
    height: Pixels,
    /// 框在行盒内的垂直偏移：与行内其它内容一样垂直居中。
    top: Pixels,
}

/// 一条硬换行内的一段 kbd 内容：它的逻辑范围与整体占用的宽度（内容宽 + 两侧内边距）。
/// 段内所有片段在折行时被换成这一个不可拆分的元素，因此它整体落在同一行上；定位阶段
/// （[`place_lines`]）也直接用这个宽度当框宽——画出来的框与折行用的是同一个数。
struct KbdRun {
    range: Range<usize>,
    width: Pixels,
}

struct PlacedFragment {
    kind: FragmentKind,
    left: Pixels,
    width: Pixels,
}

enum FragmentKind {
    /// 一段已塑形的文字。它整体落在同一个链接内，所以下划线只需要 `left .. left + width`；
    /// 它的高就是整行行盒的高（[`FlowLine::height`]），不必单独记。
    Text {
        line: ShapedLine,
        link: Option<LinkId>,
    },
    /// 行内图片整体是一个固定尺寸的盒子：宽是 [`PlacedFragment::width`]，高是这里的 `height`。
    /// 两个数都要记——图片的宽高互相独立（`size=24,48`），只记宽度画不出它。
    /// 不需要记它在 `images` 里的下标，prepaint / paint 按片段顺序依次取出即可。
    InlineImage {
        link: Option<LinkId>,
        height: Pixels,
    },
}

impl TextFlow {
    /// `scale`：标题用 `HeadingLevel::scale()`，段落用 1.0；`force_bold`：标题为 true；
    /// `align`：段落的水平对齐，标题恒为 [`ParagraphAlign::Left`]。
    ///
    /// 字号、行高、默认字体与每个 item 的字重 / 字形 / 颜色都在这里用 `window.text_style()`
    /// 与 `window.rem_size()` 解析成像素：组件放在 `.text_xs()` 之类的容器里时会整体变小，
    /// 标题按 `scale` 成比例跟随；元素内部不再读取文本样式。
    pub(super) fn new(
        id: ElementId,
        pieces: Arc<[MarkdownPiece]>,
        scale: f32,
        force_bold: bool,
        align: ParagraphAlign,
        links: Arc<[SharedString]>,
        entity: WeakEntity<MarkdownState>,
        link_color: Hsla,
        window: &Window,
    ) -> Self {
        let text_style = window.text_style();
        let rem_size = window.rem_size();
        let font_size = text_style.font_size.to_pixels(rem_size) * scale;
        let line_height = text_style.line_height_in_pixels(rem_size) * scale;
        let default_weight = if force_bold {
            HEADING_WEIGHT
        } else {
            text_style.font_weight
        };
        let default_style = text_style.font_style;
        let default_font = Font {
            weight: default_weight,
            style: default_style,
            ..text_style.font()
        };

        let mut items = Vec::with_capacity(pieces.len());
        // 上界已知：一个 piece 最多产出一张图片。
        let mut images = Vec::with_capacity(pieces.len());
        for piece in pieces.iter() {
            match piece {
                MarkdownPiece::Text {
                    text,
                    strong,
                    emphasis,
                    link,
                    kbd,
                } => items.push(FlowItem::Text(TextItem {
                    text: text.clone(),
                    weight: if *strong {
                        FontWeight::BOLD
                    } else {
                        default_weight
                    },
                    style: if *emphasis {
                        FontStyle::Italic
                    } else {
                        default_style
                    },
                    color: if link.is_some() {
                        link_color
                    } else {
                        text_style.color
                    },
                    link: *link,
                    kbd: *kbd,
                })),
                MarkdownPiece::InlineImage {
                    path,
                    format,
                    size,
                    link,
                    kbd,
                } => {
                    // 两种画法各自走 gpui 自己的元素（见 [`inline_image_element`]）。
                    let drawn = inline_image_element(
                        path.clone(),
                        *format,
                        *size,
                        *link,
                        link_color,
                        text_style.color,
                    );
                    // `images` 与 `items` 里的 `InlineImage` 必须同步 push：两者的下标一一对应，
                    // prepaint 依赖这个不变量。
                    images.push(drawn);
                    items.push(FlowItem::InlineImage(InlineImageItem {
                        size: *size,
                        link: *link,
                        kbd: *kbd,
                    }));
                }
                MarkdownPiece::LineBreak => items.push(FlowItem::Text(TextItem {
                    // 每帧、每个换行都要构造一次，用零分配的 `new_static`（`const fn`）。
                    // 换行符本身不属于任何一行（Pass B 会把它切掉），所以不带 kbd 编号。
                    text: SharedString::new_static("\n"),
                    weight: default_weight,
                    style: default_style,
                    color: text_style.color,
                    link: None,
                    kbd: None,
                })),
            }
        }

        Self {
            id,
            items: items.into(),
            images,
            font_size,
            line_height,
            default_font,
            align,
            links,
            entity,
            layout: Rc::new(RefCell::new(None)),
        }
    }
}

impl IntoElement for TextFlow {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextFlow {
    type RequestLayoutState = ();
    type PrepaintState = (Hitbox, Vec<AnyElement>);

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        _cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let font_size = self.font_size;
        let line_height = self.line_height;
        let font = self.default_font.clone();
        let align = self.align;
        let items = self.items.clone();
        let layout = self.layout.clone();

        let layout_id = window.request_measured_layout(
            Style::default(),
            // 闭包是 `Fn`：同一帧可能被 taffy 用不同的可用宽度探测多次，结果按 `wrap_width` 缓存。
            move |known, available, window, _cx| {
                let wrap_width = known.width.or(match available.width {
                    AvailableSpace::Definite(width) => Some(width),
                    _ => None,
                });
                if let Some(cached) = layout.borrow().as_ref()
                    && cached.wrap_width == wrap_width
                {
                    return cached.size;
                }
                let measured = build_layout(
                    &items,
                    &font,
                    font_size,
                    line_height,
                    align,
                    wrap_width,
                    window,
                );
                let measured_size = measured.size;
                layout.borrow_mut().replace(measured);
                measured_size
            },
        );
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        let layout_ref = self.layout.borrow();
        // 测量闭包在某些布局路径下可能没被调用过。UI 组件不允许 panic，这里退化为"什么也不画"。
        let Some(layout) = layout_ref.as_ref() else {
            return (hitbox, Vec::new());
        };

        let images = std::mem::take(&mut self.images);
        let mut prepainted = Vec::with_capacity(images.len());
        let mut images = images.into_iter();
        // 图片必须按"行顺序 → 行内片段顺序"prepaint，`paint` 用完全相同的顺序绘制。
        for flow_line in &layout.lines {
            for fragment in &flow_line.fragments {
                if !matches!(&fragment.kind, FragmentKind::InlineImage { .. }) {
                    continue;
                }
                let Some(mut element) = images.next() else {
                    // 每个 `InlineImageItem` 恰好产生一张图片片段，走不到这里。
                    return (hitbox, prepainted);
                };
                // 片段的宽度就是图片的宽，高度是它自己的高：两者一起交给元素自己的布局，
                // 它按这个尺寸画。
                let FragmentKind::InlineImage { height, .. } = fragment.kind else {
                    continue;
                };
                let origin = bounds.origin
                    + point(
                        fragment.left,
                        flow_line.top + (flow_line.height - height) / 2.,
                    );
                let _ = element.prepaint_as_root(
                    origin,
                    size(
                        AvailableSpace::Definite(fragment.width),
                        AvailableSpace::Definite(height),
                    ),
                    window,
                    cx,
                );
                prepainted.push(element);
            }
        }

        (hitbox, prepainted)
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        state: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let layout_ref = self.layout.borrow();
        // 没测量过就没有可画的东西，也不注册鼠标回调（见 `prepaint`）。
        let Some(layout) = layout_ref.as_ref() else {
            return;
        };
        let (hitbox, images) = state;
        let link_color = cx.theme().roles.brand.base.hsla;
        // 画什么由本实例现算，不问 `MarkdownState`（它会被别的实例改写）。
        let hovered = hovered_link(hitbox, bounds, layout, window);

        // kbd 框：一圈主题边框色的 1px 圆角边框，透明底，画在文字下面。
        let kbd_color = cx.theme().roles.line.divider.hsla;
        let kbd_radius = self.font_size * KBD_RADIUS;
        for flow_line in &layout.lines {
            for kbd_box in &flow_line.kbd_boxes {
                let rect = Bounds::new(
                    bounds.origin + point(kbd_box.left, flow_line.top + kbd_box.top),
                    size(kbd_box.width, kbd_box.height),
                );
                window.paint_quad(
                    outline(rect, kbd_color, BorderStyle::default()).corner_radii(kbd_radius),
                );
            }
        }

        for flow_line in &layout.lines {
            for fragment in &flow_line.fragments {
                let FragmentKind::Text { line, .. } = &fragment.kind else {
                    continue;
                };
                let _ = line.paint(
                    bounds.origin + point(fragment.left, flow_line.top),
                    flow_line.height,
                    TextAlign::Left,
                    None,
                    window,
                    cx,
                );
            }
        }

        // hover 下划线：一个文字片段整体落在同一个链接内，所以一段片段就是一段连续下划线。
        for flow_line in &layout.lines {
            for fragment in &flow_line.fragments {
                let FragmentKind::Text { line, link } = &fragment.kind else {
                    continue;
                };
                if link.is_none() || *link != hovered {
                    continue;
                }
                let baseline = flow_line.top
                    + (flow_line.height - line.ascent - line.descent) / 2.
                    + line.ascent;
                let rect = Bounds::new(
                    bounds.origin + point(fragment.left, baseline + px(1.)),
                    size(fragment.width, UNDERLINE_THICKNESS),
                );
                window.paint_quad(fill(rect, link_color));
            }
        }

        for element in images.iter_mut() {
            element.paint(window, cx);
        }

        if hovered.is_some() {
            window.set_cursor_style(CursorStyle::PointingHand, hitbox);
        }

        let hover_links: Rc<[LinkId]> = self
            .items
            .iter()
            .filter_map(|item| match item {
                FlowItem::Text(item) => item.link,
                FlowItem::InlineImage(item) => item.link,
            })
            .collect();
        let hovered_hitbox = hitbox.clone();
        let clicked_hitbox = hitbox.clone();
        let pressed_hitbox = hitbox.clone();
        let clickable_links = self.links.clone();
        let hover_entity = self.entity.clone();
        let press_entity = self.entity.clone();
        let click_entity = self.entity.clone();
        let hover_layout = self.layout.clone();
        let click_layout = self.layout.clone();
        let press_layout = self.layout.clone();

        window.on_mouse_event(
            move |event: &MouseMoveEvent, phase: DispatchPhase, window, cx| {
                if !phase.bubble() {
                    return;
                }
                let hovered = if hovered_hitbox.is_hovered(window) {
                    let layout = hover_layout.borrow();
                    layout
                        .as_ref()
                        .and_then(|layout| link_at(bounds, layout, event.position))
                } else {
                    None
                };
                let Ok(current) = hover_entity.read_with(cx, |state, _| state.hovered_link())
                else {
                    // 状态实体已释放，不需要再维护 hover。
                    return;
                };
                if hovered == current {
                    return;
                }
                // 指针不在本块上时，只有"当前 hover 的链接属于本块"才允许清空，
                // 否则会把别的块设下的 hover 误清掉。
                if hovered.is_none()
                    && !hovered_hitbox.is_hovered(window)
                    && !current.is_some_and(|id| hover_links.contains(&id))
                {
                    return;
                }
                let _ = hover_entity.update(cx, |state, cx| state.set_hovered_link(hovered, cx));
            },
        );

        window.on_mouse_event(
            move |event: &MouseDownEvent, phase: DispatchPhase, window, cx| {
                // 按下点只由左键记录：右键 / 中键不该给一次左键点击留下可用的按下点。
                if event.button != MouseButton::Left {
                    return;
                }
                if !phase.bubble() {
                    return;
                }
                // 指针是否压在本块上，一次算好，两个判定共用。
                let hit = pressed_hitbox.is_hovered(window);

                // 第一步：作废本块的残留按下点。本块记下的那次按下若真的在本块里开始，
                // 它的位置一定落在本块的 `bounds` 内；现在指针压在本块之外按下，说明
                // 那一次手势已经结束（`MouseUp` 没落到本块），
                // 这次按下与它无关。不作废的话，"按在链接上 → 在块外松手 → 在别处按下 →
                // 拖回本链接松手"会把残留的按下点当成同一条链接上的按下而误开链接。
                //
                // 这里不能无条件清零，也不能 `cx.stop_propagation()`：
                // 冒泡阶段按注册顺序**逆序**派发（`gpui::Window::dispatch_mouse_event`），
                // 而同一份 `MarkdownState` 可以被多个 `Markdown` 实例渲染、共享同一批文档级
                // `LinkId`，于是别的实例会把刚写好的按下点当成"自己的残留"清掉；用
                // `stop_propagation` 挡住它则会连带吞掉祖先元素的 `MouseDown`，而 gpui 的
                // `on_click` 正是在 `MouseDown` 上记录待触发点击的（`div.rs` 的
                // `pending_mouse_down`），祖先的点击会被一起吞掉。
                if let Ok(pressed) = press_entity.read_with(cx, |state, _| state.pressed())
                    && pressed.is_some_and(|pressed| bounds.contains(&pressed.at))
                    && !hit
                {
                    let _ = press_entity.update(cx, |state, _| state.set_pressed(None));
                }

                // 第二步：指针在本块内才写入新的按下点；不在本块内就保持上一步的结果
                // （本块没有发生这次按下）。
                if !hit {
                    return;
                }
                let layout = press_layout.borrow();
                let link = layout
                    .as_ref()
                    .and_then(|layout| link_at(bounds, layout, event.position));
                let _ = press_entity.update(cx, |state, _| {
                    state.set_pressed(Some(PressedLink {
                        link,
                        at: event.position,
                    }))
                });
            },
        );

        window.on_mouse_event(
            move |event: &MouseUpEvent, phase: DispatchPhase, window, cx| {
                // 只有左键才是"点击"：右键 / 中键抬起既不打开链接，也不吞掉祖先的上下文菜单处理。
                if event.button != MouseButton::Left {
                    return;
                }
                if !phase.bubble() {
                    return;
                }
                // 指针不在本块上就不是本块的点击，也不去动别的块的按下状态。
                if !clicked_hitbox.is_hovered(window) {
                    return;
                }
                let Ok(pressed) = click_entity.read_with(cx, |state, _| state.pressed()) else {
                    return;
                };
                // 抬手即"这次手势结束"：无论接着是否打开链接，都先把按下状态清掉。
                let _ = click_entity.update(cx, |state, _| state.set_pressed(None));
                let Some(pressed) = pressed else {
                    return;
                };
                // 这次按下必须是在**本块**里开始的：`LinkId` 是文档级的，同一份
                // `MarkdownState` 被两处渲染时两个实例画同一条链接、取到同一个 id，
                // 只比较 id 会把"在副本 1 上按下、在副本 2 上抬起"当成一次点击。
                // 按下点落在本块几何内才是本块的按下（跨实例误开的直接防线）。
                if !bounds.contains(&pressed.at) {
                    return;
                }
                let layout = click_layout.borrow();
                let Some(id) = layout
                    .as_ref()
                    .and_then(|layout| link_at(bounds, layout, event.position))
                else {
                    return;
                };
                let Some(url) = clickable_links.get(id.0) else {
                    return;
                };
                // 按下点与松开点必须在同一条链接上：在别处按下、拖到链接上松手不算点击。
                if pressed.link != Some(id) {
                    return;
                }
                cx.stop_propagation();
                cx.open_url(url.as_ref());
            },
        );
    }
}

/// 指针当前压在本块上的哪条链接。hover 是**实例级**的：同一份 `MarkdownState` 可以被
/// 多个 `Markdown` 渲染，所以绘制必须用本实例自己的命中盒与当前指针位置现算，
/// 不能依赖 `MarkdownState` 里那个会被别的实例改写的值。
fn hovered_link(
    hitbox: &Hitbox,
    bounds: Bounds<Pixels>,
    layout: &FlowLayout,
    window: &Window,
) -> Option<LinkId> {
    if !hitbox.is_hovered(window) {
        return None;
    }
    link_at(bounds, layout, window.mouse_position())
}

/// Pass A：逻辑索引空间。一个 `TextItem` 占 `text.len()` 个字节，一张行内图片占
/// [`IMAGE_LEN`] 个。
///
/// 这个索引空间同时是 [`LineFragment`] 的索引空间（文字片段贡献 `len`、图片片段贡献 1），
/// 所以折行器返回的 `Boundary.ix` 可以直接当逻辑索引用；图片字节不会混进任何真实字符串。
fn item_spans(items: &[FlowItem]) -> Vec<Range<usize>> {
    let mut spans = Vec::with_capacity(items.len());
    let mut cursor = 0;
    for item in items {
        let len = match item {
            FlowItem::Text(item) => item.text.len(),
            FlowItem::InlineImage(_) => IMAGE_LEN,
        };
        spans.push(cursor..cursor + len);
        cursor += len;
    }
    spans
}

/// Pass B：硬换行。换行符本身不属于任何一行；尾部的空行保留。
fn hard_line_ranges(
    items: &[FlowItem],
    spans: &[Range<usize>],
    total_len: usize,
) -> Vec<Range<usize>> {
    let mut hard_lines = Vec::new();
    let mut line_start = 0;
    for (item, span) in items.iter().zip(spans) {
        let FlowItem::Text(item) = item else {
            continue;
        };
        for (offset, _) in item.text.match_indices('\n') {
            let newline = span.start + offset;
            hard_lines.push(line_start..newline);
            line_start = newline + 1;
        }
    }
    hard_lines.push(line_start..total_len);
    hard_lines
}

/// 正在累积的 kbd 段：编号、逻辑范围与内容宽度（还没加两侧内边距）。
struct OpenKbdRun {
    id: KbdId,
    range: Range<usize>,
    content: Pixels,
}

/// 收集一条硬换行内的 kbd 段：编号相同的相邻片段属于同一段，编号变化（含离开 kbd）即换段。
///
/// 尺寸在这里一次算清：每段文字按它自己的字体塑形量宽，图片取宽度，再加两侧水平内边距。
/// 这一份宽度是折行（Pass C）与定位（Pass D）共用的唯一一份：折行器拿它摆这一段，[`place_lines`]
/// 拿它当框宽，所以画出来的框永远等于折行用的宽度，两次塑形之间差不出零头。
fn kbd_runs(
    items: &[FlowItem],
    spans: &[Range<usize>],
    hard_line: &Range<usize>,
    default_font: &Font,
    font_size: Pixels,
    window: &Window,
) -> Vec<KbdRun> {
    let padding = font_size * KBD_PADDING;
    let mut runs = Vec::new();
    let mut open: Option<OpenKbdRun> = None;

    for (item, span) in items.iter().zip(spans) {
        if span.end <= hard_line.start || span.start >= hard_line.end {
            continue;
        }
        let kbd = item.kbd();
        if open.as_ref().map(|open| open.id) != kbd {
            if let Some(open) = open.take() {
                runs.push(KbdRun {
                    range: open.range,
                    width: open.content + padding + padding,
                });
            }
            if let Some(id) = kbd {
                open = Some(OpenKbdRun {
                    id,
                    range: span.start.max(hard_line.start)..span.end.min(hard_line.end),
                    content: Pixels::ZERO,
                });
            }
        }
        let Some(open) = open.as_mut() else {
            continue;
        };
        open.range.end = span.end.min(hard_line.end);
        open.content += match item {
            FlowItem::Text(item) => {
                let start = hard_line.start.max(span.start) - span.start;
                let end = hard_line.end.min(span.end) - span.start;
                measure_segment(
                    &item.text[start..end],
                    &item.font(default_font),
                    font_size,
                    item.color,
                    window,
                )
            }
            FlowItem::InlineImage(item) => item.size.width,
        };
    }
    if let Some(open) = open.take() {
        runs.push(KbdRun {
            range: open.range,
            width: open.content + padding + padding,
        });
    }

    runs
}

/// Pass A 的片段规则：把 `items` 落在 `hard_line` 内的部分变成折行片段。
///
/// 一段 kbd 内容（[`KbdRun`]）整体变成一个不可拆分的元素片段：折行器只会在它之前换行
/// （`LineFragment::element` 只产出一个折行候选），整段因此永远落在同一行上。
fn wrap_fragments<'a>(
    items: &'a [FlowItem],
    spans: &[Range<usize>],
    hard_line: &Range<usize>,
    runs: &[KbdRun],
    default_font: &Font,
    font_size: Pixels,
    wrap_width: Pixels,
    window: &mut Window,
) -> Vec<LineFragment<'a>> {
    let mut fragments = Vec::with_capacity(items.len());
    let mut next_run = 0;
    // 已经整体变成元素的 kbd 段末尾：段内剩下的片段不再单独产出。
    let mut run_end = 0;
    for (item, span) in items.iter().zip(spans) {
        if span.end <= hard_line.start || span.start >= hard_line.end {
            continue;
        }
        if span.start < run_end {
            continue;
        }
        if let Some(run) = runs.get(next_run)
            && span.start == run.range.start
        {
            fragments.push(LineFragment::element(run.width, run.range.len()));
            run_end = run.range.end;
            next_run += 1;
            continue;
        }
        let start = hard_line.start.max(span.start) - span.start;
        let end = hard_line.end.min(span.end) - span.start;
        match item {
            FlowItem::InlineImage(item) => {
                // 图片不能像文字那样被切开：只有整体落在这一行里才计入。
                if hard_line.start <= span.start && span.end <= hard_line.end {
                    fragments.push(LineFragment::element(item.size.width, IMAGE_LEN));
                }
            }
            FlowItem::Text(item) => {
                let text = &item.text[start..end];
                if (item.weight, item.style) == (default_font.weight, default_font.style) {
                    fragments.push(LineFragment::text(text));
                } else {
                    // 加粗 / 斜体段与默认字体不同，而折行器只会用默认字体量宽：自己塑形量宽。
                    push_measured_fragments(
                        &mut fragments,
                        text,
                        item,
                        default_font,
                        font_size,
                        wrap_width,
                        window,
                    );
                }
            }
        }
    }
    fragments
}

/// 按空白把 `text` 切成"空白段 / 非空白段"：空白段的宽度与默认字体一致，交给折行器自己量；
/// 非空白段用这一段的真实字体塑形量宽。比一行还宽的段再按字符兜底切开。
fn push_measured_fragments<'a>(
    fragments: &mut Vec<LineFragment<'a>>,
    text: &'a str,
    item: &'a TextItem,
    default_font: &Font,
    font_size: Pixels,
    wrap_width: Pixels,
    window: &mut Window,
) {
    let font = item.font(default_font);
    let mut cursor = 0;
    while cursor < text.len() {
        let whitespace = text[cursor..]
            .chars()
            .next()
            .is_some_and(char::is_whitespace);
        let end = text[cursor..]
            .find(|ch: char| ch.is_whitespace() != whitespace)
            .map_or(text.len(), |offset| cursor + offset);
        let segment = &text[cursor..end];
        cursor = end;

        if whitespace {
            fragments.push(LineFragment::text(segment));
            continue;
        }
        let width = measure_segment(segment, &font, font_size, item.color, window);
        if width <= wrap_width {
            fragments.push(LineFragment::element(width, segment.len()));
            continue;
        }
        // 一个词比一行还宽：按字符切（CJK 本就逐字断行，Latin 属于极端情况）。
        for (offset, ch) in segment.char_indices() {
            let character = &segment[offset..offset + ch.len_utf8()];
            let width = measure_segment(character, &font, font_size, item.color, window);
            fragments.push(LineFragment::element(width, ch.len_utf8()));
        }
    }
}

/// 塑形一段文字并给出它的宽度。
fn measure_segment(
    text: &str,
    font: &Font,
    font_size: Pixels,
    color: Hsla,
    window: &Window,
) -> Pixels {
    let run = text_run(font, color, text.len());
    window
        .text_system()
        .shape_line(SharedString::from(text), font_size, &[run], None)
        .width()
}

/// 一个没有装饰的 [`TextRun`]。
fn text_run(font: &Font, color: Hsla, len: usize) -> TextRun {
    TextRun {
        len,
        font: font.clone(),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    }
}

/// 一条硬换行内已塑形的片段：文本按它的真实字体塑形（这是复核折行宽度的依据），图片与 kbd 段是
/// 固定宽度（kbd 段的宽度与 [`kbd_runs`] 算出的折行宽度一致）。
enum ShapedSpan {
    Text {
        range: Range<usize>,
        line: Box<ShapedLine>,
    },
    Fixed {
        range: Range<usize>,
        width: Pixels,
    },
}

fn shaped_spans(
    items: &[FlowItem],
    spans: &[Range<usize>],
    hard_line: &Range<usize>,
    runs: &[KbdRun],
    default_font: &Font,
    font_size: Pixels,
    window: &Window,
) -> Vec<ShapedSpan> {
    let mut shaped = Vec::with_capacity(items.len());
    let mut next_run = 0;
    // 已经整体量过宽的 kbd 段末尾：段内剩下的片段不再单独塑形。
    let mut run_end = 0;
    for (item, span) in items.iter().zip(spans) {
        if span.end <= hard_line.start || span.start >= hard_line.end {
            continue;
        }
        if span.start < run_end {
            continue;
        }
        if let Some(run) = runs.get(next_run)
            && span.start == run.range.start
        {
            shaped.push(ShapedSpan::Fixed {
                range: run.range.clone(),
                width: run.width,
            });
            run_end = run.range.end;
            next_run += 1;
            continue;
        }
        match item {
            FlowItem::InlineImage(item) => {
                if hard_line.start <= span.start && span.end <= hard_line.end {
                    shaped.push(ShapedSpan::Fixed {
                        range: span.clone(),
                        width: item.size.width,
                    });
                }
            }
            FlowItem::Text(item) => {
                let start = hard_line.start.max(span.start) - span.start;
                let end = hard_line.end.min(span.end) - span.start;
                let font = item.font(default_font);
                let run = text_run(&font, item.color, end - start);
                let line = Box::new(window.text_system().shape_line(
                    SharedString::from(&item.text[start..end]),
                    font_size,
                    &[run],
                    None,
                ));
                shaped.push(ShapedSpan::Text {
                    range: span.start + start..span.start + end,
                    line,
                });
            }
        }
    }
    shaped
}

/// `range` 落在已塑形片段上的真实宽度之和：文本取字形推进宽度，固定片段只有整体落入才计入。
fn spans_width(spans: &[ShapedSpan], range: Range<usize>) -> Pixels {
    spans
        .iter()
        .map(|span| match span {
            ShapedSpan::Text {
                range: span_range,
                line,
            } => {
                let start = range.start.max(span_range.start);
                let end = range.end.min(span_range.end);
                if start >= end {
                    return Pixels::ZERO;
                }
                line.x_for_index(end - span_range.start)
                    - line.x_for_index(start - span_range.start)
            }
            ShapedSpan::Fixed {
                range: span_range,
                width,
            } => {
                if range.start <= span_range.start && span_range.end <= range.end {
                    *width
                } else {
                    Pixels::ZERO
                }
            }
        })
        .sum()
}

/// `fragments` 的第一行在 `wrap_width` 处折在哪里；整段放得下时就是 `end`。
fn next_wrap(
    wrapper: &mut LineWrapperHandle,
    fragments: &[LineFragment<'_>],
    wrap_width: Pixels,
    start: usize,
    end: usize,
) -> usize {
    wrapper
        .wrap_line(fragments, wrap_width, IndentAdjustment::SameIndent)
        .next()
        .map_or(end, |boundary| (start + boundary.ix).min(end))
}

/// `fragments` 去掉前 `skip` 个字节。折行点落在字符或元素的开头，所以文字片段能按字符边界切开。
fn fragments_from<'a>(fragments: &[LineFragment<'a>], skip: usize) -> Vec<LineFragment<'a>> {
    let mut offset = 0;
    let mut out = Vec::with_capacity(fragments.len());
    for fragment in fragments {
        let len = match fragment {
            LineFragment::Text { text } => text.len(),
            LineFragment::Element { len_utf8, .. } => *len_utf8,
        };
        if offset + len <= skip {
            offset += len;
            continue;
        }
        out.push(match fragment {
            LineFragment::Text { text } => LineFragment::text(&text[skip.saturating_sub(offset)..]),
            LineFragment::Element { width, len_utf8 } => LineFragment::element(*width, *len_utf8),
        });
        offset += len;
    }
    out
}

/// Pass C：折行。对每条硬换行逐行折，并用塑形宽度复核后收紧预算重折。
fn wrap_ranges(
    items: &[FlowItem],
    spans: &[Range<usize>],
    runs: &[KbdRun],
    default_font: &Font,
    font_size: Pixels,
    wrap_width: Pixels,
    hard_lines: Vec<Range<usize>>,
    window: &mut Window,
) -> Vec<Range<usize>> {
    let mut wrapper = window
        .text_system()
        .line_wrapper(default_font.clone(), font_size);
    let mut ranges = Vec::with_capacity(hard_lines.len());
    // `runs` 是按硬换行顺序铺开的一整份 kbd 段账，游标切出当前硬换行的那一段。
    let mut next_run = 0;

    for hard_line in hard_lines {
        let line_run_start = next_run;
        while next_run < runs.len() && runs[next_run].range.start < hard_line.end {
            next_run += 1;
        }
        let line_runs = &runs[line_run_start..next_run];
        // kbd 段在折行与宽度复核两条路径上都必须被当成一个整体：这同一份账两条路径共用。
        let fragments = wrap_fragments(
            items,
            spans,
            &hard_line,
            line_runs,
            default_font,
            font_size,
            wrap_width,
            window,
        );
        let shaped = shaped_spans(
            items,
            spans,
            &hard_line,
            line_runs,
            default_font,
            font_size,
            window,
        );
        let mut start = hard_line.start;

        while start < hard_line.end {
            let rest = fragments_from(&fragments, start - hard_line.start);
            let mut budget = wrap_width;
            let mut end = next_wrap(&mut wrapper, &rest, budget, start, hard_line.end);
            // 行尾空白悬挂：折行点之后紧邻的空白整段留在**上一行**行尾，于是**折行续行**一定以
            // 非空白字符开头、不会只剩空白（续行的起点不再是空白，也就不需要旧实现里那个
            // "跳过行首空白"的补救步骤）。悬挂管不到**硬换行之后的行首**：那里若是一整段宽于
            // 容器的空白（可达的是 `U+00A0` / `U+3000` 这类 `char::is_whitespace()` 为真、又不被
            // CommonMark 当行尾空白剥掉的字符；ASCII 空格会被 comrak 从续行行首剥掉），悬挂会把
            // 整段扩到该行末尾，它仍然独占一个行盒。本组件"不折叠、不丢弃空白"，所以这是有意
            // 取舍，不是缺陷——旧实现把这类空白直接丢掉，那才是缺陷。
            end = hang_trailing_whitespace(items, spans, end, hard_line.end);
            let mut trimmed_end = trim_trailing_whitespace(items, spans, start, end);
            // 复核宽度**不含**行尾悬挂的空白：这些空白不占这一行的宽度，也不该让这一行被判定为溢出。
            let mut overflow = spans_width(&shaped, start..trimmed_end) - wrap_width;
            // 折行器按单字体逐字符宽度求和，可能比真实塑形宽度窄（例如 CJK 标点），
            // 于是这一行会溢出容器。按溢出量收紧预算重折直到折行点前移；折行点不动时把步长翻倍。
            // 单个字形就宽于一行时预算会降到 0，此时保留这一行让它溢出（与超长单词一致）。
            let mut step = overflow;
            while overflow > Pixels::ZERO {
                budget -= step.max(px(0.5));
                if budget <= Pixels::ZERO {
                    break;
                }
                let tighter = next_wrap(&mut wrapper, &rest, budget, start, hard_line.end);
                if tighter < end {
                    end = hang_trailing_whitespace(items, spans, tighter, hard_line.end);
                    trimmed_end = trim_trailing_whitespace(items, spans, start, end);
                    overflow = spans_width(&shaped, start..trimmed_end) - wrap_width;
                    step = overflow;
                } else {
                    step *= 2.;
                }
            }
            ranges.push(start..end);
            // 抬起点的位置就是下一行的起点：上面的悬挂保证它不是空白，不必再跳过什么。
            start = end;
        }

        // 空行不产生片段，上面的循环不会执行，但它在布局里仍占一行的高度。
        if hard_line.is_empty() {
            ranges.push(hard_line);
        }
    }

    ranges
}

/// 把 `end` 向后扩到其后紧邻的空白串末尾（**行尾空白悬挂**，CSS 与平台塑形的通行做法），
/// 并返回扩后的位置；`end` 处不是空白时原样返回。
///
/// 这样折行点后面的空白留在上一行行尾：**折行续行**一定以非空白字符开头、不会只剩空白；这些
/// 空白也不会被丢掉（旧实现把行首空白直接跳过，等于把源码里的空格吃掉了）。悬挂管不到
/// **硬换行之后的行首**：那里若是一整段宽于容器的空白（可达的是 `U+00A0` / `U+3000` 这类
/// `char::is_whitespace()` 为真、又不被 CommonMark 当行尾空白剥掉的字符），悬挂会把整段扩到该行
/// 末尾，它仍然独占一个行盒——这是本组件"不折叠、不丢弃空白"的有意取舍。
/// 本组件**不做** CSS 那样的空白折叠：多个空格会被逐个绘制、照样占宽度，所以一段很长的空格
/// 仍会让文字多折一行，这是忠实渲染，不是缺陷。
///
/// 只扩 `FlowItem::Text` 里 `char::is_whitespace()` 的字符，按 `char` 边界推进，并且不越过
/// `limit`。**契约**：本函数的空白判定就是 `char::is_whitespace()`，而 `'\n'.is_whitespace()`
/// **为真**，所以"不吞掉换行符"完全依赖调用方传入的 `limit`——调用方一律传当前的
/// `hard_line.end`，它恰好是终止换行符的下标（换行符本身不属于任何一行），循环于是到不了
/// `'\n'`；传一个越过换行符的 `limit` 就会把 `'\n'` 也当成空白扩进来。
fn hang_trailing_whitespace(
    items: &[FlowItem],
    spans: &[Range<usize>],
    mut end: usize,
    limit: usize,
) -> usize {
    while end < limit {
        // `spans` 连续且有序，落点所在的下标可以直接二分。
        let index = spans.partition_point(|span| span.end <= end);
        let Some((FlowItem::Text(item), span)) = items.get(index).zip(spans.get(index)) else {
            break;
        };
        // kbd 段是整体成框的单元：段内空白不属于行尾空白，悬挂不越过它的边界。
        if item.kbd.is_some() {
            break;
        }
        let Some(character) = item.text[end - span.start..].chars().next() else {
            break;
        };
        if !character.is_whitespace() {
            break;
        }
        end += character.len_utf8();
    }
    end
}

/// 把 `end` 往回退掉行尾空白，返回复核宽度时该用的位置：悬挂在行尾的空白不占这一行的宽度。
///
/// 只退 `FlowItem::Text` 里 `char::is_whitespace()` 的字符，按 `char` 边界回退，不越过 `start`。
fn trim_trailing_whitespace(
    items: &[FlowItem],
    spans: &[Range<usize>],
    start: usize,
    mut end: usize,
) -> usize {
    while end > start {
        // `end - 1` 落点所在的下标：`spans` 连续且有序，可以直接二分。
        let index = spans.partition_point(|span| span.end < end);
        let Some((FlowItem::Text(item), span)) = items.get(index).zip(spans.get(index)) else {
            break;
        };
        // kbd 段内的空白不参与行尾裁剪：框的宽度是固定的，框内尾随空白也不会被吃掉。
        if item.kbd.is_some() {
            break;
        }
        let Some(character) = item.text[..end - span.start].chars().next_back() else {
            break;
        };
        if !character.is_whitespace() {
            break;
        }
        end -= character.len_utf8();
    }
    end
}

/// 正在累积的 kbd 框：编号、左边界、框宽（折行阶段算好的那一份，含两侧内边距）与内容高
/// （纵向：段内最高的那一项，文字字形高或图片高）。
struct OpenKbdBox {
    id: KbdId,
    left: Pixels,
    width: Pixels,
    content: Pixels,
}

/// 收尾一个还在累积的 kbd 框：把框记进 `boxes`、抬高本行行高，并把 `left` 推到框的右边缘。
///
/// 框宽是折行阶段（[`kbd_runs`]）算好的那一份，右边缘由「框的左边缘 + 框宽」得出，而不是再把段内
/// 内容按绝对位置累加一遍、再相减比较——f32 的加法顺序会让两个绝对值之差漂出 1e-5 的零头，
/// 框宽于是不再等于折行用的宽度。段内片段自己仍按片段累加定位，落在框内是它们的位置契约，
/// 这里只在 debug 下复核一次它们确实没有画到框外。
fn close_kbd_box(
    open: &mut Option<OpenKbdBox>,
    left: &mut Pixels,
    height: &mut Pixels,
    boxes: &mut Vec<KbdBox>,
    padding: Pixels,
) {
    let Some(open) = open.take() else {
        return;
    };
    debug_assert!(
        *left + padding <= open.left + open.width + px(0.01),
        "the fragments placed inside a kbd box must fit the width the wrapper measured"
    );
    *left = open.left + open.width;
    let box_height = open.content + padding + padding;
    *height = (*height).max(box_height);
    boxes.push(KbdBox {
        left: open.left,
        width: open.width,
        height: box_height,
        top: Pixels::ZERO,
    });
}

/// Pass D：定位。逐行累加 x / y，并把每行塑形成可绘制的片段。
///
/// kbd 段在这里同样按段处理：段首 / 段尾各留一份水平内边距、上下各留一份同值的内边距，段内
/// 片段的位置因此都落在框内；框本身的宽度取自折行阶段算好的那一份（见 [`close_kbd_box`]），
/// 几何（含垂直居中）记进 [`FlowLine::kbd_boxes`]。
///
/// 每行同时算出 [`FlowLine::content_width`]：本行**可见**内容（含 kbd 框的边框与内边距）的
/// 右边缘，行尾悬挂的空白不算——它与 [`wrap_ranges`] 复核宽度时用的是同一个口径
/// （[`trim_trailing_whitespace`]），段落对齐因此不会因为行尾的空格而偏。
fn place_lines(
    items: &[FlowItem],
    spans: &[Range<usize>],
    ranges: &[Range<usize>],
    runs: &[KbdRun],
    default_font: &Font,
    font_size: Pixels,
    line_height: Pixels,
    window: &Window,
) -> (Vec<FlowLine>, Pixels) {
    let padding = font_size * KBD_PADDING;
    let mut lines = Vec::with_capacity(ranges.len());
    let mut max_width = Pixels::ZERO;
    let mut top = Pixels::ZERO;
    // 定位按文档顺序走过每一条行，kbd 段在 `runs` 里也是文档顺序，所以一个游标就够。
    let mut next_run = 0;

    for range in ranges {
        let mut fragments = Vec::with_capacity(items.len());
        let mut left = Pixels::ZERO;
        let mut height = line_height;
        let mut kbd_boxes = Vec::new();
        let mut open: Option<OpenKbdBox> = None;
        // 本行最后一个非空白字符的右边界：行尾悬挂的空白不画进可见宽度里。
        let visible_end = trim_trailing_whitespace(items, spans, range.start, range.end);
        let mut content_width = Pixels::ZERO;

        for (item, span) in items.iter().zip(spans) {
            if span.end <= range.start || span.start >= range.end {
                continue;
            }
            // 段首 / 段尾各留一份内边距：框宽 = 内容宽 + 2 × 内边距，与 [`kbd_runs`] 算的
            // 折行宽度一致；框高 = 段内最高的内容 + 2 × 内边距，行高不够时跟着长高。
            let kbd = item.kbd();
            if open.as_ref().map(|open| open.id) != kbd {
                close_kbd_box(&mut open, &mut left, &mut height, &mut kbd_boxes, padding);
                if let Some(id) = kbd {
                    // 框的左边缘就是内容的左边界，内边距从它向右留：先记框，再推 `left`。
                    // 顺序颠倒会把框的左边缘记成内容起点，左右内边距就不对称了。
                    //
                    // 框宽是折行阶段算好的那一份（[`kbd_runs`]）：这里只取用，不再按片段累加
                    // 第二份账，画出来的框与折行用的宽度因此是同一个数。
                    let width = runs
                        .get(next_run)
                        .filter(|run| run.range.start == span.start)
                        .map_or(Pixels::ZERO, |run| run.width);
                    debug_assert_ne!(
                        width,
                        Pixels::ZERO,
                        "every kbd segment the locator walks was measured by the wrapper"
                    );
                    next_run += 1;
                    open = Some(OpenKbdBox {
                        id,
                        left,
                        width,
                        content: Pixels::ZERO,
                    });
                    left += padding;
                }
            }
            match item {
                FlowItem::InlineImage(item) => {
                    if range.start <= span.start && span.end <= range.end {
                        fragments.push(PlacedFragment {
                            kind: FragmentKind::InlineImage {
                                link: item.link,
                                height: item.size.height,
                            },
                            left,
                            width: item.size.width,
                        });
                        left += item.size.width;
                        // 图片整体在可见范围内（行尾裁剪碰到非文字项就停，见
                        // [`trim_trailing_whitespace`]），所以它的右边缘就是本行的可见右边界。
                        content_width = left;
                        // 图片的高把整行撑起来，行内其它内容（含 kbd 框）随后按行高垂直居中。
                        height = height.max(item.size.height);
                        if let Some(open) = open.as_mut() {
                            open.content = open.content.max(item.size.height);
                        }
                    }
                }
                FlowItem::Text(item) => {
                    let start = range.start.max(span.start) - span.start;
                    let end = range.end.min(span.end) - span.start;
                    let font = item.font(default_font);
                    let run = text_run(&font, item.color, end - start);
                    // 子串一定不含 `'\n'`（Pass B 已经按换行切过）。
                    let line = window.text_system().shape_line(
                        SharedString::from(&item.text[start..end]),
                        font_size,
                        &[run],
                        None,
                    );
                    let width = line.width();
                    // 可见右边界落在这段文字里时，它在段内的字符位置上——不是段尾：段尾那些
                    // 悬挂的空白不算宽度，所以这里问的是字形推进位置。整段都可见（折行点不在
                    // 它里面）时直接用塑形宽度，省掉一次逐字形的查找。
                    //
                    // 行尾悬挂的空白可以整段落在这段文字**之前**：折行点落在跨片段的空白串里
                    // （上一片段以空白收尾、本片段又以空白开头，例如两张被丢掉的图片把两个空格
                    // 拼到一起），本段在可见范围里就一个字符都不占。这次减法必须待在被挡住的分支
                    // 里：`then_some` 的参数是**先求值**的，写在条件里也挡不住它——窗口一窄就
                    // `attempt to subtract with overflow`。
                    let visible_start = span.start + start;
                    let visible_stop = if visible_end > visible_start {
                        Some(visible_end.min(span.start + end) - visible_start)
                    } else {
                        None
                    };
                    if let Some(stop) = visible_stop {
                        content_width = left
                            + if stop == end - start {
                                width
                            } else {
                                line.x_for_index(stop)
                            };
                    }
                    if let Some(open) = open.as_mut() {
                        // 文字的垂直范围就是它自己的字形盒（ascent + descent），与绘制时的配方一致。
                        open.content = open.content.max(line.ascent + line.descent);
                    }
                    fragments.push(PlacedFragment {
                        kind: FragmentKind::Text {
                            line,
                            link: item.link,
                        },
                        left,
                        width,
                    });
                    left += width;
                }
            }
        }
        close_kbd_box(&mut open, &mut left, &mut height, &mut kbd_boxes, padding);
        // kbd 框的右边缘（含右侧内边距）也是可见内容，而且比框内最后一个片段的右边缘更靠右。
        // 行尾裁剪不会裁进 kbd 段里（见 [`trim_trailing_whitespace`]），所以本行可见范围内
        // 的框都算数：取最靠右的那个。
        for kbd_box in &kbd_boxes {
            content_width = content_width.max(kbd_box.left + kbd_box.width);
        }
        for kbd_box in &mut kbd_boxes {
            kbd_box.top = (height - kbd_box.height) / 2.;
        }

        max_width = max_width.max(left);
        lines.push(FlowLine {
            top,
            height,
            content_width,
            fragments,
            kbd_boxes,
        });
        top += height;
    }

    (lines, max_width)
}

/// Pass E：段落对齐。每一行在自己的行盒里水平偏移，行内一切内容（文字、图片、kbd 框、
/// 链接下划线、命中盒）的相对位置因此不变，只有整行的左边缘动了。
///
/// 行盒的宽度就是块自己的宽度：已知可用宽度时是它，否则是最宽那一行的宽度（块按内容宽度定
/// 尺寸，见 [`build_layout`] 结尾的 `size`）——容器不给可用宽度时行盒不会更宽，更短的行仍然
/// 在这个宽度里偏移。整行只有空白时没有可见内容，也不偏移。
fn align_lines(lines: &mut [FlowLine], align: ParagraphAlign, available: Pixels) {
    for line in lines {
        let offset = if line.content_width > Pixels::ZERO {
            align_offset(align, available, line.content_width)
        } else {
            Pixels::ZERO
        };
        for fragment in &mut line.fragments {
            fragment.left += offset;
        }
        for kbd_box in &mut line.kbd_boxes {
            kbd_box.left += offset;
        }
    }
}

/// 宽 `width` 的一行内容在宽 `available` 的行盒里的左偏移。
///
/// 左对齐恒为 0；内容比行盒还宽时（超宽图片、拆不开的长词）也不偏移：溢出留在右侧，
/// 与本组件其它超宽内容的表现一致。
pub(super) fn align_offset(align: ParagraphAlign, available: Pixels, width: Pixels) -> Pixels {
    let free = (available - width).max(Pixels::ZERO);
    match align {
        ParagraphAlign::Left => Pixels::ZERO,
        ParagraphAlign::Right => free,
        ParagraphAlign::Center => free / 2.,
    }
}

fn build_layout(
    items: &[FlowItem],
    font: &Font,
    font_size: Pixels,
    line_height: Pixels,
    align: ParagraphAlign,
    wrap_width: Option<Pixels>,
    window: &mut Window,
) -> FlowLayout {
    let spans = item_spans(items);
    let total_len = spans.last().map_or(0, |span| span.end);
    // 防御分支：`parse.rs` 不会产生空块，而且每种 piece 至少占 1 个字节（`IMAGE_LEN` 与
    // `LineBreak` 的 `"\n"` 都是 1），所以 `build_layout` 永远拿不到 `total_len == 0`。
    // 删掉它则会走成"给空块排出一个行盒"：`place_lines` 会把唯一的空硬换行排成一行，
    // 空块于是白占一行高度。
    if total_len == 0 {
        return FlowLayout {
            wrap_width,
            size: Size::default(),
            lines: Vec::new(),
        };
    }

    let hard_lines = hard_line_ranges(items, &spans, total_len);
    // kbd 段的宽度在折行与定位两条路径上是同一份账：按硬换行算一次，两条路径共用
    // （见 [`kbd_runs`] 与 [`close_kbd_box`]）。
    let runs = hard_lines
        .iter()
        .flat_map(|hard_line| kbd_runs(items, &spans, hard_line, font, font_size, window))
        .collect::<Vec<_>>();
    let ranges = match wrap_width {
        Some(wrap_width) => wrap_ranges(
            items, &spans, &runs, font, font_size, wrap_width, hard_lines, window,
        ),
        None => hard_lines,
    };
    let (mut lines, max_width) = place_lines(
        items,
        &spans,
        &ranges,
        &runs,
        font,
        font_size,
        line_height,
        window,
    );
    // 已知可用宽度时行盒就是它，否则是内容宽度：后者正是下面 `size` 里要报出去的宽度。
    align_lines(&mut lines, align, wrap_width.unwrap_or(max_width));

    FlowLayout {
        wrap_width,
        // 已知可用宽度时占满，否则按内容宽度。
        //
        // 自报的宽度**会**传给布局：块本身是 `div().min_w_0()`，它的宽度就来自这里（实测把
        // 下面改成恒用 `max_width`，块会缩到内容宽，占位里的单行文案随之变窄）。所以这一支
        // 不是"只给将来预留"：`wrap_width` 为 `None` 时块会按内容宽度收缩。
        size: size(
            wrap_width.unwrap_or(max_width),
            lines
                .last()
                .map_or(Pixels::ZERO, |line| line.top + line.height),
        ),
        lines,
    }
}

/// 命中测试：`position` 落在哪个链接片段上（链接里的图片同样能命中）。
fn link_at(bounds: Bounds<Pixels>, layout: &FlowLayout, position: Point<Pixels>) -> Option<LinkId> {
    let local = position - bounds.origin;
    for line in &layout.lines {
        if local.y < line.top || local.y >= line.top + line.height {
            continue;
        }
        for fragment in &line.fragments {
            if local.x >= fragment.left && local.x < fragment.left + fragment.width {
                return match &fragment.kind {
                    FragmentKind::Text { link, .. } | FragmentKind::InlineImage { link, .. } => {
                        *link
                    }
                };
            }
        }
        return None;
    }
    None
}
