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
    math::{LogicalPoint, LogicalRect, px},
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

pub(crate) struct ViewIvars {
    events: RefCell<Vec<Event>>,
    close: Rc<CloseBridge>,
    dispatcher: Arc<Dispatcher>,
    ime: RefCell<ImeState>,
    a11y: RefCell<Option<MacA11y>>,
    modifiers: Cell<u8>,
    ime_cursor: Cell<Option<LogicalRect>>,
    cursor: Cell<Cursor>,
    pointer_inside: Cell<bool>,
    ime_enabled: Cell<bool>,
    edit: Cell<EditAvailability>,
    drag_regions: RefCell<Vec<LogicalRect>>,
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

        #[unsafe(method_id(inputContext))]
        fn input_context(&self) -> Option<Retained<NSTextInputContext>> {
            if self.ivars().ime_enabled.get() {
                unsafe { msg_send![super(self), inputContext] }
            } else {
                None
            }
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            self.note_modifiers(event.modifierFlags());
            let modifiers = self.modifiers();
            let repeat = event.isARepeat();
            let timestamp = event_timestamp(event);
            let ivars = self.ivars();
            let token = ivars.ime.borrow_mut().begin_key();
            let _scope = KeyScope {
                ime: &ivars.ime,
                token,
            };
            let ime_handled = self
                .inputContext()
                .is_some_and(|context| context.handleEvent(event));
            let (composing, text_taken) = {
                let ime = ivars.ime.borrow();
                (ime.has_marked(), ime.takes_text(token))
            };
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
            if let Some(context) = self.inputContext() {
                context.handleEvent(event);
            }
            let modifiers = self.modifiers();
            self.push_event(Event::Input(InputEvent::Key {
                code: key_code(event.keyCode()),
                state: KeyState::Up,
                repeat: false,
                modifiers,
                timestamp: event_timestamp(event),
            }));
        }

        #[unsafe(method(flagsChanged:))]
        fn flags_changed(&self, event: &NSEvent) {
            let previous = self.modifiers();
            self.note_modifiers(event.modifierFlags());
            let modifiers = self.modifiers();
            let timestamp = event_timestamp(event);
            for (code, state) in modifier_key_transitions(previous, modifiers) {
                self.push_event(Event::Input(InputEvent::Key {
                    code,
                    state,
                    repeat: false,
                    modifiers,
                    timestamp,
                }));
            }
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            let mut drag = self.ivars().window_drag.get();
            let deliver = drag.hover_move((NSEvent::pressedMouseButtons() & 1) != 0);
            self.ivars().window_drag.set(drag);
            if deliver {
                self.push_pointer(event, PointerPhase::Move);
            }
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
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
            self.ivars().pointer_inside.set(false);
            let position = self.logical_point(event.locationInWindow());
            self.note_modifiers(event.modifierFlags());
            self.push_event(Event::Input(InputEvent::PointerLeave {
                position,
                modifiers: self.modifiers(),
                timestamp: event_timestamp(event),
            }));
        }

        #[unsafe(method(cursorUpdate:))]
        fn cursor_update(&self, _event: &NSEvent) {
            self.ivars().pointer_inside.set(true);
            self.apply_cursor();
        }

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

        #[unsafe(method(rotateWithEvent:))]
        fn rotate(&self, event: &NSEvent) {
            let Some(phase) = gesture_phase(event) else {
                return;
            };
            self.note_modifiers(event.modifierFlags());
            self.push_event(Event::Input(InputEvent::Rotate {
                degrees: event.rotation(),
                phase,
                modifiers: self.modifiers(),
                timestamp: event_timestamp(event),
            }));
        }

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
            let Some((delta, unit)) = scroll_delta(
                event.hasPreciseScrollingDeltas(),
                event.scrollingDeltaX(),
                event.scrollingDeltaY(),
            ) else {
                return;
            };
            let position = self.logical_point(event.locationInWindow());
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
                unsafe { Retained::retain(self as *const Self as *mut AnyObject) }
                    .expect("self is never null")
            })
        }
    }

    unsafe impl NSUserInterfaceValidations for GuiView {
        #[unsafe(method(validateUserInterfaceItem:))]
        fn validate_user_interface_item(
            &self,
            item: &ProtocolObject<dyn NSValidatedUserInterfaceItem>,
        ) -> bool {
            let command = item
                .action()
                .and_then(|action| action.name().to_str().ok())
                .and_then(MenuCommand::from_selector);
            match command {
                Some(command) => self.ivars().edit.get().allows(command),
                None => true,
            }
        }
    }

    unsafe impl NSTextInputClient for GuiView {
        #[unsafe(method(insertText:replacementRange:))]
        unsafe fn insert_text_replacement_range(&self, text: &AnyObject, range: NSRange) {
            let value = object_text(text);
            if value.is_empty() {
                return;
            }
            let replace = self.relative_replacement_range(range);
            let verdict = self.ivars().ime.borrow_mut().note_commit(&value);
            self.ivars().ime.borrow_mut().clear_marked();
            match verdict {
                CommitVerdict::Duplicate => {}
                CommitVerdict::DeliverInScope | CommitVerdict::DeliverOutsideScope => {
                    self.push_event(Event::Ime(ImeEvent::Commit {
                        text: smol_str::SmolStr::new(value),
                        replace,
                    }));
                    let emit_end = self.ivars().ime.borrow_mut().note_commit_end();
                    if emit_end {
                        self.push_event(Event::Ime(ImeEvent::End));
                    }
                }
            }
        }

        #[unsafe(method(doCommandBySelector:))]
        unsafe fn do_command_by_selector(&self, _selector: objc2::runtime::Sel) {
        }

        #[unsafe(method(setMarkedText:selectedRange:replacementRange:))]
        unsafe fn set_marked_text(&self, text: &AnyObject, selected: NSRange, replacement: NSRange) {
            let value = object_text(text);
            let utf16: Vec<u16> = value.encode_utf16().collect();
            let replace = self.relative_replacement_range(replacement);
            {
                let mut ime = self.ivars().ime.borrow_mut();
                ime.set_marked(&utf16, (selected.location, selected.length));
            }
            let caret = selected.location.min(utf16.len()) as u32;
            self.push_event(Event::Ime(ImeEvent::Preedit {
                text: smol_str::SmolStr::new(value),
                caret: Some(caret),
                clauses: smallvec::SmallVec::new(),
                replace,
            }));
        }

        #[unsafe(method(unmarkText))]
        fn unmark_text(&self) {
            let mut ime = self.ivars().ime.borrow_mut();
            ime.clear_marked();
            if ime.note_end() {
                self.push_event(Event::Ime(ImeEvent::End));
            }
        }

        #[unsafe(method(selectedRange))]
        fn selected_range(&self) -> NSRange {
            let ime = self.ivars().ime.borrow();
            if !ime.has_marked() {
                return NSRange::new(isize::MAX as usize, 0);
            }
            let (location, length) = ime.selected_range();
            NSRange::new(location, length)
        }

        #[unsafe(method(markedRange))]
        fn marked_range(&self) -> NSRange {
            let ime = self.ivars().ime.borrow();
            if !ime.has_marked() {
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
        unsafe fn first_rect(&self, range: NSRange, actual: NonNull<NSRange>) -> NSRect {
            unsafe { *actual.as_ptr() = range };
            let local = match self.ivars().ime_cursor.get() {
                Some(area) => NSRect::new(
                    NSPoint::new(
                        f64::from(area.origin.x.value()),
                        f64::from(area.origin.y.value()),
                    ),
                    NSSize::new(
                        f64::from(area.size.width.max(px(1.0)).value()),
                        f64::from(area.size.height.max(px(1.0)).value()),
                    ),
                ),
                None => NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0)),
            };
            let in_window = self.convertRect_toView(local, None);
            self.window()
                .map(|window| window.convertRectToScreen(in_window))
                .unwrap_or(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0)))
        }

        #[unsafe(method(characterIndexForPoint:))]
        fn character_index_for_point(&self, _point: NSPoint) -> NSUInteger {
            isize::MAX as usize
        }
    }
);

enum PointerPhase {
    Move,
    Down(PointerButton),
    Up(PointerButton),
}

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
        let this: Retained<Self> = unsafe {
            msg_send![
                super(this),
                initWithFrame: NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1.0, 1.0))
            ]
        };
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

    pub(crate) fn install_a11y(&self, a11y: MacA11y) {
        *self.ivars().a11y.borrow_mut() = Some(a11y);
    }

    pub(crate) fn uninstall_a11y(&self) {
        let adapter = self.ivars().a11y.borrow_mut().take();
        drop(adapter);
    }

    pub(crate) fn update_a11y(&self, update: impl FnOnce() -> accesskit::TreeUpdate) {
        let mut adapter = self.ivars().a11y.take();
        if let Some(a11y) = adapter.as_mut() {
            a11y.update(update);
        }
        *self.ivars().a11y.borrow_mut() = adapter;
    }

    pub(crate) fn focus_changed(&self, focused: bool) {
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

    pub(crate) fn apply_ime_enabled(&self, enabled: bool) -> bool {
        if enabled {
            self.ivars().ime_enabled.set(true);
            return false;
        }
        let needs_end = self.reset_ime_composition();
        self.ivars().ime_enabled.set(false);
        needs_end
    }

    pub(crate) fn reset_ime_composition(&self) -> bool {
        self.ivars().ime_cursor.set(None);
        if !self.ivars().ime.borrow().has_pending_end() {
            return false;
        }
        if let Some(context) = self.inputContext() {
            context.discardMarkedText();
        }
        let mut ime = self.ivars().ime.borrow_mut();
        ime.clear_marked();
        ime.note_end()
    }

    pub(crate) fn take_events(&self) -> Vec<Event> {
        std::mem::take(&mut *self.ivars().events.borrow_mut())
    }

    pub(crate) fn set_ime_cursor_area(&self, area: Option<LogicalRect>) {
        self.ivars().ime_cursor.set(area);
    }

    pub(crate) fn set_edit_availability(&self, availability: EditAvailability) {
        self.ivars().edit.set(availability);
    }

    pub(crate) fn set_cursor(&self, cursor: Cursor) {
        self.ivars().cursor.set(cursor);
        if self.ivars().pointer_inside.get() {
            self.apply_cursor();
        }
    }

    pub(crate) fn set_drag_regions(&self, regions: &[LogicalRect]) {
        let mut slot = self.ivars().drag_regions.borrow_mut();
        if slot.as_slice() == regions {
            return;
        }
        slot.clear();
        slot.extend_from_slice(regions);
    }

    fn begin_window_drag(&self, event: &NSEvent) -> bool {
        let position = self.logical_point(event.locationInWindow());
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

    fn apply_cursor(&self) {
        ns_cursor(self.ivars().cursor.get()).set();
    }

    fn run_menu_command(&self, command: MenuCommand) {
        let Some((code, modifiers)) = command.engine_key() else {
            return;
        };
        if !self.ivars().edit.get().allows(command) {
            self.forward_to_next_responder(command);
            return;
        }
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

    fn forward_to_next_responder(&self, command: MenuCommand) {
        let Some(next) = (unsafe { self.nextResponder() }) else {
            return;
        };
        let _ = unsafe { next.tryToPerform_with(menu::selector(command), None) };
    }

    fn relative_replacement_range(&self, range: NSRange) -> Option<std::ops::Range<usize>> {
        let ime = self.ivars().ime.borrow();
        let (marked_location, marked_length) = ime.marked_range();
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

    fn logical_point(&self, point: NSPoint) -> LogicalPoint {
        let local = self.convertPoint_fromView(point, None);
        LogicalPoint::new(px(local.x as f32), px(local.y as f32))
    }
}

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

fn object_text(object: &AnyObject) -> String {
    if let Some(attributed) = object.downcast_ref::<NSAttributedString>() {
        return attributed.string().to_string();
    }
    if let Some(text) = object.downcast_ref::<NSString>() {
        return text.to_string();
    }
    let description: Retained<NSString> = unsafe { msg_send![object, description] };
    description.to_string()
}

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

fn event_timestamp(event: &NSEvent) -> Instant {
    let now = Instant::now();
    let now_ms = (NSProcessInfo::processInfo().systemUptime().max(0.0) * 1000.0) as u64;
    let event_ms = (event.timestamp().max(0.0) * 1000.0) as u64;
    timestamp_from_ticks(now, now_ms, event_ms)
}

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
        unsafe {
            attributed.addAttribute_value_range(
                NSUnderlineStyleAttributeName,
                as_any_object(&*underline),
                NSRange::new(0, plain.length()),
            );
        }
        let description: Retained<NSString> = unsafe { msg_send![&*attributed, description] };
        assert!(
            description.to_string().contains("NSUnderline"),
            "attributed fixture must carry attributes: {description}"
        );
        assert_eq!(object_text(as_any_object(&*attributed)), "ni'hao");
    }

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
        assert_eq!(key_code(0xFFFF), KeyCode::Unidentified(0xFFFF));
    }
}
