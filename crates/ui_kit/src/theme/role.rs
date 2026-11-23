#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
#[repr(usize)]
pub enum Role {
    SurfaceCanvas,
    SurfaceRaised,
    SurfaceSunken,
    SurfaceOverlay,
    SurfaceControl,
    SurfaceInverse,

    LineDivider,
    LineBorder,
    LineStrong,
    LineFocus,

    FgDefault,
    FgMuted,
    FgSubtle,
    FgDisabled,
    FgOnBrand,
    FgOnInverse,

    BrandBase,
    BrandHover,
    BrandActive,

    StatusDangerBase,
    StatusDangerSurface,
    StatusDangerBorder,
    StatusWarningBase,
    StatusWarningSurface,
    StatusWarningBorder,
    StatusSuccessBase,
    StatusSuccessSurface,
    StatusSuccessBorder,
    StatusInfoBase,
    StatusInfoSurface,
    StatusInfoBorder,
    StatusElevatedBase,
    StatusElevatedSurface,
    StatusElevatedBorder,

    MarketUp,
    MarketDown,
    MarketFlat,
    MarketUpSurface,
    MarketDownSurface,
    MarketUpWeak,
    MarketDownWeak,

    FearGreedExtremeFear,
    FearGreedFear,
    FearGreedNeutral,
    FearGreedGreed,
    FearGreedExtremeGreed,

    ChartGrid,
    ChartAxis,
    ChartCrosshair,
    ChartMapOcean,
    ChartMapLand,
    ChartMapBorder,
    ChartMapRoad,
    ChartMapRoadMinor,
    ChartSeries0,
    ChartSeries1,
    ChartSeries2,
    ChartSeries3,
    ChartSeries4,
    ChartSeries5,

    Selection,
    VeilHover,
    VeilActive,
    Scrim,
}

impl Role {
    pub const COUNT: usize = 64;

    pub const ALL: [Role; Self::COUNT] = [
        Role::SurfaceCanvas,
        Role::SurfaceRaised,
        Role::SurfaceSunken,
        Role::SurfaceOverlay,
        Role::SurfaceControl,
        Role::SurfaceInverse,
        Role::LineDivider,
        Role::LineBorder,
        Role::LineStrong,
        Role::LineFocus,
        Role::FgDefault,
        Role::FgMuted,
        Role::FgSubtle,
        Role::FgDisabled,
        Role::FgOnBrand,
        Role::FgOnInverse,
        Role::BrandBase,
        Role::BrandHover,
        Role::BrandActive,
        Role::StatusDangerBase,
        Role::StatusDangerSurface,
        Role::StatusDangerBorder,
        Role::StatusWarningBase,
        Role::StatusWarningSurface,
        Role::StatusWarningBorder,
        Role::StatusSuccessBase,
        Role::StatusSuccessSurface,
        Role::StatusSuccessBorder,
        Role::StatusInfoBase,
        Role::StatusInfoSurface,
        Role::StatusInfoBorder,
        Role::StatusElevatedBase,
        Role::StatusElevatedSurface,
        Role::StatusElevatedBorder,
        Role::MarketUp,
        Role::MarketDown,
        Role::MarketFlat,
        Role::MarketUpSurface,
        Role::MarketDownSurface,
        Role::MarketUpWeak,
        Role::MarketDownWeak,
        Role::FearGreedExtremeFear,
        Role::FearGreedFear,
        Role::FearGreedNeutral,
        Role::FearGreedGreed,
        Role::FearGreedExtremeGreed,
        Role::ChartGrid,
        Role::ChartAxis,
        Role::ChartCrosshair,
        Role::ChartMapOcean,
        Role::ChartMapLand,
        Role::ChartMapBorder,
        Role::ChartMapRoad,
        Role::ChartMapRoadMinor,
        Role::ChartSeries0,
        Role::ChartSeries1,
        Role::ChartSeries2,
        Role::ChartSeries3,
        Role::ChartSeries4,
        Role::ChartSeries5,
        Role::Selection,
        Role::VeilHover,
        Role::VeilActive,
        Role::Scrim,
    ];

    pub const fn index(self) -> usize {
        self as usize
    }

    pub const fn name(self) -> &'static str {
        match self {
            Role::SurfaceCanvas => "surface.canvas",
            Role::SurfaceRaised => "surface.raised",
            Role::SurfaceSunken => "surface.sunken",
            Role::SurfaceOverlay => "surface.overlay",
            Role::SurfaceControl => "surface.control",
            Role::SurfaceInverse => "surface.inverse",
            Role::LineDivider => "line.divider",
            Role::LineBorder => "line.border",
            Role::LineStrong => "line.strong",
            Role::LineFocus => "line.focus",
            Role::FgDefault => "fg.default",
            Role::FgMuted => "fg.muted",
            Role::FgSubtle => "fg.subtle",
            Role::FgDisabled => "fg.disabled",
            Role::FgOnBrand => "fg.on_brand",
            Role::FgOnInverse => "fg.on_inverse",
            Role::BrandBase => "brand.base",
            Role::BrandHover => "brand.hover",
            Role::BrandActive => "brand.active",
            Role::StatusDangerBase => "status.danger.base",
            Role::StatusDangerSurface => "status.danger.surface",
            Role::StatusDangerBorder => "status.danger.border",
            Role::StatusWarningBase => "status.warning.base",
            Role::StatusWarningSurface => "status.warning.surface",
            Role::StatusWarningBorder => "status.warning.border",
            Role::StatusSuccessBase => "status.success.base",
            Role::StatusSuccessSurface => "status.success.surface",
            Role::StatusSuccessBorder => "status.success.border",
            Role::StatusInfoBase => "status.info.base",
            Role::StatusInfoSurface => "status.info.surface",
            Role::StatusInfoBorder => "status.info.border",
            Role::StatusElevatedBase => "status.elevated.base",
            Role::StatusElevatedSurface => "status.elevated.surface",
            Role::StatusElevatedBorder => "status.elevated.border",
            Role::MarketUp => "market.up",
            Role::MarketDown => "market.down",
            Role::MarketFlat => "market.flat",
            Role::MarketUpSurface => "market.up_surface",
            Role::MarketDownSurface => "market.down_surface",
            Role::MarketUpWeak => "market.up_weak",
            Role::MarketDownWeak => "market.down_weak",
            Role::FearGreedExtremeFear => "fear_greed.extreme_fear",
            Role::FearGreedFear => "fear_greed.fear",
            Role::FearGreedNeutral => "fear_greed.neutral",
            Role::FearGreedGreed => "fear_greed.greed",
            Role::FearGreedExtremeGreed => "fear_greed.extreme_greed",
            Role::ChartGrid => "chart.grid",
            Role::ChartAxis => "chart.axis",
            Role::ChartCrosshair => "chart.crosshair",
            Role::ChartMapOcean => "chart.map_ocean",
            Role::ChartMapLand => "chart.map_land",
            Role::ChartMapBorder => "chart.map_border",
            Role::ChartMapRoad => "chart.map_road",
            Role::ChartMapRoadMinor => "chart.map_road_minor",
            Role::ChartSeries0 => "chart.series[0]",
            Role::ChartSeries1 => "chart.series[1]",
            Role::ChartSeries2 => "chart.series[2]",
            Role::ChartSeries3 => "chart.series[3]",
            Role::ChartSeries4 => "chart.series[4]",
            Role::ChartSeries5 => "chart.series[5]",
            Role::Selection => "selection",
            Role::VeilHover => "veil.hover",
            Role::VeilActive => "veil.active",
            Role::Scrim => "scrim",
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::Role;

    #[test]
    fn the_role_set_is_the_full_sixty_four() {
        assert_eq!(Role::ALL.len(), Role::COUNT);
        assert_eq!(Role::COUNT, 64);
        assert_eq!(Role::Scrim.index(), Role::COUNT - 1);
    }

    #[test]
    fn every_role_has_a_unique_index_and_name() {
        let mut names = HashSet::with_capacity(Role::COUNT);

        for (position, role) in Role::ALL.into_iter().enumerate() {
            assert_eq!(role.index(), position, "{} 的下标", role.name());
            assert!(names.insert(role.name()), "{} 的名字重复", role.name());
        }

        assert_eq!(names.len(), Role::COUNT);
    }

    #[test]
    fn the_old_grouping_is_visible_in_the_names() {
        let names: HashSet<&str> = Role::ALL.into_iter().map(Role::name).collect();

        for expected in [
            "surface.canvas",
            "line.divider",
            "fg.muted",
            "brand.base",
            "status.elevated.border",
            "market.down_weak",
            "fear_greed.extreme_fear",
            "chart.series[5]",
            "veil.active",
            "scrim",
        ] {
            assert!(names.contains(expected), "缺少角色 {expected}");
        }
    }
}
