//! 组件侧：控件被画成什么（颜色配方）与组件度量 token。
//!
//! 这是旧 `style/components.rs` 的移植：一个配方是一个角色或一小组合角色合成出一个控件需要
//! 的颜色——按钮每种状态的填充/文字/边缘、输入框的字段与边、菜单的表面、对话框的阴影——在主题加
//! 载时解析一次，组件读字段而不是现场合成。
//!
//! **唯一有意的颜色差异**是 tonal 的 hover/active 填充：旧实现把洗色合成在 sRGB 编码空间
//! （`Hsla::blend` → 通道加权），新实现合成在引擎的线性预乘空间（[`over`]，§5.8.4）。8 位 sRGB
//! 差是 Light hover 7、Light active 12、Dark hover 34、Dark active 43（/255）；这是用户裁决的
//! 新视觉基线（保持引擎空间），逐值 golden 与旧/新差异表在本模块测试里，登记在
//! `spec-deltas/theme.md` §6，M2 视觉基线需确认。其余颜色（含 tonal 的文字/边缘）逐值相同。
//!
//! 除颜色外，本模块还给出一套**组件度量 token**（`ComponentTokens`）：控件高度、间距、水平内边
//! 距、字号、图标尺寸、边框线宽、动画 tick、阴影与滚动条 alpha。这些值此前散落在 `size.rs`、
//! `animation.rs` 与 `components.rs` 的私有常量里；把它们的**唯一来源**放进主题，后续批次迁移组件
//! 时不再各自抄一份。token 与外观无关（深色/浅色只换颜色）。
//!
//! 配方只用角色写，从不写调色板数值：这就是换外观只换调色板的可行性来源。

use std::time::Duration;

use ui_core::paint::color::Color;

use super::{
    color::{black_with_alpha, over, with_alpha},
    roles::RoleColors,
};

/// 交互控件的四种外观变体。
#[derive(Debug, Default, Copy, Clone, Eq, PartialEq, Hash)]
pub enum InteractiveVariant {
    /// 饱和品牌块：主按钮。
    #[default]
    Filled,
    /// 控件色块：次要按钮、下拉触发器。
    Tonal,
    /// 透明块 + 边缘：描边按钮。
    Outline,
    /// 只有文字与 hover 洗色：工具栏/菜单里的静默按钮。
    Ghost,
}

/// 交互控件的四种状态。
#[derive(Debug, Default, Copy, Clone, Eq, PartialEq, Hash)]
pub enum InteractiveState {
    /// 未接触。
    #[default]
    Normal,
    /// 指针所在。
    Hover,
    /// 按下中。
    Active,
    /// 不可用。
    Disabled,
}

/// 控件尺寸阶梯（旧 `size.rs::UnifiedSize` 的引擎侧对应物）。
#[derive(Debug, Default, Copy, Clone, Eq, PartialEq, Hash)]
pub enum ControlSize {
    /// 紧凑：小图标、密集列表。
    Small,
    /// 常规控件。
    #[default]
    Medium,
    /// 大号：对话框主操作。
    Large,
}

/// 一种控件尺寸的度量 token。
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct SizeTokens {
    /// 控件高度（逻辑像素）。
    pub height: f32,
    /// 控件内部间隙（图标与文字之间等）。
    pub gap: f32,
    /// 水平内边距。
    pub padding_x: f32,
    /// 字号（逻辑像素；按 16 px 根字号把旧 `rems` 阶梯换算而来）。
    pub font_size: f32,
    /// 图标边长。
    pub icon_size: f32,
}

/// 阴影 token：黑色 + alpha（阴影是两套外观里唯一没有色相可写的东西）。
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct ShadowTokens {
    /// 对话框阴影的 alpha（整个窗口上方的模态）。
    pub dialog_alpha: f32,
    /// 菜单阴影的 alpha（扁平浮层，比对话框轻）。
    pub menu_alpha: f32,
}

/// 滚动条 token：拇指静止与被指/拖动时的 alpha。
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct ScrollbarTokens {
    /// 指针在别处时的拇指：只提示还有内容可滚。
    pub resting_alpha: f32,
    /// 指针在上或正在拖动的拇指：它是个控件。
    pub active_alpha: f32,
}

/// 组件度量 token（与外观无关）。
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct ComponentTokens {
    /// 紧凑尺寸。
    pub small: SizeTokens,
    /// 常规尺寸。
    pub medium: SizeTokens,
    /// 大号尺寸。
    pub large: SizeTokens,
    /// 控件的边框线宽（逻辑像素）。
    ///
    /// 旧 `.border_1()` 同时做两件事：让边框参与布局（`border_widths` 挤占盒子）、并画一条线。
    /// 新引擎把两者拆开——布局占位走 [`StyleBuilder::border`](crate::style_builder::StyleBuilder::border)
    /// （`Style.inner.border`），画线走
    /// [`PaintBuilder::border`](crate::style_builder::PaintBuilder::border)（`PaintSpec.border`）；
    /// P2-4 迁移旧控件时用同一个 token 值成对写两处。
    pub border_width: f32,
    /// 动画的一帧时长（旧 `animation.rs::ANIMATION_TICK`）。
    pub animation_tick: Duration,
    /// 阴影。
    pub shadow: ShadowTokens,
    /// 滚动条拇指。
    pub scrollbar: ScrollbarTokens,
}

impl ComponentTokens {
    /// 标准 token 集。
    ///
    /// 尺寸阶梯的数值来自旧 `size.rs::UnifiedSize`（20/30/40 高、4/6/8 间隙、4/6/10 水平内边距、
    /// 0.75/0.875/1.125 rem 字号、8/12/16 图标）；线宽来自控件普遍画的 1 px 边缘；动画 tick 来自
    /// 旧 `animation.rs`；阴影与滚动条 alpha 来自旧 `components.rs` 的私有常量。
    pub fn standard() -> Self {
        let size = |height, gap, padding_x, font_size, icon_size| SizeTokens {
            height,
            gap,
            padding_x,
            font_size,
            icon_size,
        };

        Self {
            small: size(20.0, 4.0, 4.0, 12.0, 8.0),
            medium: size(30.0, 6.0, 6.0, 14.0, 12.0),
            large: size(40.0, 8.0, 10.0, 18.0, 16.0),
            border_width: 1.0,
            animation_tick: Duration::from_millis(150),
            shadow: ShadowTokens {
                dialog_alpha: 0.25,
                menu_alpha: 0.12,
            },
            scrollbar: ScrollbarTokens {
                resting_alpha: 0.22,
                active_alpha: 0.40,
            },
        }
    }

    /// 某个尺寸阶梯的 token。
    pub fn for_size(&self, size: ControlSize) -> SizeTokens {
        match size {
            ControlSize::Small => self.small,
            ControlSize::Medium => self.medium,
            ControlSize::Large => self.large,
        }
    }
}

impl Default for ComponentTokens {
    fn default() -> Self {
        Self::standard()
    }
}

/// 组件画的全部颜色，从一个外观的角色解析出来。
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct ComponentColors {
    /// 四种交互变体及其状态。
    pub interactive: InteractiveVariantColors,
    /// 输入框。
    pub input: InputColors,
    /// 菜单、下拉面板。
    pub menu: MenuColors,
    /// 对话框面板、遮罩、阴影。
    pub dialog: DialogColors,
    /// 开关/单选。
    pub toggle: ToggleStateColors,
    /// 进度条。
    pub progress: ProgressColors,
    /// 滚动条拇指。
    pub scrollbar: ScrollbarColors,
    /// tooltip。
    pub tooltip: TooltipColors,
    /// 图表的部件。
    pub chart: ChartColors,
}

impl ComponentColors {
    /// 解析一个外观的全部配方（主题加载时一次）。
    pub(crate) fn resolve(roles: &RoleColors, tokens: &ComponentTokens) -> Self {
        Self {
            interactive: InteractiveVariantColors::resolve(roles),
            input: InputColors::resolve(roles),
            menu: MenuColors::resolve(roles, tokens),
            dialog: DialogColors::resolve(roles, tokens),
            toggle: ToggleStateColors::resolve(roles),
            progress: ProgressColors::resolve(roles),
            scrollbar: ScrollbarColors::resolve(roles, tokens),
            tooltip: TooltipColors::resolve(roles),
            chart: ChartColors::resolve(roles),
        }
    }
}

/// 四种交互变体的画法。
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct InteractiveVariantColors {
    /// 饱和块。
    pub filled: InteractiveStateColors,
    /// 控件色块。
    pub tonal: InteractiveStateColors,
    /// 透明块 + 边缘。
    pub outline: InteractiveStateColors,
    /// 只有文字与洗色。
    pub ghost: InteractiveStateColors,
}

impl InteractiveVariantColors {
    fn resolve(roles: &RoleColors) -> Self {
        Self {
            filled: InteractiveStateColors::resolve(roles, InteractiveVariant::Filled),
            tonal: InteractiveStateColors::resolve(roles, InteractiveVariant::Tonal),
            outline: InteractiveStateColors::resolve(roles, InteractiveVariant::Outline),
            ghost: InteractiveStateColors::resolve(roles, InteractiveVariant::Ghost),
        }
    }
}

/// 一种交互变体的四种状态。
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct InteractiveStateColors {
    /// 未接触。
    pub normal: InteractiveElementColors,
    /// 指针所在。
    pub hover: InteractiveElementColors,
    /// 按下中。
    pub active: InteractiveElementColors,
    /// 不可用。
    pub disabled: InteractiveElementColors,
}

impl InteractiveStateColors {
    fn resolve(roles: &RoleColors, variant: InteractiveVariant) -> Self {
        Self {
            normal: InteractiveElementColors::resolve(roles, variant, InteractiveState::Normal),
            hover: InteractiveElementColors::resolve(roles, variant, InteractiveState::Hover),
            active: InteractiveElementColors::resolve(roles, variant, InteractiveState::Active),
            disabled: InteractiveElementColors::resolve(roles, variant, InteractiveState::Disabled),
        }
    }
}

/// 一个交互元素当前状态的三块颜色。
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct InteractiveElementColors {
    /// 填充。`Color::TRANSPARENT` 表示这个变体不填任何东西。
    pub background: Color,
    /// 填充上的文字/图标颜色。
    pub foreground: Color,
    /// 边缘。`filled` 用填充色画它、`ghost` 画透明，这样"没有边框"仍然保留它一直布局的 1 px；
    /// `tonal` 与 `outline` 画真边缘，控件区域在填充与所在表面同值的地方也看得见。
    pub border: Color,
}

impl InteractiveElementColors {
    fn resolve(roles: &RoleColors, variant: InteractiveVariant, state: InteractiveState) -> Self {
        let colors = |background: Color, foreground: Color, border: Color| Self {
            background,
            foreground,
            border,
        };
        let transparent = Color::TRANSPARENT;
        let role = |role| roles.get(role);

        // 旧 §8.1：每个变体、每种状态的填充、文字与边缘。
        match variant {
            InteractiveVariant::Filled => match state {
                InteractiveState::Normal => colors(
                    role(super::role::Role::BrandBase),
                    role(super::role::Role::FgOnBrand),
                    role(super::role::Role::BrandBase),
                ),
                InteractiveState::Hover => colors(
                    role(super::role::Role::BrandHover),
                    role(super::role::Role::FgOnBrand),
                    role(super::role::Role::BrandHover),
                ),
                InteractiveState::Active => colors(
                    role(super::role::Role::BrandActive),
                    role(super::role::Role::FgOnBrand),
                    role(super::role::Role::BrandActive),
                ),
                InteractiveState::Disabled => colors(
                    role(super::role::Role::SurfaceControl),
                    role(super::role::Role::FgDisabled),
                    role(super::role::Role::SurfaceControl),
                ),
            },
            // tonal 在可用的每种状态里保持同一填充，只在上面移动一层中性洗色。填充不透明，因此
            // 洗色在这里合成掉（像素相同，只有一种颜色要过渡）。它画真边缘：浅色外观里
            // `surface.control` 与 `surface.overlay` 同值，边缘是控件在菜单/对话框上唯一的边界。
            InteractiveVariant::Tonal => {
                let base = role(super::role::Role::SurfaceControl);

                match state {
                    InteractiveState::Normal => colors(
                        base,
                        role(super::role::Role::FgDefault),
                        role(super::role::Role::LineBorder),
                    ),
                    InteractiveState::Hover => colors(
                        over(base, role(super::role::Role::VeilHover)),
                        role(super::role::Role::FgDefault),
                        role(super::role::Role::LineStrong),
                    ),
                    InteractiveState::Active => colors(
                        over(base, role(super::role::Role::VeilActive)),
                        role(super::role::Role::FgDefault),
                        role(super::role::Role::LineStrong),
                    ),
                    InteractiveState::Disabled => colors(
                        base,
                        role(super::role::Role::FgDisabled),
                        role(super::role::Role::LineDivider),
                    ),
                }
            }
            // outline 与 ghost 不填自己的东西，hover 洗色保留 alpha：它合成在调用方放它的任何表面
            // 上，一份洗色因此在工具栏、菜单、内容面上都对。
            InteractiveVariant::Outline => match state {
                InteractiveState::Normal => colors(
                    transparent,
                    role(super::role::Role::FgDefault),
                    role(super::role::Role::LineBorder),
                ),
                InteractiveState::Hover => colors(
                    role(super::role::Role::VeilHover),
                    role(super::role::Role::FgDefault),
                    role(super::role::Role::LineStrong),
                ),
                InteractiveState::Active => colors(
                    role(super::role::Role::VeilActive),
                    role(super::role::Role::FgDefault),
                    role(super::role::Role::LineStrong),
                ),
                InteractiveState::Disabled => colors(
                    transparent,
                    role(super::role::Role::FgDisabled),
                    role(super::role::Role::LineDivider),
                ),
            },
            InteractiveVariant::Ghost => match state {
                InteractiveState::Normal => {
                    colors(transparent, role(super::role::Role::FgDefault), transparent)
                }
                InteractiveState::Hover => colors(
                    role(super::role::Role::VeilHover),
                    role(super::role::Role::FgDefault),
                    transparent,
                ),
                InteractiveState::Active => colors(
                    role(super::role::Role::VeilActive),
                    role(super::role::Role::FgDefault),
                    transparent,
                ),
                InteractiveState::Disabled => colors(
                    transparent,
                    role(super::role::Role::FgDisabled),
                    transparent,
                ),
            },
        }
    }
}

/// 输入框的颜色。
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct InputColors {
    /// 字段填充。
    pub background: InputBackgroundColors,
    /// 输入的文字与只读文字。
    pub foreground: InputForegroundColors,
    /// 字段边缘。
    pub border: InputBorderColors,
    /// 占位符（比值再淡一档）。
    pub placeholder: Color,
    /// 字段内的文本选择。
    pub selection: Color,
}

/// 输入框的填充（可用/不可用）。
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct InputBackgroundColors {
    /// 可输入。
    pub enabled: Color,
    /// 不可输入：凹进内容面而不是浮起来。
    pub disabled: Color,
}

/// 输入框的文字（可用/不可用）。
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct InputForegroundColors {
    /// 可输入字段的值。
    pub enabled: Color,
    /// 不可输入字段的值。
    pub disabled: Color,
}

/// 输入框的边缘（逐状态）。
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct InputBorderColors {
    /// 光标不在其中的字段。
    pub normal: Color,
    /// 光标在其中：品牌色，不是文字色。
    pub focused: Color,
    /// 不可输入。
    pub disabled: Color,
}

impl InputColors {
    fn resolve(roles: &RoleColors) -> Self {
        let role = |role| roles.get(role);

        Self {
            background: InputBackgroundColors {
                enabled: role(super::role::Role::SurfaceControl),
                disabled: role(super::role::Role::SurfaceSunken),
            },
            foreground: InputForegroundColors {
                enabled: role(super::role::Role::FgDefault),
                disabled: role(super::role::Role::FgDisabled),
            },
            border: InputBorderColors {
                normal: role(super::role::Role::LineBorder),
                focused: role(super::role::Role::BrandBase),
                disabled: role(super::role::Role::LineDivider),
            },
            placeholder: role(super::role::Role::FgSubtle),
            selection: role(super::role::Role::Selection),
        }
    }
}

/// 菜单的颜色。
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct MenuColors {
    /// 菜单表面（浮层，不是它盖住的面板）。
    pub background: Color,
    /// 分组标题：菜单画的最淡文字。
    pub group_foreground: Color,
    /// 面板投下的阴影。
    pub shadow: Color,
}

impl MenuColors {
    fn resolve(roles: &RoleColors, tokens: &ComponentTokens) -> Self {
        Self {
            background: roles.get(super::role::Role::SurfaceOverlay),
            group_foreground: roles.get(super::role::Role::FgSubtle),
            shadow: black_with_alpha(tokens.shadow.menu_alpha),
        }
    }
}

/// 对话框的颜色。
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct DialogColors {
    /// 面板表面。
    pub background: Color,
    /// 面板背后压在窗口上的遮罩。
    pub backdrop: Color,
    /// 面板投下的阴影。
    pub shadow: Color,
}

impl DialogColors {
    fn resolve(roles: &RoleColors, tokens: &ComponentTokens) -> Self {
        Self {
            background: roles.get(super::role::Role::SurfaceOverlay),
            backdrop: roles.get(super::role::Role::Scrim),
            shadow: black_with_alpha(tokens.shadow.dialog_alpha),
        }
    }
}

/// 开关的颜色（可用/不可用）。
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct ToggleStateColors {
    /// 可点击。
    pub enabled: ToggleColors,
    /// 不可点击。
    pub disabled: ToggleColors,
}

/// 一种开关状态的颜色。
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct ToggleColors {
    /// 打开时的轨道。
    pub checked: Color,
    /// 关闭时的轨道。
    pub unchecked: Color,
    /// 标签与轨道旁的几何。
    pub foreground: Color,
}

impl ToggleStateColors {
    fn resolve(roles: &RoleColors) -> Self {
        let role = |role| roles.get(role);

        Self {
            enabled: ToggleColors {
                checked: role(super::role::Role::BrandBase),
                unchecked: role(super::role::Role::LineStrong),
                foreground: role(super::role::Role::FgDefault),
            },
            // 禁用开关保留形状，用禁用文字色说"现在不行"；轨道退到主题最淡的边缘。
            disabled: ToggleColors {
                checked: role(super::role::Role::FgDisabled),
                unchecked: role(super::role::Role::LineDivider),
                foreground: role(super::role::Role::FgDisabled),
            },
        }
    }
}

/// 进度条的颜色。
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct ProgressColors {
    /// 进度跑在其中的轨道。
    pub track: Color,
    /// 已填充部分。
    pub progress: Color,
}

impl ProgressColors {
    fn resolve(roles: &RoleColors) -> Self {
        Self {
            track: roles.get(super::role::Role::SurfaceControl),
            progress: roles.get(super::role::Role::BrandBase),
        }
    }
}

/// 滚动条拇指的两种颜色。
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct ScrollbarColors {
    /// 指针在别处：只提示还有内容可滚。
    pub normal: Color,
    /// 指针在上或正在拖动：它是个控件。
    pub active: Color,
}

impl ScrollbarColors {
    fn resolve(roles: &RoleColors, tokens: &ComponentTokens) -> Self {
        let thumb = |alpha: f32| with_alpha(roles.get(super::role::Role::FgDefault), alpha);

        Self {
            normal: thumb(tokens.scrollbar.resting_alpha),
            active: thumb(tokens.scrollbar.active_alpha),
        }
    }
}

/// tooltip 的颜色。
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct TooltipColors {
    /// 浮在页面上的标签：被翻到反面而不是给个品牌色——它是被读的，不是被按的。
    pub background: Color,
    /// 标签文字。
    pub foreground: Color,
}

impl TooltipColors {
    fn resolve(roles: &RoleColors) -> Self {
        Self {
            background: roles.get(super::role::Role::SurfaceInverse),
            foreground: roles.get(super::role::Role::FgOnInverse),
        }
    }
}

/// 图表配方点名的部件（旧 §8.7 的名字）。
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct ChartColors {
    /// 网格线与轴刻度。
    pub grid: Color,
    /// 轴标签与时间刻度。
    pub axis: Color,
    /// 指针画的十字线。
    pub crosshair: Color,
    /// 阳线、上涨流量条、上涨格子。
    pub bullish: Color,
    /// 阴线、下跌流量条、下跌格子。
    pub bearish: Color,
    /// 六个图表系列，按顺序。
    pub series: [Color; 6],
}

impl ChartColors {
    fn resolve(roles: &RoleColors) -> Self {
        Self {
            grid: roles.get(super::role::Role::ChartGrid),
            axis: roles.get(super::role::Role::ChartAxis),
            crosshair: roles.get(super::role::Role::ChartCrosshair),
            bullish: roles.get(super::role::Role::MarketUp),
            bearish: roles.get(super::role::Role::MarketDown),
            series: [
                roles.get(super::role::Role::ChartSeries0),
                roles.get(super::role::Role::ChartSeries1),
                roles.get(super::role::Role::ChartSeries2),
                roles.get(super::role::Role::ChartSeries3),
                roles.get(super::role::Role::ChartSeries4),
                roles.get(super::role::Role::ChartSeries5),
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ComponentTokens, ControlSize, InteractiveVariant};
    use crate::theme::{
        appearance::Appearance, color::contrast, palette::Palette, resolved::Theme, role::Role,
        roles::resolve_roles,
    };

    /// 旧测试的移植：指向中的滚动条拇指在它所在的表面上必须比静止的拇指更醒目（比 alpha 或未合
    /// 成的颜色都说明不了这一点——同一个 alpha 在亮表面上读起来比暗表面弱）。
    #[test]
    fn the_active_scrollbar_thumb_stands_out_more_than_the_resting_one() {
        for appearance in [Palette::light(), Palette::dark()] {
            let theme = Theme::of(appearance.appearance);
            let colors = theme.components();
            let base = theme.role(Role::SurfaceCanvas);
            let resting = contrast(colors.scrollbar.normal, base);
            let active = contrast(colors.scrollbar.active, base);

            assert!(
                active > resting,
                "{:?}: 活动拇指 {active:.2}:1 ≤ 静止拇指 {resting:.2}:1",
                appearance.appearance
            );
        }
    }

    /// 洗色合成在不透明填充上必须仍是不透明（tonal 按钮不需要后面的层）；不填东西的变体保留洗色
    /// 的 alpha，因为它合成在调用方选的表面上。
    #[test]
    fn a_washed_fill_is_opaque_exactly_when_the_variant_fills_something() {
        for appearance in [Palette::light(), Palette::dark()] {
            let theme = Theme::of(appearance.appearance);
            let colors = theme.components();

            for state in [
                colors.interactive.tonal.hover,
                colors.interactive.tonal.active,
            ] {
                assert!(
                    crate::theme::color::oklch_components(state.background)[3] == 1.0,
                    "{:?}: tonal 的 hover 是不透明填充上的洗色",
                    appearance.appearance
                );
            }

            for state in [
                colors.interactive.outline.hover,
                colors.interactive.outline.active,
                colors.interactive.ghost.hover,
                colors.interactive.ghost.active,
            ] {
                assert!(
                    crate::theme::color::oklch_components(state.background)[3] < 1.0,
                    "{:?}: outline/ghost 的洗色要保留 alpha",
                    appearance.appearance
                );
            }
        }
    }

    /// tonal 控件在浅色外观里填充与它所在的对话框/菜单同值，边界是唯一说得出控件在哪里的东西：
    /// 边缘对填充和对背后表面都要过 WCAG 非文本 3:1。
    #[test]
    fn a_tonal_control_draws_its_edge_against_its_fill_and_the_overlay() {
        for appearance in [Palette::light(), Palette::dark()] {
            let theme = Theme::of(appearance.appearance);
            let roles = resolve_roles(&appearance);
            let tonal = theme.components().interactive.tonal;

            assert_ne!(
                tonal.normal.border, tonal.normal.background,
                "{:?}: tonal 的静止边缘就是它自己的填充，控件没有边界",
                appearance.appearance
            );

            for (state, colors) in [
                ("normal", tonal.normal),
                ("hover", tonal.hover),
                ("active", tonal.active),
            ] {
                for (surface_name, surface) in [
                    ("surface.control", roles.get(Role::SurfaceControl)),
                    ("surface.overlay", roles.get(Role::SurfaceOverlay)),
                ] {
                    let ratio = contrast(colors.border, surface);

                    assert!(
                        ratio >= 3.0,
                        "{:?}: tonal {state} 边缘对 {surface_name} 只读 {ratio:.2}:1",
                        appearance.appearance
                    );
                }
            }
        }
    }

    /// 菜单是扁平浮层：黑色半透明阴影，比对话框轻；不透明阴影会画出一块板，带色阴影在暗页面上
    /// 读成光晕、在亮页面上读成涂抹。
    #[test]
    fn a_menu_casts_a_light_translucent_black_shadow() {
        for appearance in [Palette::light(), Palette::dark()] {
            let theme = Theme::of(appearance.appearance);
            let colors = theme.components();
            let shadow = colors.menu.shadow;
            let [lightness, chroma, _, alpha] = crate::theme::color::oklch_components(shadow);

            assert!(alpha > 0.0 && alpha < 1.0, "菜单阴影必须可叠");
            assert!(lightness < 1e-3 && chroma < 1e-3, "阴影恒为黑");
            assert!(
                alpha < crate::theme::color::oklch_components(colors.dialog.shadow)[3],
                "菜单阴影不能比对话框的还重"
            );
        }
    }

    /// 四种变体 × 四种状态都能解析（无缺失、无 panic），且 ghost/outline 的填充是透明的。
    #[test]
    fn every_variant_and_state_resolves() {
        let theme = Theme::of(crate::theme::appearance::Appearance::Light);
        let components = theme.components();

        for variant in [
            InteractiveVariant::Filled,
            InteractiveVariant::Tonal,
            InteractiveVariant::Outline,
            InteractiveVariant::Ghost,
        ] {
            let states = match variant {
                InteractiveVariant::Filled => components.interactive.filled,
                InteractiveVariant::Tonal => components.interactive.tonal,
                InteractiveVariant::Outline => components.interactive.outline,
                InteractiveVariant::Ghost => components.interactive.ghost,
            };

            for state in [states.normal, states.hover, states.active, states.disabled] {
                let _ = (state.background, state.foreground, state.border);
            }
        }

        assert_eq!(
            crate::theme::color::oklch_components(components.interactive.ghost.normal.background)
                [3],
            0.0
        );
        assert_eq!(
            crate::theme::color::oklch_components(components.interactive.outline.normal.background)
                [3],
            0.0
        );
    }

    /// tonal 四种状态的三块颜色在两种外观下的新基线（引擎线性合成）逐值锁定。
    ///
    /// 用户裁决：tonal 洗色采用引擎的线性预乘合成空间（保持当前实现），接受视觉变化。因此这份
    /// 8 位 sRGB 表是**新的**视觉基线；旧实现的 sRGB 编码空间差异由下一个测试登记，M2 视觉基线
    /// 要按这两张表确认（spec-delta `theme.md` §6）。
    #[test]
    fn the_tonal_palette_is_locked_to_the_engine_linear_composite() {
        // (外观, 状态, 背景, 文字, 边缘) 的 8 位 sRGB。
        let cases: [(Appearance, &str, [u8; 3], [u8; 3], [u8; 3]); 8] = [
            (
                Appearance::Light,
                "normal",
                [255, 255, 255],
                [29, 31, 46],
                [140, 142, 150],
            ),
            (
                Appearance::Light,
                "hover",
                [248, 248, 248],
                [29, 31, 46],
                [87, 89, 101],
            ),
            (
                Appearance::Light,
                "active",
                [244, 244, 244],
                [29, 31, 46],
                [87, 89, 101],
            ),
            (
                Appearance::Light,
                "disabled",
                [255, 255, 255],
                [156, 158, 165],
                [189, 190, 195],
            ),
            (
                Appearance::Dark,
                "normal",
                [27, 33, 40],
                [235, 241, 248],
                [107, 114, 121],
            ),
            (
                Appearance::Dark,
                "hover",
                [78, 82, 87],
                [235, 241, 248],
                [129, 135, 143],
            ),
            (
                Appearance::Dark,
                "active",
                [99, 103, 108],
                [235, 241, 248],
                [129, 135, 143],
            ),
            (
                Appearance::Dark,
                "disabled",
                [27, 33, 40],
                [93, 99, 106],
                [72, 78, 86],
            ),
        ];

        for (appearance, state, background, foreground, border) in cases {
            let tonal = Theme::of(appearance).components().interactive.tonal;
            let colors = match state {
                "normal" => tonal.normal,
                "hover" => tonal.hover,
                "active" => tonal.active,
                "disabled" => tonal.disabled,
                other => panic!("未知状态 {other}"),
            };

            for (field, color, expected) in [
                ("background", colors.background, background),
                ("foreground", colors.foreground, foreground),
                ("border", colors.border, border),
            ] {
                let actual = crate::theme::color::to_srgb8(color);
                assert_eq!(
                    [actual[0], actual[1], actual[2]],
                    expected,
                    "{appearance:?} tonal {state}.{field} 的引擎线性合成基线"
                );
            }
        }
    }

    /// 旧（sRGB 编码空间混合）与新（引擎线性预乘合成）tonal 洗色的逐值差异表。
    ///
    /// 这张表就是 P2-1a 审查 F1-2 的登记：用户裁决保留线性合成，因此差异是**已知且被接受的
    /// 视觉变化**，不是移植缺陷。表里同时锁旧值（从旧实现现场计算）与新值，任一侧改动都会红。
    #[test]
    fn the_tonal_washes_register_the_old_srgb_versus_new_linear_difference() {
        // (外观, 状态, 旧 [u8;3], 新 [u8;3], 逐通道差)。
        let cases: [(Appearance, &str, [u8; 3], [u8; 3], [i32; 3]); 4] = [
            (
                Appearance::Light,
                "hover",
                [241, 242, 242],
                [248, 248, 248],
                [7, 6, 6],
            ),
            (
                Appearance::Light,
                "active",
                [232, 233, 234],
                [244, 244, 244],
                [12, 11, 10],
            ),
            (
                Appearance::Dark,
                "hover",
                [44, 50, 57],
                [78, 82, 87],
                [34, 32, 30],
            ),
            (
                Appearance::Dark,
                "active",
                [56, 62, 69],
                [99, 103, 108],
                [43, 41, 39],
            ),
        ];

        for (appearance, state, old_expected, new_expected, registered) in cases {
            let theme = Theme::of(appearance);
            let old_washes = old_tonal_washes(appearance);
            let new_washes = theme.components().interactive.tonal;
            let (old_color, new_color) = match state {
                "hover" => (old_washes.hover.background, new_washes.hover.background),
                "active" => (old_washes.active.background, new_washes.active.background),
                other => panic!("未知状态 {other}"),
            };

            let old = old_srgb8(old_color);
            let new = crate::theme::color::to_srgb8(new_color);
            assert_eq!(
                [old[0], old[1], old[2]],
                old_expected,
                "{appearance:?} tonal {state} 的旧 sRGB 合成值（登记表）"
            );
            assert_eq!(
                [new[0], new[1], new[2]],
                new_expected,
                "{appearance:?} tonal {state} 的新线性合成值"
            );
            let delta = [
                i32::from(new[0]) - i32::from(old[0]),
                i32::from(new[1]) - i32::from(old[1]),
                i32::from(new[2]) - i32::from(old[2]),
            ];
            assert_eq!(delta, registered, "{appearance:?} tonal {state} 的登记差异");
        }
    }

    /// 旧 `style/components.rs` 的 tonal 配方（sRGB 编码空间合成）。
    fn old_tonal_washes(
        appearance: Appearance,
    ) -> crate::style::components::InteractiveStateColors {
        use crate::style::{
            components::ComponentColors, palette::Palette as OldPalette, roles::Roles,
        };

        let palette = match appearance {
            Appearance::Light => OldPalette::light(),
            Appearance::Dark => OldPalette::dark(),
        };

        ComponentColors::resolve(&Roles::resolve(&palette))
            .interactive
            .tonal
    }

    /// 旧 `ResolvedColor` 的 8 位 sRGB（旧实现渲染的像素）。
    fn old_srgb8(color: crate::style::color::ResolvedColor) -> [u8; 4] {
        let rgba = gpui_kit::Rgba::from(color.hsla);

        [
            (rgba.r.clamp(0.0, 1.0) * 255.0).round() as u8,
            (rgba.g.clamp(0.0, 1.0) * 255.0).round() as u8,
            (rgba.b.clamp(0.0, 1.0) * 255.0).round() as u8,
            (rgba.a.clamp(0.0, 1.0) * 255.0).round() as u8,
        ]
    }

    /// token 阶梯的数值必须与旧 `size.rs::UnifiedSize` 现场对齐（20/30/40 高、4/6/8 间隙、
    /// 4/6/10 内边距、12/14/18 字号、8/12/16 图标）。
    ///
    /// 旧值从 `crate::size` 读，而不是抄一份硬编码期望：P2-5 删旧模块之前，旧值一改这里就会红。
    #[test]
    fn the_size_token_ladder_matches_the_old_unified_size() {
        use crate::size::UnifiedSize;

        /// 旧 gpui 的 `Rems` 在布局里按 16 px 根字号换算。
        const ROOT_FONT_SIZE: f32 = 16.0;

        let tokens = ComponentTokens::standard();
        for (size, old) in [
            (ControlSize::Small, UnifiedSize::Small),
            (ControlSize::Medium, UnifiedSize::Medium),
            (ControlSize::Large, UnifiedSize::Large),
        ] {
            let token = tokens.for_size(size);
            assert_eq!(token.height, f32::from(old.h()), "高度");
            assert_eq!(token.gap, f32::from(old.gap()), "间隙");
            assert_eq!(token.padding_x, f32::from(old.px()), "水平内边距");
            assert_eq!(
                token.font_size,
                old.text_size().0 * ROOT_FONT_SIZE,
                "字号（rems × 根字号）"
            );
            assert_eq!(token.icon_size, f32::from(old.icon_size()), "图标边长");
        }

        assert_eq!(tokens.border_width, 1.0);
        assert_eq!(tokens.animation_tick, std::time::Duration::from_millis(150));
        // 旧 `components.rs` 的四个私有 alpha 常量。
        assert_eq!(tokens.shadow.dialog_alpha, 0.25);
        assert_eq!(tokens.shadow.menu_alpha, 0.12);
        assert_eq!(tokens.scrollbar.resting_alpha, 0.22);
        assert_eq!(tokens.scrollbar.active_alpha, 0.40);
    }
}
