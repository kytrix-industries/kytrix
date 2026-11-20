//! `NSView` 子类：`CAMetalLayer` 宿主 + 输入收集 + `NSTextInputClient` + NSAccessibility +
//! 主菜单的 responder 动作。
//!
//! 纪律：
//! - **平台回调不碰 `App`** —— 事件只 push 进 `ViewIvars.events`，泵在主线程 drain 后 dispatch
//!   （与 Win32 `WndProc` 同一职责边界）。菜单动作也一样：它们是**事件**（等价物理按键），
//!   不是状态改写。
//! - **IME 组合态只由 `NSTextInputClient` 的四个回调驱动**（`setMarkedText:` / `unmarkText:` /
//!   `insertText:` / 焦点变化）。`flagsChanged:` 只维护修饰键位图，绝不替输入法做「放弃组合」的
//!   决定（R09-2：那会让日文/韩文再変換、Shift 临时切换丢字串字）。
//! - **`Text` 抑制与提交去重是按键作用域状态**（R09-3）：`keyDown:` 开一个作用域，`insertText:`
//!   只在作用域内记账；作用域外（鼠标点候选窗、听写、表情面板）的提交**不会**影响下一次按键。
//! - **裸 `Key` 在组合期不可见**（B4）：判定由平台无关的 `platform::ime::key_visible` 统一
//!   给出，与 Win32 的 `VK_PROCESSKEY` 过滤对齐 —— 组合中 / 被输入法消费的按键不投递，
//!   带 Cmd/Ctrl/Option 的命令组合始终投递（见 `key_down` / `key_up`）。

use std::{
    cell::{Cell, RefCell},
    ptr::NonNull,
    rc::Rc,
    sync::Arc,
    time::Instant,
};

use objc2::{
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::Retained,
    runtime::{AnyObject, ProtocolObject},
};
use objc2_app_kit::{
    NSCursor, NSEvent, NSEventModifierFlags, NSEventPhase, NSTextInputClient, NSTextInputContext,
    NSTrackingArea, NSTrackingAreaOptions, NSUserInterfaceValidations,
    NSValidatedUserInterfaceItem, NSView,
};
use objc2_foundation::{
    NSArray, NSAttributedString, NSAttributedStringKey, NSPoint, NSProcessInfo, NSRange, NSRect,
    NSSize, NSString, NSUInteger,
};

use crate::{
    event::{
        Event, GestureFlags, GesturePhase, ImeEvent, InputEvent, KeyCode, KeyState, Modifiers,
        PointerButton, ScrollPhase, modifier_key_transitions, scroll_delta, should_emit_text,
        timestamp_from_ticks,
    },
    math::{Point, Rect},
    paint::style::Cursor,
    platform::{
        dispatcher::Dispatcher,
        ime::{CommitVerdict, KeyScopeToken, key_visible, replacement_range},
        macos::{a11y::MacA11y, ime::ImeState, menu},
        macos_close::CloseBridge,
        macos_drag::{WindowDragState, drag_region_at},
        menu::{EditAvailability, MenuCommand},
    },
};

/// `GuiView` 的实例变量（主线程独占）。
pub(crate) struct ViewIvars {
    /// 待泵消费的平台事件（**不**在这里调 `App`）。
    events: RefCell<Vec<Event>>,
    /// 关闭桥（B5）：`performClose:` 置位、泵消费；与 `WindowDelegate` 的
    /// `windowShouldClose:` / `windowWillClose:` 共享同一份状态（同一窗口只有一个真值）。
    close: Rc<CloseBridge>,
    /// 唤醒泵：`performClose:` 只置位关闭桥，裁决与真正的 `NSWindow.close()` 在泵里做；
    /// 不唤醒的话请求要等到下一个 run loop 事件才会被处理（关闭会「延迟一拍」）。
    dispatcher: Arc<Dispatcher>,
    /// IME 组合状态 + 按键作用域的提交记账（R09-3）。
    ime: RefCell<ImeState>,
    /// a11y adapter（首显前构造；`None` = 尚未安装 / 已卸载）。
    a11y: RefCell<Option<MacA11y>>,
    /// 当前修饰键位图（`flagsChanged:` 维护）。
    modifiers: Cell<u8>,
    /// IME caret 区域（窗口逻辑坐标；`None` = 无 hint）。
    ime_cursor: Cell<Option<Rect>>,
    /// 箭头 `cursorUpdate:` / `mouseEntered:` 重新 apply 它，R09-11）。
    cursor: Cell<Cursor>,
    /// 指针是否在 view 内（决定 `set_cursor` 能否立刻 `NSCursor::set`）。
    pointer_inside: Cell<bool>,
    /// 应用是否允许该窗口使用 IME（B6；默认启用）。`false` ⇒ [`GuiView::input_context`]
    /// 返回 `nil`，按下时 `keyDown:`/`keyUp:` 里的 `inputContext()` 也是 `None`，按键因此
    /// 走普通 `characters` 通路（`InputEvent::Text`），不再被路由给输入法。
    ///
    /// 唯一写者是泵里的 `NativeWindow::sync_ime_enabled`（目标状态来自
    /// `App::ime_enabled`）；平台回调只读，不在回调里改它。
    ime_enabled: Cell<bool>,
    /// Edit 菜单项的可用性快照（泵每轮由 `sync_ime_cursor_area` 写入）：`validateUserInterfaceItem:`
    /// 与菜单动作（`run_menu_command`）据此决定启用/上抛（R09-1 收尾 / AR3 P2-11）。
    ///
    /// 存快照而不是「文本编辑态」一个 bool：启用判据是「文本编辑态 **且** 该命令的按键在引擎
    /// 侧有接收者」，而平台回调里拿不到 `App`（见文件头的纪律）。唯一生产者是
    /// `App::edit_menu_availability`。
    edit: Cell<EditAvailability>,
    /// 自绘标题栏拖动区（X2；逻辑窗口坐标、客户区左上原点、y 向下 —— view 是 flipped，
    /// 与 [`GuiView::logical_point`] 同一坐标系）。唯一写者是泵
    /// （`NativeWindow::sync_drag_regions`，每轮对账、值变化才写）；`mouseDown:` 只读：
    /// 命中即把拖动交给 WindowServer 且**不**投递 `PointerDown`（拖动区是窗口 chrome）。
    drag_regions: RefCell<Vec<Rect>>,
    /// 拖动区命中后的主键指针事件配对状态（RB-08）：命中期间补发的 Up / Dragged 没有配对的
    /// Down，必须吞掉。状态机（含「何时清状态」）在平台无关的
    /// [`crate::platform::macos_drag::WindowDragState`]，这里只存当前值。
    window_drag: Cell<WindowDragState>,
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

        /// `NSView.inputContext` 的覆盖（B6）：禁用 IME 时返回 `nil`。
        ///
        /// AppKit 在把按键路由给输入法之前会向第一响应者要输入上下文；返回 `nil` ⇒ 按键
        /// 不经过输入法，`keyDown:` 里的 `characters` 直接产出 `InputEvent::Text`。这是
        /// WebKit `-[WKView inputContext]`「非 editable 返回 nil」的同一成熟做法
        /// （NSTextInputContext 没有公开的「摘除/恢复」API，覆盖 `inputContext` 是唯一
        /// 不依赖私有行为的开关）。
        ///
        /// 启用时必须调用 **super** 的默认实现取回 AppKit 维护的上下文：`NSView::inputContext`
        /// 是同一 selector 的 Rust 绑定，直接调用会自递归。
        ///
        /// `method_id`（而不是 `method`）是 `define_class!` 对**返回对象**的方法要求：
        /// 返回值走 `RetainedReturnValue`（`Option<Retained<_>>` 是合法返回），普通
        /// `method` 只接受 `Encode` 的值类型。
        #[unsafe(method_id(inputContext))]
        fn input_context(&self) -> Option<Retained<NSTextInputContext>> {
            // 注意：`method_id` 的返回值必须走**尾部表达式**（宏把函数体包成
            // `RetainedReturnValue` 转换），因此这里用 if/else 而不是提前 `return`。
            if self.ivars().ime_enabled.get() {
                // SAFETY: `inputContext` 是 NSView 的既有方法；`super(self)` 直接把消息发给
                // 父类实现（不会重新进入本覆盖），返回值按 `Option<Retained<_>>` 持有。
                unsafe { msg_send![super(self), inputContext] }
            } else {
                None
            }
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            self.note_modifiers(event.modifierFlags());
            let modifiers = self.modifiers();
            // R02-2：`NSEvent.isARepeat` 是 AppKit 的自动重复标志（长按产生的重复 `keyDown:`）。
            let repeat = event.isARepeat();
            // R02-14：事件时刻取 `NSEvent.timestamp`（系统启动起的秒），不是分发时刻。
            let timestamp = event_timestamp(event);
            // R09-3：**一次 `keyDown:` = 一个按键作用域**。`insertText:` 交付的正文去重与
            // `Text` 抑制都只在这个作用域内有效；守卫在作用域结束时清空全部记账（panic 也清），
            // 因此「鼠标点候选窗提交后吞掉下一个真实字符」在结构上不可能再发生。
            let ivars = self.ivars();
            let token = ivars.ime.borrow_mut().begin_key();
            let _scope = KeyScope {
                ime: &ivars.ime,
                token,
            };
            // **IME 先**：输入法可能吞掉这个键（组合/提交走 NSTextInputClient 回调）。
            // `handleEvent:` 的返回值是「输入上下文是否消费了本事件」的**权威**信号；丢掉它
            // 就只能靠 `has_marked()` 推断，这是 R09-3 的同源缺陷。
            let ime_handled = self
                .inputContext()
                .is_some_and(|context| context.handleEvent(event));
            let (composing, text_taken) = {
                let ime = ivars.ime.borrow();
                (ime.has_marked(), ime.takes_text(token))
            };
            // B4：`Key` 的可见性与 Win32 的 `VK_PROCESSKEY` 过滤对齐（三端共用
            // `platform::ime::key_visible`）：
            // - 组合期（`hasMarkedText`）或被输入上下文消费的按键**不投递**裸 `Key` —— 拼音/
            //   日语组合中的 B/S/数字若同时被应用看到，就会命中裸键绑定（下单误触级别）；
            // - 带 Cmd/Ctrl/Option 的命令组合**始终投递**：`Cmd+C` / `Cmd+W` 不能被吞；
            //   AppKit 的菜单键等价在 `keyDown:` 之前处理，物理按键照常投递不会二次触发
            //   菜单动作。Shift 不算命令修饰键（`Shift+字母` 是正常上屏输入）。
            let command_modifiers =
                modifiers.intersects(Modifiers::CONTROL | Modifiers::ALT | Modifiers::META);
            if key_visible(ime_handled, composing, command_modifiers) {
                self.push_event(Event::Input(InputEvent::Key {
                    code: key_code(event.keyCode()),
                    state: KeyState::Down,
                    repeat,
                    modifiers,
                    timestamp,
                }));
            }
            // `Text` 只在「本按键的正文没被 IME 交付」时才发，避免与 `ImeEvent::Commit` 重复插入。
            // 判据与 Linux 的 XKB 回退通路共用 `should_emit_text`（含「重复不得抑制文本」的回归）。
            if should_emit_text(
                KeyState::Down,
                repeat,
                ime_handled || text_taken,
                composing,
            ) && !modifiers.intersects(Modifiers::CONTROL | Modifiers::META)
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
            // 输入上下文也收 `keyUp:`：部分输入法在抬起阶段才结束组合/提交（Qt/Chromium 同样
            // 成对投递；只投 `keyDown:` 会让这些输入法的组合态悬空）。返回值只对按下有意义。
            if let Some(context) = self.inputContext() {
                context.handleEvent(event);
            }
            // B4/RB-02：抬起**始终**投递裸 `Key`（组合期也不例外）。
            //
            // 抬起不产生正文（`Text` 判定只认 Down），应用侧只拿它清 `held_keys`。组合期若把
            // 抬起也吞掉（首键按下 → 组合开始 → 抬起），应用的「按住」状态会永久残留
            // （`App::key_down` 长期 true）。Win32 同样只替换按下的 `VK_PROCESSKEY`，抬起以真
            // 实 `VK_*` 到达 —— 三端在这里语义对齐。`handleEvent:` 的返回值也不作为抬起判据：
            // 它只对按下有意义。
            let modifiers = self.modifiers();
            self.push_event(Event::Input(InputEvent::Key {
                code: key_code(event.keyCode()),
                state: KeyState::Up,
                // 抬起没有重复语义。
                repeat: false,
                modifiers,
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
            //
            // 这里**只维护修饰键位图**：绝不碰 `ImeState`。组合串的存在只由
            // `setMarkedText:`/`unmarkText:`/`insertText:` 决定（R09-2）；旧实现在「松开的
            // 修饰键不含 Shift」时清 marked text，于是组合中途松 CapsLock/Fn/Option 都会丢字，
            // 且下一次 `setMarkedText:` 的 `replacementRange` 换算基准被清成 0 → 串字。
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
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            // RB-08：`mouseMoved:` 只在**没有按键按下**时到达 ⇒ 它证明上一次拖动会话已经结束
            // （AppKit 可能始终没补发 `mouseUp:`）。在这里清掉配对状态，否则拖动结束后悬停
            // 会一直冻到下一次点击。悬停移动本身与拖动无关，照常投递。
            let mut drag = self.ivars().window_drag.get();
            drag.hover_move();
            self.ivars().window_drag.set(drag);
            self.push_pointer(event, PointerPhase::Move);
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            // RB-08：命中拖动区的那次按下把拖动交给了 WindowServer、没有投递 `PointerDown`；
            // AppKit 补发的主键 `mouseDragged:` 因此没有配对的 Down ⇒ 吞掉（拖动区是窗口
            // chrome，拖动期间不进入引擎指针分发）。
            if self.ivars().window_drag.get().is_dragging() {
                return;
            }
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
            // X2：自绘标题栏拖动区命中 ⇒ 同步交给 WindowServer 拖动，且**不**投递
            // `PointerDown`（不开始引擎指针会话 —— 拖动期间没有后续 mouseUp 保证）。
            //
            // RB-08：命中状态记进 `WindowDragState`，后续主键 Up / Dragged 据此吞掉；非命中
            // 的按下会清掉上一次拖动可能残留的状态（拖动区外的一次点击 = 正常指针会话）。
            let hit = self.begin_window_drag(event);
            let mut drag = self.ivars().window_drag.get();
            let deliver = drag.primary_down(hit);
            self.ivars().window_drag.set(drag);
            if deliver {
                self.push_pointer(event, PointerPhase::Down(PointerButton::Primary));
            }
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            // RB-08：拖动区消费的按下没有投递 Down，补发的 Up 同样没有配对 ⇒ 吞掉。
            let mut drag = self.ivars().window_drag.get();
            let deliver = drag.primary_up();
            self.ivars().window_drag.set(drag);
            if deliver {
                self.push_pointer(event, PointerPhase::Up(PointerButton::Primary));
            }
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
            self.ivars().pointer_inside.set(true);
            self.apply_cursor();
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
            self.ivars().pointer_inside.set(false);
            let position = self.logical_point(event.locationInWindow());
            self.note_modifiers(event.modifierFlags());
            self.push_event(Event::Input(InputEvent::PointerLeave {
                position,
                modifiers: self.modifiers(),
                timestamp: event_timestamp(event),
            }));
        }

        /// `NSTrackingArea` 的 `CursorUpdate` 回调（R09-11）。
        ///
        /// AppKit 的光标契约是「视图声明区域 → 光标」：`NSCursor::set()` 只是**瞬时**动作，
        /// 激活窗口 / 鼠标移动 / 视图重绘都会把它重置回 cursor rect 的结果。因此引擎请求的
        /// 形状必须能在 `cursorUpdate:` 里被重新 apply —— 这里做的正是这件事。
        #[unsafe(method(cursorUpdate:))]
        fn cursor_update(&self, _event: &NSEvent) {
            self.ivars().pointer_inside.set(true);
            self.apply_cursor();
        }

        /// 捏合缩放（R09-4）：`magnification` 是**本次增量**（不是累计），阶段由 `phase` 给出。
        #[unsafe(method(magnifyWithEvent:))]
        fn magnify(&self, event: &NSEvent) {
            let Some(phase) = gesture_phase(event) else {
                return;
            };
            self.note_modifiers(event.modifierFlags());
            self.push_event(Event::Input(InputEvent::Magnify {
                factor: event.magnification() as f32,
                phase,
                center: self.logical_point(event.locationInWindow()),
                modifiers: self.modifiers(),
                timestamp: event_timestamp(event),
            }));
        }

        /// 旋转手势（`rotation` 单位是**度**，同样是本次增量）。
        #[unsafe(method(rotateWithEvent:))]
        fn rotate(&self, event: &NSEvent) {
            let Some(phase) = gesture_phase(event) else {
                return;
            };
            self.note_modifiers(event.modifierFlags());
            self.push_event(Event::Input(InputEvent::Rotate {
                // `NSEvent.rotation` 本来就是 `c_float`（f32），不需要再 cast。
                degrees: event.rotation(),
                phase,
                modifiers: self.modifiers(),
                timestamp: event_timestamp(event),
            }));
        }

        /// 双指双击缩放（`smartMagnify:`）：离散手势，没有增量与阶段。
        #[unsafe(method(smartMagnifyWithEvent:))]
        fn smart_magnify(&self, event: &NSEvent) {
            self.note_modifiers(event.modifierFlags());
            self.push_event(Event::Input(InputEvent::SmartMagnify {
                center: self.logical_point(event.locationInWindow()),
                modifiers: self.modifiers(),
                timestamp: event_timestamp(event),
            }));
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            // R09-8：单位与 delta 由 `event::scroll_delta` 统一（精确设备 = Pixel 直传、
            // 传统滚轮 = Line 直传），平台层不再自造 `/10`、`/3` 归一化。
            let Some((delta, unit)) = scroll_delta(
                event.hasPreciseScrollingDeltas(),
                event.scrollingDeltaX(),
                event.scrollingDeltaY(),
            ) else {
                return;
            };
            let position = self.logical_point(event.locationInWindow());
            // R02-10：保留系统的 `phase` / `momentumPhase`，引擎才能区分「手指在板上」与
            // 「惯性滑行」（Chrome 的 `WebMouseWheelEvent.phase` / `momentumPhase` 同一做法）。
            let phase = scroll_phase(event);
            self.note_modifiers(event.modifierFlags());
            self.push_event(Event::Input(InputEvent::Scroll {
                position,
                delta,
                unit,
                phase,
                modifiers: self.modifiers(),
                timestamp: event_timestamp(event),
            }));
        }

        // ---- 主菜单的 responder 动作（R09-1）----
        //
        // 菜单项用 AppKit **标准 selector**（`copy:` / `paste:` / …）沿 responder chain 找目标：
        // 引擎是自绘控件，必须自己实现这些方法才参与这条通路（否则 Cmd+C 会「无响应」）。
        // 动作**不直接改状态**：它被翻译成与物理按键等价的 `Key` 事件，走 `App::dispatch`
        // 的绑定派发（菜单、快捷键、a11y 动作因此语义一致）。

        /// 标准关闭动作（B5）：红点重定向、`Cmd+W` 与菜单 Close 的 responder 路径都到这里。
        ///
        /// **不调用 `super`**：一旦走 `NSWindow.performClose:`，AppKit 会先问 delegate
        /// `windowShouldClose:`，返回 `false` 就播放系统提示音（Apple 文档明确），且关闭变成
        /// AppKit 的同步决定。这里只把请求交给关闭桥并唤醒泵，由泵统一走
        /// 「`Event::CloseRequested` → `App::on_close` 裁决 → `NSWindow.close()`」——与 Windows
        /// 同序、保留异步否决、无提示音。
        ///
        /// 红点的 target/action 在 `NativeWindow::create` 里被重定向到本 view（标准关闭按钮
        /// 默认发 `performClose:` 给窗口，会让 AppKit 走那条会响提示音的同步路径）。
        #[unsafe(method(performClose:))]
        fn perform_close(&self, _sender: Option<&AnyObject>) {
            if self.ivars().close.request() {
                self.ivars().dispatcher.wake();
            }
        }

        #[unsafe(method(undo:))]
        fn undo(&self, _sender: Option<&AnyObject>) {
            self.run_menu_command(MenuCommand::Undo);
        }

        #[unsafe(method(redo:))]
        fn redo(&self, _sender: Option<&AnyObject>) {
            self.run_menu_command(MenuCommand::Redo);
        }

        #[unsafe(method(cut:))]
        fn cut(&self, _sender: Option<&AnyObject>) {
            self.run_menu_command(MenuCommand::Cut);
        }

        #[unsafe(method(copy:))]
        fn copy(&self, _sender: Option<&AnyObject>) {
            self.run_menu_command(MenuCommand::Copy);
        }

        #[unsafe(method(paste:))]
        fn paste(&self, _sender: Option<&AnyObject>) {
            self.run_menu_command(MenuCommand::Paste);
        }

        #[unsafe(method(delete:))]
        fn delete(&self, _sender: Option<&AnyObject>) {
            self.run_menu_command(MenuCommand::Delete);
        }

        #[unsafe(method(selectAll:))]
        fn select_all(&self, _sender: Option<&AnyObject>) {
            self.run_menu_command(MenuCommand::SelectAll);
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

    // `NSUserInterfaceValidations`：Edit 菜单项的启用状态（R09-1）。AppKit 的菜单在
    // `autoenablesItems` 下会问 responder chain 上的第一个 `validateUserInterfaceItem:` 实现；
    // 没有它，编辑项只能靠「responder 实现了该 selector」判定为**恒启用** —— 于是没有文本焦点时
    // 点 Copy 会走一遍空动作，而 Cmd+C 也被菜单项吞掉（无法退回 keyDown 通路）。
    unsafe impl NSUserInterfaceValidations for GuiView {
        #[unsafe(method(validateUserInterfaceItem:))]
        fn validate_user_interface_item(
            &self,
            item: &ProtocolObject<dyn NSValidatedUserInterfaceItem>,
        ) -> bool {
            // `define_class!` 的方法体返回值会经 `ConvertReturn` 归一化（`bool` 的 `Inner` 是
            // objc2 的 `Bool`），方法体被塞进一个 `extern "C-unwind" fn(..) -> Bool` 里 ⇒
            // **不能**在这里写 `return true;`（那会从 `-> Bool` 的函数里返回 `bool`，E0308）。
            // 因此判定全部写成表达式（`?` 在这里也不可用，同理）。
            let command = item
                .action()
                .and_then(|action| action.name().to_str().ok())
                .and_then(MenuCommand::from_selector);
            match command {
                // Edit 菜单项只在「应用真的能执行它」时启用（R09-1 收尾 / AR3 P2-11）：
                // 没有文本编辑态、或该命令的按键在引擎侧没有接收者 ⇒ 置灰。置灰不只是诚实：
                // AppKit 对不可用的项**不消费**键等价物，Cmd+C 会退回 keyDown 通路，应用的
                // 绑定仍然收得到（见 `run_menu_command` 与文件头）。
                Some(command) => self.ivars().edit.get().allows(command),
                // 不是主菜单里的命令（应用自定义菜单项 / 系统 selector）：不干预。
                None => true,
            }
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
            // R09-3：提交记账带**作用域**。`DeliverInScope` = 本按键的正文已由 IME 交付
            // （同一次 `keyDown:` 的 `Text` 要抑制）；`DeliverOutsideScope` = 提交发生在
            // 按键之外（鼠标点候选窗 / 听写 / 粘贴）—— 只交付正文，绝不影响任何按键的 `Text`。
            let verdict = self.ivars().ime.borrow_mut().note_commit(&value);
            // 提交之后组合串一定结束（输入法不保证再发 `unmarkText:`）。
            self.ivars().ime.borrow_mut().clear_marked();
            match verdict {
                CommitVerdict::Duplicate => {}
                CommitVerdict::DeliverInScope | CommitVerdict::DeliverOutsideScope => {
                    self.push_event(Event::Ime(ImeEvent::Commit {
                        text: smol_str::SmolStr::new(value),
                        replace,
                    }));
                }
            }
        }

        #[unsafe(method(doCommandBySelector:))]
        // SAFETY: AppKit 只在本 view 是第一响应者时于主线程调用；参数按 ObjC 约定保证非空。
        unsafe fn do_command_by_selector(&self, _selector: objc2::runtime::Sel) {
            // 命令选择器（方向键 / 回车等）已由 `keyDown:` 转成 `Key` 事件；这里吞掉。
            // Esc（`cancelOperation:`）同理：组合态由输入法的下一次回调结束，不由平台层代劳。
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
            // `selectedRange` 就是组合串内的插入点（拼音串里用 ←/→ 改字时停在串中间）：
            // 文本层据此画插入点，否则光标恒在末尾。越界（含 `NSNotFound`）clamp 到串尾。
            let caret = selected.location.min(utf16.len()) as u32;
            self.push_event(Event::Ime(ImeEvent::Preedit {
                text: smol_str::SmolStr::new(value),
                caret: Some(caret),
                // AppKit 的可视子句由输入法自己的候选窗呈现；引擎文本层只需要插入点，
                // 组合串高亮暂不提供（`NSMarkedClauseSegment` 的解析是后续工作）。
                clauses: smallvec::SmallVec::new(),
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
            let local = match self.ivars().ime_cursor.get() {
                Some(area) => NSRect::new(
                    NSPoint::new(f64::from(area.origin.x), f64::from(area.origin.y)),
                    NSSize::new(
                        f64::from(area.size.width.max(1.0)),
                        f64::from(area.size.height.max(1.0)),
                    ),
                ),
                // 无 hint ⇒ 空矩形：AppKit 明确要求**屏幕坐标**，返回 view 局部坐标
                // （旧实现）会把候选窗丢到屏幕角落（R09-19）。空矩形用构造函数写：
                // 绑定里没有 `NSRect::ZERO`（`NSZeroRect` 是 extern static）。
                None => NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0)),
            };
            let in_window = self.convertRect_toView(local, None);
            self.window()
                .map(|window| window.convertRectToScreen(in_window))
                .unwrap_or(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0)))
        }

        #[unsafe(method(characterIndexForPoint:))]
        fn character_index_for_point(&self, _point: NSPoint) -> NSUInteger {
            // `NSNotFound`（`NSIntegerMax`）= 「无法映射到字符」；返回 0 会被输入法当成
            // 「第一个字符」（R09-19）。
            isize::MAX as usize
        }
    }
);

/// 指针事件的三相（内部用）。
enum PointerPhase {
    Move,
    Down(PointerButton),
    Up(PointerButton),
}

/// 按键作用域的 RAII 守卫：作用域结束时清空本按键的提交记账（R09-3）。
///
/// 不变式：`ImeState` 里「本按键的正文已交付」这件事的生命周期 ⊂ 一次 `keyDown:`。守卫保证
/// 无论正常返回还是 panic 展开，下一个按键都不会继承上一次的抑制位。
struct KeyScope<'a> {
    ime: &'a RefCell<ImeState>,
    token: KeyScopeToken,
}

impl Drop for KeyScope<'_> {
    fn drop(&mut self) {
        self.ime.borrow_mut().end_key(self.token);
    }
}

impl GuiView {
    /// 建 view（`frame` 先给 1×1，`NativeWindow` 随后 `setFrame`）。
    ///
    /// `dispatcher` 与 `close` 由 `NativeWindow` 持有并共享：关闭请求只置位关闭桥 + 唤醒泵，
    /// 真正的裁决/关闭在泵里做（平台回调不碰 `App` 的纪律，B5）。
    pub(crate) fn new(
        mtm: MainThreadMarker,
        dispatcher: Arc<Dispatcher>,
        close: Rc<CloseBridge>,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ViewIvars {
            events: RefCell::new(Vec::with_capacity(16)),
            close,
            dispatcher,
            ime: RefCell::new(ImeState::new()),
            a11y: RefCell::new(None),
            modifiers: Cell::new(0),
            ime_cursor: Cell::new(None),
            ime_enabled: Cell::new(true),
            cursor: Cell::new(Cursor::Default),
            pointer_inside: Cell::new(false),
            edit: Cell::new(EditAvailability::NONE),
            drag_regions: RefCell::new(Vec::new()),
            window_drag: Cell::new(WindowDragState::default()),
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
        // `CursorUpdate` 让 AppKit 在「需要更新光标」时回调 `cursorUpdate:`（R09-11）：
        // `NSCursor::set()` 是瞬时 API，激活/移动/重绘都会把它重置回 cursor rect 的结果，
        // 只有 tracking area 的这条通路能让引擎请求的形状**持续**生效。
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
                    | NSTrackingAreaOptions::CursorUpdate
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

    /// 卸下 a11y adapter（**窗口仍然存活时**调用，R09-7）。
    ///
    /// `accesskit_macos::Context::drop` 会对每个 platform node 发
    /// `NSAccessibilityPostNotification(.., UIElementDestroyed)`，AppKit 会**同步回查**元素的
    /// parent/frame；而 accesskit 的 `to_ns_rect` 对 `view.window()` 做了 `unwrap()`。若 adapter
    /// 随 view 的 `dealloc` 析构（旧行为），那一刻 `NSWindow` 已经释放、`window` 为 nil ⇒ panic
    /// 穿过 ObjC 帧 = `abort()`；即便不 panic，`accessibilityChildren` 也会 `borrow_mut` 正在
    /// `drop_in_place` 的 ivars（UB）。因此关窗顺序必须是：卸 a11y → 关窗 → 释放 view。
    pub(crate) fn uninstall_a11y(&self) {
        // 先取出再 drop：drop 期间的 AppKit 回查看到的是 `None`（返回空，不 panic）。
        let adapter = self.ivars().a11y.borrow_mut().take();
        drop(adapter);
    }

    /// 交付一笔 a11y 更新（泵每轮调用；`a11y_pending` 是 O(1) 过滤）。
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
            self.ivars().pointer_inside.set(false);
        }
        let mut adapter = self.ivars().a11y.take();
        if let Some(a11y) = adapter.as_mut() {
            a11y.focus_changed(focused);
        }
        *self.ivars().a11y.borrow_mut() = adapter;
    }

    /// 应用 IME 目标状态（B6；唯一调用点是泵里的 `NativeWindow::sync_ime_enabled`）。
    ///
    /// 禁用：先丢弃进行中的组合、清平台侧 IME 状态（返回值语义见
    /// [`Self::reset_ime_composition`]），**再**翻标志位 —— 顺序不能反：一旦
    /// `inputContext` 开始返回 `nil`，就拿不到上下文去 `discardMarkedText`，输入法那边的
    /// 转换会话会悬空。启用只翻标志：下一次按键自然取回 super 的上下文。
    ///
    /// 返回 `true` = 有组合被丢弃且需要调用方补一条 `ImeEvent::End`（与焦点移走路径共用
    /// 同一收尾判据）。
    pub(crate) fn apply_ime_enabled(&self, enabled: bool) -> bool {
        if enabled {
            self.ivars().ime_enabled.set(true);
            return false;
        }
        let needs_end = self.reset_ime_composition();
        self.ivars().ime_enabled.set(false);
        needs_end
    }

    /// 丢弃进行中的 IME 组合并清掉平台侧 IME 缓存（B6/B9 的统一清场路径：禁用 IME /
    /// 窗口失焦）。
    ///
    /// 两步：① 让 AppKit 丢弃它那边的转换会话（`NSTextInputContext.discardMarkedText`，
    /// 公开 API，WebKit/Qt 同款）；② 清引擎侧的 `ImeState` 与 caret 缓存。返回 `true` =
    /// 需要调用方补一条 `ImeEvent::End` —— `discardMarkedText` 若同步回调了
    /// `unmarkText:`（该回调已经 push 过 `End`），这里就不再补，**End 恰好一条**。
    ///
    /// caret 缓存是 AppKit 摆候选窗的依据（`firstRectForCharacterRange:`）；失焦/禁用不是
    /// 「caret 移到别处」，清场后必须回到空矩形，否则下一次组合会拿旧插入点摆候选窗。
    pub(crate) fn reset_ime_composition(&self) -> bool {
        self.ivars().ime_cursor.set(None);
        if !self.ivars().ime.borrow().has_marked() {
            return false;
        }
        if let Some(context) = self.inputContext() {
            context.discardMarkedText();
        }
        let mut ime = self.ivars().ime.borrow_mut();
        let still_composing = ime.has_marked();
        ime.clear_marked();
        still_composing
    }

    /// 取走本轮的平台事件（泵唯一读点）。
    pub(crate) fn take_events(&self) -> Vec<Event> {
        std::mem::take(&mut *self.ivars().events.borrow_mut())
    }

    /// 保存 IME caret 区域（窗口逻辑坐标；`None` = 无 hint，`firstRect` 回退空矩形）。
    pub(crate) fn set_ime_cursor_area(&self, area: Option<Rect>) {
        self.ivars().ime_cursor.set(area);
    }

    /// 写入 Edit 菜单项的可用性快照（来源：`App::edit_menu_availability`；泵每轮调用一次）。
    ///
    /// `validateUserInterfaceItem:` 与 `run_menu_command` 都读这一份，因此「菜单可点但没有
    /// 动作」在结构上不可能出现（R09-1 收尾 / AR3 P2-11）。
    pub(crate) fn set_edit_availability(&self, availability: EditAvailability) {
        self.ivars().edit.set(availability);
    }

    /// 引擎请求的光标形状（R09-11）。指针在窗口内时立刻生效；否则等 `cursorUpdate:`
    /// / `mouseEntered:` 重新 apply。
    pub(crate) fn set_cursor(&self, cursor: Cursor) {
        self.ivars().cursor.set(cursor);
        if self.ivars().pointer_inside.get() {
            self.apply_cursor();
        }
    }

    /// 保存自绘标题栏拖动区（X2；唯一调用点是泵里的 `NativeWindow::sync_drag_regions`）。
    ///
    /// 值不变时零写入（拖动区只在布局变化后由应用更新，泵每轮对账但绝大多数轮次相同）。
    pub(crate) fn set_drag_regions(&self, regions: &[Rect]) {
        let mut slot = self.ivars().drag_regions.borrow_mut();
        if slot.as_slice() == regions {
            return;
        }
        slot.clear();
        slot.extend_from_slice(regions);
    }

    /// `mouseDown:` 的拖动区分支（X2）：命中任一区域 ⇒ `performWindowDragWithEvent:`。
    ///
    /// 返回 `true` = 本次按下已被拖动区消费（调用方不得再投递 `PointerDown`）。拖动由
    /// WindowServer 同步接管（Apple 文档：可参与 Space 切换等系统行为；调用立即返回，可能
    /// 不再收到 mouseUp）—— 因此绝不能给应用发一个没有后续 `PointerUp` 的按下会话。
    ///
    /// 返回 `false` 有两种情形：没命中拖动区；或命中了但窗口已消失（无法发起原生拖动）——
    /// 后者必须回退成普通点击（投递 `PointerDown`），否则这次点击被静默吃掉。配对状态机
    /// （[`WindowDragState`]）据此区分「命中且已拖动」与「命中但没拖动」。
    fn begin_window_drag(&self, event: &NSEvent) -> bool {
        let position = self.logical_point(event.locationInWindow());
        // 借用在判定后立即结束，避免 `performWindowDragWithEvent:` 期间 AppKit 回调进来
        // （例如拖动触发窗口状态变化）时 `RefCell` 仍被借用。
        let hit = drag_region_at(&self.ivars().drag_regions.borrow(), position).is_some();
        if !hit {
            return false;
        }
        let Some(window) = self.window() else {
            return false;
        };
        window.performWindowDragWithEvent(event);
        true
    }

    /// 把光标形状交给 AppKit（`NSCursor::set`：cursor rect / `cursorUpdate:` 之外唯一有效的
    /// 即时通路，且会被下一次 cursor 更新覆盖 —— 因此必须在每次 `cursorUpdate:` 里重放）。
    fn apply_cursor(&self) {
        ns_cursor(self.ivars().cursor.get()).set();
    }

    /// 菜单动作 → 引擎事件，或在引擎不处理时沿 responder chain 上抛（R09-1）。
    ///
    /// 为什么不在平台层直接「复制/粘贴」：引擎的编辑语义由应用绑定与文本输入层拥有，平台层只
    /// 负责把菜单点击翻译成同一个按键序列，因此菜单、Cmd+C、a11y 动作三条通路完全一致
    /// （不会出现「菜单能粘贴、快捷键不能」）。
    ///
    /// 判据与 `validateUserInterfaceItem:` 完全一致（同一份 [`EditAvailability`] 快照，AR3
    /// P2-11）：没有文本编辑态、或应用没绑定该命令的按键 ⇒ 动作转给 `nextResponder`
    /// （AppKit 的 responder chain 语义），而不是**静默吞掉**命令 —— 后者正是「菜单项可点却
    /// 什么都不发生」的成因。AppleScript / `sendAction:` 可以绕过校验直接打进来，所以这里也
    /// 必须再判一次。
    fn run_menu_command(&self, command: MenuCommand) {
        let Some((code, modifiers)) = command.engine_key() else {
            return;
        };
        if !self.ivars().edit.get().allows(command) {
            self.forward_to_next_responder(command);
            return;
        }
        // 菜单点击不是 NSEvent：时间戳取分发时刻（双击/连击判定与文本编辑都与它无关）。
        let timestamp = Instant::now();
        for state in [KeyState::Down, KeyState::Up] {
            self.push_event(Event::Input(InputEvent::Key {
                code,
                state,
                repeat: false,
                modifiers,
                timestamp,
            }));
        }
    }

    /// 把一条菜单命令转给 responder chain 上的下一个响应者（等价 `[[self nextResponder]
    /// tryToPerform:_cmd with:sender]`）。
    fn forward_to_next_responder(&self, command: MenuCommand) {
        // SAFETY: objc2 把 `nextResponder` 标为 `unsafe`（响应链上的对象类型由运行时决定，
        // 绑定无法静态保证）；这里只取指针并立刻用 `NSResponder` 自己的 `tryToPerform:with:`
        // 转发，不假设下一个响应者的具体类型。
        let Some(next) = (unsafe { self.nextResponder() }) else {
            return;
        };
        // SAFETY: 同上：`next` 是 AppKit 保证存活到下一次 run loop 的响应者对象，
        // `tryToPerform:with:` 对它与它之后的链是同步、安全的调用；`object` 传 `None`
        //（菜单命令没有 sender 语义需求）。返回值只表示「链上有响应者处理了」，引擎不据此
        // 做决策（没有处理者时保持安静，不触发 `noResponderFor:` 的 beep —— 自绘控件不应因
        // 程序化调用而响铃）。
        let _ = unsafe { next.tryToPerform_with(menu::selector(command), None) };
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

/// 引擎 [`Cursor`] → `NSCursor`（R09-11）。
///
/// 全部用**非废弃**类方法：`resizeLeftRightCursor` / `resizeUpDownCursor` 在 macOS 15 已被
/// 标记 deprecated（消息明确指向 `columnResizeCursorInDirections:` / `frameResizeCursorFromPosition:inDirections:`），
/// 而 `columnResizeCursor()` / `rowResizeCursor()` 是"拖动分隔条"场景的**官方替代**且未废弃，
/// 也不需要额外 feature（`columnResizeCursorInDirections:` 要 `NSDirection` feature + macOS 15）。
/// 其余（`arrowCursor` / `pointingHandCursor` / `IBeamCursor` / `operationNotAllowedCursor` /
/// `openHandCursor` / `closedHandCursor`）都不在绑定文件的 deprecated 段里。
fn ns_cursor(cursor: Cursor) -> Retained<NSCursor> {
    match cursor {
        Cursor::Default => NSCursor::arrowCursor(),
        Cursor::Pointer => NSCursor::pointingHandCursor(),
        Cursor::Text => NSCursor::IBeamCursor(),
        Cursor::NotAllowed => NSCursor::operationNotAllowedCursor(),
        Cursor::Grab => NSCursor::openHandCursor(),
        Cursor::Grabbing => NSCursor::closedHandCursor(),
        Cursor::ResizeHorizontal => NSCursor::columnResizeCursor(),
        Cursor::ResizeVertical => NSCursor::rowResizeCursor(),
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

/// 手势事件的阶段（`NSEvent.phase`；R09-4）。
///
/// `None` = 本事件没有可交付的阶段（`NSEventPhaseMayBegin`：手势**可能**开始，还没有增量）。
/// 归一化规则在平台无关的 [`GestureFlags::phase`] 里，可在开发机上单测。
fn gesture_phase(event: &NSEvent) -> Option<GesturePhase> {
    let phase = event.phase();
    GestureFlags {
        may_begin: phase.contains(NSEventPhase::MayBegin),
        began: phase.contains(NSEventPhase::Began),
        changed: phase.contains(NSEventPhase::Changed) || phase.contains(NSEventPhase::Stationary),
        ended: phase.contains(NSEventPhase::Ended),
        cancelled: phase.contains(NSEventPhase::Cancelled),
    }
    .phase()
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
