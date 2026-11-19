//! 主菜单的 AppKit 构建（`NSMenu` / `NSMenuItem`；R09-1）。
//!
//! 结构、标题、快捷键与 selector 全部来自平台无关的 [`crate::platform::menu`]；本文件只负责把
//! 它翻译成 ObjC 对象并把菜单挂到 `NSApplication` 上。菜单项一律 `target = nil`（first
//! responder 路由），因此动作由 responder chain 上的真正处理者接住：
//!
//! | 菜单 | 目标（responder chain） | selector |
//! |---|---|---|
//! | App | `NSApplication`（shared） | `orderFrontStandardAboutPanel:` / `hide:` / `hideOtherApplications:` / `unhideAllApplications:` / `terminate:` |
//! | Edit | `KytrixGuiView`（有文本焦点时）→ 否则不启用 | `undo:` / `redo:` / `cut:` / `copy:` / `paste:` / `delete:` / `selectAll:` |
//! | Window | `NSWindow`（key window） | `performMiniaturize:` / `performZoom:` / `performClose:` / `toggleFullScreen:` |

use std::ffi::CString;

use objc2::{MainThreadMarker, rc::Retained, runtime::Sel};
use objc2_app_kit::{NSApplication, NSEventModifierFlags, NSMenu, NSMenuItem};
use objc2_foundation::{NSProcessInfo, NSString};

use crate::platform::menu::{MAIN_MENU, MenuCommand, MenuEntry, MenuModifiers};

/// 安装主菜单（`finishLaunching` 之后再设 `mainMenu` 仍然生效）。
///
/// 何时调用：`run()` 里创建任何窗口**之前**。AppKit 的键等价分发是
/// `NSApp.sendEvent:` → `[mainMenu performKeyEquivalent:]` → 失败才给 key window，所以主菜单
/// 必须在第一帧输入之前就位（否则 Cmd+Q 会被当成普通 `keyDown:` 丢掉）。
pub(crate) fn install(mtm: MainThreadMarker, ns_app: &NSApplication) {
    let app_name = NSProcessInfo::processInfo().processName().to_string();
    let main = NSMenu::new(mtm);
    let mut window_menu: Option<Retained<NSMenu>> = None;
    for section in &MAIN_MENU {
        let submenu = NSMenu::new(mtm);
        for entry in section.entries {
            submenu.addItem(&item(mtm, *entry, &app_name));
        }
        // 应用菜单的标题用进程名：菜单栏第一格显示的粗体文字来自这一项。
        let title = if section.title.is_empty() {
            NSString::from_str(&app_name)
        } else {
            NSString::from_str(section.title)
        };
        // 容器项：`action = nil`、无快捷键。AppKit 只在**有子菜单**时把它按应用菜单渲染。
        // 全部走**安全**的属性 API（见 [`item`] 的说明）。
        let container = NSMenuItem::new(mtm);
        container.setTitle(&title);
        container.setSubmenu(Some(&submenu));
        main.addItem(&container);
        if section.is_window_menu {
            window_menu = Some(submenu);
        }
    }
    ns_app.setMainMenu(Some(&main));
    // 注册 Window 菜单：AppKit 会往里追加窗口列表与「Bring All to Front」，
    // 并把它当成系统语义上的 Window 菜单（全屏项的标题切换也依赖这个身份）。
    //
    // B5：`event_loop::run` 在建窗之前调了 `NSWindow.setAllowsAutomaticWindowTabbing(false)`，
    // 因此 AppKit **不会**再往这里追加「Merge All Windows」/「Show All Tabs」等 tab 相关项
    // （自动 tabbing 关闭后这些系统项没有意义）。窗口列表与 Bring All to Front 照常保留。
    // 本模块不自己增删这些系统项 —— 它们由 AppKit 按上述开关维护。
    ns_app.setWindowsMenu(window_menu.as_deref());
}

/// 一个菜单项（分隔线或命令）。
///
/// 构造用 `NSMenuItem::new(mtm)` + 属性 setter，而**不用** `initWithTitle:action:keyEquivalent:`：
/// 后者是 `unsafe fn` 且需要 `Allocated<Self>`（`alloc(mtm)` 来自 `objc2::MainThreadOnly` trait，
/// 是"裸分配 + 未校验初始化"的路径）。`new(mtm)` 内部完成 `alloc` + `init`，只留 `setAction:`
/// 一处真正需要调用方担保的 `unsafe`（任意 selector 的发送目标由我们决定）。
fn item(mtm: MainThreadMarker, entry: MenuEntry, app_name: &str) -> Retained<NSMenuItem> {
    let MenuEntry::Command(command) = entry else {
        return NSMenuItem::separatorItem(mtm);
    };
    let item = NSMenuItem::new(mtm);
    item.setTitle(&NSString::from_str(&command.display_title(app_name)));
    item.setKeyEquivalent(&NSString::from_str(command.key_equivalent()));
    // SAFETY: selector 来自 `MenuCommand::selector()` —— 全是 AppKit 已注册的标准 responder 动作
    // （`terminate:` / `copy:` / `performClose:` …），且 target 保持 nil（first responder 路由），
    // 不会把引擎不认识的方法发给任意对象。
    unsafe { item.setAction(Some(selector(command))) };
    item.setKeyEquivalentModifierMask(modifiers(command.modifiers()));
    item
}

/// 命令 → AppKit selector。
///
/// 菜单构建与 responder 动作的上抛（`GuiView` 没有编辑能力时转 `nextResponder`）共用这一份
/// 映射，因此「菜单里的 selector」与「view 里实现的 selector」不可能分叉。
///
/// `Sel::register` 对已注册的 selector 是幂等查找（AppKit 的这套 selector 全部已在运行时里），
/// 因此这里不需要 `sel!` 宏的静态展开。
pub(crate) fn selector(command: MenuCommand) -> Sel {
    let name = CString::new(command.selector()).expect("selector 是 ASCII 常量且不含 NUL");
    Sel::register(&name)
}

/// 菜单修饰键 → `NSEventModifierFlags`。
fn modifiers(modifiers: MenuModifiers) -> NSEventModifierFlags {
    match modifiers {
        // `NSMenuItem` 的缺省修饰键是 Command，所以「无修饰键」必须显式清零。
        MenuModifiers::None => NSEventModifierFlags::empty(),
        MenuModifiers::Command => NSEventModifierFlags::Command,
        MenuModifiers::CommandShift => NSEventModifierFlags::Command | NSEventModifierFlags::Shift,
        MenuModifiers::CommandAlt => NSEventModifierFlags::Command | NSEventModifierFlags::Option,
        MenuModifiers::CommandControl => {
            NSEventModifierFlags::Command | NSEventModifierFlags::Control
        }
    }
}
