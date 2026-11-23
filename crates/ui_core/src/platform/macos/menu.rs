use std::ffi::CString;

use objc2::{MainThreadMarker, rc::Retained, runtime::Sel};
use objc2_app_kit::{NSApplication, NSEventModifierFlags, NSMenu, NSMenuItem};
use objc2_foundation::{NSProcessInfo, NSString};

use crate::platform::menu::{MAIN_MENU, MenuCommand, MenuEntry, MenuModifiers};

pub(crate) fn install(mtm: MainThreadMarker, ns_app: &NSApplication) {
    let app_name = NSProcessInfo::processInfo().processName().to_string();
    let main = NSMenu::new(mtm);
    let mut window_menu: Option<Retained<NSMenu>> = None;
    for section in &MAIN_MENU {
        let submenu = NSMenu::new(mtm);
        for entry in section.entries {
            submenu.addItem(&item(mtm, *entry, &app_name));
        }
        let title = if section.title.is_empty() {
            NSString::from_str(&app_name)
        } else {
            NSString::from_str(section.title)
        };
        let container = NSMenuItem::new(mtm);
        container.setTitle(&title);
        container.setSubmenu(Some(&submenu));
        main.addItem(&container);
        if section.is_window_menu {
            window_menu = Some(submenu);
        }
    }
    ns_app.setMainMenu(Some(&main));
    ns_app.setWindowsMenu(window_menu.as_deref());
}

fn item(mtm: MainThreadMarker, entry: MenuEntry, app_name: &str) -> Retained<NSMenuItem> {
    let MenuEntry::Command(command) = entry else {
        return NSMenuItem::separatorItem(mtm);
    };
    let item = NSMenuItem::new(mtm);
    item.setTitle(&NSString::from_str(&command.display_title(app_name)));
    item.setKeyEquivalent(&NSString::from_str(command.key_equivalent()));
    unsafe { item.setAction(Some(selector(command))) };
    item.setKeyEquivalentModifierMask(modifiers(command.modifiers()));
    item
}

pub(crate) fn selector(command: MenuCommand) -> Sel {
    let name = CString::new(command.selector()).expect("selector 是 ASCII 常量且不含 NUL");
    Sel::register(&name)
}

fn modifiers(modifiers: MenuModifiers) -> NSEventModifierFlags {
    match modifiers {
        MenuModifiers::None => NSEventModifierFlags::empty(),
        MenuModifiers::Command => NSEventModifierFlags::Command,
        MenuModifiers::CommandShift => NSEventModifierFlags::Command | NSEventModifierFlags::Shift,
        MenuModifiers::CommandAlt => NSEventModifierFlags::Command | NSEventModifierFlags::Option,
        MenuModifiers::CommandControl => {
            NSEventModifierFlags::Command | NSEventModifierFlags::Control
        }
    }
}
