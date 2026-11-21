//! 界面被绘制成哪一种外观。
//!
//! 外观是设置层唯一决定的东西：调色板、角色与组件配方都从它读出来。名字说的是"应用被画成什么
//! 样"，不是"哪张表"——没有 schema，只有两套同形状的调色板。

/// 界面被绘制成哪一种外观。
#[derive(Debug, Default, Copy, Clone, Eq, PartialEq, Hash)]
pub enum Appearance {
    /// 浅色表面上的深色文字。
    #[default]
    Light,
    /// 深色表面上的浅色文字。
    Dark,
}
