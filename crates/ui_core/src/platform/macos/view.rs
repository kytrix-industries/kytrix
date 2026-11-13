//! `NSView` 子类：`CAMetalLayer` 宿主 + 输入收集 + `NSTextInputClient` + NSAccessibility
//! （`design/phase-12-macos-linux.md` §7.2 / §7.5 / §7.7）。
//!
//! 纪律：**平台回调不碰 `App`** —— 事件只 push 进 `ViewIvars.events`，泵在主线程 drain 后 dispatch
//! （与 Win32 `WndProc` 同一职责边界）。

use std::{
    cell::{Cell, RefCell},
    ptr::NonNull,
};

use objc2::{
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::Retained,
    runtime::{AnyObject, Sel},
};
use objc2_app_kit::{
    NSEvent, NSEventModifierFlags, NSTextInputClient, NSTrackingArea, NSTrackingAreaOptions, NSView,
};
use objc2_foundation::{
    NSArray, NSAttributedString, NSAttributedStringKey, NSPoint, NSRange, NSRect, NSSize, NSString,
    NSUInteger,
};

use crate::{
    event::{Event, ImeEvent, InputEvent, Key, KeyState, Modifiers, PointerButton},
    math::{Point, Rect},
    platform::macos::{a11y::MacA11y, ime::ImeState},
};

/// `GuiView` 的实例变量（主线程独占）。
#[derive(Debug)]
pub(crate) struct ViewIvars {
    /// 待泵消费的平台事件（**不**在这里调 `App`）。
    events: RefCell<Vec<Event>>,
    /// IME 组合状态。
    ime: RefCell<ImeState>,
    /// a11y adapter（首显前构造；`None` = 尚未安装）。
    a11y: RefCell<Option<MacA11y>>,
    /// 当前修饰键位图（`flagsChanged:` 维护）。
    modifiers: Cell<u8>,
    /// `insertText:` 刚提交过文本：同一次按键的 `characters` 不能再发一遍。
    suppress_text: Cell<bool>,
    /// 最近一次读到的 `backingScaleFactor`。
    scale: Cell<f64>,
    /// IME caret 区域（窗口逻辑坐标；`None` = 无 hint）。
    ime_cursor: Cell<Option<Rect>>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "KytrixGuiView"]
    #[ivars = ViewIvars]
    pub(crate) struct GuiView;

    impl GuiView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(wantsUpdateLayer))]
        fn wants_update_layer(&self) -> bool {
            true
        }

        #[unsafe(method(viewDidChangeBackingProperties))]
        fn backing_properties_changed(&self) {
            let scale = self
                .window()
                .map(|window| window.backingScaleFactor())
                .unwrap_or(1.0);
            self.ivars().scale.set(scale);
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            self.note_modifiers(event.modifierFlags());
            let modifiers = self.modifiers();
            // 新按键边界：同一按键内的重复 `insertText:` 才去重（`"ll"` / 双空格必须都能进）。
            self.ivars().ime.borrow_mut().begin_key();
            // **IME 先**：输入法可能吞掉这个键（组合/提交走 NSTextInputClient 回调）。
            if let Some(context) = self.inputContext() {
                context.handleEvent(event);
            }
            let composing = self.ivars().ime.borrow().has_marked();
            // 本键的正文是否已经由 IME 交付（`insertText:` 真的发出过 `Commit`）。
            let commit_consumed = self.ivars().suppress_text.replace(false);
            // `Key` **永不**被 IME 抑制：快捷键要能看到物理按键（与 Win32 一致）。
            if let Some(key) = map_keycode(event.keyCode()) {
                self.push_event(Event::Input(InputEvent::Key {
                    key,
                    state: KeyState::Down,
                    modifiers,
                }));
            }
            // `Text` 只在「正文没被 IME 交付」时才发，避免与 `ImeEvent::Commit` 重复插入。
            if !composing
                && !commit_consumed
                && !modifiers.intersects(Modifiers::CONTROL | Modifiers::META)
                && let Some(text) = event.characters()
            {
                let text = text.to_string();
                if !text.is_empty() && text.chars().all(|c| !c.is_control()) {
                    self.push_event(Event::Input(InputEvent::Text {
                        text: smol_str::SmolStr::new(text),
                    }));
                }
            }
        }

        #[unsafe(method(keyUp:))]
        fn key_up(&self, event: &NSEvent) {
            self.note_modifiers(event.modifierFlags());
            if let Some(key) = map_keycode(event.keyCode()) {
                self.push_event(Event::Input(InputEvent::Key {
                    key,
                    state: KeyState::Up,
                    modifiers: self.modifiers(),
                }));
            }
        }

        #[unsafe(method(flagsChanged:))]
        fn flags_changed(&self, event: &NSEvent) {
            // 只维护修饰键位图。macOS 的修饰键**不产生** `keyDown:`/`keyUp:`，若在这里合成
            // `Key::Shift`，应用会收到 Win32 上不存在的按键序列（Win32 的 Shift 是独立 VK 消息）。
            self.note_modifiers(event.modifierFlags());
            let modifiers = self.modifiers();
            let mut ime = self.ivars().ime.borrow_mut();
            if ime.has_marked() && !modifiers.intersects(Modifiers::SHIFT) {
                // 组合态里松开 Shift 视为放弃组合（输入法不会总发 `unmarkText:`）。
                ime.clear_marked();
                drop(ime);
                self.push_event(Event::Ime(ImeEvent::End));
            }
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            self.push_pointer(event, PointerPhase::Move);
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            self.push_pointer(event, PointerPhase::Move);
        }

        #[unsafe(method(rightMouseDragged:))]
        fn right_mouse_dragged(&self, event: &NSEvent) {
            self.push_pointer(event, PointerPhase::Move);
        }

        #[unsafe(method(otherMouseDragged:))]
        fn other_mouse_dragged(&self, event: &NSEvent) {
            self.push_pointer(event, PointerPhase::Move);
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            self.push_pointer(event, PointerPhase::Down(PointerButton::Primary));
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            self.push_pointer(event, PointerPhase::Up(PointerButton::Primary));
        }

        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, event: &NSEvent) {
            self.push_pointer(event, PointerPhase::Down(PointerButton::Secondary));
        }

        #[unsafe(method(rightMouseUp:))]
        fn right_mouse_up(&self, event: &NSEvent) {
            self.push_pointer(event, PointerPhase::Up(PointerButton::Secondary));
        }

        #[unsafe(method(otherMouseDown:))]
        fn other_mouse_down(&self, event: &NSEvent) {
            self.push_pointer(event, PointerPhase::Down(PointerButton::Middle));
        }

        #[unsafe(method(otherMouseUp:))]
        fn other_mouse_up(&self, event: &NSEvent) {
            self.push_pointer(event, PointerPhase::Up(PointerButton::Middle));
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            self.push_event(Event::Input(InputEvent::PointerLeave));
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            let position = self.logical_point(event.locationInWindow());
            let precise = event.hasPreciseScrollingDeltas();
            let per_line = if precise { 10.0 } else { 3.0 };
            // macOS 的 `scrollingDeltaY > 0` = 向上滚（与 Win32 的 `WM_MOUSEWHEEL` 正方向一致）。
            let delta_x = (event.scrollingDeltaX() / per_line) as f32;
            let delta_y = (event.scrollingDeltaY() / per_line) as f32;
            self.note_modifiers(event.modifierFlags());
            self.push_event(Event::Input(InputEvent::Scroll {
                position,
                delta_x,
                delta_y,
                modifiers: self.modifiers(),
            }));
        }

        // ---- NSAccessibility（容器 + 三条查询）----
        #[unsafe(method(isAccessibilityElement))]
        fn is_accessibility_element(&self) -> bool {
            false
        }

        #[unsafe(method_id(accessibilityChildren))]
        fn accessibility_children(&self) -> Option<Retained<NSArray>> {
            let mut slot = self.ivars().a11y.borrow_mut();
            slot.as_mut().and_then(MacA11y::children)
        }

        #[unsafe(method_id(accessibilityHitTest:))]
        fn accessibility_hit_test(&self, point: NSPoint) -> Option<Retained<AnyObject>> {
            let mut slot = self.ivars().a11y.borrow_mut();
            slot.as_mut()
                .and_then(|a11y| a11y.hit_test(point.x, point.y))
        }

        #[unsafe(method_id(accessibilityFocusedUIElement))]
        fn accessibility_focused_ui_element(&self) -> Retained<AnyObject> {
            let focused = {
                let mut slot = self.ivars().a11y.borrow_mut();
                slot.as_mut().and_then(MacA11y::focused)
            };
            focused.unwrap_or_else(|| {
                // SAFETY: `self` 是本窗口持有的有效 ObjC 对象；retain 后交回调用方。
                unsafe { Retained::retain(self as *const Self as *mut AnyObject) }
                    .expect("self is never null")
            })
        }
    }

    // SAFETY: `NSTextInputClient` 的每个方法都是 AppKit 在主线程同步调用的；
    // 实现里只碰 `ImeState` 与 view 的事件队列（都没有跨线程访问），签名由 objc2 编译期校验。
    unsafe impl NSTextInputClient for GuiView {
        #[unsafe(method(insertText:replacementRange:))]
        // SAFETY: AppKit 只在本 view 是第一响应者时于主线程调用；参数按 ObjC 约定保证非空。
        unsafe fn insert_text_replacement_range(&self, text: &AnyObject, _range: NSRange) {
            let value = object_text(text);
            if value.is_empty() {
                return;
            }
            let first = self.ivars().ime.borrow_mut().note_commit(&value);
            self.ivars().ime.borrow_mut().clear_marked();
            // 只有「真的交付了正文」才抑制本按键的 `Text`；重复回调（`first == false`）时
            // 同一次按键的正文已经交付过，抑制标记仍然有效。
            if first {
                self.ivars().suppress_text.set(true);
                self.push_event(Event::Ime(ImeEvent::Commit(smol_str::SmolStr::new(value))));
            }
        }

        #[unsafe(method(doCommandBySelector:))]
        // SAFETY: AppKit 只在本 view 是第一响应者时于主线程调用；参数按 ObjC 约定保证非空。
        unsafe fn do_command_by_selector(&self, _selector: Sel) {
            // 命令选择器（方向键 / 回车等）已由 `keyDown:` 转成 `Key` 事件；这里吞掉。
        }

        #[unsafe(method(setMarkedText:selectedRange:replacementRange:))]
        // SAFETY: AppKit 只在本 view 是第一响应者时于主线程调用；参数按 ObjC 约定保证非空。
        unsafe fn set_marked_text(&self, text: &AnyObject, selected: NSRange, _replacement: NSRange) {
            let value = object_text(text);
            let utf16: Vec<u16> = value.encode_utf16().collect();
            {
                let mut ime = self.ivars().ime.borrow_mut();
                ime.set_marked(&utf16, (selected.location, selected.length));
            }
            self.push_event(Event::Ime(ImeEvent::Preedit(smol_str::SmolStr::new(
                value,
            ))));
        }

        #[unsafe(method(unmarkText))]
        fn unmark_text(&self) {
            self.ivars().ime.borrow_mut().clear_marked();
            self.push_event(Event::Ime(ImeEvent::End));
        }

        #[unsafe(method(selectedRange))]
        fn selected_range(&self) -> NSRange {
            let ime = self.ivars().ime.borrow();
            if !ime.has_marked() {
                return NSRange::new(NSUInteger::MAX, 0);
            }
            let (location, length) = ime.selected_range();
            NSRange::new(location, length)
        }

        #[unsafe(method(markedRange))]
        fn marked_range(&self) -> NSRange {
            let ime = self.ivars().ime.borrow();
            if !ime.has_marked() {
                return NSRange::new(NSUInteger::MAX, 0);
            }
            let (location, length) = ime.marked_range();
            NSRange::new(location, length)
        }

        #[unsafe(method(hasMarkedText))]
        fn has_marked_text(&self) -> bool {
            self.ivars().ime.borrow().has_marked()
        }

        #[unsafe(method_id(attributedSubstringForProposedRange:actualRange:))]
        // SAFETY: AppKit 只在本 view 是第一响应者时于主线程调用；`actualRange` 可为空或不写。
        unsafe fn attributed_substring(
            &self,
            _range: NSRange,
            _actual: NonNull<NSRange>,
        ) -> Option<Retained<NSAttributedString>> {
            None
        }

        #[unsafe(method_id(validAttributesForMarkedText))]
        fn valid_attributes(&self) -> Retained<NSArray<NSAttributedStringKey>> {
            NSArray::new()
        }

        #[unsafe(method(firstRectForCharacterRange:actualRange:))]
        // SAFETY: AppKit 只在本 view 是第一响应者时于主线程调用；参数按 ObjC 约定保证非空。
        unsafe fn first_rect(&self, range: NSRange, actual: NonNull<NSRange>) -> NSRect {
            // `actualRange` 必须写回（不写会让输入法拿到未定义值）。
            // SAFETY: AppKit 传进来的 `actualRange` 指向本方法可写的 `NSRange`。
            unsafe { *actual.as_ptr() = range };
            // P16：应用/引擎提供的 caret 区域（窗口逻辑坐标，view 已 isFlipped）→ 屏幕坐标。
            // 无 hint 时保留 P12 的窗口原点 1×1 回退。
            let local = match self.ivars().ime_cursor.get() {
                Some(area) => NSRect::new(
                    NSPoint::new(f64::from(area.origin.x), f64::from(area.origin.y)),
                    NSSize::new(
                        f64::from(area.size.width.max(1.0)),
                        f64::from(area.size.height.max(1.0)),
                    ),
                ),
                None => NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1.0, 1.0)),
            };
            let in_window = self.convertRect_toView(local, None);
            self.window()
                .map(|window| window.convertRectToScreen(in_window))
                .unwrap_or(local)
        }

        #[unsafe(method(characterIndexForPoint:))]
        fn character_index_for_point(&self, _point: NSPoint) -> NSUInteger {
            0
        }
    }
);

/// 指针事件的三相（内部用）。
enum PointerPhase {
    Move,
    Down(PointerButton),
    Up(PointerButton),
}

impl GuiView {
    /// 建 view（`frame` 先给 1×1，`NativeWindow` 随后 `setFrame`）。
    pub(crate) fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ViewIvars {
            events: RefCell::new(Vec::with_capacity(16)),
            ime: RefCell::new(ImeState::new()),
            a11y: RefCell::new(None),
            modifiers: Cell::new(0),
            suppress_text: Cell::new(false),
            scale: Cell::new(1.0),
            ime_cursor: Cell::new(None),
        });
        // SAFETY: `initWithFrame:` 是 NSView 的指定初始化器；`this` 由 `alloc` 产生且未初始化。
        let this: Retained<Self> = unsafe {
            msg_send![
                super(this),
                initWithFrame: NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1.0, 1.0))
            ]
        };
        // 鼠标移出窗口需要 `NSTrackingArea` 才会产生 `mouseExited:`（§7.2）；`InVisibleRect`
        // 让区域随 view 尺寸自动更新，`ActiveInKeyWindow` 与 Win32 的 hover 语义一致。
        // 不声明 `.MouseMoved`：窗口的 `acceptsMouseMovedEvents` 已经投递 `mouseMoved:`，
        // 两者同时开启会让每个移动产生两次 `PointerMove`（实机探针复现）。
        //
        // SAFETY: owner 是刚初始化、即将返回给调用方的 `this`（NSView 存活期覆盖 tracking area）；
        // `userInfo = None`；rect 被 `InVisibleRect` 忽略。
        let owner: &AnyObject = unsafe { &*(&*this as *const Self).cast::<AnyObject>() };
        let tracking = unsafe {
            NSTrackingArea::initWithRect_options_owner_userInfo(
                NSTrackingArea::alloc(),
                NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1.0, 1.0)),
                NSTrackingAreaOptions::MouseEnteredAndExited
                    | NSTrackingAreaOptions::ActiveInKeyWindow
                    | NSTrackingAreaOptions::InVisibleRect,
                Some(owner),
                None,
            )
        };
        this.addTrackingArea(&tracking);
        this
    }

    /// 首显前安装 a11y adapter（`None` = 已经装过）。
    pub(crate) fn install_a11y(&self, a11y: MacA11y) {
        *self.ivars().a11y.borrow_mut() = Some(a11y);
    }

    /// 交付一笔 a11y 更新（泵每轮调用；`a11y_pending` 是 O(1) 过滤）。
    /// 交付一笔 a11y 更新。
    ///
    /// `raise()` 会让 AppKit 同步回调进来（`accessibilityChildren` / `accessibilityHitTest:`），
    /// 那些方法也要 `borrow_mut` ⇒ 这里先把 adapter **取出来**，回调期间 `RefCell` 里是 `None`
    /// （查询返回空，不 panic），完成后再放回。
    pub(crate) fn update_a11y(&self, update: impl FnOnce() -> accesskit::TreeUpdate) {
        let mut adapter = self.ivars().a11y.take();
        if let Some(a11y) = adapter.as_mut() {
            a11y.update(update);
        }
        *self.ivars().a11y.borrow_mut() = adapter;
    }

    pub(crate) fn focus_changed(&self, focused: bool) {
        let mut adapter = self.ivars().a11y.take();
        if let Some(a11y) = adapter.as_mut() {
            a11y.focus_changed(focused);
        }
        *self.ivars().a11y.borrow_mut() = adapter;
    }

    /// 取走本轮的平台事件（泵唯一读点）。
    pub(crate) fn take_events(&self) -> Vec<Event> {
        std::mem::take(&mut *self.ivars().events.borrow_mut())
    }

    pub(crate) fn set_scale(&self, scale: f64) {
        self.ivars().scale.set(scale);
    }

    /// 保存 IME caret 区域（窗口逻辑坐标；`None` = 无 hint，`firstRect` 回退 1×1）。
    pub(crate) fn set_ime_cursor_area(&self, area: Option<Rect>) {
        self.ivars().ime_cursor.set(area);
    }

    fn modifiers(&self) -> Modifiers {
        Modifiers::from_bits_truncate(self.ivars().modifiers.get())
    }

    fn note_modifiers(&self, flags: NSEventModifierFlags) {
        let mut bits = Modifiers::empty();
        if flags.contains(NSEventModifierFlags::Shift) {
            bits |= Modifiers::SHIFT;
        }
        if flags.contains(NSEventModifierFlags::Control) {
            bits |= Modifiers::CONTROL;
        }
        if flags.contains(NSEventModifierFlags::Option) {
            bits |= Modifiers::ALT;
        }
        if flags.contains(NSEventModifierFlags::Command) {
            bits |= Modifiers::META;
        }
        self.ivars().modifiers.set(bits.bits());
    }

    fn push_event(&self, event: Event) {
        self.ivars().events.borrow_mut().push(event);
    }

    fn push_pointer(&self, event: &NSEvent, phase: PointerPhase) {
        let position = self.logical_point(event.locationInWindow());
        self.note_modifiers(event.modifierFlags());
        let modifiers = self.modifiers();
        let input = match phase {
            PointerPhase::Move => InputEvent::PointerMove {
                position,
                modifiers,
            },
            PointerPhase::Down(button) => InputEvent::PointerDown {
                position,
                button,
                modifiers,
            },
            PointerPhase::Up(button) => InputEvent::PointerUp {
                position,
                button,
                modifiers,
            },
        };
        self.push_event(Event::Input(input));
    }

    /// 窗口逻辑点 → 引擎逻辑点（view 是 flipped，左上原点，与引擎坐标同向）。
    fn logical_point(&self, point: NSPoint) -> Point {
        let local = self.convertPoint_fromView(point, None);
        Point::new(local.x as f32, local.y as f32)
    }
}

/// `AnyObject`（`NSString` / `NSAttributedString`）→ `String`。
///
/// AppKit 的 `insertText:` 可能给 `NSString`，而 `setMarkedText:` 给带下划线属性的
/// `NSAttributedString`；后者的 `-description` 会连同 `NSUnderline` 等属性一起输出，
/// 不能作为文本内容使用（真实输入法实测：预编辑串被污染成 `ni'hao{ NSUnderline = 2; … }`）。
fn object_text(object: &AnyObject) -> String {
    if let Some(attributed) = object.downcast_ref::<NSAttributedString>() {
        return attributed.string().to_string();
    }
    if let Some(text) = object.downcast_ref::<NSString>() {
        return text.to_string();
    }
    // 非文本对象不在 `NSTextInputClient` 契约内；保留 description 作为诊断用回退。
    // SAFETY: `description` 对所有 NSObject 都成立，且返回 NSString。
    let description: Retained<NSString> = unsafe { msg_send![object, description] };
    description.to_string()
}

/// `NSEvent.keyCode`（硬件码）→ 引擎 `Key`（未映射 ⇒ `Key::Other`，与 Win32 的 `Key::Other(VK)` 同约定）。
fn map_keycode(code: u16) -> Option<Key> {
    let key = match code {
        53 => Key::Escape,
        36 | 76 => Key::Enter,
        48 => Key::Tab,
        51 => Key::Backspace,
        117 => Key::Delete,
        49 => Key::Space,
        126 => Key::ArrowUp,
        125 => Key::ArrowDown,
        123 => Key::ArrowLeft,
        124 => Key::ArrowRight,
        115 => Key::Home,
        119 => Key::End,
        116 => Key::PageUp,
        121 => Key::PageDown,
        122 => Key::Function(1),
        120 => Key::Function(2),
        99 => Key::Function(3),
        118 => Key::Function(4),
        96 => Key::Function(5),
        97 => Key::Function(6),
        98 => Key::Function(7),
        100 => Key::Function(8),
        101 => Key::Function(9),
        109 => Key::Function(10),
        103 => Key::Function(11),
        111 => Key::Function(12),
        56 | 60 => Key::Shift,
        59 | 62 => Key::Control,
        58 | 61 => Key::Alt,
        55 | 54 => Key::Meta,
        other => Key::Other(other),
    };
    Some(key)
}

#[cfg(test)]
mod tests {
    use objc2::{AnyThread, msg_send, rc::Retained, runtime::AnyObject};
    use objc2_app_kit::NSUnderlineStyleAttributeName;
    use objc2_foundation::{NSMutableAttributedString, NSNumber, NSRange, NSString};

    use super::object_text;

    fn as_any_object<T>(object: &T) -> &AnyObject {
        // SAFETY: 测试里传入的都是 NSObject 子类；AnyObject 是同一对象的类型擦除视图。
        unsafe { &*(object as *const T).cast::<AnyObject>() }
    }

    #[test]
    fn object_text_reads_plain_strings() {
        let plain = NSString::from_str("ni'hao");
        assert_eq!(object_text(as_any_object(&*plain)), "ni'hao");
    }

    #[test]
    fn object_text_strips_attributed_string_attributes() {
        let plain = NSString::from_str("ni'hao");
        let attributed =
            NSMutableAttributedString::initWithString(NSMutableAttributedString::alloc(), &plain);
        let underline = NSNumber::numberWithInteger(2);
        // SAFETY: `NSUnderlineStyleAttributeName` 是本测试刚创建的字符串上的合法属性键；
        // value 是 NSNumber（2 = 单下划线）。
        unsafe {
            attributed.addAttribute_value_range(
                NSUnderlineStyleAttributeName,
                as_any_object(&*underline),
                NSRange::new(0, plain.length()),
            );
        }
        // 夹具必须真的带属性，否则 `-description` 不会复现污染。
        let description: Retained<NSString> = unsafe { msg_send![&*attributed, description] };
        assert!(
            description.to_string().contains("NSUnderline"),
            "attributed fixture must carry attributes: {description}"
        );
        assert_eq!(object_text(as_any_object(&*attributed)), "ni'hao");
    }
}
