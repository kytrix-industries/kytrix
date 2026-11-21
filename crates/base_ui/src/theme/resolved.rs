//! 一种外观加载后的完整主题：调色板 + 64 个角色 + 组件颜色配方 + 度量 token。
//!
//! 四件东西是一个整体、一个决定：读者不会看到与调色板不一致的角色，或与角色不一致的组件颜色。
//! 解析在主题加载时做一次（约四十次混合 + 组件的组合），之后一帧只读字段；[`Theme::resolve`] 是
//! **挂载期**把角色交给引擎 `ColorResolver` 的唯一出口（线性预乘 RGBA，设计 §5.8.2）。
//!
//! 谁决定应用画在哪种外观里不在本模块：持久化偏好、操作系统报告的外观与两者的优先级属于设置层；
//! 它交给 [`ThemeHandle`](crate::theme::handle::ThemeHandle) 的是它解析出来的那个外观。

use ui_core::paint::color::{Color, ColorResolver, LinearPremulRgba};

use super::{
    appearance::Appearance,
    components::{ComponentColors, ComponentTokens},
    palette::Palette,
    role::Role,
    roles::{RoleColors, resolve_roles},
};

/// 一种外观，加载完成：它的调色板与从调色板派生的一切。
#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    appearance: Appearance,
    palette: Palette,
    roles: RoleColors,
    components: ComponentColors,
    tokens: ComponentTokens,
}

impl Theme {
    /// 加载一种外观的主题。
    ///
    /// 解析就是主题的全部成本；一次外观切换付一次，之后一帧只读已解析的值、不做转换。
    pub fn of(appearance: Appearance) -> Self {
        let palette = Palette::of(appearance);
        let tokens = ComponentTokens::standard();
        let roles = resolve_roles(&palette);
        let components = ComponentColors::resolve(&roles, &tokens);

        Self {
            appearance,
            palette,
            roles,
            components,
            tokens,
        }
    }

    /// 这个主题画的外观。
    pub fn appearance(&self) -> Appearance {
        self.appearance
    }

    /// 手写的调色板。
    pub fn palette(&self) -> &Palette {
        &self.palette
    }

    /// 一个语义角色的引擎颜色（挂载期常量）。
    ///
    /// 动态取色（深浅切换要失效节点）请走
    /// [`ThemeHandle::role`](crate::theme::handle::ThemeHandle::role)：它读主题信号并登记依赖。
    pub fn role(&self, role: Role) -> Color {
        self.roles.get(role)
    }

    /// 全部 64 个角色，供调色板审计与测试。
    pub fn resolved_roles(&self) -> impl Iterator<Item = (Role, Color)> + '_ {
        self.roles.iter()
    }

    /// 挂载期解析：角色 → 线性预乘 RGBA（GPU 消费的唯一表示）。
    pub fn resolve(&self, role: Role) -> LinearPremulRgba {
        ColorResolver.resolve(self.role(role))
    }

    /// 组件颜色配方。
    pub fn components(&self) -> &ComponentColors {
        &self.components
    }

    /// 组件度量 token。
    pub fn tokens(&self) -> &ComponentTokens {
        &self.tokens
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::of(Appearance::default())
    }
}

#[cfg(test)]
mod tests {
    use ui_core::paint::color::{Color, ColorResolver};

    use super::Theme;
    use crate::theme::{appearance::Appearance, role::Role};

    #[test]
    fn the_default_theme_draws_in_the_light_appearance() {
        assert_eq!(Theme::default().appearance(), Appearance::Light);
        assert_eq!(Theme::of(Appearance::Light), Theme::default());
    }

    /// 主题是一整个：角色与组件颜色来自同一套调色板，从不来自上一套。
    ///
    /// 用文本选择做探针：它既是一个角色、又是输入框组件配方的输入，所以"外观换了、画没换"的主题
    /// 会报出新外观却仍用旧高亮选文字。
    #[test]
    fn a_theme_carries_the_roles_and_components_of_the_appearance_it_names() {
        let dark = Theme::of(Appearance::Dark);
        let light = Theme::of(Appearance::Light);

        assert_eq!(dark.appearance(), Appearance::Dark);
        assert_ne!(dark.role(Role::Selection), light.role(Role::Selection));
        assert_ne!(
            dark.components().input.selection,
            light.components().input.selection
        );
        assert_eq!(
            dark.components().input.selection,
            dark.role(Role::Selection),
            "组件配方必须从同一套角色解析"
        );

        // 旧 §7.4 把深色选择色写成品牌混入页面 62%：OKLCH L 0.408、C 0.059。它同时证明"角色与
        // 组件一起动了"。
        let [lightness, chroma, _, _] =
            crate::theme::color::oklch_components(dark.role(Role::Selection));
        assert!(
            (lightness - 0.408).abs() < 0.002 && (chroma - 0.059).abs() < 0.002,
            "深色选择色 L {lightness:.3}、C {chroma:.3}，文档是 L 0.408、C 0.059"
        );
    }

    /// 挂载期解析路径与直接调用引擎解析器逐位一致。
    #[test]
    fn mount_time_resolution_goes_through_the_engine_color_resolver() {
        let theme = Theme::of(Appearance::Light);

        for role in Role::ALL {
            assert_eq!(
                theme.resolve(role),
                ColorResolver.resolve(theme.role(role)),
                "{}",
                role.name()
            );
        }
    }

    /// 全部 64 个角色在两种外观下都能读出来（无缺失、无 panic）。
    #[test]
    fn every_role_is_readable_in_both_appearances() {
        for appearance in [Appearance::Light, Appearance::Dark] {
            let theme = Theme::of(appearance);
            let mut count = 0;

            for (role, color) in theme.resolved_roles() {
                assert!(
                    matches!(color, Color::Oklch { .. }),
                    "{appearance:?}: {} 不是 OKLCH 分量",
                    role.name()
                );
                count += 1;
            }

            assert_eq!(count, Role::COUNT, "{appearance:?}: 角色覆盖");
        }
    }

    /// 全 128 对（64 角色 × 2 外观）与旧实现 8 位 sRGB 的逐值对照。
    ///
    /// 差异只有一个来源：旧实现逐通道 clamp、引擎做 CSS Color 4 chroma 二分。实测**只有 8 对越界色**
    /// 受影响（其余 120 对逐位相同），本测试把「只有这 8 对」这个事实钉住：多一对、少一对、或
    /// in-gamut 角色被改色，都会红。8 对的具体差异由下一个测试锁定，越界映射的裁决见
    /// `spec-deltas/theme.md` §3（用户裁决：以引擎映射为准）。
    #[test]
    fn every_role_is_compared_with_the_old_style_srgb_output() {
        use crate::style::{palette::Palette as OldPalette, roles::Roles as OldRoles};

        // 实测越界清单（角色 × 外观）；顺序与枚举顺序一致，便于阅读。
        let expected_out_of_gamut = [
            (Appearance::Light, Role::LineFocus),
            (Appearance::Light, Role::BrandBase),
            (Appearance::Light, Role::BrandHover),
            (Appearance::Light, Role::BrandActive),
            (Appearance::Light, Role::StatusWarningBase),
            (Appearance::Light, Role::ChartSeries2),
            (Appearance::Light, Role::ChartSeries4),
            (Appearance::Dark, Role::BrandActive),
        ];

        let mut compared = 0;
        let mut found_out_of_gamut = Vec::with_capacity(expected_out_of_gamut.len());
        for (appearance, old_palette) in [
            (Appearance::Light, OldPalette::light()),
            (Appearance::Dark, OldPalette::dark()),
        ] {
            let theme = Theme::of(appearance);
            let old = OldRoles::resolve(&old_palette);

            for role in Role::ALL {
                let (out_of_gamut, old_color) = old_srgb8(old_color(&old, role));
                compared += 1;

                if out_of_gamut {
                    found_out_of_gamut.push((appearance, role));
                    continue;
                }

                let actual = crate::theme::color::to_srgb8(theme.role(role));
                for channel in 0..3 {
                    let difference = i32::from(actual[channel]) - i32::from(old_color[channel]);
                    assert!(
                        difference.abs() <= 1,
                        "{appearance:?} {} 通道 {channel}: 新 {} vs 旧 {}（in-gamut 角色只允许 8 位舍入）",
                        role.name(),
                        actual[channel],
                        old_color[channel]
                    );
                }
            }
        }

        assert_eq!(compared, 128, "64 角色 × 2 外观");
        assert_eq!(
            found_out_of_gamut, expected_out_of_gamut,
            "越界色集合是登记事实：只允许这 8 对（spec-deltas/theme.md §3）"
        );
    }

    /// 8 对越界色的新基线：引擎 gamut mapping 的 8 位 sRGB，与旧实现逐通道 clamp 的差异。
    ///
    /// 用户裁决：以引擎映射为准，不改调色板源值、不在应用层 clamp。这张表同时锁旧值与新值，
    /// 任何一侧改动（调色板、引擎解析器、旧实现）都必须重新做 M2 视觉确认。
    #[test]
    fn the_eight_out_of_gamut_roles_are_locked_to_the_engine_gamut_mapping() {
        use crate::style::{palette::Palette as OldPalette, roles::Roles as OldRoles};

        // (外观, 角色, 旧逐通道 clamp, 引擎 gamut mapping)。
        let cases: [(Appearance, Role, [u8; 3], [u8; 3]); 8] = [
            (
                Appearance::Light,
                Role::LineFocus,
                [0, 109, 189],
                [0, 109, 187],
            ),
            (
                Appearance::Light,
                Role::BrandBase,
                [0, 109, 189],
                [0, 109, 187],
            ),
            (
                Appearance::Light,
                Role::BrandHover,
                [0, 95, 175],
                [0, 96, 166],
            ),
            (
                Appearance::Light,
                Role::BrandActive,
                [0, 86, 165],
                [0, 88, 152],
            ),
            (
                Appearance::Light,
                Role::StatusWarningBase,
                [150, 101, 0],
                [150, 101, 0],
            ),
            (
                Appearance::Light,
                Role::ChartSeries2,
                [0, 139, 139],
                [0, 134, 133],
            ),
            (
                Appearance::Light,
                Role::ChartSeries4,
                [102, 121, 0],
                [102, 121, 0],
            ),
            (
                Appearance::Dark,
                Role::BrandActive,
                [105, 198, 255],
                [120, 198, 255],
            ),
        ];

        let mut worst_difference = 0;
        for (appearance, role, old_expected, new_expected) in cases {
            let theme = Theme::of(appearance);
            let new = crate::theme::color::to_srgb8(theme.role(role));
            assert_eq!(
                [new[0], new[1], new[2]],
                new_expected,
                "{appearance:?} {} 的引擎 gamut mapping 基线",
                role.name()
            );

            // 旧值从旧实现现场计算：登记表不是抄来的常数，旧实现改了也会红。
            let old_palette = match appearance {
                Appearance::Light => OldPalette::light(),
                Appearance::Dark => OldPalette::dark(),
            };
            let old_roles = OldRoles::resolve(&old_palette);
            let (out_of_gamut, old) = old_srgb8(old_color(&old_roles, role));
            assert!(out_of_gamut, "{appearance:?} {} 必须是越界色", role.name());
            assert_eq!(
                [old[0], old[1], old[2]],
                old_expected,
                "{appearance:?} {} 的旧逐通道 clamp 值",
                role.name()
            );

            for channel in 0..3 {
                let difference = i32::from(new[channel]) - i32::from(old[channel]);
                worst_difference = worst_difference.max(difference.abs());
                assert!(
                    difference.abs() <= 15,
                    "{appearance:?} {} 通道 {channel}: 差异 {difference} 超过登记的 15/255",
                    role.name()
                );
            }
        }

        assert_eq!(
            worst_difference, 15,
            "登记的最大差是 Dark brand.active 的 15/255；上限变化必须同步台账"
        );
    }

    /// 旧 `ResolvedColor` 的角色选择（保持与旧 `roles.rs` 的 64 个字段一一对应的显式映射）。
    fn old_color(
        old: &crate::style::roles::Roles,
        role: Role,
    ) -> crate::style::color::ResolvedColor {
        use crate::style::roles::Roles;

        let Roles {
            surface,
            line,
            fg,
            brand,
            status,
            market,
            fear_greed,
            chart,
            selection,
            veil,
            scrim,
        } = *old;

        match role {
            Role::SurfaceCanvas => surface.canvas,
            Role::SurfaceRaised => surface.raised,
            Role::SurfaceSunken => surface.sunken,
            Role::SurfaceOverlay => surface.overlay,
            Role::SurfaceControl => surface.control,
            Role::SurfaceInverse => surface.inverse,
            Role::LineDivider => line.divider,
            Role::LineBorder => line.border,
            Role::LineStrong => line.strong,
            Role::LineFocus => line.focus,
            Role::FgDefault => fg.default,
            Role::FgMuted => fg.muted,
            Role::FgSubtle => fg.subtle,
            Role::FgDisabled => fg.disabled,
            Role::FgOnBrand => fg.on_brand,
            Role::FgOnInverse => fg.on_inverse,
            Role::BrandBase => brand.base,
            Role::BrandHover => brand.hover,
            Role::BrandActive => brand.active,
            Role::StatusDangerBase => status.danger.base,
            Role::StatusDangerSurface => status.danger.surface,
            Role::StatusDangerBorder => status.danger.border,
            Role::StatusWarningBase => status.warning.base,
            Role::StatusWarningSurface => status.warning.surface,
            Role::StatusWarningBorder => status.warning.border,
            Role::StatusSuccessBase => status.success.base,
            Role::StatusSuccessSurface => status.success.surface,
            Role::StatusSuccessBorder => status.success.border,
            Role::StatusInfoBase => status.info.base,
            Role::StatusInfoSurface => status.info.surface,
            Role::StatusInfoBorder => status.info.border,
            Role::StatusElevatedBase => status.elevated.base,
            Role::StatusElevatedSurface => status.elevated.surface,
            Role::StatusElevatedBorder => status.elevated.border,
            Role::MarketUp => market.up,
            Role::MarketDown => market.down,
            Role::MarketFlat => market.flat,
            Role::MarketUpSurface => market.up_surface,
            Role::MarketDownSurface => market.down_surface,
            Role::MarketUpWeak => market.up_weak,
            Role::MarketDownWeak => market.down_weak,
            Role::FearGreedExtremeFear => fear_greed.extreme_fear,
            Role::FearGreedFear => fear_greed.fear,
            Role::FearGreedNeutral => fear_greed.neutral,
            Role::FearGreedGreed => fear_greed.greed,
            Role::FearGreedExtremeGreed => fear_greed.extreme_greed,
            Role::ChartGrid => chart.grid,
            Role::ChartAxis => chart.axis,
            Role::ChartCrosshair => chart.crosshair,
            Role::ChartMapOcean => chart.map_ocean,
            Role::ChartMapLand => chart.map_land,
            Role::ChartMapBorder => chart.map_border,
            Role::ChartMapRoad => chart.map_road,
            Role::ChartMapRoadMinor => chart.map_road_minor,
            Role::ChartSeries0 => chart.series[0],
            Role::ChartSeries1 => chart.series[1],
            Role::ChartSeries2 => chart.series[2],
            Role::ChartSeries3 => chart.series[3],
            Role::ChartSeries4 => chart.series[4],
            Role::ChartSeries5 => chart.series[5],
            Role::Selection => selection,
            Role::VeilHover => veil.hover,
            Role::VeilActive => veil.active,
            Role::Scrim => scrim,
        }
    }

    /// 旧颜色的 8 位 sRGB，以及它是否越出 sRGB 色域。
    ///
    /// 越界标志用**未 clamp** 的 OKLCH → sRGB 转换判定；返回的 8 位值则是旧实现真正画出来的像素
    /// （旧 `Oklch::hsla()` 逐通道 clamp 后转 HSL，`Rgba::from` 再回到 sRGB 编码空间）。
    fn old_srgb8(color: crate::style::color::ResolvedColor) -> (bool, [u8; 4]) {
        use color::{ColorSpaceTag, DynamicColor, Flags};
        use gpui_kit::Rgba;

        let oklch = color.oklch;
        let srgb = DynamicColor {
            cs: ColorSpaceTag::Oklch,
            flags: Flags::default(),
            components: [
                oklch.lightness(),
                oklch.chroma(),
                oklch.hue(),
                oklch.alpha(),
            ],
        }
        .convert(ColorSpaceTag::Srgb)
        .components;
        let out_of_gamut = srgb[..3]
            .iter()
            .any(|channel| !(-1e-4..=1.0 + 1e-4).contains(channel));

        let rgba = Rgba::from(color.hsla);

        (
            out_of_gamut,
            [
                (rgba.r.clamp(0.0, 1.0) * 255.0).round() as u8,
                (rgba.g.clamp(0.0, 1.0) * 255.0).round() as u8,
                (rgba.b.clamp(0.0, 1.0) * 255.0).round() as u8,
                (rgba.a.clamp(0.0, 1.0) * 255.0).round() as u8,
            ],
        )
    }
}
