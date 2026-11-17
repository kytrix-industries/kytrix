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
    // ── Edit 菜单（`GuiView` 只在「应用真的能执行它」时接住，否则菜单项置灰）──
    //
    // 可用性由 [`EditAvailability`] 决定，判据是**能力**而不是「有文本焦点」：引擎没有内建
    // 文本控件，剪贴板/撤销语义由应用绑定拥有（`engine_key` 把菜单动作翻译成等价按键）。
    // 因此「文本编辑态为真、但应用没绑定该键」的菜单项必须置灰 —— 启用一个点了什么都不做的
    // 菜单项，比置灰更糟（用户会认为剪贴板/撤销坏了）。
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

    /// 是否是 Edit 菜单命令（选框/剪贴板/撤销 —— `GuiView` 只在应用绑定了对应按键时接住）。
    ///
    /// 定义成 [`Self::edit_index`] 的成员判定，唯一真值是那张位序表（测试
    /// `every_edit_command_has_its_own_bit` 用 `COMMANDS` 全表钉住它与 `engine_key` 对齐）。
    pub(crate) const fn is_edit(self) -> bool {
        self.edit_index().is_some()
    }

    /// Edit 命令在 [`EditAvailability`] 位图里的位序；非 Edit 命令 ⇒ `None`。
    ///
    /// 位序是纯内部约定（0 = Undo，…，6 = SelectAll）。放在这里的理由是它必须与
    /// [`Self::is_edit`] / [`Self::engine_key`] 逐条对齐：测试
    /// `every_edit_command_has_its_own_bit` 用 `COMMANDS` 全表钉住三者一致。
    pub(crate) const fn edit_index(self) -> Option<usize> {
        match self {
            Self::Undo => Some(0),
            Self::Redo => Some(1),
            Self::Cut => Some(2),
            Self::Copy => Some(3),
            Self::Paste => Some(4),
            Self::Delete => Some(5),
            Self::SelectAll => Some(6),
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

/// Edit 菜单项的**可用性快照**（R09-1 收尾 / AR3 macOS P2-11）。
///
/// AppKit 在 `autoenablesItems` 下逐项问 responder chain 的 `validateUserInterfaceItem:`，
/// 而那个回调里拿不到 `App`（平台回调不碰引擎状态的纪律）：可用性必须**预先算成快照**，
/// 由泵每轮写进 view。唯一生产者是 `App::edit_menu_availability`。
///
/// 语义：某一位为真 ⇔ 该命令**既有文本编辑态、又有引擎侧接收者**（应用 `bind_key` 绑定了它，
/// 且 `on_action` 注册了处理器）。没有接收者时置灰：启用一个「点了什么都不做」的菜单项比置灰
/// 更糟。`validateUserInterfaceItem:` 与 `GuiView::run_menu_command` 共用同一份快照，因此
/// 「菜单可点但快捷键不响应」这类分叉在结构上不可能出现。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EditAvailability(u8);

impl EditAvailability {
    /// 全部置灰（没有文本编辑态）。
    pub(crate) const NONE: Self = Self(0);

    /// 计算快照：`text_editing` 为假 ⇒ 全部置灰（连绑定都不问：非编辑态下菜单动作会被上抛给
    /// responder chain，启用它没有意义）；否则逐条 Edit 命令问 `handled`「这个键组合在引擎侧
    /// 有没有接收者」。
    ///
    /// 参数是闭包而不是 `&App`：本模块是纯数据 + 纯函数，能在开发机（Windows）上单测（见模块
    /// 文档）；`App` 侧的读取（绑定表 + 处理器表）由 `App::edit_menu_availability` 完成。
    pub(crate) fn resolve(
        text_editing: bool,
        mut handled: impl FnMut(KeyCode, Modifiers) -> bool,
    ) -> Self {
        if !text_editing {
            return Self::NONE;
        }
        let mut bits = 0_u8;
        for command in COMMANDS {
            let Some(index) = command.edit_index() else {
                continue;
            };
            // 全部 Edit 命令都有 `engine_key`（`edit_index` 与它逐条对齐，见测试）。
            let Some((key, modifiers)) = command.engine_key() else {
                continue;
            };
            if handled(key, modifiers) {
                bits |= 1 << index;
            }
        }
        Self(bits)
    }

    /// 该命令的菜单项是否启用。
    ///
    /// 非 Edit 命令恒为 `true`：它们由 AppKit 自己实现（`MenuCommand::engine_key` 返回 `None`），
    /// 不受应用的编辑能力影响。
    pub(crate) fn allows(self, command: MenuCommand) -> bool {
        if !command.is_edit() {
            return true;
        }
        let Some(index) = command.edit_index() else {
            // 不可达：`is_edit` 就是 `edit_index().is_some()`。兜底成「可用」而不是 panic ——
            // 菜单可用性算错不应让整个应用崩掉。
            return true;
        };
        self.0 & (1 << index) != 0
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
    use super::{COMMANDS, EditAvailability, MAIN_MENU, MenuCommand, MenuEntry, MenuModifiers};
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

    /// Edit 命令的位序、菜单树位置与 `engine_key` 必须逐条对齐：任一处单独改动都会让
    /// `validateUserInterfaceItem:` 把启用状态打到错误的菜单项上。
    ///
    /// 成员判据用**菜单树里的 Edit 段**当第二来源（而不是 `is_edit` 自己），否则这条测试只是
    /// 自证。
    #[test]
    fn every_edit_command_has_its_own_bit() {
        let in_edit_menu: Vec<MenuCommand> = MAIN_MENU[1]
            .entries
            .iter()
            .filter_map(|entry| match entry {
                MenuEntry::Command(command) => Some(*command),
                MenuEntry::Separator => None,
            })
            .collect();
        let mut seen = Vec::new();
        for command in COMMANDS {
            assert_eq!(
                in_edit_menu.contains(&command),
                command.edit_index().is_some(),
                "{command:?} 是否出现在 Edit 菜单段，必须与位序表一致"
            );
            let Some(index) = command.edit_index() else {
                continue;
            };
            assert!(index < 8, "{command:?} 的位序超出 u8 位图");
            assert!(
                !seen.contains(&index),
                "{command:?} 与其它命令共用位 {index}"
            );
            seen.push(index);
            assert!(
                command.engine_key().is_some(),
                "{command:?} 是 Edit 命令，必须能翻译成引擎按键"
            );
        }
        assert_eq!(
            seen.len(),
            in_edit_menu.len(),
            "位序表条数必须等于 Edit 菜单项数"
        );
    }

    /// AR3 macOS P2-11：**只有引擎侧真有接收者的项才启用**，其余置灰（不能「可点却没动作」）。
    #[test]
    fn edit_availability_greys_out_commands_without_a_receiver() {
        // 应用只绑了 Cmd+C（处理器由 `App` 侧保证存在）。
        let only_copy = EditAvailability::resolve(true, |key, modifiers| {
            key == KeyCode::KeyC && modifiers == Modifiers::META
        });
        assert!(only_copy.allows(MenuCommand::Copy));
        for grey in [
            MenuCommand::Cut,
            MenuCommand::Paste,
            MenuCommand::Undo,
            MenuCommand::Redo,
            MenuCommand::Delete,
            MenuCommand::SelectAll,
        ] {
            assert!(!only_copy.allows(grey), "{grey:?} 没有接收者，必须置灰");
        }
        // 非 Edit 命令不受应用编辑能力影响（由 AppKit 自己实现）。
        for command in [
            MenuCommand::About,
            MenuCommand::Hide,
            MenuCommand::Quit,
            MenuCommand::Minimize,
            MenuCommand::Zoom,
            MenuCommand::ToggleFullScreen,
            MenuCommand::Close,
        ] {
            assert!(only_copy.allows(command), "{command:?} 不受编辑能力影响");
        }
    }

    /// 没有文本编辑态时 Edit 命令全部置灰（连绑定都不问）：菜单动作在非编辑态会被上抛给
    /// responder chain。
    #[test]
    fn edit_availability_requires_text_editing() {
        let everything_bound = EditAvailability::resolve(false, |_, _| true);
        for command in COMMANDS {
            assert_eq!(
                everything_bound.allows(command),
                !command.is_edit(),
                "{command:?} 的启用状态只由「是否 Edit 命令」决定"
            );
        }
        assert_eq!(everything_bound, EditAvailability::NONE);

        let all = EditAvailability::resolve(true, |_, _| true);
        for command in COMMANDS {
            if command.is_edit() {
                assert!(all.allows(command), "{command:?} 全部绑定 ⇒ 启用");
            }
        }
    }

    /// 键组合必须**精确**匹配（修饰键不同就是另一个绑定）：只绑 Cmd+Shift+Z 时 Undo 不能启用。
    #[test]
    fn edit_availability_matches_the_exact_modifiers() {
        let redo_only = EditAvailability::resolve(
            true,
            |key, modifiers| matches!(MenuCommand::Redo.engine_key(), Some((k, m)) if k == key && m == modifiers),
        );
        assert!(
            redo_only.allows(MenuCommand::Redo),
            "Cmd+Shift+Z ⇒ Redo 启用"
        );
        assert!(!redo_only.allows(MenuCommand::Undo), "Cmd+Z 是另一个绑定");

        // 只有 Cmd（无 Shift）：命中的是五条 Cmd+X 命令，Redo（Cmd+Shift+Z）与 Delete
        // （无修饰键）都不能被误命中。
        let command_only =
            EditAvailability::resolve(true, |_, modifiers| modifiers == Modifiers::META);
        for allowed in [
            MenuCommand::Undo,
            MenuCommand::Cut,
            MenuCommand::Copy,
            MenuCommand::Paste,
            MenuCommand::SelectAll,
        ] {
            assert!(command_only.allows(allowed), "{allowed:?} 是 Cmd+X");
        }
        for greyed in [MenuCommand::Redo, MenuCommand::Delete] {
            assert!(!command_only.allows(greyed), "{greyed:?} 的修饰键不同");
        }
    }
}
