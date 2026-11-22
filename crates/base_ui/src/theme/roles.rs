//! 解析侧：把一套调色板解析成 64 个语义角色（引擎 `Color`）。
//!
//! 解析规则逐条移植自旧 `style/roles.rs`：表面直接取调色板；文字与线以 `text` 为源按权重混入
//! `canvas`；品牌与状态阶梯的方向跟随外观（浅色变暗、深色变亮）；chip 的填充是状态色几乎全部混进
//! 画布。旧实现把 58 条对比度地板写成表（设计规格即测试），这里原样保留：**调色板读不出来就是
//! debug 构建加载主题时失败**，而不是等到截图。
//!
//! 与旧实现的两处已登记数学差异：
//!
//! - **对比度**在**线性亮度**上算（引擎的合成空间），半透明前景也在线性空间合成；旧的 gpui 路径
//!   在 sRGB 编码空间合成。58 条地板的底色/前景都不透明，因此**地板值（角色对与下限）逐条相同**；
//!   参与越界色（8 对）的少数地板，比值因 gamut mapping 有 <0.25 的微小差（见 `resolved.rs` 与
//!   `spec-deltas/theme.md` §3/§4）。
//! - **越界色渲染**：旧实现逐通道 clamp、引擎做 CSS Color 4 chroma 二分，8 对角色 × 外观的 8 位
//!   sRGB 不同（最大 15/255）。用户裁决以引擎映射为准；golden 表在 `resolved.rs`。

use ui_core::paint::color::Color;

use super::{
    appearance::Appearance,
    color::{black_with_alpha, contrast, mix_oklab, step, tint, with_alpha},
    palette::Palette,
    role::Role,
};

/// 一个外观解析后的全部角色，按 [`Role::index`] 存放。
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct RoleColors {
    colors: [Color; Role::COUNT],
}

impl RoleColors {
    /// 读一个角色。
    pub fn get(&self, role: Role) -> Color {
        self.colors[role.index()]
    }

    /// 全部角色（审计与测试用）。
    pub fn iter(&self) -> impl Iterator<Item = (Role, Color)> + '_ {
        Role::ALL
            .into_iter()
            .map(|role| (role, self.colors[role.index()]))
    }

    /// 58 条对比度地板，作为「前景角色、背景角色、下限」的表。
    ///
    /// 这张表同时被 debug 期检查与验收测试读取：读不出来的角色报一次，报的是失败的那一对。
    pub(crate) fn contrast_floors(&self) -> [(Role, Role, f32); 58] {
        [
            (Role::FgDefault, Role::SurfaceCanvas, 12.0),
            (Role::FgDefault, Role::SurfaceSunken, 12.0),
            (Role::FgMuted, Role::SurfaceCanvas, 4.5),
            (Role::FgMuted, Role::SurfaceSunken, 4.5),
            (Role::FgSubtle, Role::SurfaceCanvas, 3.0),
            (Role::FgSubtle, Role::SurfaceRaised, 3.0),
            (Role::FgDisabled, Role::SurfaceCanvas, 2.0),
            (Role::LineDivider, Role::SurfaceCanvas, 1.5),
            (Role::LineDivider, Role::SurfaceRaised, 1.5),
            (Role::LineDivider, Role::SurfaceSunken, 1.5),
            (Role::LineDivider, Role::SurfaceOverlay, 1.5),
            (Role::LineBorder, Role::SurfaceControl, 3.0),
            (Role::LineBorder, Role::SurfaceCanvas, 3.0),
            (Role::LineStrong, Role::SurfaceCanvas, 3.7),
            (Role::MarketUp, Role::SurfaceCanvas, 4.5),
            (Role::MarketUp, Role::SurfaceSunken, 4.5),
            (Role::MarketDown, Role::SurfaceCanvas, 4.5),
            (Role::MarketDown, Role::SurfaceSunken, 4.5),
            (Role::StatusDangerBase, Role::SurfaceCanvas, 4.5),
            (Role::StatusWarningBase, Role::SurfaceCanvas, 4.5),
            (Role::StatusSuccessBase, Role::SurfaceCanvas, 4.5),
            (Role::StatusInfoBase, Role::SurfaceCanvas, 4.5),
            (Role::StatusElevatedBase, Role::SurfaceCanvas, 4.5),
            (Role::BrandBase, Role::SurfaceCanvas, 4.5),
            (Role::BrandBase, Role::SurfaceSunken, 4.5),
            (Role::BrandBase, Role::SurfaceRaised, 4.5),
            (Role::FgMuted, Role::SurfaceRaised, 4.5),
            (Role::StatusDangerBase, Role::StatusDangerSurface, 4.5),
            (Role::StatusWarningBase, Role::StatusWarningSurface, 4.5),
            (Role::StatusSuccessBase, Role::StatusSuccessSurface, 4.5),
            (Role::StatusInfoBase, Role::StatusInfoSurface, 4.5),
            (Role::StatusElevatedBase, Role::StatusElevatedSurface, 4.5),
            (Role::MarketUp, Role::MarketUpSurface, 4.5),
            (Role::MarketDown, Role::MarketDownSurface, 4.5),
            (Role::FgOnBrand, Role::BrandBase, 4.5),
            (Role::FgOnBrand, Role::BrandHover, 4.5),
            (Role::FgOnInverse, Role::SurfaceInverse, 12.0),
            (Role::LineFocus, Role::SurfaceCanvas, 3.0),
            (Role::ChartAxis, Role::SurfaceSunken, 3.0),
            (Role::ChartGrid, Role::SurfaceSunken, 1.15),
            (Role::ChartSeries0, Role::SurfaceSunken, 3.0),
            (Role::ChartSeries1, Role::SurfaceSunken, 3.0),
            (Role::ChartSeries2, Role::SurfaceSunken, 3.0),
            (Role::ChartSeries3, Role::SurfaceSunken, 3.0),
            (Role::ChartSeries4, Role::SurfaceSunken, 3.0),
            (Role::ChartSeries5, Role::SurfaceSunken, 3.0),
            // 海岸线地板：OSM 那一对填充自己的 1.40:1，略留余量。两个填充是整个地图的底，调色板
            // 就是交通层的，因此这条记录的是这对颜色测到的值，而不是要求它变成别的颜色。
            (Role::ChartMapLand, Role::ChartMapOcean, MAP_FILL_FLOOR),
            // 海面同时是画在图表内容面上的填充：浅色外观的纸陆地在那上面读 1.01:1——这是调色板
            // 所有者的选择（OSM 纸陆地优先于地图与面板的可见分界），因此陆地与内容面的分界被**刻意
            // 放弃**，只保留海面对内容面的这条地板（深色外观的近黑海面 1.04:1、浅色 1.38:1）。
            (Role::ChartMapOcean, Role::SurfaceSunken, 1.03),
            // 画在填充之上的描边，按制图学给线的地板：每条都是读者要跟着走的线，因此在陆地上读
            // 2.5:1；边界跨水、桥梁/堤道把干道带过水，因此这两条在水上也读 2.0:1；主干道在 1.2 px
            // 时太细，用同样 2.0:1 而不是更弱的一档。
            (
                Role::ChartMapBorder,
                Role::ChartMapLand,
                MAP_STROKE_LAND_FLOOR,
            ),
            (
                Role::ChartMapBorder,
                Role::ChartMapOcean,
                MAP_STROKE_OCEAN_FLOOR,
            ),
            (
                Role::ChartMapRoad,
                Role::ChartMapLand,
                MAP_STROKE_LAND_FLOOR,
            ),
            (
                Role::ChartMapRoad,
                Role::ChartMapOcean,
                MAP_STROKE_OCEAN_FLOOR,
            ),
            (
                Role::ChartMapRoadMinor,
                Role::ChartMapLand,
                MAP_STROKE_LAND_FLOOR,
            ),
            (
                Role::ChartMapRoadMinor,
                Role::ChartMapOcean,
                MAP_STROKE_OCEAN_FLOOR,
            ),
            (Role::SurfaceRaised, Role::SurfaceCanvas, 1.02),
            (Role::SurfaceSunken, Role::SurfaceCanvas, 1.05),
            (Role::SurfaceOverlay, Role::SurfaceCanvas, 1.02),
            (Role::SurfaceInverse, Role::SurfaceCanvas, 7.0),
        ]
    }

    /// debug 构建里加载主题就检查 58 条地板：release 发布的是两张手检过的调色板，不付这笔检查。
    pub(crate) fn assert_contrast_floors(&self) {
        if cfg!(debug_assertions) {
            for (foreground, background, floor) in self.contrast_floors() {
                let ratio = contrast(self.get(foreground), self.get(background));

                debug_assert!(
                    ratio >= floor,
                    "{} on {} reads {ratio:.2}:1, where its floor is {floor:.2}:1",
                    foreground.name(),
                    background.name()
                );
            }
        }
    }
}

/// 把一个外观的调色板解析成全部角色。
pub(crate) fn resolve_roles(palette: &Palette) -> RoleColors {
    let dark = palette.appearance == Appearance::Dark;
    let mix = |from: Color, to: Color, weight: f32| mix_oklab(from, to, weight);
    let canvas = palette.canvas;
    let text = palette.text;
    let mut colors = [Color::TRANSPARENT; Role::COUNT];
    {
        let mut set = |role: Role, color: Color| colors[role.index()] = color;

        // 表面：z 轴，由远及近。反色块就是文字色——浅色外观的近黑、深色外观的近白，正好是"页面的
        // 反面"。
        set(Role::SurfaceCanvas, canvas);
        set(Role::SurfaceRaised, palette.surface_raised);
        set(Role::SurfaceSunken, palette.surface_sunken);
        set(Role::SurfaceOverlay, palette.overlay);
        set(Role::SurfaceControl, palette.control);
        set(Role::SurfaceInverse, text);

        // 前景：正文、次级、占位、禁用，再混出两阶；两个"在填充上"的文字直接取调色板。
        set(Role::FgDefault, text);
        set(Role::FgMuted, mix(text, canvas, 0.30));
        set(Role::FgSubtle, mix(text, canvas, 0.50));
        set(Role::FgDisabled, mix(text, canvas, 0.62));
        set(Role::FgOnBrand, palette.on_brand);
        set(Role::FgOnInverse, palette.on_inverse);

        // 线：分隔线画在页面/面板头/表格面/菜单上，按页面混；深色外观把它带得更远（那里的 `raised`
        // 与 `overlay` 只比页面高 0.05–0.06 L）。强边缘在深色外观里少走一步，因为亮色混进暗页面比
        // 暗色混进亮页面涨对比涨得快。
        set(
            Role::LineDivider,
            mix(text, canvas, if dark { 0.72 } else { 0.76 }),
        );
        set(Role::LineBorder, mix(text, canvas, 0.55));
        set(
            Role::LineStrong,
            mix(text, canvas, if dark { 0.45 } else { 0.30 }),
        );
        set(Role::LineFocus, palette.brand);

        // 品牌色与它的两阶。
        set(Role::BrandBase, palette.brand);
        set(Role::BrandHover, step(palette.brand, 0.045, dark));
        set(Role::BrandActive, step(palette.brand, 0.075, dark));

        // 状态阶梯：一个配方给每一档——本色、几乎所有混进页面的 chip 填充、两者之间的边缘。
        let status_role =
            |color: Color| [color, tint(color, canvas, dark), mix(color, canvas, 0.62)];
        let statuses = [
            (Role::StatusDangerBase, palette.status.danger),
            (Role::StatusWarningBase, palette.status.warning),
            (Role::StatusSuccessBase, palette.status.success),
            (Role::StatusInfoBase, palette.status.info),
            (Role::StatusElevatedBase, palette.status.elevated),
        ];
        for (base, color) in statuses {
            let [base_color, surface, border] = status_role(color);
            let index = base.index();
            set(base, base_color);
            set(Role::ALL[index + 1], surface);
            set(Role::ALL[index + 2], border);
        }

        // 行情色：与状态色分离；chip 填充用同一配方，weak 是本色 22% alpha 的淡洗。
        set(Role::MarketUp, palette.market.up);
        set(Role::MarketDown, palette.market.down);
        set(Role::MarketFlat, mix(text, canvas, 0.50));
        set(Role::MarketUpSurface, tint(palette.market.up, canvas, dark));
        set(
            Role::MarketDownSurface,
            tint(palette.market.down, canvas, dark),
        );
        set(Role::MarketUpWeak, with_alpha(palette.market.up, 0.22));
        set(Role::MarketDownWeak, with_alpha(palette.market.down, 0.22));

        // 恐慌贪婪：调色板原样（这是刻度自己的阶梯，不是状态色）。
        set(Role::FearGreedExtremeFear, palette.fear_greed.extreme_fear);
        set(Role::FearGreedFear, palette.fear_greed.fear);
        set(Role::FearGreedNeutral, palette.fear_greed.neutral);
        set(Role::FearGreedGreed, palette.fear_greed.greed);
        set(
            Role::FearGreedExtremeGreed,
            palette.fear_greed.extreme_greed,
        );

        // 图表家具画在内容面（`surface.sunken`）上，因此网格与十字线按它混；地图的两个填充与三个描
        // 边是作者写的值（它们互相读，不对表面读）。
        set(Role::ChartGrid, mix(text, palette.surface_sunken, 0.86));
        set(Role::ChartAxis, mix(text, canvas, 0.50));
        set(
            Role::ChartCrosshair,
            mix(text, palette.surface_sunken, 0.45),
        );
        set(Role::ChartMapOcean, palette.map_ocean);
        set(Role::ChartMapLand, palette.map_land);
        set(Role::ChartMapBorder, palette.map_border);
        set(Role::ChartMapRoad, palette.map_road);
        set(Role::ChartMapRoadMinor, palette.map_road_minor);
        for (position, series) in [
            Role::ChartSeries0,
            Role::ChartSeries1,
            Role::ChartSeries2,
            Role::ChartSeries3,
            Role::ChartSeries4,
            Role::ChartSeries5,
        ]
        .into_iter()
        .zip(palette.series)
        {
            set(position, series);
        }

        // 选择与叠层。
        set(Role::Selection, mix(palette.brand, canvas, 0.62));
        set(Role::VeilHover, with_alpha(text, palette.veil_hover));
        set(Role::VeilActive, with_alpha(text, palette.veil_active));
        set(Role::Scrim, black_with_alpha(palette.scrim_alpha));
    }

    let roles = RoleColors { colors };
    roles.assert_contrast_floors();

    roles
}

/// 地图两个填充互相之间的地板。
///
/// 这对填充是 OSM 交通层自己的：海岸线读 1.40:1、海面在图表内容面上读 1.38:1。地板略低于两个测
/// 量值，因此它钉住调色板**已有**的数值，而不是要求 OSM 那对颜色给出它没有的一步。
const MAP_FILL_FLOOR: f32 = 1.35;

/// 地图上每条描边在它穿过的陆地上的地板。
const MAP_STROKE_LAND_FLOOR: f32 = 2.5;

/// 描边跨水时的地板（边界跨水，桥梁/堤道把道路带过水）。
const MAP_STROKE_OCEAN_FLOOR: f32 = 2.0;

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{
        MAP_FILL_FLOOR, MAP_STROKE_LAND_FLOOR, MAP_STROKE_OCEAN_FLOOR, contrast, resolve_roles,
    };
    use crate::theme::{appearance::Appearance, palette::Palette, resolved::Theme, role::Role};

    fn palettes() -> [Palette; 2] {
        [Palette::light(), Palette::dark()]
    }

    /// A1：两种外观下每条地板都成立。失败信息报出失败的那一对与它的地板。
    #[test]
    fn every_contrast_floor_holds_in_both_appearances() {
        for palette in palettes() {
            let roles = resolve_roles(&palette);

            for (foreground, background, floor) in roles.contrast_floors() {
                let ratio = contrast(roles.get(foreground), roles.get(background));

                assert!(
                    ratio >= floor,
                    "{:?}: {} on {} reads {ratio:.2}:1, where its floor is {floor:.2}:1",
                    palette.appearance,
                    foreground.name(),
                    background.name()
                );
            }
        }
    }

    /// 地板表本身：58 条、每组都有覆盖。截断/改序会被这条挡住。
    #[test]
    fn the_contrast_floor_table_covers_fifty_eight_pairs() {
        use std::collections::HashSet;

        let roles = resolve_roles(&Palette::light());
        let floors = roles.contrast_floors();
        assert_eq!(floors.len(), 58, "旧实现的 58 条对比度地板必须逐条保留");

        let foregrounds: HashSet<_> = floors
            .iter()
            .map(|(foreground, _, _)| *foreground)
            .collect();
        let backgrounds: HashSet<_> = floors
            .iter()
            .map(|(_, background, _)| *background)
            .collect();
        // 五个状态色、六个图表系列、行情与品牌这些「必须被读」的前景都在表里。
        for role in [
            Role::StatusDangerBase,
            Role::StatusWarningBase,
            Role::StatusSuccessBase,
            Role::StatusInfoBase,
            Role::StatusElevatedBase,
            Role::ChartSeries0,
            Role::ChartSeries1,
            Role::ChartSeries2,
            Role::ChartSeries3,
            Role::ChartSeries4,
            Role::ChartSeries5,
            Role::MarketUp,
            Role::MarketDown,
            Role::BrandBase,
            Role::FgDefault,
            Role::FgMuted,
        ] {
            assert!(
                foregrounds.contains(&role),
                "{} 必须出现在地板前景里",
                role.name()
            );
        }
        for role in [
            Role::SurfaceCanvas,
            Role::SurfaceRaised,
            Role::SurfaceSunken,
            Role::SurfaceOverlay,
            Role::SurfaceControl,
            Role::SurfaceInverse,
        ] {
            assert!(
                backgrounds.contains(&role),
                "{} 必须出现在地板背景里",
                role.name()
            );
        }
        assert!(backgrounds.contains(&Role::ChartMapLand));
        assert!(backgrounds.contains(&Role::ChartMapOcean));
    }

    /// debug 期检查的守护：地板不达标时**加载主题就会失败**。
    ///
    /// `assert_contrast_floors` 是 `debug_assert!`，它的失败会 panic；如果实现被删空（或
    /// `resolve_roles` 不再调用它），本测试会红——这正是审查 F3-1 的覆盖缺口。
    #[test]
    #[cfg(debug_assertions)]
    fn a_palette_that_breaks_a_floor_is_rejected_when_the_theme_loads() {
        let mut broken = Palette::light();
        // 正文提到近白：`fg.default on surface.canvas >= 12.0` 不可能成立。
        broken.text = ui_core::paint::color::Color::oklch(0.9, 0.0, 0.0, 1.0);

        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| resolve_roles(&broken)));
        assert!(
            result.is_err(),
            "debug 构建必须在 `resolve_roles` 里断言对比度地板（空实现 = 审查 F3-1 回归）"
        );
    }

    /// A2：两种外观里表面都按 `raised → canvas → sunken` 堆叠（旧灰阶在一个表里向下一步、在另一
    /// 个表里向上一步，正是这条要挡住的）。
    #[test]
    fn surfaces_stack_raised_above_canvas_above_sunken() {
        for palette in palettes() {
            let roles = resolve_roles(&palette);
            let lightness = |role| crate::theme::color::oklch_components(roles.get(role))[0];
            let raised = lightness(Role::SurfaceRaised);
            let canvas = lightness(Role::SurfaceCanvas);
            let sunken = lightness(Role::SurfaceSunken);

            assert!(
                raised > canvas,
                "{:?}: 面板头 L {raised:.3} 不高于页面 L {canvas:.3}",
                palette.appearance
            );
            assert!(
                canvas > sunken,
                "{:?}: 页面 L {canvas:.3} 不低于内容面 L {sunken:.3}",
                palette.appearance
            );
        }
    }

    /// A3：品牌色的彩度是正文的好几倍，才读得出"这是颜色"而不是"这是粗体正文"。
    #[test]
    fn brand_is_three_times_more_chromatic_than_text() {
        for palette in palettes() {
            let roles = resolve_roles(&palette);
            let chroma = |role| crate::theme::color::oklch_components(roles.get(role))[1];
            let brand = chroma(Role::BrandBase);
            let text = chroma(Role::FgDefault);

            assert!(
                brand >= 3.0 * text,
                "{:?}: 品牌色 C {brand:.3} 不到正文 C {text:.3} 的三倍",
                palette.appearance
            );
        }
    }

    /// A4：六个图表系列要能靠色相之外的东西区分（Oklab 距离 ≥ 0.13）。
    #[test]
    fn chart_series_are_told_apart_by_oklab_distance() {
        let mut compared = 0;

        for palette in palettes() {
            let roles = resolve_roles(&palette);
            let series: Vec<_> = Role::ALL
                .into_iter()
                .filter(|role| {
                    matches!(
                        role,
                        Role::ChartSeries0
                            | Role::ChartSeries1
                            | Role::ChartSeries2
                            | Role::ChartSeries3
                            | Role::ChartSeries4
                            | Role::ChartSeries5
                    )
                })
                .collect();

            assert_eq!(series.len(), 6);

            for (first, first_role) in series.iter().enumerate() {
                for second_role in series.iter().skip(first + 1) {
                    let first = crate::theme::color::oklab(roles.get(*first_role));
                    let second = crate::theme::color::oklab(roles.get(*second_role));
                    let distance = ((first[0] - second[0]).powi(2)
                        + (first[1] - second[1]).powi(2)
                        + (first[2] - second[2]).powi(2))
                    .sqrt();

                    compared += 1;
                    assert!(
                        distance >= 0.13,
                        "{:?}: {} 与 {} 在 Oklab 里只差 {distance:.3}",
                        palette.appearance,
                        first_role.name(),
                        second_role.name()
                    );
                }
            }
        }

        assert_eq!(compared, 30, "两种外观各比较 15 对");
    }

    /// 六个系列在内容面上都读得清。
    #[test]
    fn every_chart_series_reads_on_the_sunken_surface() {
        for palette in palettes() {
            let roles = resolve_roles(&palette);

            for series in [
                Role::ChartSeries0,
                Role::ChartSeries1,
                Role::ChartSeries2,
                Role::ChartSeries3,
                Role::ChartSeries4,
                Role::ChartSeries5,
            ] {
                let ratio = contrast(roles.get(series), roles.get(Role::SurfaceSunken));

                assert!(
                    ratio >= 3.0,
                    "{:?}: {} reads {ratio:.2}:1 on surface.sunken",
                    palette.appearance,
                    series.name()
                );
            }
        }
    }

    /// 恐慌贪婪五档靠 Oklab 距离区分；深色外观的两端在内容面上读得清。
    #[test]
    fn the_fear_greed_scale_is_told_apart_and_reads_on_the_dark_face() {
        let scale = [
            Role::FearGreedExtremeFear,
            Role::FearGreedFear,
            Role::FearGreedNeutral,
            Role::FearGreedGreed,
            Role::FearGreedExtremeGreed,
        ];

        for palette in palettes() {
            let roles = resolve_roles(&palette);

            for (first, first_role) in scale.iter().enumerate() {
                for second_role in scale.iter().skip(first + 1) {
                    let first = crate::theme::color::oklab(roles.get(*first_role));
                    let second = crate::theme::color::oklab(roles.get(*second_role));
                    let distance = ((first[0] - second[0]).powi(2)
                        + (first[1] - second[1]).powi(2)
                        + (first[2] - second[2]).powi(2))
                    .sqrt();

                    assert!(
                        distance >= 0.13,
                        "{:?}: {} 与 {} 在 Oklab 里只差 {distance:.3}",
                        palette.appearance,
                        first_role.name(),
                        second_role.name()
                    );
                }
            }
        }

        let dark = resolve_roles(&Palette::dark());
        for role in scale {
            let ratio = contrast(dark.get(role), dark.get(Role::SurfaceSunken));
            assert!(
                ratio >= 3.0,
                "深色外观：{} 在内容面上只读 {ratio:.2}:1",
                role.name()
            );
        }
    }

    /// A5：只有必须透视的角色才是半透明的（叠洗、遮罩、行情淡洗）。
    #[test]
    fn only_the_veils_the_scrim_and_the_weak_market_colors_are_translucent() {
        for palette in palettes() {
            let roles = resolve_roles(&palette);

            for (role, color) in roles.iter() {
                let alpha = crate::theme::color::oklch_components(color)[3];
                let translucent = matches!(
                    role,
                    Role::VeilHover
                        | Role::VeilActive
                        | Role::Scrim
                        | Role::MarketUpWeak
                        | Role::MarketDownWeak
                );

                if translucent {
                    assert!(
                        alpha > 0.0 && alpha < 1.0,
                        "{:?}: {} alpha {alpha:.2}，它必须可叠",
                        palette.appearance,
                        role.name()
                    );
                } else {
                    assert!(
                        (alpha - 1.0).abs() < 1e-6,
                        "{:?}: {} alpha {alpha:.2}，它必须不透明才能被读",
                        palette.appearance,
                        role.name()
                    );
                }
            }
        }
    }

    /// A6：地图两个填充在两种外观里都分得开（地图自己的地板，不是 §7.8 的表面阶梯）。
    #[test]
    fn map_land_and_map_ocean_stay_apart_in_both_appearances() {
        for palette in palettes() {
            let roles = resolve_roles(&palette);
            let ratio = contrast(
                roles.get(Role::ChartMapLand),
                roles.get(Role::ChartMapOcean),
            );

            assert!(
                ratio >= MAP_FILL_FLOOR,
                "{:?}: 海岸线只读 {ratio:.2}:1，地板 {MAP_FILL_FLOOR:.2}:1",
                palette.appearance
            );
        }
    }

    /// 三种地图描边在陆地与海面上都读得清。
    #[test]
    fn map_strokes_read_on_both_fills_in_both_appearances() {
        for palette in palettes() {
            let roles = resolve_roles(&palette);

            for stroke in [
                Role::ChartMapBorder,
                Role::ChartMapRoad,
                Role::ChartMapRoadMinor,
            ] {
                for (fill, floor) in [
                    (Role::ChartMapLand, MAP_STROKE_LAND_FLOOR),
                    (Role::ChartMapOcean, MAP_STROKE_OCEAN_FLOOR),
                ] {
                    let ratio = contrast(roles.get(stroke), roles.get(fill));

                    assert!(
                        ratio >= floor,
                        "{:?}: {} on {} reads {ratio:.2}:1, where the map floor is {floor:.2}:1",
                        palette.appearance,
                        stroke.name(),
                        fill.name()
                    );
                }
            }
        }
    }

    /// 旧测试的移植：浅色外观画 CMC 自己的五个颜色，深色外观只抬两端。
    ///
    /// 这里比较的是**作者写下的调色板色值**（硬编码 golden），不是旧实现的渲染值：恐慌贪婪五色
    /// 实测都在 sRGB 色域内（旧/新 8 位差 0），因此本测试与 gamut mapping 无关；越界色的引擎映射
    /// 基线在 `resolved.rs` 的 8 组 golden 里（`spec-deltas/theme.md` §3）。
    #[test]
    fn the_fear_greed_scale_paints_its_authored_colors() {
        let cases: [(Appearance, Role, [u8; 3]); 8] = [
            (
                Appearance::Light,
                Role::FearGreedExtremeFear,
                [0xB6, 0x14, 0x29],
            ),
            (Appearance::Light, Role::FearGreedFear, [0xF6, 0x46, 0x5D]),
            (
                Appearance::Light,
                Role::FearGreedNeutral,
                [0xF8, 0xD3, 0x3A],
            ),
            (Appearance::Light, Role::FearGreedGreed, [0x2F, 0xE4, 0x9F]),
            (
                Appearance::Light,
                Role::FearGreedExtremeGreed,
                [0x27, 0xA1, 0x72],
            ),
            (
                Appearance::Dark,
                Role::FearGreedExtremeFear,
                [0xCC, 0x32, 0x3C],
            ),
            (Appearance::Dark, Role::FearGreedFear, [0xFA, 0x67, 0x73]),
            (
                Appearance::Dark,
                Role::FearGreedExtremeGreed,
                [0x3C, 0xB0, 0x80],
            ),
        ];

        for (appearance, role, expected) in cases {
            let theme = Theme::of(appearance);
            let actual = crate::theme::color::to_srgb8(theme.role(role));

            for channel in 0..3 {
                let difference = i32::from(actual[channel]) - i32::from(expected[channel]);
                assert!(
                    difference.abs() <= 2,
                    "{appearance:?} {} 通道 {channel}: 引擎 {} vs 旧实现 {}",
                    role.name(),
                    actual[channel],
                    expected[channel]
                );
            }
        }
    }

    /// 每个角色都能被解析（无缺失、无 panic）；除恐慌贪婪刻度中间两档（两套外观刻意同值）外，
    /// 其余角色在深浅外观下都换一个颜色（切换是可见的）。
    #[test]
    fn every_role_resolves_in_both_appearances() {
        let light = resolve_roles(&Palette::light());
        let dark = resolve_roles(&Palette::dark());
        // 旧 `Palette::dark` 的注释：刻度中间三档"已经读得清"，只抬升两端；因此 neutral 与 greed
        // 在两套外观里是同一个颜色（这是数据决定，不是遗漏）。
        let shared_by_design = [Role::FearGreedNeutral, Role::FearGreedGreed];
        let mut seen = HashSet::with_capacity(Role::COUNT);
        let mut differing = 0;

        for role in Role::ALL {
            assert!(seen.insert(role.index()));
            let light_color = light.get(role);
            let dark_color = dark.get(role);
            assert!(
                matches!(light_color, ui_core::paint::color::Color::Oklch { .. }),
                "{} 必须是 OKLCH 分量",
                role.name()
            );
            let differs = light_color != dark_color;
            if shared_by_design.contains(&role) {
                assert!(
                    !differs,
                    "{} 是两套外观共享的刻度中间档，不应换色",
                    role.name()
                );
            } else {
                assert!(differs, "{} 在两种外观下必须换一个颜色", role.name());
                differing += 1;
            }
        }

        assert_eq!(seen.len(), Role::COUNT);
        assert_eq!(differing, Role::COUNT - shared_by_design.len());
    }
}
