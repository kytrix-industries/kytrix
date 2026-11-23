//! 语义色角色：组件只报"这是画布/次要文字/分隔线"，不报"这是第几阶灰"。
//!
//! 这是旧 `style/roles.rs` 角色集合的**完整移植**（64 个角色，名称与语义逐个保留）：旧实现把角色
//! 分成 `Surfaces`/`Lines`/`Foregrounds`/... 等结构体，是因为旧实现逐帧读一个字段；引擎
//! 的依赖追踪要求角色是**可枚举、可作 Signal 依赖粒度**的键，因此这里摊平成扁平枚举
//! （`SurfaceCanvas`、`FgMuted`、`ChartSeries0`…）。分组只保留在文档里：同一前缀就是一个组，
//! [`Role::name`] 返回旧实现的点号路径（`surface.canvas`、`fg.muted`、`chart.series[0]`），
//! 主题审计与对比度失败信息沿用它。
//!
//! 两个旧规则原样保留：
//!
//! - **堆叠与外观无关**：`surface.raised` 永远亮于 `surface.canvas`，`canvas` 永远亮于
//!   `surface.sunken`（两种外观都读作 `raised → canvas → sunken`）；
//! - **阶梯方向跟随外观**：品牌色上的 hover 在浅色外观变暗、深色外观变亮。

/// 一个语义色角色。
///
/// 变体顺序就是 [`Role::ALL`] 的顺序，也是 `RoleColors` 数组的下标（`#[repr(usize)]`）；新增角色
/// 必须同步 [`Role::ALL`]、[`Role::name`] 与 `roles` 模块里的解析，测试会逐一覆盖。
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
#[repr(usize)]
pub enum Role {
    // ── 表面（z 轴，由远及近）────────────────────────────────────────────
    /// 舞台与面板地板（同一色）。
    SurfaceCanvas,
    /// 面板头、表头、工具栏：页面上方一步。
    SurfaceRaised,
    /// 控件的内容面（图表地板、表格地板）：页面下方一步。
    SurfaceSunken,
    /// 菜单、对话框、下拉、浮层：在最上方，另加阴影。
    SurfaceOverlay,
    /// 输入框、软按钮、下拉触发器的填充。
    SurfaceControl,
    /// 反色块（tooltip、纯色中性徽标）：另一种外观会画成的那个实心块。
    SurfaceInverse,

    // ── 线（每种边缘一档强度）────────────────────────────────────────────
    /// 分隔线、表格线、面板边框、非交互边缘。
    LineDivider,
    /// 控件边界（输入框、描边按钮）。
    LineBorder,
    /// 指针所在控件的边缘、图表十字线。
    LineStrong,
    /// 键盘焦点环。
    LineFocus,

    // ── 前景（读者要能区分的文字阶梯）────────────────────────────────────
    /// 正文、数字、标题。
    FgDefault,
    /// 次要文字：时间戳、单位、表头、副标题。
    FgMuted,
    /// 占位符、菜单分组标题、图表轴标签。
    FgSubtle,
    /// 禁用文字。
    FgDisabled,
    /// 饱和填充（`brand`、`status.danger`）上的文字。
    FgOnBrand,
    /// 反色块上的文字。
    FgOnInverse,

    // ── 品牌色与它的两阶 ─────────────────────────────────────────────────
    /// 链接、选中、开启的开关、进度条、焦点环、拖拽把手。
    BrandBase,
    /// 指针下的品牌色元素。
    BrandHover,
    /// 按下中的品牌色元素。
    BrandActive,

    // ── 状态色（每个状态：本色 + chip 填充 + chip 边缘）──────────────────
    /// 错误、失败操作、破坏性控件。
    StatusDangerBase,
    /// 危险 chip 的填充。
    StatusDangerSurface,
    /// 危险 chip 的边缘。
    StatusDangerBorder,
    /// 连接降级、接近阈值。
    StatusWarningBase,
    /// 警告 chip 的填充。
    StatusWarningSurface,
    /// 警告 chip 的边缘。
    StatusWarningBorder,
    /// 保存完成、连接健康。
    StatusSuccessBase,
    /// 成功 chip 的填充。
    StatusSuccessSurface,
    /// 成功 chip 的边缘。
    StatusSuccessBorder,
    /// 中性通知。
    StatusInfoBase,
    /// 信息 chip 的填充。
    StatusInfoSurface,
    /// 信息 chip 的边缘。
    StatusInfoBorder,
    /// 越过 `warning`、未到 `danger` 的一档。
    StatusElevatedBase,
    /// elevated chip 的填充。
    StatusElevatedSurface,
    /// elevated chip 的边缘。
    StatusElevatedBorder,

    // ── 行情色（与状态色刻意分离：涨不是成功，跌不是错误）────────────────
    /// 上涨价格、正收益、买盘、阳线。
    MarketUp,
    /// 下跌价格、负收益、卖盘、阴线。
    MarketDown,
    /// 不变的价格。
    MarketFlat,
    /// 涨色 chip 的填充。
    MarketUpSurface,
    /// 跌色 chip 的填充。
    MarketDownSurface,
    /// 涨色的淡洗（深度条、热力图格子）。
    MarketUpWeak,
    /// 跌色的淡洗（深度条、热力图格子）。
    MarketDownWeak,

    // ── 恐慌贪婪（独立阶梯，不是一排状态色）──────────────────────────────
    /// 阶梯最底：最恐惧。
    FearGreedExtremeFear,
    /// 底部上一阶。
    FearGreedFear,
    /// 阶梯中间。
    FearGreedNeutral,
    /// 顶部下一阶。
    FearGreedGreed,
    /// 阶梯最顶：最贪婪。
    FearGreedExtremeGreed,

    // ── 图表家具 ────────────────────────────────────────────────────────
    /// 网格线与轴刻度（画在 `surface.sunken` 上）。
    ChartGrid,
    /// 轴标签与时间刻度。
    ChartAxis,
    /// 指针画出的十字线。
    ChartCrosshair,
    /// 地图的海面填充。
    ChartMapOcean,
    /// 地图的陆地填充。
    ChartMapLand,
    /// 地图上画的陆地边界。
    ChartMapBorder,
    /// 高速/干道（地图两种道路描边中较宽的一种）。
    ChartMapRoad,
    /// 主干道（较细的一种）。
    ChartMapRoadMinor,
    /// 多线/多柱图表的六个系列，按图表分配顺序。
    ChartSeries0,
    /// 系列 1。
    ChartSeries1,
    /// 系列 2。
    ChartSeries2,
    /// 系列 3。
    ChartSeries3,
    /// 系列 4。
    ChartSeries4,
    /// 系列 5。
    ChartSeries5,

    // ── 选择与叠层 ──────────────────────────────────────────────────────
    /// 输入框/可选文本画出的文本选择。
    Selection,
    /// 指针所在元素上的中性洗色。
    VeilHover,
    /// 按下中元素上的中性洗色。
    VeilActive,
    /// 模态背后压在窗口上的黑幕。
    Scrim,
}

impl Role {
    /// 角色总数（与 [`Role::ALL`] 等长；对比度地板、覆盖测试都按它循环）。
    pub const COUNT: usize = 64;

    /// 全部角色，按声明顺序。
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

    /// 数组下标（`RoleColors` 的存储键）。
    pub const fn index(self) -> usize {
        self as usize
    }

    /// 旧实现的点号角色名（`surface.canvas`、`chart.series[0]`…）：诊断与审计用。
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
