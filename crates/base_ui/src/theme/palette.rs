//! 主题被手写的一侧：每种外观一套调色板。
//!
//! 调色板是整个 crate 里**唯一**写死原始颜色的地方：组件画的每个颜色都是某个角色从这些值派生出来
//! 的（[`crate::theme::roles`]），所以换主题 = 换这份数据，没有绘制代码跟着动。两套内置调色板是
//! 同一个形状的两个实例：只在明度上不同，从不在语义上不同。
//!
//! 数据与旧 `style/palette.rs` **逐个数值一致**（OKLCH 分量），测试逐字段对照。颜色一律是引擎
//! [`Color`]（`Oklch` 变体），不引入 UI 框架类型。

use ui_core::paint::color::Color;

use super::appearance::Appearance;

/// 调色板手写的五个状态色。
///
/// 与行情色刻意分开：涨不是成功、跌不是错误，改一个不会动另一个。阶梯顺序是
/// `success → info → warning → elevated → danger`。
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct StatusPalette {
    /// 错误、失败操作、破坏性控件。
    pub danger: Color,
    /// 连接降级、最暖的一档。
    pub warning: Color,
    /// 保存完成、连接健康。
    pub success: Color,
    /// 中性通知，以及正常之上偏冷的一档。
    pub info: Color,
    /// 越过 `warning`、未到 `danger` 的一档。
    pub elevated: Color,
}

/// 调色板手写的两个行情方向。
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct MarketPalette {
    /// 上涨价格、正收益、买盘、阳线。
    pub up: Color,
    /// 下跌价格、负收益、卖盘、阴线。
    pub down: Color,
}

/// 恐慌贪婪刻度的五个颜色，从恐惧到贪婪。
///
/// 它既不是状态阶梯也不是一对行情方向：它是一个读数，五个档要在并排时被区分开。浅色外观是 CMC
/// 指数自己的五个颜色；深色外观保留中间三个、只抬升两端。
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct FearGreedPalette {
    /// 最恐惧。
    pub extreme_fear: Color,
    /// 底部上一阶。
    pub fear: Color,
    /// 中间。
    pub neutral: Color,
    /// 顶部下一阶。
    pub greed: Color,
    /// 最贪婪。
    pub extreme_greed: Color,
}

/// 一种外观，按主题作者手写的方式给出。
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct Palette {
    /// 这些值属于哪种外观。
    pub appearance: Appearance,
    /// 舞台与面板地板（同一色）。
    pub canvas: Color,
    /// 面板头、表头、工具栏。
    pub surface_raised: Color,
    /// 控件内容面（图表地板、表格地板）。
    pub surface_sunken: Color,
    /// 菜单、对话框、下拉、浮层。
    pub overlay: Color,
    /// 输入框、软按钮、下拉触发器的填充。
    pub control: Color,
    /// 正文文字，也是其它文字阶梯与线混出来的源色。
    pub text: Color,
    /// 链接、选中、开启的开关、进度条、焦点环。
    pub brand: Color,
    /// 五个状态色。
    pub status: StatusPalette,
    /// 两个行情方向。
    pub market: MarketPalette,
    /// 恐慌贪婪刻度的五个颜色。
    pub fear_greed: FearGreedPalette,
    /// 多线/多柱图表的六个系列，按图表给新线分配的顺序。
    pub series: [Color; 6],
    /// 地图的海面：陆地下面的水。
    pub map_ocean: Color,
    /// 地图的陆地填充，与海面差一个可见步。
    pub map_land: Color,
    /// 画在陆地与海面之上的陆地边界。
    pub map_border: Color,
    /// 高速与干道：地图两种道路描边里较宽的一种。
    pub map_road: Color,
    /// 主干道：较细的一种。
    pub map_road_minor: Color,
    /// 饱和填充（`brand`、`status.danger`）上的文字。
    pub on_brand: Color,
    /// 反色块（tooltip、纯色中性徽标）上的文字。
    pub on_inverse: Color,
    /// 模态背后压在整个窗口上的黑的 alpha。
    pub scrim_alpha: f32,
    /// 指针所在控件上的中性洗色的 alpha。
    pub veil_hover: f32,
    /// 按下中控件上的中性洗色的 alpha。
    pub veil_active: f32,
}

impl Palette {
    /// 浅色外观。
    ///
    /// 舞台不是纯白：面板头要能在页面上方一步、控件内容面要在下方一步，两者都要有地方可站。表面
    /// 无彩度（读作纸而不是带色的灰），品牌色是饱和的蓝而不是过去实心按钮的黑——黑色按钮挨着黑色
    /// 正文说不出"是这一个"。
    pub fn light() -> Self {
        let opacity = 1.0;

        Self {
            appearance: Appearance::Light,
            canvas: Color::oklch(0.978, 0.0, 0.0, opacity),
            surface_raised: Color::oklch(1.0, 0.0, 0.0, opacity),
            surface_sunken: Color::oklch(0.948, 0.0, 0.0, opacity),
            overlay: Color::oklch(1.0, 0.0, 0.0, opacity),
            control: Color::oklch(1.0, 0.0, 0.0, opacity),
            text: Color::oklch(0.245, 0.028, 278.0, opacity),
            brand: Color::oklch(0.526, 0.152, 249.5, opacity),
            status: StatusPalette {
                danger: Color::oklch(0.532, 0.190, 26.0, opacity),
                warning: Color::oklch(0.545, 0.116, 76.0, opacity),
                success: Color::oklch(0.520, 0.124, 154.0, opacity),
                info: Color::oklch(0.530, 0.104, 232.0, opacity),
                elevated: Color::oklch(0.545, 0.148, 45.0, opacity),
            },
            market: MarketPalette {
                up: Color::oklch(0.514, 0.128, 152.0, opacity),
                down: Color::oklch(0.542, 0.180, 22.0, opacity),
            },
            // CMC 恐慌贪婪指数自己的五个颜色：深红、红、黄、薄荷、深绿。
            fear_greed: FearGreedPalette {
                extreme_fear: Color::oklch(0.4958, 0.1905, 22.83, opacity),
                fear: Color::oklch(0.6529, 0.2110, 18.20, opacity),
                neutral: Color::oklch(0.8724, 0.1636, 94.59, opacity),
                greed: Color::oklch(0.8148, 0.1705, 161.41, opacity),
                extreme_greed: Color::oklch(0.6306, 0.1263, 162.21, opacity),
            },
            series: [
                Color::oklch(0.620, 0.140, 62.0, opacity),
                Color::oklch(0.500, 0.150, 276.0, opacity),
                Color::oklch(0.560, 0.130, 194.0, opacity),
                Color::oklch(0.560, 0.170, 350.0, opacity),
                Color::oklch(0.540, 0.130, 120.0, opacity),
                Color::oklch(0.500, 0.010, 0.0, opacity),
            ],
            // 地图是 OSM Carto 交通层自己的调色板：纸陆地 #f2efe9、水 #aad3df。两者读作 1.40:1，
            // 低于设计要的 1.5:1 海岸线地板——地板记录的是这对颜色实际测到的值，不是要求它变成
            // 别的颜色（详见 `roles.rs` 的对比度表注释）。
            map_ocean: Color::oklch(0.841, 0.046, 217.5, opacity),
            map_land: Color::oklch(0.953, 0.009, 84.6, opacity),
            map_border: Color::oklch(0.647, 0.133, 311.1, opacity),
            map_road: Color::oklch(0.591, 0.213, 7.2, opacity),
            map_road_minor: Color::oklch(0.570, 0.120, 74.9, opacity),
            on_brand: Color::oklch(0.978, 0.0, 0.0, opacity),
            on_inverse: Color::oklch(0.978, 0.0, 0.0, opacity),
            scrim_alpha: 0.42,
            veil_hover: 0.06,
            veil_active: 0.10,
        }
    }

    /// 深色外观。
    ///
    /// 表面在浅色外观里向下阶梯，这里向上：面板头比页面亮、内容面比页面暗，因为这就是应用对它们的
    /// 含义。深色基底带一点蓝（文字色的 hue、品牌色约十分之一的彩度），灰读起来是一家人而不是彩色
    /// 界面上的中性窟窿。状态色比浅色外观更亮：在近黑页面上它们先被读作文本、其次才是填充。
    pub fn dark() -> Self {
        let opacity = 1.0;

        Self {
            appearance: Appearance::Dark,
            canvas: Color::oklch(0.215, 0.016, 252.4, opacity),
            surface_raised: Color::oklch(0.262, 0.016, 252.4, opacity),
            surface_sunken: Color::oklch(0.176, 0.016, 252.4, opacity),
            overlay: Color::oklch(0.278, 0.016, 252.4, opacity),
            control: Color::oklch(0.245, 0.016, 252.4, opacity),
            text: Color::oklch(0.955, 0.012, 255.0, opacity),
            brand: Color::oklch(0.722, 0.130, 242.0, opacity),
            status: StatusPalette {
                danger: Color::oklch(0.708, 0.170, 20.0, opacity),
                warning: Color::oklch(0.822, 0.150, 84.0, opacity),
                success: Color::oklch(0.762, 0.150, 160.0, opacity),
                info: Color::oklch(0.752, 0.118, 226.0, opacity),
                elevated: Color::oklch(0.760, 0.150, 52.0, opacity),
            },
            market: MarketPalette {
                up: Color::oklch(0.782, 0.158, 150.0, opacity),
                down: Color::oklch(0.702, 0.186, 20.0, opacity),
            },
            // 同一刻度在深色页面上：中间三个本就读得清，只有两端移动。
            fear_greed: FearGreedPalette {
                extreme_fear: Color::oklch(0.5600, 0.1900, 22.83, opacity),
                fear: Color::oklch(0.7000, 0.1800, 18.20, opacity),
                neutral: Color::oklch(0.8724, 0.1636, 94.59, opacity),
                greed: Color::oklch(0.8148, 0.1705, 161.41, opacity),
                extreme_greed: Color::oklch(0.6800, 0.1263, 162.21, opacity),
            },
            series: [
                Color::oklch(0.800, 0.150, 78.0, opacity),
                Color::oklch(0.720, 0.130, 282.0, opacity),
                Color::oklch(0.780, 0.120, 196.0, opacity),
                Color::oklch(0.740, 0.170, 350.0, opacity),
                Color::oklch(0.700, 0.140, 120.0, opacity),
                Color::oklch(0.660, 0.010, 0.0, opacity),
            ],
            // 中性夜地图：近黑的海，以及仍比它高一步的最暗陆地；道路也是灰，基底上仅剩的颜色是
            // 边界的紫（并排时靠它和虚线区分边界与道路）。
            map_ocean: Color::oklch(0.150, 0.000, 0.0, opacity),
            map_land: Color::oklch(0.283, 0.000, 0.0, opacity),
            map_border: Color::oklch(0.813, 0.090, 311.0, opacity),
            map_road: Color::oklch(0.633, 0.000, 0.0, opacity),
            map_road_minor: Color::oklch(0.530, 0.000, 0.0, opacity),
            on_brand: Color::oklch(0.176, 0.020, 245.0, opacity),
            on_inverse: Color::oklch(0.176, 0.016, 252.4, opacity),
            scrim_alpha: 0.55,
            veil_hover: 0.08,
            veil_active: 0.14,
        }
    }

    /// 某个外观的调色板。
    pub fn of(appearance: Appearance) -> Self {
        match appearance {
            Appearance::Light => Self::light(),
            Appearance::Dark => Self::dark(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Appearance, Palette};
    use crate::theme::color::oklch_components;

    /// 旧实现（`style/palette.rs`）的 Light 调色板 OKLCH 值（P5-2 在旧模块删除前逐位冻结）。
    const LIGHT_LEGACY: [(&str, [f32; 4]); 35] = [
        ("canvas", [0.978, 0.0, 0.0, 1.0]),
        ("surface_raised", [1.0, 0.0, 0.0, 1.0]),
        ("surface_sunken", [0.948, 0.0, 0.0, 1.0]),
        ("overlay", [1.0, 0.0, 0.0, 1.0]),
        ("control", [1.0, 0.0, 0.0, 1.0]),
        ("text", [0.245, 0.028, 278.0, 1.0]),
        ("brand", [0.526, 0.152, 249.5, 1.0]),
        ("status.danger", [0.532, 0.19, 26.0, 1.0]),
        ("status.warning", [0.545, 0.116, 76.0, 1.0]),
        ("status.success", [0.52, 0.124, 154.0, 1.0]),
        ("status.info", [0.53, 0.104, 232.0, 1.0]),
        ("status.elevated", [0.545, 0.148, 45.0, 1.0]),
        ("market.up", [0.514, 0.128, 152.0, 1.0]),
        ("market.down", [0.542, 0.18, 22.0, 1.0]),
        ("fear_greed.extreme_fear", [0.4958, 0.1905, 22.83, 1.0]),
        ("fear_greed.fear", [0.6529, 0.211, 18.2, 1.0]),
        ("fear_greed.neutral", [0.8724, 0.1636, 94.59, 1.0]),
        ("fear_greed.greed", [0.8148, 0.1705, 161.41, 1.0]),
        ("fear_greed.extreme_greed", [0.6306, 0.1263, 162.21, 1.0]),
        ("series[0]", [0.62, 0.14, 62.0, 1.0]),
        ("series[1]", [0.5, 0.15, 276.0, 1.0]),
        ("series[2]", [0.56, 0.13, 194.0, 1.0]),
        ("series[3]", [0.56, 0.17, 350.0, 1.0]),
        ("series[4]", [0.54, 0.13, 120.0, 1.0]),
        ("series[5]", [0.5, 0.01, 0.0, 1.0]),
        ("map_ocean", [0.841, 0.046, 217.5, 1.0]),
        ("map_land", [0.953, 0.009, 84.6, 1.0]),
        ("map_border", [0.647, 0.133, 311.1, 1.0]),
        ("map_road", [0.591, 0.213, 7.2, 1.0]),
        ("map_road_minor", [0.57, 0.12, 74.9, 1.0]),
        ("on_brand", [0.978, 0.0, 0.0, 1.0]),
        ("on_inverse", [0.978, 0.0, 0.0, 1.0]),
        ("scrim_alpha", [0.42, 0.0, 0.0, 0.0]),
        ("veil_hover", [0.06, 0.0, 0.0, 0.0]),
        ("veil_active", [0.1, 0.0, 0.0, 0.0]),
    ];

    /// 旧实现（`style/palette.rs`）的 Dark 调色板 OKLCH 值（P5-2 冻结）。
    const DARK_LEGACY: [(&str, [f32; 4]); 35] = [
        ("canvas", [0.215, 0.016, 252.4, 1.0]),
        ("surface_raised", [0.262, 0.016, 252.4, 1.0]),
        ("surface_sunken", [0.176, 0.016, 252.4, 1.0]),
        ("overlay", [0.278, 0.016, 252.4, 1.0]),
        ("control", [0.245, 0.016, 252.4, 1.0]),
        ("text", [0.955, 0.012, 255.0, 1.0]),
        ("brand", [0.722, 0.13, 242.0, 1.0]),
        ("status.danger", [0.708, 0.17, 20.0, 1.0]),
        ("status.warning", [0.822, 0.15, 84.0, 1.0]),
        ("status.success", [0.762, 0.15, 160.0, 1.0]),
        ("status.info", [0.752, 0.118, 226.0, 1.0]),
        ("status.elevated", [0.76, 0.15, 52.0, 1.0]),
        ("market.up", [0.782, 0.158, 150.0, 1.0]),
        ("market.down", [0.702, 0.186, 20.0, 1.0]),
        ("fear_greed.extreme_fear", [0.56, 0.19, 22.83, 1.0]),
        ("fear_greed.fear", [0.7, 0.18, 18.2, 1.0]),
        ("fear_greed.neutral", [0.8724, 0.1636, 94.59, 1.0]),
        ("fear_greed.greed", [0.8148, 0.1705, 161.41, 1.0]),
        ("fear_greed.extreme_greed", [0.68, 0.1263, 162.21, 1.0]),
        ("series[0]", [0.8, 0.15, 78.0, 1.0]),
        ("series[1]", [0.72, 0.13, 282.0, 1.0]),
        ("series[2]", [0.78, 0.12, 196.0, 1.0]),
        ("series[3]", [0.74, 0.17, 350.0, 1.0]),
        ("series[4]", [0.7, 0.14, 120.0, 1.0]),
        ("series[5]", [0.66, 0.01, 0.0, 1.0]),
        ("map_ocean", [0.15, 0.0, 0.0, 1.0]),
        ("map_land", [0.283, 0.0, 0.0, 1.0]),
        ("map_border", [0.813, 0.09, 311.0, 1.0]),
        ("map_road", [0.633, 0.0, 0.0, 1.0]),
        ("map_road_minor", [0.53, 0.0, 0.0, 1.0]),
        ("on_brand", [0.176, 0.02, 245.0, 1.0]),
        ("on_inverse", [0.176, 0.016, 252.4, 1.0]),
        ("scrim_alpha", [0.55, 0.0, 0.0, 0.0]),
        ("veil_hover", [0.08, 0.0, 0.0, 0.0]),
        ("veil_active", [0.14, 0.0, 0.0, 0.0]),
    ];

    /// 新调色板每个字段的 OKLCH 分量（顺序与 [`LIGHT_LEGACY`] / [`DARK_LEGACY`] 一致）。
    fn components(palette: &Palette) -> [(&'static str, [f32; 4]); 35] {
        [
            ("canvas", oklch_components(palette.canvas)),
            ("surface_raised", oklch_components(palette.surface_raised)),
            ("surface_sunken", oklch_components(palette.surface_sunken)),
            ("overlay", oklch_components(palette.overlay)),
            ("control", oklch_components(palette.control)),
            ("text", oklch_components(palette.text)),
            ("brand", oklch_components(palette.brand)),
            ("status.danger", oklch_components(palette.status.danger)),
            ("status.warning", oklch_components(palette.status.warning)),
            ("status.success", oklch_components(palette.status.success)),
            ("status.info", oklch_components(palette.status.info)),
            ("status.elevated", oklch_components(palette.status.elevated)),
            ("market.up", oklch_components(palette.market.up)),
            ("market.down", oklch_components(palette.market.down)),
            (
                "fear_greed.extreme_fear",
                oklch_components(palette.fear_greed.extreme_fear),
            ),
            ("fear_greed.fear", oklch_components(palette.fear_greed.fear)),
            (
                "fear_greed.neutral",
                oklch_components(palette.fear_greed.neutral),
            ),
            (
                "fear_greed.greed",
                oklch_components(palette.fear_greed.greed),
            ),
            (
                "fear_greed.extreme_greed",
                oklch_components(palette.fear_greed.extreme_greed),
            ),
            ("series[0]", oklch_components(palette.series[0])),
            ("series[1]", oklch_components(palette.series[1])),
            ("series[2]", oklch_components(palette.series[2])),
            ("series[3]", oklch_components(palette.series[3])),
            ("series[4]", oklch_components(palette.series[4])),
            ("series[5]", oklch_components(palette.series[5])),
            ("map_ocean", oklch_components(palette.map_ocean)),
            ("map_land", oklch_components(palette.map_land)),
            ("map_border", oklch_components(palette.map_border)),
            ("map_road", oklch_components(palette.map_road)),
            ("map_road_minor", oklch_components(palette.map_road_minor)),
            ("on_brand", oklch_components(palette.on_brand)),
            ("on_inverse", oklch_components(palette.on_inverse)),
            ("scrim_alpha", [palette.scrim_alpha, 0.0, 0.0, 0.0]),
            ("veil_hover", [palette.veil_hover, 0.0, 0.0, 0.0]),
            ("veil_active", [palette.veil_active, 0.0, 0.0, 0.0]),
        ]
    }

    /// 新调色板的每个字段与旧实现的冻结 OKLCH 数值一致（数据移植的逐位证据）。
    ///
    /// 旧模块已在 P5-2 删除，期望值冻结在 [`LIGHT_LEGACY`] / [`DARK_LEGACY`]（见
    /// `test-ledger/p5-2.md` §2）：任何调色板数值改动都会红。
    #[test]
    fn every_palette_value_matches_the_frozen_legacy_table() {
        for (appearance, legacy) in [
            (Appearance::Light, LIGHT_LEGACY),
            (Appearance::Dark, DARK_LEGACY),
        ] {
            let new = Palette::of(appearance);
            for ((legacy_name, old), (name, new)) in legacy.into_iter().zip(components(&new)) {
                assert_eq!(legacy_name, name, "{appearance:?} 的字段顺序漂移");
                for index in 0..4 {
                    assert!(
                        (old[index] - new[index]).abs() < 1e-6,
                        "{appearance:?} {name} 分量 {index}: 旧 {} vs 新 {}",
                        old[index],
                        new[index]
                    );
                }
            }
        }
    }

    #[test]
    fn both_appearances_have_the_same_shape_and_their_own_values() {
        let light = Palette::light();
        let dark = Palette::dark();

        assert_eq!(light.appearance, Appearance::Light);
        assert_eq!(dark.appearance, Appearance::Dark);
        assert_ne!(
            oklch_components(light.canvas),
            oklch_components(dark.canvas)
        );
        assert_ne!(light.scrim_alpha, dark.scrim_alpha);
        assert_eq!(light.series.len(), 6);
    }
}
