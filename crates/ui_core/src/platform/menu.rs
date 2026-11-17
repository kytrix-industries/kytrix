//! 原生主菜单的**平台无关模型**：菜单树、键盘等价物、AppKit 标准 selector、以及
//! 「selector → 引擎按键」的映射（R09-1）。
//!
//! macOS 的 `NSMenu` 由 [`crate::platform::macos::menu`] 按本模型构建，构建器里没有结构决策；
//! 模型本身是纯数据 + 纯函数，因此能在开发机（Windows）上用单测钉死 —— macOS 包装器只在
//! macOS 目标编译，测不到。
//!
//! 为什么需要主菜单（而不是让应用自己从原始按键重建快捷键）：AppKit 的键等价分发是
//! `NSApp.sendEvent:` → `[mainMenu performKeyEquivalent:]` → 失败才给 key window。`mainMenu`
//! 为 nil 时 `Cmd+Q`(`terminate:`)/`Cmd+W`(`performClose:`)/`Cmd+C`(`copy:`) 这些**根本没有承载
//! 对象**，只能退化成普通 `keyDown:`；同时 `AXMenuBar` 也拿不到任何命令（无障碍缺口）。

use crate::event::{KeyCode, Modifiers};

/// 菜单项键盘等价物的修饰键。
///
/// 必须**显式**表达「无修饰键」：`NSMenuItem` 的 `keyEquivalentModifierMask` 缺省值是 Command，
/// 因此没有快捷键的项要清零，不能靠「不设置」。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MenuModifiers {
    /// 无修饰键。
    None,
    /// Command（`⌘`）。
    Command,
    /// Command + Shift（重做）。
    CommandShift,
    /// Command + Option（隐藏其他）。
    CommandAlt,
    /// Command + Control（全屏）。
    CommandControl,
}

/// 一条主菜单命令（与一个 `NSMenuItem` 一一对应；分隔线见 [`MenuEntry::Separator`]）。
///
/// 命令身份 = **标准 selector**：动作沿 responder chain 找到真正的处理者
/// （`GuiView` / `NSWindow` / `NSApplication`），引擎不自造 target/action 协议。这条通路让菜单、
/// 快捷键、AppleScript/`AXMenuBar` 走同一份分发。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum MenuCommand {
    // ── App 菜单（`NSApplication` 实现，target = nil）──
    /// About。
    About,
    /// Hide（`Cmd+H`）。
    Hide,
    /// Hide Others（`Cmd+Option+H`）。
    HideOthers,
    /// Show All。
    ShowAll,
    /// Quit（`Cmd+Q`）。
    Quit,
    // ── Edit 菜单（`GuiView` 在文本编辑态实现，其余情况由 responder chain 继续上抛）──
    /// Undo。
    Undo,
    /// Redo。
    Redo,
    /// Cut。
    Cut,
    /// Copy。
    Copy,
    /// Paste。
    Paste,
    /// Delete。
    Delete,
    /// Select All。
    SelectAll,
    // ── Window 菜单（`NSWindow` 实现，target = nil）──
    /// Minimize（`Cmd+M`）。
    Minimize,
    /// Zoom。
    Zoom,
    /// Enter/Exit Full Screen（`Ctrl+Cmd+F`）。
    ToggleFullScreen,
    /// Close（`Cmd+W`）。
    Close,
}

/// 全部命令（`from_selector` 的查表来源；顺序无意义）。
const COMMANDS: [MenuCommand; 16] = [
    MenuCommand::About,
    MenuCommand::Hide,
    MenuCommand::HideOthers,
    MenuCommand::ShowAll,
    MenuCommand::Quit,
    MenuCommand::Undo,
    MenuCommand::Redo,
    MenuCommand::Cut,
    MenuCommand::Copy,
    MenuCommand::Paste,
    MenuCommand::Delete,
    MenuCommand::SelectAll,
    MenuCommand::Minimize,
    MenuCommand::Zoom,
    MenuCommand::ToggleFullScreen,
    MenuCommand::Close,
];

impl MenuCommand {
    /// 菜单项标题（规范英文；系统菜单的本地化由 AppKit 的 nib/menu 本地化机制负责，自建菜单
    /// 不翻译 —— 与 winit/GPUI 的默认菜单一致）。
    ///
    /// `About` 是唯一需要应用名的项，见 [`Self::display_title`]。
    pub(crate) const fn title(self) -> &'static str {
        match self {
            Self::About => "About",
            Self::Hide => "Hide",
            Self::HideOthers => "Hide Others",
            Self::ShowAll => "Show All",
            Self::Quit => "Quit",
            Self::Undo => "Undo",
            Self::Redo => "Redo",
            Self::Cut => "Cut",
            Self::Copy => "Copy",
            Self::Paste => "Paste",
            Self::Delete => "Delete",
            Self::SelectAll => "Select All",
            Self::Minimize => "Minimize",
            Self::Zoom => "Zoom",
            Self::ToggleFullScreen => "Enter Full Screen",
            Self::Close => "Close",
        }
    }

    /// 实际显示的标题：`About` 按 macOS 惯例拼上应用名，其余同 [`Self::title`]。
    pub(crate) fn display_title(self, app_name: &str) -> String {
        if matches!(self, Self::About) {
            format!("About {app_name}")
        } else {
            self.title().to_owned()
        }
    }

    /// AppKit 标准 selector（responder chain 上的动作名；`target = nil` ⇒ 沿响应链查找）。
    pub(crate) const fn selector(self) -> &'static str {
        match self {
            Self::About => "orderFrontStandardAboutPanel:",
            Self::Hide => "hide:",
            Self::HideOthers => "hideOtherApplications:",
            Self::ShowAll => "unhideAllApplications:",
            Self::Quit => "terminate:",
            Self::Undo => "undo:",
            Self::Redo => "redo:",
            Self::Cut => "cut:",
            Self::Copy => "copy:",
            Self::Paste => "paste:",
            Self::Delete => "delete:",
            Self::SelectAll => "selectAll:",
            Self::Minimize => "performMiniaturize:",
            Self::Zoom => "performZoom:",
            Self::ToggleFullScreen => "toggleFullScreen:",
            Self::Close => "performClose:",
        }
    }

    /// 键盘等价物的小写字符（`""` = 没有快捷键）。
    pub(crate) const fn key_equivalent(self) -> &'static str {
        match self {
            Self::About | Self::ShowAll | Self::Delete | Self::Zoom => "",
            Self::Hide | Self::HideOthers => "h",
            Self::Quit => "q",
            Self::Undo | Self::Redo => "z",
            Self::Cut => "x",
            Self::Copy => "c",
            Self::Paste => "v",
            Self::SelectAll => "a",
            Self::Minimize => "m",
            Self::ToggleFullScreen => "f",
            Self::Close => "w",
        }
    }

    /// 键盘等价物的修饰键。
    pub(crate) const fn modifiers(self) -> MenuModifiers {
        match self {
            Self::HideOthers => MenuModifiers::CommandAlt,
            Self::Redo => MenuModifiers::CommandShift,
            Self::ToggleFullScreen => MenuModifiers::CommandControl,
            Self::About | Self::ShowAll | Self::Delete | Self::Zoom => MenuModifiers::None,
            _ => MenuModifiers::Command,
        }
    }

    /// 是否是 Edit 菜单命令（选框/剪贴板/撤销 —— 由拥有文本编辑态的 responder 处理）。
    pub(crate) const fn is_edit(self) -> bool {
        matches!(
            self,
            Self::Undo
                | Self::Redo
                | Self::Cut
                | Self::Copy
                | Self::Paste
                | Self::Delete
                | Self::SelectAll
        )
    }

    /// Edit 命令 → 引擎按键事件（`None` = 纯 AppKit 命令，不经过引擎）。
    ///
    /// 平台层**不直接改引擎状态**：菜单动作统一映射成与物理按键等价的 [`KeyCode`] + 修饰键，
    /// 由 `App::dispatch` → 绑定派发这一条通路处理（菜单、快捷键、a11y 动作因此语义一致）。
    /// 这也意味着应用注册的 `Cmd+C` 绑定对菜单点击生效，不需要为菜单单独接线。
    pub(crate) const fn engine_key(self) -> Option<(KeyCode, Modifiers)> {
        match self {
            Self::Undo => Some((KeyCode::KeyZ, Modifiers::META)),
            Self::Redo => Some((KeyCode::KeyZ, Modifiers::META.union(Modifiers::SHIFT))),
            Self::Cut => Some((KeyCode::KeyX, Modifiers::META)),
            Self::Copy => Some((KeyCode::KeyC, Modifiers::META)),
            Self::Paste => Some((KeyCode::KeyV, Modifiers::META)),
            Self::SelectAll => Some((KeyCode::KeyA, Modifiers::META)),
            // `Delete` 项没有快捷键（Delete 键走文本输入通路），菜单点击映射成同名物理键。
            Self::Delete => Some((KeyCode::Delete, Modifiers::empty())),
            Self::About
            | Self::Hide
            | Self::HideOthers
            | Self::ShowAll
            | Self::Quit
            | Self::Minimize
            | Self::Zoom
            | Self::ToggleFullScreen
            | Self::Close => None,
        }
    }

    /// selector 字符串 → 命令身份。
    ///
    /// `GuiView::validateUserInterfaceItem:` 拿到的只有 selector，需要折回命令身份才能决定
    /// 启用状态（AppKit 不告诉 responder「这是哪个菜单项」的其他信息）；纯字符串比较，可在
    /// 开发机上单测。
    pub(crate) fn from_selector(selector: &str) -> Option<Self> {
        COMMANDS
            .into_iter()
            .find(|command| command.selector() == selector)
    }
}

/// 菜单项：分隔线或一条命令。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MenuEntry {
    /// 分隔线。
    Separator,
    /// 一条命令。
    Command(MenuCommand),
}

/// 菜单栏上的一个菜单（标题 + 子项）。
#[derive(Clone, Copy, Debug)]
pub(crate) struct MenuSection {
    /// 菜单标题。`""` = **应用菜单**：标题由构建器填进程名（AppKit 用第一个菜单当应用菜单，
    /// 其项标题决定菜单栏上显示的粗体文字）。
    pub title: &'static str,
    /// 是否是 Window 菜单（构建器据此交给 `NSApp.setWindowsMenu:`，让 AppKit 追加窗口列表与
    /// 「Bring All to Front」）。
    pub is_window_menu: bool,
    /// 子项。
    pub entries: &'static [MenuEntry],
}

/// 主菜单树：App / Edit / Window（R09-1 要求的最小可用集合）。
pub(crate) const MAIN_MENU: [MenuSection; 3] = [
    MenuSection {
        title: "",
        is_window_menu: false,
        entries: &[
            MenuEntry::Command(MenuCommand::About),
            MenuEntry::Separator,
            MenuEntry::Command(MenuCommand::Hide),
            MenuEntry::Command(MenuCommand::HideOthers),
            MenuEntry::Command(MenuCommand::ShowAll),
            MenuEntry::Separator,
            MenuEntry::Command(MenuCommand::Quit),
        ],
    },
    MenuSection {
        title: "Edit",
        is_window_menu: false,
        entries: &[
            MenuEntry::Command(MenuCommand::Undo),
            MenuEntry::Command(MenuCommand::Redo),
            MenuEntry::Separator,
            MenuEntry::Command(MenuCommand::Cut),
            MenuEntry::Command(MenuCommand::Copy),
            MenuEntry::Command(MenuCommand::Paste),
            MenuEntry::Command(MenuCommand::Delete),
            MenuEntry::Command(MenuCommand::SelectAll),
        ],
    },
    MenuSection {
        title: "Window",
        is_window_menu: true,
        entries: &[
            MenuEntry::Command(MenuCommand::Minimize),
            MenuEntry::Command(MenuCommand::Zoom),
            MenuEntry::Command(MenuCommand::Close),
            MenuEntry::Separator,
            MenuEntry::Command(MenuCommand::ToggleFullScreen),
        ],
    },
];

#[cfg(test)]
mod tests {
    use super::{COMMANDS, MAIN_MENU, MenuCommand, MenuEntry, MenuModifiers};
    use crate::event::{KeyCode, Modifiers};

    /// 菜单树里出现的全部命令（按出现顺序）。
    fn commands() -> Vec<MenuCommand> {
        MAIN_MENU
            .iter()
            .flat_map(|section| section.entries.iter())
            .filter_map(|entry| match entry {
                MenuEntry::Command(command) => Some(*command),
                MenuEntry::Separator => None,
            })
            .collect()
    }

    #[test]
    fn menu_tree_has_the_three_required_menus() {
        assert_eq!(MAIN_MENU.len(), 3, "App / Edit / Window");
        assert_eq!(
            MAIN_MENU[0].title, "",
            "第一个菜单是应用菜单（标题=进程名）"
        );
        assert_eq!(MAIN_MENU[1].title, "Edit");
        assert_eq!(MAIN_MENU[2].title, "Window");
        assert!(MAIN_MENU[2].is_window_menu, "Window 菜单要注册给 NSApp");
        assert!(
            MAIN_MENU[..2].iter().all(|section| !section.is_window_menu),
            "只有 Window 菜单进 setWindowsMenu:"
        );
    }

    /// R09-1 的核心：没有主菜单时 Cmd+Q / Cmd+W / Cmd+H 完全没有承载对象。
    #[test]
    fn app_menu_carries_the_standard_commands_and_shortcuts() {
        let app_menu = MAIN_MENU[0].entries;
        assert_eq!(app_menu[0], MenuEntry::Command(MenuCommand::About));
        assert_eq!(app_menu[1], MenuEntry::Separator, "About 后必须分隔");

        let quit = app_menu
            .iter()
            .find(|entry| **entry == MenuEntry::Command(MenuCommand::Quit))
            .expect("Cmd+Q 必须有菜单项");
        assert_eq!(*quit, MenuEntry::Command(MenuCommand::Quit));

        assert_eq!(MenuCommand::Quit.selector(), "terminate:");
        assert_eq!(MenuCommand::Quit.key_equivalent(), "q");
        assert_eq!(MenuCommand::Quit.modifiers(), MenuModifiers::Command);
        assert_eq!(MenuCommand::Hide.key_equivalent(), "h");
        assert_eq!(MenuCommand::HideOthers.key_equivalent(), "h");
        assert_eq!(
            MenuCommand::HideOthers.modifiers(),
            MenuModifiers::CommandAlt
        );
        assert_eq!(
            MenuCommand::ShowAll.key_equivalent(),
            "",
            "Show All 没有快捷键"
        );
        assert_eq!(
            MenuCommand::About.selector(),
            "orderFrontStandardAboutPanel:"
        );
    }

    /// R09-1：Edit 菜单项必须用**标准 selector**，否则不参与 responder chain。
    #[test]
    fn edit_menu_uses_the_standard_selectors() {
        let expected = [
            (MenuCommand::Undo, "undo:", "z"),
            (MenuCommand::Redo, "redo:", "z"),
            (MenuCommand::Cut, "cut:", "x"),
            (MenuCommand::Copy, "copy:", "c"),
            (MenuCommand::Paste, "paste:", "v"),
            (MenuCommand::Delete, "delete:", ""),
            (MenuCommand::SelectAll, "selectAll:", "a"),
        ];
        let edit: Vec<MenuCommand> = MAIN_MENU[1]
            .entries
            .iter()
            .filter_map(|entry| match entry {
                MenuEntry::Command(command) => Some(*command),
                MenuEntry::Separator => None,
            })
            .collect();
        assert_eq!(
            edit,
            expected
                .iter()
                .map(|(command, _, _)| *command)
                .collect::<Vec<_>>(),
            "Edit 菜单顺序：Undo/Redo/Cut/Copy/Paste/Delete/SelectAll"
        );
        for (command, selector, key) in expected {
            assert_eq!(command.selector(), selector, "{command:?} 的 selector");
            assert_eq!(command.key_equivalent(), key, "{command:?} 的快捷键");
            assert!(command.is_edit(), "{command:?} 属于 Edit 菜单");
        }
        assert_eq!(
            MenuCommand::Redo.modifiers(),
            MenuModifiers::CommandShift,
            "重做是 Cmd+Shift+Z"
        );
        assert_eq!(
            MenuCommand::Delete.modifiers(),
            MenuModifiers::None,
            "Delete 项没有快捷键，修饰键必须显式清零（NSMenuItem 缺省是 Command）"
        );
    }

    #[test]
    fn window_menu_has_minimize_zoom_close_and_fullscreen() {
        let window_menu: Vec<MenuCommand> = MAIN_MENU[2]
            .entries
            .iter()
            .filter_map(|entry| match entry {
                MenuEntry::Command(command) => Some(*command),
                MenuEntry::Separator => None,
            })
            .collect();
        assert_eq!(
            window_menu,
            [
                MenuCommand::Minimize,
                MenuCommand::Zoom,
                MenuCommand::Close,
                MenuCommand::ToggleFullScreen,
            ]
        );
        assert_eq!(MenuCommand::Minimize.selector(), "performMiniaturize:");
        assert_eq!(MenuCommand::Zoom.selector(), "performZoom:");
        assert_eq!(MenuCommand::Close.selector(), "performClose:");
        assert_eq!(MenuCommand::Close.key_equivalent(), "w");
        assert_eq!(
            MenuCommand::ToggleFullScreen.selector(),
            "toggleFullScreen:"
        );
        assert_eq!(
            MenuCommand::ToggleFullScreen.modifiers(),
            MenuModifiers::CommandControl,
            "全屏是 Ctrl+Cmd+F"
        );
    }

    /// Edit 菜单的每个命令都映射到**与物理按键等价**的引擎事件（统一入口，R09-1）。
    #[test]
    fn edit_commands_map_to_engine_keys() {
        let expected = [
            (MenuCommand::Undo, KeyCode::KeyZ, Modifiers::META),
            (
                MenuCommand::Redo,
                KeyCode::KeyZ,
                Modifiers::META | Modifiers::SHIFT,
            ),
            (MenuCommand::Cut, KeyCode::KeyX, Modifiers::META),
            (MenuCommand::Copy, KeyCode::KeyC, Modifiers::META),
            (MenuCommand::Paste, KeyCode::KeyV, Modifiers::META),
            (MenuCommand::SelectAll, KeyCode::KeyA, Modifiers::META),
            (MenuCommand::Delete, KeyCode::Delete, Modifiers::empty()),
        ];
        for (command, code, modifiers) in expected {
            assert_eq!(
                command.engine_key(),
                Some((code, modifiers)),
                "{command:?} 必须映射成引擎按键"
            );
        }
        // 非 Edit 命令由 AppKit 自己完成（quit/close/minimize/fullscreen）。
        for command in [
            MenuCommand::About,
            MenuCommand::Hide,
            MenuCommand::HideOthers,
            MenuCommand::ShowAll,
            MenuCommand::Quit,
            MenuCommand::Minimize,
            MenuCommand::Zoom,
            MenuCommand::ToggleFullScreen,
            MenuCommand::Close,
        ] {
            assert_eq!(command.engine_key(), None, "{command:?} 不该进引擎按键通道");
            assert!(!command.is_edit(), "{command:?} 不是 Edit 命令");
        }
    }

    /// `validateUserInterfaceItem:` 与菜单构建共用同一份 selector 表：任一命令都必须能被
    /// selector 折回来（否则菜单项永远拿不到正确的启用状态）。
    #[test]
    fn every_command_round_trips_through_its_selector() {
        for command in COMMANDS {
            assert_eq!(
                MenuCommand::from_selector(command.selector()),
                Some(command),
                "{command:?} 的 selector 必须唯一且可反查"
            );
        }
        assert_eq!(MenuCommand::from_selector("copy:"), Some(MenuCommand::Copy));
        assert_eq!(
            MenuCommand::from_selector("performClose:"),
            Some(MenuCommand::Close)
        );
        assert_eq!(MenuCommand::from_selector("notAPlatformSelector:"), None);
        assert_eq!(
            MenuCommand::from_selector("copy"),
            None,
            "selector 必须带冒号"
        );
    }

    /// 菜单树里出现的命令不重复（同一个 selector 出现两次会让 validation 与动作歧义）。
    #[test]
    fn menu_tree_lists_each_command_once() {
        let mut seen = Vec::new();
        for command in commands() {
            assert!(!seen.contains(&command), "{command:?} 在菜单树里重复出现");
            seen.push(command);
        }
        assert_eq!(seen.len(), COMMANDS.len(), "菜单树应覆盖全部命令");
    }

    #[test]
    fn about_title_comes_from_the_application_name() {
        assert_eq!(MenuCommand::About.display_title("Kytrix"), "About Kytrix");
        assert_eq!(MenuCommand::Quit.display_title("Kytrix"), "Quit");
        assert_eq!(MenuCommand::About.title(), "About");
    }

    /// 分隔线不是命令：`from_selector` 不可能返回它的「身份」，树里也不能有相邻分隔线。
    #[test]
    fn separators_are_grouped_and_never_adjacent() {
        for section in &MAIN_MENU {
            let mut previous_separator = true;
            for entry in section.entries {
                let separator = *entry == MenuEntry::Separator;
                assert!(!(separator && previous_separator), "禁相邻/首尾分隔线");
                previous_separator = separator;
            }
            assert!(!previous_separator, "禁止以分隔线结尾");
        }
    }
}
