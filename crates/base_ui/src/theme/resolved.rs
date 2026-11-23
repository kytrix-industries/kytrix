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

    /// 旧实现（`style/roles.rs`）的 8 位 sRGB 基线（P5-2 在旧模块删除前逐位冻结）。
    ///
    /// (外观, 角色, 是否越出 sRGB 色域, 旧实现画出的 8 位 sRGB)。
    const LEGACY_SRGB: [(Appearance, Role, bool, [u8; 3]); 128] = [
        (
            Appearance::Light,
            Role::SurfaceCanvas,
            false,
            [248, 248, 248],
        ),
        (
            Appearance::Light,
            Role::SurfaceRaised,
            false,
            [255, 255, 255],
        ),
        (
            Appearance::Light,
            Role::SurfaceSunken,
            false,
            [238, 238, 238],
        ),
        (
            Appearance::Light,
            Role::SurfaceOverlay,
            false,
            [255, 255, 255],
        ),
        (
            Appearance::Light,
            Role::SurfaceControl,
            false,
            [255, 255, 255],
        ),
        (Appearance::Light, Role::SurfaceInverse, false, [29, 31, 46]),
        (Appearance::Light, Role::LineDivider, false, [189, 190, 195]),
        (Appearance::Light, Role::LineBorder, false, [140, 142, 150]),
        (Appearance::Light, Role::LineStrong, false, [87, 89, 101]),
        (Appearance::Light, Role::LineFocus, true, [0, 109, 189]),
        (Appearance::Light, Role::FgDefault, false, [29, 31, 46]),
        (Appearance::Light, Role::FgMuted, false, [87, 89, 101]),
        (Appearance::Light, Role::FgSubtle, false, [129, 131, 140]),
        (Appearance::Light, Role::FgDisabled, false, [156, 158, 165]),
        (Appearance::Light, Role::FgOnBrand, false, [248, 248, 248]),
        (Appearance::Light, Role::FgOnInverse, false, [248, 248, 248]),
        (Appearance::Light, Role::BrandBase, true, [0, 109, 189]),
        (Appearance::Light, Role::BrandHover, true, [0, 95, 175]),
        (Appearance::Light, Role::BrandActive, true, [0, 86, 165]),
        (
            Appearance::Light,
            Role::StatusDangerBase,
            false,
            [194, 40, 42],
        ),
        (
            Appearance::Light,
            Role::StatusDangerSurface,
            false,
            [247, 240, 239],
        ),
        (
            Appearance::Light,
            Role::StatusDangerBorder,
            false,
            [236, 175, 168],
        ),
        (
            Appearance::Light,
            Role::StatusWarningBase,
            true,
            [150, 101, 0],
        ),
        (
            Appearance::Light,
            Role::StatusWarningSurface,
            false,
            [244, 242, 239],
        ),
        (
            Appearance::Light,
            Role::StatusWarningBorder,
            false,
            [211, 191, 163],
        ),
        (
            Appearance::Light,
            Role::StatusSuccessBase,
            false,
            [20, 125, 70],
        ),
        (
            Appearance::Light,
            Role::StatusSuccessSurface,
            false,
            [239, 243, 240],
        ),
        (
            Appearance::Light,
            Role::StatusSuccessBorder,
            false,
            [168, 200, 177],
        ),
        (
            Appearance::Light,
            Role::StatusInfoBase,
            false,
            [16, 117, 157],
        ),
        (
            Appearance::Light,
            Role::StatusInfoSurface,
            false,
            [239, 242, 244],
        ),
        (
            Appearance::Light,
            Role::StatusInfoBorder,
            false,
            [168, 197, 214],
        ),
        (
            Appearance::Light,
            Role::StatusElevatedBase,
            false,
            [179, 77, 17],
        ),
        (
            Appearance::Light,
            Role::StatusElevatedSurface,
            false,
            [246, 241, 239],
        ),
        (
            Appearance::Light,
            Role::StatusElevatedBorder,
            false,
            [226, 183, 164],
        ),
        (Appearance::Light, Role::MarketUp, false, [20, 123, 64]),
        (Appearance::Light, Role::MarketDown, false, [194, 50, 60]),
        (Appearance::Light, Role::MarketFlat, false, [129, 131, 140]),
        (
            Appearance::Light,
            Role::MarketUpSurface,
            false,
            [239, 243, 240],
        ),
        (
            Appearance::Light,
            Role::MarketDownSurface,
            false,
            [247, 240, 240],
        ),
        (Appearance::Light, Role::MarketUpWeak, false, [20, 123, 64]),
        (
            Appearance::Light,
            Role::MarketDownWeak,
            false,
            [194, 50, 60],
        ),
        (
            Appearance::Light,
            Role::FearGreedExtremeFear,
            false,
            [182, 20, 41],
        ),
        (Appearance::Light, Role::FearGreedFear, false, [246, 70, 93]),
        (
            Appearance::Light,
            Role::FearGreedNeutral,
            false,
            [248, 211, 58],
        ),
        (
            Appearance::Light,
            Role::FearGreedGreed,
            false,
            [47, 228, 159],
        ),
        (
            Appearance::Light,
            Role::FearGreedExtremeGreed,
            false,
            [39, 161, 114],
        ),
        (Appearance::Light, Role::ChartGrid, false, [205, 205, 208]),
        (Appearance::Light, Role::ChartAxis, false, [129, 131, 140]),
        (
            Appearance::Light,
            Role::ChartCrosshair,
            false,
            [114, 116, 126],
        ),
        (
            Appearance::Light,
            Role::ChartMapOcean,
            false,
            [170, 211, 223],
        ),
        (
            Appearance::Light,
            Role::ChartMapLand,
            false,
            [242, 239, 233],
        ),
        (
            Appearance::Light,
            Role::ChartMapBorder,
            false,
            [168, 117, 201],
        ),
        (Appearance::Light, Role::ChartMapRoad, false, [220, 42, 103]),
        (
            Appearance::Light,
            Role::ChartMapRoadMinor,
            false,
            [160, 107, 1],
        ),
        (Appearance::Light, Role::ChartSeries0, false, [192, 111, 10]),
        (Appearance::Light, Role::ChartSeries1, false, [79, 86, 183]),
        (Appearance::Light, Role::ChartSeries2, true, [0, 139, 139]),
        (Appearance::Light, Role::ChartSeries3, false, [185, 64, 130]),
        (Appearance::Light, Role::ChartSeries4, true, [102, 121, 0]),
        (Appearance::Light, Role::ChartSeries5, false, [104, 97, 99]),
        (Appearance::Light, Role::Selection, false, [164, 195, 228]),
        (Appearance::Light, Role::VeilHover, false, [29, 31, 46]),
        (Appearance::Light, Role::VeilActive, false, [29, 31, 46]),
        (Appearance::Light, Role::Scrim, false, [0, 0, 0]),
        (Appearance::Dark, Role::SurfaceCanvas, false, [20, 26, 33]),
        (Appearance::Dark, Role::SurfaceRaised, false, [31, 37, 44]),
        (Appearance::Dark, Role::SurfaceSunken, false, [12, 17, 24]),
        (Appearance::Dark, Role::SurfaceOverlay, false, [35, 41, 48]),
        (Appearance::Dark, Role::SurfaceControl, false, [27, 33, 40]),
        (
            Appearance::Dark,
            Role::SurfaceInverse,
            false,
            [235, 241, 248],
        ),
        (Appearance::Dark, Role::LineDivider, false, [72, 78, 86]),
        (Appearance::Dark, Role::LineBorder, false, [107, 114, 121]),
        (Appearance::Dark, Role::LineStrong, false, [129, 135, 143]),
        (Appearance::Dark, Role::LineFocus, false, [80, 174, 239]),
        (Appearance::Dark, Role::FgDefault, false, [235, 241, 248]),
        (Appearance::Dark, Role::FgMuted, false, [163, 169, 177]),
        (Appearance::Dark, Role::FgSubtle, false, [118, 124, 132]),
        (Appearance::Dark, Role::FgDisabled, false, [93, 99, 106]),
        (Appearance::Dark, Role::FgOnBrand, false, [9, 18, 25]),
        (Appearance::Dark, Role::FgOnInverse, false, [12, 17, 24]),
        (Appearance::Dark, Role::BrandBase, false, [80, 174, 239]),
        (Appearance::Dark, Role::BrandHover, false, [95, 189, 255]),
        (Appearance::Dark, Role::BrandActive, true, [105, 198, 255]),
        (
            Appearance::Dark,
            Role::StatusDangerBase,
            false,
            [249, 110, 116],
        ),
        (
            Appearance::Dark,
            Role::StatusDangerSurface,
            false,
            [41, 35, 41],
        ),
        (
            Appearance::Dark,
            Role::StatusDangerBorder,
            false,
            [100, 59, 64],
        ),
        (
            Appearance::Dark,
            Role::StatusWarningBase,
            false,
            [242, 187, 60],
        ),
        (
            Appearance::Dark,
            Role::StatusWarningSurface,
            false,
            [39, 40, 39],
        ),
        (
            Appearance::Dark,
            Role::StatusWarningBorder,
            false,
            [96, 82, 53],
        ),
        (
            Appearance::Dark,
            Role::StatusSuccessBase,
            false,
            [66, 206, 144],
        ),
        (
            Appearance::Dark,
            Role::StatusSuccessSurface,
            false,
            [26, 41, 43],
        ),
        (
            Appearance::Dark,
            Role::StatusSuccessBorder,
            false,
            [42, 88, 73],
        ),
        (
            Appearance::Dark,
            Role::StatusInfoBase,
            false,
            [75, 190, 231],
        ),
        (
            Appearance::Dark,
            Role::StatusInfoSurface,
            false,
            [27, 40, 49],
        ),
        (
            Appearance::Dark,
            Role::StatusInfoBorder,
            false,
            [43, 82, 101],
        ),
        (
            Appearance::Dark,
            Role::StatusElevatedBase,
            false,
            [250, 147, 78],
        ),
        (
            Appearance::Dark,
            Role::StatusElevatedSurface,
            false,
            [40, 37, 39],
        ),
        (
            Appearance::Dark,
            Role::StatusElevatedBorder,
            false,
            [100, 70, 55],
        ),
        (Appearance::Dark, Role::MarketUp, false, [98, 212, 128]),
        (Appearance::Dark, Role::MarketDown, false, [253, 101, 109]),
        (Appearance::Dark, Role::MarketFlat, false, [118, 124, 132]),
        (Appearance::Dark, Role::MarketUpSurface, false, [28, 42, 42]),
        (
            Appearance::Dark,
            Role::MarketDownSurface,
            false,
            [41, 34, 40],
        ),
        (Appearance::Dark, Role::MarketUpWeak, false, [98, 212, 128]),
        (
            Appearance::Dark,
            Role::MarketDownWeak,
            false,
            [253, 101, 109],
        ),
        (
            Appearance::Dark,
            Role::FearGreedExtremeFear,
            false,
            [204, 50, 60],
        ),
        (
            Appearance::Dark,
            Role::FearGreedFear,
            false,
            [250, 103, 115],
        ),
        (
            Appearance::Dark,
            Role::FearGreedNeutral,
            false,
            [248, 211, 58],
        ),
        (
            Appearance::Dark,
            Role::FearGreedGreed,
            false,
            [47, 228, 159],
        ),
        (
            Appearance::Dark,
            Role::FearGreedExtremeGreed,
            false,
            [60, 176, 128],
        ),
        (Appearance::Dark, Role::ChartGrid, false, [37, 43, 50]),
        (Appearance::Dark, Role::ChartAxis, false, [118, 124, 132]),
        (
            Appearance::Dark,
            Role::ChartCrosshair,
            false,
            [124, 130, 138],
        ),
        (Appearance::Dark, Role::ChartMapOcean, false, [11, 11, 11]),
        (Appearance::Dark, Role::ChartMapLand, false, [42, 42, 42]),
        (
            Appearance::Dark,
            Role::ChartMapBorder,
            false,
            [213, 178, 237],
        ),
        (Appearance::Dark, Role::ChartMapRoad, false, [138, 138, 138]),
        (
            Appearance::Dark,
            Role::ChartMapRoadMinor,
            false,
            [108, 108, 108],
        ),
        (Appearance::Dark, Role::ChartSeries0, false, [242, 176, 54]),
        (Appearance::Dark, Role::ChartSeries1, false, [152, 153, 244]),
        (Appearance::Dark, Role::ChartSeries2, false, [59, 207, 208]),
        (Appearance::Dark, Role::ChartSeries3, false, [247, 121, 186]),
        (Appearance::Dark, Role::ChartSeries4, false, [148, 171, 57]),
        (Appearance::Dark, Role::ChartSeries5, false, [152, 144, 146]),
        (Appearance::Dark, Role::Selection, false, [44, 77, 104]),
        (Appearance::Dark, Role::VeilHover, false, [235, 241, 248]),
        (Appearance::Dark, Role::VeilActive, false, [235, 241, 248]),
        (Appearance::Dark, Role::Scrim, false, [0, 0, 0]),
    ];

    /// 全 128 对（64 角色 × 2 外观）与旧实现 8 位 sRGB 的逐值对照（旧实现已删，基线冻结在
    /// [`LEGACY_SRGB`]）。
    ///
    /// 差异只有一个来源：旧实现逐通道 clamp、引擎做 CSS Color 4 chroma 二分。实测**只有 8 对越界色**
    /// 受影响（其余 120 对逐位相同），本测试把「只有这 8 对」这个事实钉住：多一对、少一对、或
    /// in-gamut 角色被改色，都会红。8 对的具体差异由下一个测试锁定，越界映射的裁决见
    /// `spec-deltas/theme.md` §3（用户裁决：以引擎映射为准）。
    #[test]
    fn every_role_is_compared_with_the_frozen_legacy_srgb_baseline() {
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

        let light = Theme::of(Appearance::Light);
        let dark = Theme::of(Appearance::Dark);
        let mut compared = 0;
        let mut found_out_of_gamut = Vec::with_capacity(expected_out_of_gamut.len());
        for (appearance, role, out_of_gamut, old_color) in LEGACY_SRGB {
            let theme = match appearance {
                Appearance::Light => &light,
                Appearance::Dark => &dark,
            };
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

        assert_eq!(compared, 128, "64 角色 × 2 外观");
        assert_eq!(
            found_out_of_gamut, expected_out_of_gamut,
            "越界色集合是登记事实：只允许这 8 对（spec-deltas/theme.md §3）"
        );
    }

    /// 8 对越界色的新基线：引擎 gamut mapping 的 8 位 sRGB，与旧实现逐通道 clamp 的冻结差异。
    ///
    /// 用户裁决：以引擎映射为准，不改调色板源值、不在应用层 clamp。这张表同时锁旧值（冻结）与新值，
    /// 任何一侧改动（调色板、引擎解析器）都必须重新做 M2 视觉确认。
    #[test]
    fn the_eight_out_of_gamut_roles_are_locked_to_the_engine_gamut_mapping() {
        // (外观, 角色, 旧逐通道 clamp（冻结）, 引擎 gamut mapping)。
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

            // 旧值必须与冻结基线表一致（两张表互相印证）。
            let frozen = LEGACY_SRGB
                .into_iter()
                .find(|(frozen_appearance, frozen_role, _, _)| {
                    *frozen_appearance == appearance && *frozen_role == role
                })
                .expect("越界角色必须在冻结基线表里");
            assert!(frozen.2, "{appearance:?} {} 必须是越界色", role.name());
            assert_eq!(
                frozen.3,
                old_expected,
                "{appearance:?} {} 的旧逐通道 clamp 值",
                role.name()
            );

            for channel in 0..3 {
                let difference = i32::from(new[channel]) - i32::from(old_expected[channel]);
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
}
