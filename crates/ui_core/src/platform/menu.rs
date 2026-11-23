use crate::event::{KeyCode, Modifiers};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MenuModifiers {
    None,
    Command,
    CommandShift,
    CommandAlt,
    CommandControl,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum MenuCommand {
    About,
    Hide,
    HideOthers,
    ShowAll,
    Quit,
    Undo,
    Redo,
    Cut,
    Copy,
    Paste,
    Delete,
    SelectAll,
    Minimize,
    Zoom,
    ToggleFullScreen,
    Close,
}

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

    pub(crate) fn display_title(self, app_name: &str) -> String {
        if matches!(self, Self::About) {
            format!("About {app_name}")
        } else {
            self.title().to_owned()
        }
    }

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

    pub(crate) const fn modifiers(self) -> MenuModifiers {
        match self {
            Self::HideOthers => MenuModifiers::CommandAlt,
            Self::Redo => MenuModifiers::CommandShift,
            Self::ToggleFullScreen => MenuModifiers::CommandControl,
            Self::About | Self::ShowAll | Self::Delete | Self::Zoom => MenuModifiers::None,
            _ => MenuModifiers::Command,
        }
    }

    pub(crate) const fn is_edit(self) -> bool {
        self.edit_index().is_some()
    }

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

    pub(crate) const fn engine_key(self) -> Option<(KeyCode, Modifiers)> {
        match self {
            Self::Undo => Some((KeyCode::KeyZ, Modifiers::META)),
            Self::Redo => Some((KeyCode::KeyZ, Modifiers::META.union(Modifiers::SHIFT))),
            Self::Cut => Some((KeyCode::KeyX, Modifiers::META)),
            Self::Copy => Some((KeyCode::KeyC, Modifiers::META)),
            Self::Paste => Some((KeyCode::KeyV, Modifiers::META)),
            Self::SelectAll => Some((KeyCode::KeyA, Modifiers::META)),
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

    pub(crate) fn from_selector(selector: &str) -> Option<Self> {
        COMMANDS
            .into_iter()
            .find(|command| command.selector() == selector)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EditAvailability(u8);

impl EditAvailability {
    pub(crate) const NONE: Self = Self(0);

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
            let Some((key, modifiers)) = command.engine_key() else {
                continue;
            };
            if handled(key, modifiers) {
                bits |= 1 << index;
            }
        }
        Self(bits)
    }

    pub(crate) fn allows(self, command: MenuCommand) -> bool {
        if !command.is_edit() {
            return true;
        }
        let Some(index) = command.edit_index() else {
            return true;
        };
        self.0 & (1 << index) != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MenuEntry {
    Separator,
    Command(MenuCommand),
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct MenuSection {
    pub title: &'static str,
    pub is_window_menu: bool,
    pub entries: &'static [MenuEntry],
}

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

    #[test]
    fn edit_availability_greys_out_commands_without_a_receiver() {
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
