//! `NSView` 子类：`CAMetalLayer` 宿主 + 输入收集 + `NSTextInputClient` + NSAccessibility
//! （`design/phase-12-macos-linux.md` §7.2 / §7.5 / §7.7）。
//!
//! 纪律：**平台回调不碰 `App`** —— 事件只 push 进 `ViewIvars.events`，泵在主线程 drain 后 dispatch
//! （与 Win32 `WndProc` 同一职责边界）。

use std::{
    cell::{Cell, RefCell},
    ptr::NonNull,
    time::Instant,
};

use objc2::{
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::Retained,
    runtime::{AnyObject, Sel},
};
use objc2_app_kit::{
    NSEvent, NSEventModifierFlags, NSEventPhase, NSTextInputClient, NSTrackingArea,
    NSTrackingAreaOptions, NSView,
};
use objc2_foundation::{
    NSArray, NSAttributedString, NSAttributedStringKey, NSPoint, NSProcessInfo, NSRange, NSRect,
    NSSize, NSString, NSUInteger,
};

use crate::{
    event::{
        Event, ImeEvent, InputEvent, KeyCode, KeyState, Modifiers, PointerButton, ScrollPhase,
        ScrollUnit, modifier_key_transitions, should_emit_text, timestamp_from_ticks,
    },
    math::{Point, Rect},
    platform::{
        ime::replacement_range,
        macos::{a11y::MacA11y, ime::ImeState},
    },
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
            // R02-2：`NSEvent.isARepeat` 是 AppKit 的自动重复标志（长按产生的重复 `keyDown:`）。
            let repeat = event.isARepeat();
            // R02-14：事件时刻取 `NSEvent.timestamp`（系统启动起的秒），不是分发时刻。
            let timestamp = event_timestamp(event);
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
            self.push_event(Event::Input(InputEvent::Key {
                code: key_code(event.keyCode()),
                state: KeyState::Down,
                repeat,
                modifiers,
                timestamp,
            }));
            // `Text` 只在「正文没被 IME 交付」时才发，避免与 `ImeEvent::Commit` 重复插入。
            // 判据与 Linux 的 XKB 回退通路共用 `should_emit_text`（含「重复不得抑制文本」的
            // 回归测试）：`commit_consumed` = 本次按键的正文已由 IME 交付。
            if should_emit_text(KeyState::Down, repeat, commit_consumed, composing)
                && !modifiers.intersects(Modifiers::CONTROL | Modifiers::META)
                && let Some(text) = event.characters()
            {
                let text = text.to_string();
                if !text.is_empty() && text.chars().all(|c| !c.is_control()) {
                    self.push_event(Event::Input(InputEvent::Text {
                        text: smol_str::SmolStr::new(text),
                        timestamp,
                    }));
                }
            }
        }

        #[unsafe(method(keyUp:))]
        fn key_up(&self, event: &NSEvent) {
            self.note_modifiers(event.modifierFlags());
            self.push_event(Event::Input(InputEvent::Key {
                code: key_code(event.keyCode()),
                state: KeyState::Up,
                // 抬起没有重复语义。
                repeat: false,
                modifiers: self.modifiers(),
                timestamp: event_timestamp(event),
            }));
        }

        #[unsafe(method(flagsChanged:))]
        fn flags_changed(&self, event: &NSEvent) {
            // AppKit 不为修饰键发 `keyDown:`/`keyUp:`，只发 `flagsChanged:`；Win32 与 Wayland
            // 都把 Shift/Ctrl/Alt/Meta 当普通按键投递。这里按位图变化**合成**同样的 `Key`
            // 序列，`App::held_keys` / `App::key_down` 才在三个平台语义一致（否则 macOS 上
            // `key_down(ShiftLeft)` 恒为 false）。侧别不可得（`NSEventModifierFlags` 不区分
            // 左右），统一取左变体 —— 与 `Modifiers` 位图本身不区分左右一致。
            let previous = self.modifiers();
            self.note_modifiers(event.modifierFlags());
            let modifiers = self.modifiers();
            let timestamp = event_timestamp(event);
            for (code, state) in modifier_key_transitions(previous, modifiers) {
                self.push_event(Event::Input(InputEvent::Key {
                    code,
                    state,
                    // 修饰键的 `flagsChanged:` 没有重复语义。
                    repeat: false,
                    modifiers,
                    timestamp,
                }));
            }
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

        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, event: &NSEvent) {
            // R02-4：`NSTrackingArea` 的 enter 半边（Windows 没有独立消息，Wayland 是
            // `wl_pointer.enter`）。它同时带进窗位置，之后的 `mouseMoved:` 才是纯移动。
            let position = self.logical_point(event.locationInWindow());
            self.note_modifiers(event.modifierFlags());
            self.push_event(Event::Input(InputEvent::PointerEnter {
                position,
                modifiers: self.modifiers(),
                timestamp: event_timestamp(event),
            }));
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, event: &NSEvent) {
            // R02-39：`PointerLeave` 带离开点（W3C `pointerleave` 也有坐标）。
            let position = self.logical_point(event.locationInWindow());
            self.note_modifiers(event.modifierFlags());
            self.push_event(Event::Input(InputEvent::PointerLeave {
                position,
                modifiers: self.modifiers(),
                timestamp: event_timestamp(event),
            }));
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            let position = self.logical_point(event.locationInWindow());
            let precise = event.hasPreciseScrollingDeltas();
            // 精确设备（触控板 / Magic Mouse）给的是像素增量，1:1 跟随；非精确设备（普通滚轮）
            // 给的是"行"，AppKit 一格约 3 行（`NSEvent.scrollingDeltaY` 的非精确量纲仍是
            // **需真机确认**的事项，这里保持 P12 的 /3 归一化，不凭空改手感）。
            let (delta_x, delta_y, unit) = if precise {
                (
                    event.scrollingDeltaX() as f32,
                    event.scrollingDeltaY() as f32,
                    ScrollUnit::Pixel,
                )
            } else {
                (
                    (event.scrollingDeltaX() / 3.0) as f32,
                    (event.scrollingDeltaY() / 3.0) as f32,
                    ScrollUnit::Line,
                )
            };
            // R02-10：保留系统的 `phase` / `momentumPhase`，引擎才能区分「手指在板上」与
            // 「惯性滑行」（Chrome 的 `WebMouseWheelEvent.phase` / `momentumPhase` 同一做法）。
            let phase = scroll_phase(event);
            self.note_modifiers(event.modifierFlags());
            self.push_event(Event::Input(InputEvent::Scroll {
                position,
                delta: Point::new(delta_x, delta_y),
                unit,
                phase,
                modifiers: self.modifiers(),
                timestamp: event_timestamp(event),
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

    // `replacementRange` 是**文档坐标**（与 `markedRange` 同一坐标系）；换算成组合串内相对
    // UTF-16 区间时用 `ImeState::marked_range().0` 作基准（见 `relative_replacement_range`）。
    // 已提交文本的重转换（韩文 Hanja / 日文再変換）还需要 `attributedSubstringForProposedRange:`
    // 返回真实文档文本、`selectedRange` / `markedRange` 返回文档坐标：当前 view 只维护组合串，
    // 因此指向已提交文本的区间无法表达，会被 clamp 到组合串边界（review mac-2 的范围；
    // surrounding text 通路是后续工作）。
    //
    // SAFETY: `NSTextInputClient` 的每个方法都是 AppKit 在主线程同步调用的；
    // 实现里只碰 `ImeState` 与 view 的事件队列（都没有跨线程访问），签名由 objc2 编译期校验。
    unsafe impl NSTextInputClient for GuiView {
        #[unsafe(method(insertText:replacementRange:))]
        // SAFETY: AppKit 只在本 view 是第一响应者时于主线程调用；参数按 ObjC 约定保证非空。
        unsafe fn insert_text_replacement_range(&self, text: &AnyObject, range: NSRange) {
            let value = object_text(text);
            if value.is_empty() {
                return;
            }
            // `replacementRange` 是 UTF-16 文档区间（`{NSNotFound, 0}` = 替换当前组合串）。
            // 必须在 `clear_marked` 之前换算：clamp 上界是**提交前**的组合串。
            let replace = self.relative_replacement_range(range);
            let first = self.ivars().ime.borrow_mut().note_commit(&value);
            self.ivars().ime.borrow_mut().clear_marked();
            // 只有「真的交付了正文」才抑制本按键的 `Text`；重复回调（`first == false`）时
            // 同一次按键的正文已经交付过，抑制标记仍然有效。
            if first {
                self.ivars().suppress_text.set(true);
                self.push_event(Event::Ime(ImeEvent::Commit {
                    text: smol_str::SmolStr::new(value),
                    replace,
                }));
            }
        }

        #[unsafe(method(doCommandBySelector:))]
        // SAFETY: AppKit 只在本 view 是第一响应者时于主线程调用；参数按 ObjC 约定保证非空。
        unsafe fn do_command_by_selector(&self, _selector: Sel) {
            // 命令选择器（方向键 / 回车等）已由 `keyDown:` 转成 `Key` 事件；这里吞掉。
        }

        #[unsafe(method(setMarkedText:selectedRange:replacementRange:))]
        // SAFETY: AppKit 只在本 view 是第一响应者时于主线程调用；参数按 ObjC 约定保证非空。
        unsafe fn set_marked_text(&self, text: &AnyObject, selected: NSRange, replacement: NSRange) {
            let value = object_text(text);
            let utf16: Vec<u16> = value.encode_utf16().collect();
            // 只替换当前组合串的一部分（逐子句更新 / 重转换）时，`replacementRange` 是
            // 文档坐标；`{NSNotFound, 0}` 由纯函数换算成 `None`（替换整个组合串）。
            let replace = self.relative_replacement_range(replacement);
            {
                let mut ime = self.ivars().ime.borrow_mut();
                ime.set_marked(&utf16, (selected.location, selected.length));
            }
            self.push_event(Event::Ime(ImeEvent::Preedit {
                text: smol_str::SmolStr::new(value),
                replace,
            }));
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
                // AppKit 的「无范围」是 `NSNotFound`（`NSIntegerMax`），不是 `NSUInteger::MAX`：
                // 写错会让 AppKit 当成一个巨大的有效 location（第三轮复审 M2，与 ime.rs 同源）。
                return NSRange::new(isize::MAX as usize, 0);
            }
            let (location, length) = ime.selected_range();
            NSRange::new(location, length)
        }

        #[unsafe(method(markedRange))]
        fn marked_range(&self) -> NSRange {
            let ime = self.ivars().ime.borrow();
            if !ime.has_marked() {
                // AppKit 的「无范围」是 `NSNotFound`（`NSIntegerMax`），不是 `NSUInteger::MAX`：
                // 写错会让 AppKit 当成一个巨大的有效 location（第三轮复审 M2，与 ime.rs 同源）。
                return NSRange::new(isize::MAX as usize, 0);
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
        // 失焦复位瞬时输入状态（P2-4）：修饰键由 `flagsChanged:` 维护，窗口不再是 key
        // window 时这些事件不再到达，用户松开的 Cmd/Shift 会永久"粘住"，之后点回窗口的
        // 第一次点击就被当成组合键。悬停同理：指针离开时 `mouseExited:` 也会被吞掉。
        if !focused {
            self.ivars().modifiers.set(0);
        }
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

    /// `replacementRange`（AppKit **文档坐标**）→ 组合串内相对 UTF-16 区间。
    ///
    /// AppKit 的 `replacementRange` 与 `markedRange` 使用同一文档坐标系，因此换算基准是
    /// `ImeState::marked_range().0` —— 也就是回给 AppKit 的 `markedRange.location`。该值必须
    /// 与引擎真实文本里组合串的起点一致，否则替换区间会整体偏移。当前 view 把组合串暴露为
    /// 整个文本存储（没有 surrounding text），起点恒为 0；将来接通
    /// `attributedSubstringForProposedRange:` 后由 `ImeState` 返回真实起点，本函数与
    /// `platform::ime::replacement_range` 都不用改。
    fn relative_replacement_range(&self, range: NSRange) -> Option<std::ops::Range<usize>> {
        let ime = self.ivars().ime.borrow();
        let (marked_location, marked_length) = ime.marked_range();
        // `markedRange` 与 `marked_utf16` 必须描述同一段组合串：前者是 AppKit 看到的范围，
        // 后者是换算时的 clamp 上界/代理对吸附来源。
        debug_assert_eq!(
            marked_length,
            ime.marked_utf16().len(),
            "markedRange 必须与 marked_utf16 描述同一段组合串"
        );
        debug_assert_eq!(
            marked_location, 0,
            "view 目前只把组合串暴露为文本存储，markedRange.location 必须为 0，\
             且与引擎真实文本里组合串的起点一致；接 surrounding text 后改为真实起点"
        );
        replacement_range(
            range.location,
            range.length,
            ime.marked_utf16(),
            marked_location,
        )
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
        let timestamp = event_timestamp(event);
        let input = match phase {
            PointerPhase::Move => InputEvent::PointerMove {
                position,
                modifiers,
                timestamp,
            },
            PointerPhase::Down(button) => InputEvent::PointerDown {
                position,
                button,
                modifiers,
                timestamp,
            },
            PointerPhase::Up(button) => InputEvent::PointerUp {
                position,
                button,
                modifiers,
                timestamp,
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

/// `NSEvent.keyCode`（**布局无关**的硬件码）→ 引擎 [`KeyCode`]（W3C `KeyboardEvent.code`）。
///
/// 与 Win32 用扫描码、Wayland 用 evdev 码同一个理由：`keyCode` 是物理键位，键盘布局（US /
/// AZERTY / 俄语）不改变它，因此快捷键表可以跨布局、跨平台共用（R02-1）。用
/// `NSEvent.characters` 拿到的才是布局相关字符，那条通路走 [`InputEvent::Text`]。
///
/// 未覆盖的硬件码返回 [`KeyCode::Unidentified`]（载荷是原生 `keyCode`，**不可跨平台**）。
fn key_code(code: u16) -> KeyCode {
    match code {
        0x00 => KeyCode::KeyA,
        0x01 => KeyCode::KeyS,
        0x02 => KeyCode::KeyD,
        0x03 => KeyCode::KeyF,
        0x04 => KeyCode::KeyH,
        0x05 => KeyCode::KeyG,
        0x06 => KeyCode::KeyZ,
        0x07 => KeyCode::KeyX,
        0x08 => KeyCode::KeyC,
        0x09 => KeyCode::KeyV,
        0x0A => KeyCode::IntlBackslash,
        0x0B => KeyCode::KeyB,
        0x0C => KeyCode::KeyQ,
        0x0D => KeyCode::KeyW,
        0x0E => KeyCode::KeyE,
        0x0F => KeyCode::KeyR,
        0x10 => KeyCode::KeyY,
        0x11 => KeyCode::KeyT,
        0x12 => KeyCode::Digit1,
        0x13 => KeyCode::Digit2,
        0x14 => KeyCode::Digit3,
        0x15 => KeyCode::Digit4,
        0x16 => KeyCode::Digit6,
        0x17 => KeyCode::Digit5,
        0x18 => KeyCode::Equal,
        0x19 => KeyCode::Digit9,
        0x1A => KeyCode::Digit7,
        0x1B => KeyCode::Minus,
        0x1C => KeyCode::Digit8,
        0x1D => KeyCode::Digit0,
        0x1E => KeyCode::BracketRight,
        0x1F => KeyCode::KeyO,
        0x20 => KeyCode::KeyU,
        0x21 => KeyCode::BracketLeft,
        0x22 => KeyCode::KeyI,
        0x23 => KeyCode::KeyP,
        0x24 => KeyCode::Enter,
        0x25 => KeyCode::KeyL,
        0x26 => KeyCode::KeyJ,
        0x27 => KeyCode::Quote,
        0x28 => KeyCode::KeyK,
        0x29 => KeyCode::Semicolon,
        0x2A => KeyCode::Backslash,
        0x2B => KeyCode::Comma,
        0x2C => KeyCode::Slash,
        0x2D => KeyCode::KeyN,
        0x2E => KeyCode::KeyM,
        0x2F => KeyCode::Period,
        0x30 => KeyCode::Tab,
        0x31 => KeyCode::Space,
        0x32 => KeyCode::Backquote,
        0x33 => KeyCode::Backspace,
        0x35 => KeyCode::Escape,
        0x36 => KeyCode::MetaRight,
        0x37 => KeyCode::MetaLeft,
        0x38 => KeyCode::ShiftLeft,
        0x39 => KeyCode::CapsLock,
        0x3A => KeyCode::AltLeft,
        0x3B => KeyCode::ControlLeft,
        0x3C => KeyCode::ShiftRight,
        0x3D => KeyCode::AltRight,
        0x3E => KeyCode::ControlRight,
        0x3F => KeyCode::Fn,
        0x40 => KeyCode::F17,
        0x41 => KeyCode::NumpadDecimal,
        0x42 => KeyCode::NumpadMultiply,
        0x43 => KeyCode::NumpadAdd,
        0x47 => KeyCode::NumLock,
        0x4B => KeyCode::NumpadDivide,
        0x4C => KeyCode::NumpadEnter,
        0x4E => KeyCode::NumpadSubtract,
        0x4F => KeyCode::F18,
        0x50 => KeyCode::F19,
        0x51 => KeyCode::NumpadEqual,
        0x52 => KeyCode::Numpad0,
        0x53 => KeyCode::Numpad1,
        0x54 => KeyCode::Numpad2,
        0x55 => KeyCode::Numpad3,
        0x56 => KeyCode::Numpad4,
        0x57 => KeyCode::Numpad5,
        0x58 => KeyCode::Numpad6,
        0x59 => KeyCode::Numpad7,
        0x5A => KeyCode::F20,
        0x5B => KeyCode::Numpad8,
        0x5C => KeyCode::Numpad9,
        0x5D => KeyCode::IntlYen,
        0x5E => KeyCode::IntlRo,
        0x5F => KeyCode::NumpadComma,
        0x60 => KeyCode::F5,
        0x61 => KeyCode::F6,
        0x62 => KeyCode::F7,
        0x63 => KeyCode::F3,
        0x64 => KeyCode::F8,
        0x65 => KeyCode::F9,
        0x67 => KeyCode::F11,
        0x69 => KeyCode::F13,
        0x6A => KeyCode::F16,
        0x6B => KeyCode::F14,
        0x6D => KeyCode::F10,
        0x6E => KeyCode::ContextMenu,
        0x6F => KeyCode::F12,
        0x71 => KeyCode::F15,
        0x72 => KeyCode::Help,
        0x73 => KeyCode::Home,
        0x74 => KeyCode::PageUp,
        0x75 => KeyCode::Delete,
        0x76 => KeyCode::F4,
        0x77 => KeyCode::End,
        0x78 => KeyCode::F2,
        0x79 => KeyCode::PageDown,
        0x7A => KeyCode::F1,
        0x7B => KeyCode::ArrowLeft,
        0x7C => KeyCode::ArrowRight,
        0x7D => KeyCode::ArrowDown,
        0x7E => KeyCode::ArrowUp,
        other => KeyCode::Unidentified(other),
    }
}

/// 事件时刻 → 引擎单调时钟（R02-14）。
///
/// `NSEvent.timestamp` 是「系统启动起的秒数」，与 `NSProcessInfo.systemUptime` 同源；`Instant`
/// 的原点未定义，所以按「现在 −（现在 uptime − 事件 uptime）」回推（[`timestamp_from_ticks`]）。
/// 取不到 uptime 时退化为分发时刻（不 panic）。
fn event_timestamp(event: &NSEvent) -> Instant {
    let now = Instant::now();
    let now_ms = (NSProcessInfo::processInfo().systemUptime().max(0.0) * 1000.0) as u64;
    let event_ms = (event.timestamp().max(0.0) * 1000.0) as u64;
    timestamp_from_ticks(now, now_ms, event_ms)
}

/// AppKit 的滚动阶段（`phase` 优先，其次是 `momentumPhase`）→ 引擎 [`ScrollPhase`]。
///
/// 惯性阶段必须单独表达：`phase` 为空而 `momentumPhase` 有值就是「手指已离开触控板」，
/// 丢了它引擎只能把惯性当成新的手势（R02-10）。
fn scroll_phase(event: &NSEvent) -> ScrollPhase {
    let momentum = event.momentumPhase();
    if momentum.contains(NSEventPhase::Cancelled) {
        return ScrollPhase::Cancel;
    }
    if !momentum.is_empty() {
        return ScrollPhase::Momentum;
    }
    let phase = event.phase();
    if phase.contains(NSEventPhase::Cancelled) {
        ScrollPhase::Cancel
    } else if phase.contains(NSEventPhase::Began) || phase.contains(NSEventPhase::MayBegin) {
        ScrollPhase::Begin
    } else if phase.contains(NSEventPhase::Ended) {
        ScrollPhase::End
    } else if phase.contains(NSEventPhase::Changed) || phase.contains(NSEventPhase::Stationary) {
        ScrollPhase::Update
    } else {
        ScrollPhase::Discrete
    }
}

#[cfg(test)]
mod tests {
    use objc2::{AnyThread, msg_send, rc::Retained, runtime::AnyObject};
    use objc2_app_kit::NSUnderlineStyleAttributeName;
    use objc2_foundation::{NSMutableAttributedString, NSNumber, NSRange, NSString};

    use super::{key_code, object_text};
    use crate::event::KeyCode;

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

    /// R02-1：`NSEvent.keyCode` 是**布局无关**的硬件码，映射到 W3C `code` 语义的物理键身份。
    ///
    /// 同一物理键在三个平台都得到同一个 `KeyCode`（macOS `kVK_ANSI_S` = 1 ⇒ `KeyS`；Windows
    /// 扫描码 0x1F ⇒ `KeyS`；Linux `KEY_S` ⇒ `KeyS`），因此 `KeyBinding` 只需一张表。
    #[test]
    fn keycodes_map_to_physical_identities() {
        assert_eq!(key_code(0x01), KeyCode::KeyS);
        assert_eq!(key_code(0x00), KeyCode::KeyA);
        assert_eq!(key_code(0x12), KeyCode::Digit1);
        assert_eq!(key_code(0x24), KeyCode::Enter);
        assert_eq!(key_code(0x4C), KeyCode::NumpadEnter);
        assert_eq!(key_code(0x7A), KeyCode::F1);
        assert_eq!(key_code(0x7E), KeyCode::ArrowUp);
        assert_eq!(key_code(0x38), KeyCode::ShiftLeft);
        assert_eq!(key_code(0x3C), KeyCode::ShiftRight);
        // 未覆盖的硬件码：载荷是原生 keyCode，文档明确**不可跨平台**。
        assert_eq!(key_code(0xFFFF), KeyCode::Unidentified(0xFFFF));
    }
}
