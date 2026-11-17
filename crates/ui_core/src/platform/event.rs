//! `WM_*` → [`PlatformEvent`] 的**纯函数**翻译（`design/phase-09-win32-demo.md` §4.2）。
//!
//! 这里不出现任何 Win32 句柄/结构体：输入是 `(msg, wparam, lparam)` 三个整数（外加消息时间与
//! 系统滚轮行数），输出是平台事件。唯一的平台耦合是 `WM_*` / `VK_*` 常量与「滚轮的屏幕坐标已由
//! 调用方换成客户区坐标」这一约定。因此全部翻译规则都能用内联单测钉死（`SendMessageW` 合成
//! 消息在 P09 的实机验收里另测）。
//!
//! ## 键位身份（R02-1）
//!
//! Windows 的 `wParam` 是**虚拟键码**，而字母/数字的 `VK_A..VK_Z` / `VK_0..VK_9` 是**布局相关**
//! 的：AZERTY 键盘上物理 `Q` 位置产生 `VK_A`。要让快捷键跨布局、跨平台生效，物理身份只能取自
//! **硬件扫描码**（`lParam` bit 16–23，`E0` 前缀由 bit 24 标记）—— 它与键盘布局无关，映射到
//! W3C `KeyboardEvent.code` 的 [`KeyCode`]。只有扫描码缺失（注入消息、无扫描码的虚拟键）时才
//! 退回 VK 的命名键映射。
//!
//! ## 自动重复（R02-2）
//!
//! `WM_KEYDOWN` 的 `lParam` bit 30 是 previous key state：置位表示这是**自动重复**而不是新的
//! 按下（同一 `lParam` 的 bit 16–23 是硬件扫描码，bit 24 是 `E0` 扩展位）。

use smol_str::SmolStr;
use windows::Win32::UI::{
    Input::KeyboardAndMouse::{VK_PACKET, VK_PROCESSKEY},
    WindowsAndMessaging::*,
};

use crate::{
    event::{ImeEvent, KeyCode, KeyState, Modifiers, PointerButton},
    math::Scale,
    platform::windows::ime::ImeState,
};

/// 指针事件的细分（`WM_*` 决定按钮，不读 `wParam` 的 `MK_*`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PointerKind {
    Move,
    Down(PointerButton),
    Up(PointerButton),
}

/// 平台层事件（应用层看到的是 [`crate::event::Event`]）。
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PlatformEvent {
    Pointer {
        x: i32,
        y: i32,
        kind: PointerKind,
    },
    /// 指针进入客户区（`TrackMouseEvent` 之后的首次移动，对应 `WM_MOUSELEAVE` 的另一半）。
    PointerEnter,
    Scroll {
        x: i32,
        y: i32,
        /// 水平滚动**行数**（正 = 向右）。
        dx_lines: f32,
        /// 垂直滚动**行数**（正 = 向上，与 `WM_MOUSEWHEEL` 的正方向一致）。
        dy_lines: f32,
    },
    Key {
        code: KeyCode,
        state: KeyState,
        repeat: bool,
    },
    Text(SmolStr),
    Ime(ImeEvent),
    Resize {
        width: u32,
        height: u32,
    },
    Dpi {
        /// 新 DPI（96 基准）。
        ///
        /// 系统建议矩形**不在这里**：`WndProc` 在 `WM_DPICHANGED` 处理期间已经用它
        /// `SetWindowPos`（R07-12：必须在消息处理内应用，winit / Qt 同款），平台事件只负责
        /// 让应用同步自己的 scale 与渲染目标。
        dpi: u32,
    },
    Focus(bool),
    Appearance(bool),
    PointerLeave,
    /// 指针会话被系统**取消**（捕获丢失 / 窗口失焦 / 系统弹窗夺走输入）。
    ///
    /// 映射到 `InputEvent::PointerCancel`：只收尾（结束拖拽、派发 `DragPhase::Cancel`），
    /// 绝不提交点击。
    PointerCancel,
    Close,
}

/// 一条消息 → 平台事件的**带时间戳**产物（R02-14）。
///
/// `ticks_ms` 是消息的 `MSG.time`（与 `GetTickCount` 同源的 **32 位**毫秒，49.7 天回绕），
/// `WndProc` 用 `GetMessageTime()` 取到它；平台事件转 `InputEvent` 时经
/// [`crate::event::timestamp_from_ticks_32`] 换算成引擎单调时钟（「现在」用
/// `GetTickCount64() as u32`，同源同宽）。
///
/// `scale` 是**该消息产生时**的窗口 DPI 比例（R07-12）：`WM_DPICHANGED` 在 `WndProc` 里
/// 立即生效，同批次后续输入必须按新 scale 换算成逻辑坐标；若像旧实现那样在整批派发之后
/// 才应用 DPI，同批的鼠标位移会用旧 scale 解释（100% → 200% 跨屏拖动时指针坐标跳变）。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct StampedEvent {
    /// 平台事件。
    pub event: PlatformEvent,
    /// 消息时间（毫秒，32 位回绕域）。
    pub ticks_ms: u32,
    /// 消息产生时的窗口 scale。
    pub scale: Scale,
}

/// 每条消息都要带的输入上下文（时间戳 + 系统滚轮行数 + 当时的 DPI 比例）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct MessageContext {
    /// 消息时间（`GetMessageTime()`，毫秒，32 位回绕域）。
    pub ticks_ms: u32,
    /// 一格滚轮的行数（`SPI_GETWHEELSCROLLLINES`；`0.0` = 未知/按页）。
    pub wheel_lines: f32,
    /// 该消息产生时的窗口 scale（R07-12：DPI 在 `WndProc` 内同步生效）。
    pub scale: Scale,
}

/// 消息之间需要保持的状态（修饰键、代理对、IME 组合）。
#[derive(Debug, Default)]
pub(crate) struct TranslateState {
    modifiers: Modifiers,
    /// `WM_CHAR` 可能把一对代理拆成两条消息。
    high_surrogate: Option<u16>,
    ime: ImeState,
}

impl TranslateState {
    pub(crate) fn modifiers(&self) -> Modifiers {
        self.modifiers
    }

    /// 复位瞬时输入状态（失焦时调用）。
    ///
    /// 修饰键按消息顺序维护：失焦期间抬起消息不再到达本窗口，不清位就会让 Shift/Ctrl
    /// 永久保持按下（Alt+Tab 回来后所有输入都被当成组合键）。半高代理对同理：
    /// 失焦前残留的高位代理会和回来后的字符拼成乱码。IME 的待抑制码元队列也一并清空
    /// （R07-5：提交后补发的 `WM_CHAR` 可能在失焦后不再到达，残留队列会静默吞掉之后真实
    /// 键入的字符）。
    pub(crate) fn reset_transient(&mut self) {
        self.modifiers = Modifiers::empty();
        self.high_surrogate = None;
        self.ime.clear_pending();
    }

    /// IME 状态（`event_loop` 在 `WM_IME_*` 分支里驱动它）。
    pub(crate) fn ime_mut(&mut self) -> &mut ImeState {
        &mut self.ime
    }

    /// IME 状态（只读：泵比较「目标状态 vs 已应用状态」时用，不产生任何 Win32 调用）。
    pub(crate) fn ime(&self) -> &ImeState {
        &self.ime
    }

    /// 一轮消息排空结束（泵每轮 ① 之前调用）：推进 IME 提交抑制队列的消息批次号。
    ///
    /// 队列只在「入队批次 + 紧随其后的一轮」内匹配（`ImeState::consume_char`），这样「提交后
    /// 系统不补发 `WM_CHAR`」的残留不会跨会话留到下一次真实键入（AR3 §1.5）。
    pub(crate) fn end_message_batch(&mut self) {
        self.ime.end_message_batch();
    }
}

/// 一条消息 → 至多一个平台事件。`None` = 该消息不影响应用层。
///
/// **契约**：滚轮的 `lParam` 在调用前已被换成客户区坐标（`WM_MOUSEWHEEL` 原值是屏幕坐标）。
pub(crate) fn translate(
    msg: u32,
    wparam: usize,
    lparam: isize,
    context: MessageContext,
    state: &mut TranslateState,
) -> Option<StampedEvent> {
    let x = loword_signed(lparam);
    let y = hiword_signed(lparam);
    let event = match msg {
        WM_MOUSEMOVE => Some(PlatformEvent::Pointer {
            x,
            y,
            kind: PointerKind::Move,
        }),
        WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN
        | WM_MBUTTONUP | WM_XBUTTONDOWN | WM_XBUTTONUP => {
            let button = match msg {
                WM_LBUTTONDOWN | WM_LBUTTONUP => PointerButton::Primary,
                WM_RBUTTONDOWN | WM_RBUTTONUP => PointerButton::Secondary,
                WM_MBUTTONDOWN | WM_MBUTTONUP => PointerButton::Middle,
                _ => match xbutton(wparam) {
                    1 => PointerButton::X1,
                    _ => PointerButton::X2,
                },
            };
            let kind = match msg {
                WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN => {
                    PointerKind::Down(button)
                }
                _ => PointerKind::Up(button),
            };
            Some(PlatformEvent::Pointer { x, y, kind })
        }
        WM_MOUSEWHEEL => {
            let ticks = f32::from(high_word(wparam) as i16) / WHEEL_DELTA_F;
            let lines = lines_per_tick(context.wheel_lines);
            Some(PlatformEvent::Scroll {
                x,
                y,
                dx_lines: 0.0,
                dy_lines: ticks * lines,
            })
        }
        WM_MOUSEHWHEEL => {
            let ticks = f32::from(high_word(wparam) as i16) / WHEEL_DELTA_F;
            let lines = lines_per_tick(context.wheel_lines);
            Some(PlatformEvent::Scroll {
                x,
                y,
                dx_lines: ticks * lines,
                dy_lines: 0.0,
            })
        }
        WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP => {
            // AR3 §1.5：**真实键入优先**。任何键按下都作废上一批的提交抑制队列 —— 候选窗提交
            // 之后系统不补发 `WM_CHAR` 时，残留队列会在用户下一次键入（与提交首码元相同）时吞掉
            // 一次按键。放在 VK 过滤之前：`VK_PROCESSKEY` / `VK_PACKET` 的按下同样代表一次真实
            // 输入会话（注入文本也走 `VK_PACKET` + `WM_CHAR`）。
            if matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN) {
                state.ime.clear_pending();
            }
            let vk = (wparam & 0xFF) as u16;
            // R07-6：IME 组合期间系统把 `WM_KEYDOWN` 的 wParam 换成 `VK_PROCESSKEY`（真实键码
            // 要用 `ImmGetVirtualKey` 才拿得到），`VK_PACKET` 则是 `SendInput(KEYEVENTF_UNICODE)`
            // / 语音输入 / 辅助技术注入的载体（文本由随后的 `WM_CHAR` 给出）。两者都**不是**
            // 应用可观察的按键：派发出去就是「每敲一个拼音字母，应用收到一次假按键」，
            // 会让快捷键表与「任意键」处理器误触发（Chrome `ImeInput`、Firefox `IMMHandler`、
            // winit 都显式跳过）。IME 自己的 `WM_IME_CHAR` / 提交不受影响（走 IME 分支与
            // `WM_CHAR`）。
            if matches!(vk, v if v == VK_PROCESSKEY.0 || v == VK_PACKET.0) {
                return None;
            }
            let down = matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN);
            update_modifiers(vk, down, &mut state.modifiers);
            let raw = lparam as u32;
            // R02-2：bit 30 = previous key state（置位 = 自动重复）。只对按下有意义。
            let repeat = down && (raw & (1 << 30)) != 0;
            // 硬件扫描码 bit 16–23；bit 24 = `E0` 扩展键（右 Ctrl/Alt、小键盘回车、导航键…）。
            let scancode = ((raw >> 16) & 0xFF) as u16;
            let extended = (raw & (1 << 24)) != 0;
            let code = key_code_from_native(vk, scancode, extended);
            Some(PlatformEvent::Key {
                code,
                state: if down { KeyState::Down } else { KeyState::Up },
                repeat,
            })
        }
        WM_CHAR => {
            // IME 提交之后 Windows 会再发同内容的 `WM_CHAR`（**每 UTF-16 码元一条**）：逐码元
            // 比对后跳过，避免文字重复；内容不匹配 / 过了抑制有效期时队列自愈（R07-5 /
            // AR3 §1.5：队列带消息批次与时间戳，不会跨到用户之后的真实键入）。
            if state.ime.consume_char(wparam as u16, context.ticks_ms) {
                return None;
            }
            decode_char(wparam as u16, state).map(PlatformEvent::Text)
        }
        // `WM_MOUSELEAVE` 由 `Win32::UI::Controls` 导出（不是 `WindowsAndMessaging`）。
        windows::Win32::UI::Controls::WM_MOUSELEAVE => Some(PlatformEvent::PointerLeave),
        WM_SETFOCUS => Some(PlatformEvent::Focus(true)),
        WM_KILLFOCUS => Some(PlatformEvent::Focus(false)),
        WM_SIZE => {
            // `SIZE_MINIMIZED` 没有可用的客户区尺寸。
            if wparam == SIZE_MINIMIZED as usize {
                return None;
            }
            let width = lparam as u32 & 0xFFFF;
            let height = (lparam as u32 >> 16) & 0xFFFF;
            (width > 0 && height > 0).then_some(PlatformEvent::Resize { width, height })
        }
        WM_DPICHANGED => {
            let dpi = (wparam as u32) & 0xFFFF;
            (dpi != 0).then_some(PlatformEvent::Dpi { dpi })
        }
        WM_CLOSE => Some(PlatformEvent::Close),
        _ => None,
    };
    event.map(|event| StampedEvent {
        event,
        ticks_ms: context.ticks_ms,
        scale: context.scale,
    })
}

/// `WHEEL_DELTA`（`WM_MOUSEWHEEL` 的一格 = 120）。
const WHEEL_DELTA_F: f32 = 120.0;

/// 一格滚轮的行数：系统值不可用（`SPI_GETWHEELSCROLLLINES` = 0 / `WHEEL_PAGESCROLL`）时按
/// Win32 缺省的 3 行处理（控制面板「一次滚动下列行数」默认为 3）。
pub(crate) fn lines_per_tick(wheel_lines: f32) -> f32 {
    if wheel_lines.is_finite() && wheel_lines > 0.0 && wheel_lines <= 100.0 {
        wheel_lines
    } else {
        3.0
    }
}

/// `LOWORD` 按**有符号** 16 位解码（客户端坐标可以为负）。
pub(crate) fn loword_signed(value: isize) -> i32 {
    (value as u32 & 0xFFFF) as u16 as i16 as i32
}

/// `HIWORD` 按**有符号** 16 位解码（滚轮 delta 用它，±120 的符号即方向）。
pub(crate) fn hiword_signed(value: isize) -> i32 {
    ((value as u32 >> 16) & 0xFFFF) as u16 as i16 as i32
}

/// `GET_XBUTTON_WPARAM`：高 16 位是 1（X1）或 2（X2）。
fn xbutton(wparam: usize) -> u16 {
    ((wparam as u32 >> 16) & 0xFFFF) as u16
}

/// `wParam` 的高 16 位（滚轮 delta；按有符号 16 位解释）。
fn high_word(wparam: usize) -> u16 {
    ((wparam as u32 >> 16) & 0xFFFF) as u16
}

/// `WM_CHAR` 载荷 → 文本；代理对被拆成两条消息时缓存高代理。
fn decode_char(unit: u16, state: &mut TranslateState) -> Option<SmolStr> {
    // 低代理：与前一个高代理合并。
    if (0xDC00..=0xDFFF).contains(&unit) {
        let high = state.high_surrogate.take()?;
        let combined = 0x10000 + ((u32::from(high) - 0xD800) << 10) + (u32::from(unit) - 0xDC00);
        return char::from_u32(combined).map(|c| SmolStr::new(c.to_string()));
    }
    state.high_surrogate = None;
    if (0xD800..=0xDBFF).contains(&unit) {
        state.high_surrogate = Some(unit);
        return None;
    }
    char::from_u32(u32::from(unit)).map(|c| SmolStr::new(c.to_string()))
}

/// 修饰键的按下/抬起（按消息顺序维护，不查系统状态）。
fn update_modifiers(vk: u16, down: bool, modifiers: &mut Modifiers) {
    let flag = match vk {
        VK_SHIFT_CODE => Modifiers::SHIFT,
        VK_CONTROL_CODE => Modifiers::CONTROL,
        VK_MENU_CODE => Modifiers::ALT,
        0x5B | 0x5C => Modifiers::META, // VK_LWIN / VK_RWIN
        _ => return,
    };
    modifiers.set(flag, down);
}

/// `VK_SHIFT`（0x10）。
const VK_SHIFT_CODE: u16 = 0x10;
/// `VK_CONTROL`（0x11）。
const VK_CONTROL_CODE: u16 = 0x11;
/// `VK_MENU`（Alt，0x12）。
const VK_MENU_CODE: u16 = 0x12;
/// `VK_PAUSE`（0x13）：`Pause` 的 `WM_KEYDOWN` 也带扫描码 0x45（与 `NumLock` 相同），
/// 只能用 VK 区分。
const VK_PAUSE_CODE: u16 = 0x13;
/// `VK_SNAPSHOT`（0x2C，Print Screen）。
const VK_SNAPSHOT_CODE: u16 = 0x2C;

/// 原生键身份 → [`KeyCode`]（R02-1）。
///
/// 优先用**硬件扫描码**（布局无关：AZERTY 上物理 `Q` 也得到 [`KeyCode::KeyQ`]），扫描码缺失
/// 时退回虚拟键的命名键映射；两者都不认识 ⇒ [`KeyCode::Unidentified`]（Windows 载荷 =
/// 扫描码，bit 8 = `E0` 扩展键；没有扫描码时 `0x8000 | VK`），文档明确它**不可跨平台**。
///
/// **扩展行不回落到基础行**：`E0` 前缀的扫描码只查扩展表（见 [`key_code_from_scancode`]），
/// 因此音量+/浏览器键不会被当成字母/F 键。
///
/// **VK 兜底只用于非扩展键**（AR1b N3）：`E0` 前缀换的是**整个键的身份**，扩展行的未知
/// 扫描码一律 [`KeyCode::Unidentified`]，不拿 VK 去「猜」一个命名键 —— 否则
/// `E0 6E + VK_DELETE` 会报告成 `Delete`、伪 `E0 2A + VK_SHIFT` 会报告成 `ShiftLeft`
/// （`E0 2A` 在部分键盘上是伪 Shift，物理身份并不是左 Shift）。VK 的字母/数字是布局相关的，
/// **不**参与字母身份（`key_code_from_vk` 也没有字母项）。
///
/// 唯一按 VK 定夺的是扫描码本身有歧义的 [`VK_PAUSE_CODE`] / [`VK_SNAPSHOT_CODE`]：
/// `Pause` 的扫描码 `0x45` 与 `NumLock` 相同、`PrintScreen` 是 `E0 2A E0 37` 的序列消息，
/// 只能靠 VK 区分（真实硬件上这两条消息的 VK 一定成立）。
pub(crate) fn key_code_from_native(vk: u16, scancode: u16, extended: bool) -> KeyCode {
    // 扫描码有歧义的少数键：用 VK 定夺。
    match vk {
        VK_PAUSE_CODE => return KeyCode::Pause,
        VK_SNAPSHOT_CODE => return KeyCode::PrintScreen,
        _ => {}
    }
    if scancode != 0
        && let Some(code) = key_code_from_scancode(scancode, extended)
    {
        return code;
    }
    let fallback = if extended { None } else { key_code_from_vk(vk) };
    fallback.unwrap_or(KeyCode::Unidentified(if scancode != 0 {
        scancode | if extended { 0x100 } else { 0 }
    } else {
        0x8000 | vk
    }))
}

/// 扫描码（set 1）+ `E0` 扩展位 → [`KeyCode`]（W3C `KeyboardEvent.code` 语义）。
///
/// **基础行与扩展行是两张独立的表**（P1：媒体键误映射）。`E0` 前缀换的是**整个键的身份**，
/// 而不是「同一个键的另一种形式」：
///
/// - 基础行（无 `E0`）：字母/数字/标点/功能键/小键盘；
/// - 扩展行（`E0`）：右 Ctrl/Alt、小键盘回车与 `/`、导航键、Meta/菜单键，以及
///   **媒体/浏览器/电源键**（`E0 10` 上一曲、`E0 19` 下一曲、`E0 20` 静音、`E0 22` 播放/暂停、
///   `E0 24` 停止、`E0 2E` 音量-、`E0 30` 音量+、`E0 32` 主页、`E0 65..6D` 浏览器/邮件键）。
///
/// 旧实现用 `(scancode, _)` 匹配字母行，于是 `E0 30`（音量+）被当成扫描码 `0x30` ⇒ `KeyB`、
/// `E0 65`（浏览器搜索）⇒ `F14`：音量键以**无修饰的 `KeyB`** 进入应用，会命中交易终端的
/// 裸键绑定（`B` 买入），并且打破了「同一物理键三平台同一 `KeyCode`」的承诺。
///
/// 小键盘与导航键共用扫描码（`0x47` 既是 `Numpad7` 也是 `Home`），由 `E0` 区分 —— 这正是
/// W3C 的划分：`code` 表达**物理键位**（`Numpad7`），NumLock 只影响 `key`（`7` / `Home`）。
fn key_code_from_scancode(scancode: u16, extended: bool) -> Option<KeyCode> {
    if extended {
        return key_code_from_extended_scancode(scancode);
    }
    key_code_from_base_scancode(scancode)
}

/// 基础行（无 `E0`）扫描码 → [`KeyCode`]。
fn key_code_from_base_scancode(scancode: u16) -> Option<KeyCode> {
    let code = match scancode {
        0x01 => KeyCode::Escape,
        0x02 => KeyCode::Digit1,
        0x03 => KeyCode::Digit2,
        0x04 => KeyCode::Digit3,
        0x05 => KeyCode::Digit4,
        0x06 => KeyCode::Digit5,
        0x07 => KeyCode::Digit6,
        0x08 => KeyCode::Digit7,
        0x09 => KeyCode::Digit8,
        0x0A => KeyCode::Digit9,
        0x0B => KeyCode::Digit0,
        0x0C => KeyCode::Minus,
        0x0D => KeyCode::Equal,
        0x0E => KeyCode::Backspace,
        0x0F => KeyCode::Tab,
        0x10 => KeyCode::KeyQ,
        0x11 => KeyCode::KeyW,
        0x12 => KeyCode::KeyE,
        0x13 => KeyCode::KeyR,
        0x14 => KeyCode::KeyT,
        0x15 => KeyCode::KeyY,
        0x16 => KeyCode::KeyU,
        0x17 => KeyCode::KeyI,
        0x18 => KeyCode::KeyO,
        0x19 => KeyCode::KeyP,
        0x1A => KeyCode::BracketLeft,
        0x1B => KeyCode::BracketRight,
        0x1C => KeyCode::Enter,
        0x1D => KeyCode::ControlLeft,
        0x1E => KeyCode::KeyA,
        0x1F => KeyCode::KeyS,
        0x20 => KeyCode::KeyD,
        0x21 => KeyCode::KeyF,
        0x22 => KeyCode::KeyG,
        0x23 => KeyCode::KeyH,
        0x24 => KeyCode::KeyJ,
        0x25 => KeyCode::KeyK,
        0x26 => KeyCode::KeyL,
        0x27 => KeyCode::Semicolon,
        0x28 => KeyCode::Quote,
        0x29 => KeyCode::Backquote,
        0x2A => KeyCode::ShiftLeft,
        0x2B => KeyCode::Backslash,
        0x2C => KeyCode::KeyZ,
        0x2D => KeyCode::KeyX,
        0x2E => KeyCode::KeyC,
        0x2F => KeyCode::KeyV,
        0x30 => KeyCode::KeyB,
        0x31 => KeyCode::KeyN,
        0x32 => KeyCode::KeyM,
        0x33 => KeyCode::Comma,
        0x34 => KeyCode::Period,
        0x35 => KeyCode::Slash,
        0x36 => KeyCode::ShiftRight,
        0x37 => KeyCode::NumpadMultiply,
        0x38 => KeyCode::AltLeft,
        0x39 => KeyCode::Space,
        0x3A => KeyCode::CapsLock,
        0x3B => KeyCode::F1,
        0x3C => KeyCode::F2,
        0x3D => KeyCode::F3,
        0x3E => KeyCode::F4,
        0x3F => KeyCode::F5,
        0x40 => KeyCode::F6,
        0x41 => KeyCode::F7,
        0x42 => KeyCode::F8,
        0x43 => KeyCode::F9,
        0x44 => KeyCode::F10,
        0x45 => KeyCode::NumLock,
        0x46 => KeyCode::ScrollLock,
        0x47 => KeyCode::Numpad7,
        0x48 => KeyCode::Numpad8,
        0x49 => KeyCode::Numpad9,
        0x4A => KeyCode::NumpadSubtract,
        0x4B => KeyCode::Numpad4,
        0x4C => KeyCode::Numpad5,
        0x4D => KeyCode::Numpad6,
        0x4E => KeyCode::NumpadAdd,
        0x4F => KeyCode::Numpad1,
        0x50 => KeyCode::Numpad2,
        0x51 => KeyCode::Numpad3,
        0x52 => KeyCode::Numpad0,
        0x53 => KeyCode::NumpadDecimal,
        0x56 => KeyCode::IntlBackslash,
        0x57 => KeyCode::F11,
        0x58 => KeyCode::F12,
        0x59 => KeyCode::NumpadEqual,
        0x64 => KeyCode::F13,
        0x65 => KeyCode::F14,
        0x66 => KeyCode::F15,
        0x67 => KeyCode::F16,
        0x68 => KeyCode::F17,
        0x69 => KeyCode::F18,
        0x6A => KeyCode::F19,
        0x6B => KeyCode::F20,
        0x6C => KeyCode::F21,
        0x6D => KeyCode::F22,
        0x6E => KeyCode::F23,
        0x6F => KeyCode::F24,
        0x73 => KeyCode::IntlRo,
        0x7D => KeyCode::IntlYen,
        0x7E => KeyCode::NumpadComma,
        _ => return None,
    };
    Some(code)
}

/// `E0` 扩展行扫描码 → [`KeyCode`]。
///
/// 扩展行的扫描码**不得**回落到基础行：`E0 30` 是音量+，不是 `KeyB`；`E0 65` 是浏览器搜索，
/// 不是 `F14`。表里没有的扩展键一律 `Unidentified(0x100 | scancode)`（由
/// [`key_code_from_native`] 组装），绝不猜成字母/功能键 —— **也不拿 VK 兜底**（AR1b N3）。
fn key_code_from_extended_scancode(scancode: u16) -> Option<KeyCode> {
    let code = match scancode {
        // 编辑键区的另一半（与基础行的小键盘共用扫描码）。
        0x1C => KeyCode::NumpadEnter,
        0x1D => KeyCode::ControlRight,
        0x35 => KeyCode::NumpadDivide,
        0x38 => KeyCode::AltRight,
        0x47 => KeyCode::Home,
        0x48 => KeyCode::ArrowUp,
        0x49 => KeyCode::PageUp,
        0x4B => KeyCode::ArrowLeft,
        0x4D => KeyCode::ArrowRight,
        0x4F => KeyCode::End,
        0x50 => KeyCode::ArrowDown,
        0x51 => KeyCode::PageDown,
        0x52 => KeyCode::Insert,
        0x53 => KeyCode::Delete,
        0x5B => KeyCode::MetaLeft,
        0x5C => KeyCode::MetaRight,
        0x5D => KeyCode::ContextMenu,
        // 媒体键（Win32 的 `VK_MEDIA_*` / `VK_VOLUME_*` 与扫描码一一对应）。
        0x10 => KeyCode::MediaTrackPrevious,
        0x19 => KeyCode::MediaTrackNext,
        0x20 => KeyCode::AudioVolumeMute,
        0x21 => KeyCode::LaunchApp2,
        0x22 => KeyCode::MediaPlayPause,
        0x24 => KeyCode::MediaStop,
        0x2E => KeyCode::AudioVolumeDown,
        0x30 => KeyCode::AudioVolumeUp,
        0x32 => KeyCode::BrowserHome,
        // 电源管理（系统通常自己消费，但按键确实以 E0 消息到达前台窗口）。
        0x5E => KeyCode::Power,
        0x5F => KeyCode::Sleep,
        0x63 => KeyCode::WakeUp,
        // 浏览器 / 邮件键。
        0x65 => KeyCode::BrowserSearch,
        0x66 => KeyCode::BrowserFavorites,
        0x67 => KeyCode::BrowserRefresh,
        0x68 => KeyCode::BrowserStop,
        0x69 => KeyCode::BrowserForward,
        0x6A => KeyCode::BrowserBack,
        0x6B => KeyCode::LaunchApp1,
        0x6C => KeyCode::LaunchMail,
        0x6D => KeyCode::MediaSelect,
        _ => return None,
    };
    Some(code)
}

/// 虚拟键的命名键映射（**仅非扩展键**、扫描码缺失时的回退；不用它表达字母/数字 —— VK 字母
/// 是布局相关的）。
///
/// 「仅非扩展键」由 [`key_code_from_native`] 保证：扩展行拿不到这张表，未知扩展扫描码一律
/// `Unidentified`（AR1b N3）。
fn key_code_from_vk(vk: u16) -> Option<KeyCode> {
    let code = match vk {
        0x1B => KeyCode::Escape,
        0x0D => KeyCode::Enter,
        0x09 => KeyCode::Tab,
        0x08 => KeyCode::Backspace,
        0x2E => KeyCode::Delete,
        0x2D => KeyCode::Insert,
        0x20 => KeyCode::Space,
        0x25 => KeyCode::ArrowLeft,
        0x26 => KeyCode::ArrowUp,
        0x27 => KeyCode::ArrowRight,
        0x28 => KeyCode::ArrowDown,
        0x24 => KeyCode::Home,
        0x23 => KeyCode::End,
        0x21 => KeyCode::PageUp,
        0x22 => KeyCode::PageDown,
        0x2C => KeyCode::PrintScreen,
        0x2F => KeyCode::Help,
        0x90 => KeyCode::NumLock,
        0x91 => KeyCode::ScrollLock,
        0x5B => KeyCode::MetaLeft,
        0x5C => KeyCode::MetaRight,
        0x5D => KeyCode::ContextMenu,
        0x10 => KeyCode::ShiftLeft,
        0x11 => KeyCode::ControlLeft,
        0x12 => KeyCode::AltLeft,
        0x70..=0x87 => return f_key((vk - 0x6F) as u8),
        _ => return None,
    };
    Some(code)
}

/// F1…F24 的 `KeyCode`（`n` 越界时 `None`）。
fn f_key(n: u8) -> Option<KeyCode> {
    let code = match n {
        1 => KeyCode::F1,
        2 => KeyCode::F2,
        3 => KeyCode::F3,
        4 => KeyCode::F4,
        5 => KeyCode::F5,
        6 => KeyCode::F6,
        7 => KeyCode::F7,
        8 => KeyCode::F8,
        9 => KeyCode::F9,
        10 => KeyCode::F10,
        11 => KeyCode::F11,
        12 => KeyCode::F12,
        13 => KeyCode::F13,
        14 => KeyCode::F14,
        15 => KeyCode::F15,
        16 => KeyCode::F16,
        17 => KeyCode::F17,
        18 => KeyCode::F18,
        19 => KeyCode::F19,
        20 => KeyCode::F20,
        21 => KeyCode::F21,
        22 => KeyCode::F22,
        23 => KeyCode::F23,
        24 => KeyCode::F24,
        _ => return None,
    };
    Some(code)
}

#[cfg(test)]
mod tests {
    use smol_str::SmolStr;
    use windows::Win32::UI::WindowsAndMessaging::*;

    use super::{
        MessageContext, PlatformEvent, PointerKind, TranslateState, key_code_from_native,
        lines_per_tick, translate,
    };
    use crate::{
        event::{KeyCode, KeyState, Modifiers, PointerButton},
        math::Scale,
    };

    /// 一个 KEYDOWN 的 `lParam`：扫描码在 bit 16–23，`E0` 在 bit 24，重复位在 bit 30。
    fn key_lparam(scancode: u16, extended: bool, previous_down: bool) -> isize {
        let mut raw = u32::from(scancode) << 16;
        if extended {
            raw |= 1 << 24;
        }
        if previous_down {
            raw |= 1 << 30;
        }
        raw as isize
    }

    fn lparam(x: i32, y: i32) -> isize {
        ((((y as u32) << 16) | (x as u32 & 0xFFFF)) as i32) as isize
    }

    fn context() -> MessageContext {
        MessageContext {
            ticks_ms: 1_000,
            wheel_lines: 3.0,
            scale: Scale::ONE,
        }
    }

    fn translate_raw(msg: u32, wparam: usize, lparam: isize) -> Option<PlatformEvent> {
        let mut state = TranslateState::default();
        translate(msg, wparam, lparam, context(), &mut state).map(|stamped| stamped.event)
    }

    #[test]
    fn every_message_carries_the_message_time() {
        let mut state = TranslateState::default();
        let stamped = translate(
            WM_MOUSEMOVE,
            0,
            lparam(1, 2),
            MessageContext {
                ticks_ms: 4_321,
                wheel_lines: 3.0,
                scale: Scale::ONE,
            },
            &mut state,
        )
        .expect("mousemove");

        assert_eq!(stamped.ticks_ms, 4_321, "R02-14：时间戳来自 MSG.time");
    }

    #[test]
    fn mouse_move_decodes_negative_client_coordinates() {
        let event = translate_raw(WM_MOUSEMOVE, 0, lparam(-7, -300));

        assert_eq!(
            event,
            Some(PlatformEvent::Pointer {
                x: -7,
                y: -300,
                kind: PointerKind::Move
            })
        );
    }

    #[test]
    fn buttons_come_from_the_message_not_the_wparam() {
        // 按下时真实消息的 wParam 带 MK_LBUTTON，但抬起时 wParam 是 0：按钮必须由 msg 决定。
        let down = translate_raw(WM_RBUTTONDOWN, 0, lparam(3, 4));
        let up = translate_raw(WM_RBUTTONUP, 0, lparam(3, 4));

        assert_eq!(
            down,
            Some(PlatformEvent::Pointer {
                x: 3,
                y: 4,
                kind: PointerKind::Down(PointerButton::Secondary)
            })
        );
        assert_eq!(
            up,
            Some(PlatformEvent::Pointer {
                x: 3,
                y: 4,
                kind: PointerKind::Up(PointerButton::Secondary)
            })
        );
    }

    #[test]
    fn xbuttons_map_to_x1_and_x2() {
        let x1 = translate_raw(WM_XBUTTONDOWN, 1 << 16, lparam(0, 0));
        let x2 = translate_raw(WM_XBUTTONUP, 2 << 16, lparam(0, 0));

        assert!(matches!(
            x1,
            Some(PlatformEvent::Pointer {
                kind: PointerKind::Down(PointerButton::X1),
                ..
            })
        ));
        assert!(matches!(
            x2,
            Some(PlatformEvent::Pointer {
                kind: PointerKind::Up(PointerButton::X2),
                ..
            })
        ));
    }

    /// R02-10：滚轮一格 = `120` = **系统行数**行（`SPI_GETWHEELSCROLLLINES`，缺省 3）。
    #[test]
    fn wheel_ticks_become_system_lines() {
        let up = translate_raw(WM_MOUSEWHEEL, 120 << 16, lparam(10, 20));
        let down = translate_raw(
            WM_MOUSEWHEEL,
            ((-120i32 as u16 as u32) << 16) as usize,
            lparam(10, 20),
        );
        let horizontal = translate_raw(
            WM_MOUSEHWHEEL,
            ((-120i32 as u16 as u32) << 16) as usize,
            lparam(10, 20),
        );

        assert_eq!(
            up,
            Some(PlatformEvent::Scroll {
                x: 10,
                y: 20,
                dx_lines: 0.0,
                dy_lines: 3.0,
            })
        );
        assert!(matches!(
            down,
            Some(PlatformEvent::Scroll { dy_lines, .. }) if (dy_lines + 3.0).abs() < f32::EPSILON
        ));
        assert!(matches!(
            horizontal,
            Some(PlatformEvent::Scroll { dx_lines, .. }) if (dx_lines + 3.0).abs() < f32::EPSILON
        ));
    }

    #[test]
    fn unknown_wheel_lines_fall_back_to_three() {
        assert_eq!(lines_per_tick(0.0), 3.0, "SPI 为 0 / WHEEL_PAGESCROLL");
        assert_eq!(lines_per_tick(f32::NAN), 3.0);
        assert_eq!(lines_per_tick(1.0), 1.0);
        assert_eq!(lines_per_tick(5.0), 5.0);
    }

    #[test]
    fn size_notifications_decode_the_client_extent_and_skip_minimized() {
        let event = translate_raw(WM_SIZE, SIZE_RESTORED as usize, lparam(640, 480));
        let minimized = translate_raw(WM_SIZE, SIZE_MINIMIZED as usize, lparam(0, 0));

        assert_eq!(
            event,
            Some(PlatformEvent::Resize {
                width: 640,
                height: 480
            })
        );
        assert_eq!(minimized, None, "最小化没有可用的客户区尺寸");
    }

    #[test]
    fn dpi_change_reads_the_low_word() {
        let event = translate_raw(WM_DPICHANGED, (192usize << 16) | 144, 0);

        assert_eq!(event, Some(PlatformEvent::Dpi { dpi: 144 }));
    }

    #[test]
    fn char_messages_merge_surrogate_pairs() {
        let mut state = TranslateState::default();
        // 🌍 = U+1F30D = D83C DF0D
        let high = translate(WM_CHAR, 0xD83C, 0, context(), &mut state);
        let low = translate(WM_CHAR, 0xDF0D, 0, context(), &mut state);
        let plain = translate(WM_CHAR, 'a' as usize, 0, context(), &mut state);

        assert_eq!(high, None, "高代理单独不产生文本");
        assert_eq!(
            low.map(|stamped| stamped.event),
            Some(PlatformEvent::Text("🌍".into()))
        );
        assert_eq!(
            plain.map(|stamped| stamped.event),
            Some(PlatformEvent::Text("a".into()))
        );
    }

    /// P2-6 回归：失焦必须清修饰键。
    ///
    /// 修饰键按消息顺序维护，而失焦期间的抬起消息不会到达本窗口。不复位的话
    /// Alt+Tab 回来后 Shift/Ctrl 会永久"粘住"，所有输入都被当成组合键。
    #[test]
    fn losing_focus_clears_stuck_modifiers() {
        let mut state = TranslateState::default();
        // Shift 按下 + Ctrl 按下。
        let _ = translate(
            WM_KEYDOWN,
            0x10,
            key_lparam(0x2A, false, false),
            context(),
            &mut state,
        );
        let _ = translate(
            WM_KEYDOWN,
            0x11,
            key_lparam(0x1D, false, false),
            context(),
            &mut state,
        );
        assert_eq!(state.modifiers(), Modifiers::SHIFT | Modifiers::CONTROL);

        // 失焦：抬起消息永远不会到达，只能靠显式复位。
        state.reset_transient();
        assert_eq!(state.modifiers(), Modifiers::empty());

        // 复位幂等，且不影响之后的正常跟踪。
        state.reset_transient();
        assert_eq!(state.modifiers(), Modifiers::empty());
        let _ = translate(
            WM_KEYDOWN,
            0x10,
            key_lparam(0x2A, false, false),
            context(),
            &mut state,
        );
        assert_eq!(state.modifiers(), Modifiers::SHIFT);
    }

    #[test]
    fn key_events_track_modifiers_in_message_order() {
        let mut state = TranslateState::default();
        let shift_down = translate(
            WM_KEYDOWN,
            0x10,
            key_lparam(0x2A, false, false),
            context(),
            &mut state,
        );
        let a_down = translate(
            WM_KEYDOWN,
            0x41,
            key_lparam(0x1E, false, false),
            context(),
            &mut state,
        );

        assert_eq!(
            shift_down.map(|stamped| stamped.event),
            Some(PlatformEvent::Key {
                code: KeyCode::ShiftLeft,
                state: KeyState::Down,
                repeat: false,
            })
        );
        assert_eq!(state.modifiers(), Modifiers::SHIFT);
        assert_eq!(
            a_down.map(|stamped| stamped.event),
            Some(PlatformEvent::Key {
                code: KeyCode::KeyA,
                state: KeyState::Down,
                repeat: false,
            })
        );

        let _ = translate(
            WM_KEYUP,
            0x10,
            key_lparam(0x2A, false, false),
            context(),
            &mut state,
        );
        assert_eq!(state.modifiers(), Modifiers::empty());
    }

    /// R02-2 回归：`lParam` bit 30 = 自动重复，必须与真实按下区分。
    #[test]
    fn previous_key_state_marks_auto_repeat() {
        let mut state = TranslateState::default();
        let first = translate(
            WM_KEYDOWN,
            0x0D,
            key_lparam(0x1C, false, false),
            context(),
            &mut state,
        );
        let repeat = translate(
            WM_KEYDOWN,
            0x0D,
            key_lparam(0x1C, false, true),
            context(),
            &mut state,
        );
        let up = translate(
            WM_KEYUP,
            0x0D,
            key_lparam(0x1C, false, true),
            context(),
            &mut state,
        );

        assert!(matches!(
            first.map(|stamped| stamped.event),
            Some(PlatformEvent::Key { repeat: false, .. })
        ));
        assert!(
            matches!(
                repeat.map(|stamped| stamped.event),
                Some(PlatformEvent::Key {
                    repeat: true,
                    state: KeyState::Down,
                    ..
                })
            ),
            "长按 Enter 的重复消息必须带 repeat 位"
        );
        assert!(
            matches!(
                up.map(|stamped| stamped.event),
                Some(PlatformEvent::Key {
                    repeat: false,
                    state: KeyState::Up,
                    ..
                })
            ),
            "抬起没有重复语义，即使 bit30 置位"
        );
    }

    #[test]
    fn navigation_and_function_keys_map_to_named_codes() {
        assert_eq!(key_code_from_native(0x1B, 0x01, false), KeyCode::Escape);
        assert_eq!(key_code_from_native(0x26, 0x48, true), KeyCode::ArrowUp);
        assert_eq!(key_code_from_native(0x70, 0x3B, false), KeyCode::F1);
        assert_eq!(key_code_from_native(0x87, 0x6F, false), KeyCode::F24);
        // 扫描码优先：`VK_A` + 字母 `A` 的扫描码 ⇒ `KeyA`（布局无关）。
        assert_eq!(key_code_from_native(0x41, 0x1E, false), KeyCode::KeyA);
        // 扫描码缺失时退回 VK 的命名键；字母 VK 没有命名键 ⇒ 未识别。
        assert_eq!(
            key_code_from_native(0x41, 0, false),
            KeyCode::Unidentified(0x8041)
        );
    }

    /// R02-1：同一物理键在不同布局下得到同一个 `KeyCode`（扫描码是布局无关的）。
    ///
    /// AZERTY 键盘上物理 `Q` 位置产生 `VK_A`，但扫描码仍是 `0x10` ⇒ `KeyQ`（而不是 `KeyA`）。
    #[test]
    fn physical_identity_ignores_the_layout_dependent_vk() {
        assert_eq!(key_code_from_native(0x41, 0x10, false), KeyCode::KeyQ);
        assert_eq!(key_code_from_native(0x51, 0x1E, false), KeyCode::KeyA);
        // 小键盘与导航键共用扫描码，由 E0 扩展位区分（W3C 的 `Numpad7` vs `Home`）。
        assert_eq!(key_code_from_native(0x24, 0x47, false), KeyCode::Numpad7);
        assert_eq!(key_code_from_native(0x24, 0x47, true), KeyCode::Home);
        // 右 Ctrl 的 E0 扩展位也参与身份。
        assert_eq!(
            key_code_from_native(0xA3, 0x1D, true),
            KeyCode::ControlRight
        );
        assert_eq!(
            key_code_from_native(0xA2, 0x1D, false),
            KeyCode::ControlLeft
        );
    }

    /// R02-1（P1 回归）：`E0` 扩展行**不得**命中字母/功能键行。
    ///
    /// 旧实现用 `(scancode, _)` 匹配字母行：`E0 30`（音量+）⇒ `KeyB`、`E0 2E`（音量-）⇒
    /// `KeyC`、`E0 20`（静音）⇒ `KeyD`、`E0 22`（播放/暂停）⇒ `KeyG`、`E0 24`（停止）⇒
    /// `KeyJ`、`E0 19`（下一曲）⇒ `KeyP`、`E0 10`（上一曲）⇒ `KeyQ`、`E0 32`（主页）⇒
    /// `KeyM`，`E0 65..6F`（浏览器/邮件键）⇒ `F14..F24`。后果是音量键以**无修饰的 `KeyB`**
    /// 按下/抬起进入应用（交易终端里 `B` 买入是常见裸键绑定），且同一物理键三平台身份不同
    /// （Linux 是 `Unidentified(115)`）。
    #[test]
    fn extended_row_media_keys_never_fall_through_to_letters_or_function_keys() {
        // 扩展行全部已知键：扫描码 → 期望身份。
        let table: &[(u16, KeyCode)] = &[
            (0x10, KeyCode::MediaTrackPrevious),
            (0x19, KeyCode::MediaTrackNext),
            (0x20, KeyCode::AudioVolumeMute),
            (0x21, KeyCode::LaunchApp2),
            (0x22, KeyCode::MediaPlayPause),
            (0x24, KeyCode::MediaStop),
            (0x2E, KeyCode::AudioVolumeDown),
            (0x30, KeyCode::AudioVolumeUp),
            (0x32, KeyCode::BrowserHome),
            (0x5E, KeyCode::Power),
            (0x5F, KeyCode::Sleep),
            (0x63, KeyCode::WakeUp),
            (0x65, KeyCode::BrowserSearch),
            (0x66, KeyCode::BrowserFavorites),
            (0x67, KeyCode::BrowserRefresh),
            (0x68, KeyCode::BrowserStop),
            (0x69, KeyCode::BrowserForward),
            (0x6A, KeyCode::BrowserBack),
            (0x6B, KeyCode::LaunchApp1),
            (0x6C, KeyCode::LaunchMail),
            (0x6D, KeyCode::MediaSelect),
            // 扩展行的编辑键（与基础行小键盘共用扫描码）。
            (0x1C, KeyCode::NumpadEnter),
            (0x1D, KeyCode::ControlRight),
            (0x35, KeyCode::NumpadDivide),
            (0x38, KeyCode::AltRight),
            (0x47, KeyCode::Home),
            (0x48, KeyCode::ArrowUp),
            (0x49, KeyCode::PageUp),
            (0x4B, KeyCode::ArrowLeft),
            (0x4D, KeyCode::ArrowRight),
            (0x4F, KeyCode::End),
            (0x50, KeyCode::ArrowDown),
            (0x51, KeyCode::PageDown),
            (0x52, KeyCode::Insert),
            (0x53, KeyCode::Delete),
            (0x5B, KeyCode::MetaLeft),
            (0x5C, KeyCode::MetaRight),
            (0x5D, KeyCode::ContextMenu),
        ];
        for (scancode, expected) in table {
            assert_eq!(
                key_code_from_native(0, *scancode, true),
                *expected,
                "E0 {scancode:#04x} 必须是 {expected:?}"
            );
        }

        // 反例：**非扩展**的同扫描码仍是字母/数字/功能键。
        let base: &[(u16, KeyCode)] = &[
            (0x30, KeyCode::KeyB),
            (0x2E, KeyCode::KeyC),
            (0x20, KeyCode::KeyD),
            (0x22, KeyCode::KeyG),
            (0x24, KeyCode::KeyJ),
            (0x19, KeyCode::KeyP),
            (0x10, KeyCode::KeyQ),
            (0x32, KeyCode::KeyM),
            (0x65, KeyCode::F14),
            (0x6F, KeyCode::F24),
        ];
        for (scancode, expected) in base {
            assert_eq!(
                key_code_from_native(0, *scancode, false),
                *expected,
                "基础行 {scancode:#04x} 必须是 {expected:?}"
            );
        }

        // 扩展行的未知键：**绝不**猜成字母/F 键，载荷带 `0x100` 标记。
        assert_eq!(
            key_code_from_native(0, 0x6E, true),
            KeyCode::Unidentified(0x100 | 0x6E)
        );
        assert_eq!(
            key_code_from_native(0, 0x2A, true),
            KeyCode::Unidentified(0x100 | 0x2A),
            "E0 2A（部分键盘的伪 Shift）不得报告成 ShiftLeft"
        );
    }

    /// AR1b N3（P2 回归）：**VK 兜底只用于非扩展键**。
    ///
    /// 失败面（复审探针 8）：扩展表查不到时无条件落到 `key_code_from_vk`，于是
    /// `E0 6E + VK_DELETE → Delete`、`E0 2A + VK_SHIFT → ShiftLeft` —— 一个不存在的物理键
    /// 被「美化」成真实命名键，与「未知扩展键一律 `Unidentified`」的契约相反。表驱动测试只传
    /// `vk = 0`，测不出这条分叉，所以这里全部带**真实 VK**。
    #[test]
    fn extended_keys_never_fall_back_to_the_vk_table() {
        // 已知扩展键 + 真实 VK：扫描码表说了算（VK 不参与，也不能覆盖）。
        let known: &[(u16, u16, KeyCode)] = &[
            (0x2E, 0x2E, KeyCode::AudioVolumeDown),
            (0x30, 0x30, KeyCode::AudioVolumeUp),
            (0x20, 0x20, KeyCode::AudioVolumeMute),
            (0x22, 0xB3, KeyCode::MediaPlayPause),
            (0x10, 0xB1, KeyCode::MediaTrackPrevious),
            (0x19, 0xB0, KeyCode::MediaTrackNext),
            (0x24, 0xB2, KeyCode::MediaStop),
            (0x32, 0xAC, KeyCode::BrowserHome),
            (0x6A, 0xA6, KeyCode::BrowserBack),
            (0x6B, 0xB4, KeyCode::LaunchApp1),
            (0x47, 0x24, KeyCode::Home),
            (0x53, 0x2E, KeyCode::Delete),
            (0x1D, 0xA3, KeyCode::ControlRight),
            (0x38, 0xA5, KeyCode::AltRight),
            (0x5B, 0x5B, KeyCode::MetaLeft),
            (0x5D, 0x5D, KeyCode::ContextMenu),
        ];
        for (scancode, vk, expected) in known {
            assert_eq!(
                key_code_from_native(*vk, *scancode, true),
                *expected,
                "E0 {scancode:#04x} + VK {vk:#04x} 必须是 {expected:?}（扫描码表说了算）"
            );
        }

        // **反例**：扩展表里没有的扫描码 + 一个「看起来能兜底」的真实 VK ⇒ 一律 `Unidentified`。
        let unknown: &[(u16, u16, u16)] = &[
            (0x6E, 0x2E, 0x16E), // 伪 Delete
            (0x2A, 0x10, 0x12A), // 部分键盘上的伪 Shift（物理身份不是左 Shift）
            (0x64, 0x70, 0x164), // 伪 F1（VK_F1 = 0x70）
            (0x46, 0x0D, 0x146), // 伪 Enter
            (0x37, 0x24, 0x137), // PrtSc 序列里的第一段伪 Home
            (0x1F, 0x25, 0x11F), // 伪 VK_LEFT（扩展键不得因 VK 变成 ArrowLeft）
        ];
        for (scancode, vk, payload) in unknown {
            assert_eq!(
                key_code_from_native(*vk, *scancode, true),
                KeyCode::Unidentified(*payload),
                "E0 {scancode:#04x} + VK {vk:#04x} 不得被 VK 兜底美化"
            );
        }

        // 对照：非扩展键的 VK 兜底仍然成立（扫描码缺失 / 不含对应项时才用）。
        assert_eq!(key_code_from_native(0x2E, 0, false), KeyCode::Delete);
        assert_eq!(key_code_from_native(0x0D, 0, false), KeyCode::Enter);
        assert_eq!(key_code_from_native(0x70, 0, false), KeyCode::F1);
        // 非扩展 + **未知**扫描码 + 已知 VK：这是「扫描码无对应项」的合法回退点。
        assert_eq!(key_code_from_native(0x0D, 0x55, false), KeyCode::Enter);
        // 扩展 + **无扫描码**：同样不给 VK 兜底（扩展键的 VK 与物理身份不可靠）。
        assert_eq!(
            key_code_from_native(0x2E, 0, true),
            KeyCode::Unidentified(0x8000 | 0x2E),
            "没有扫描码的扩展消息只能用 VK 载荷，不得命名成 Delete"
        );
    }

    /// AR1 §2-14：Windows 缺的 `NumpadComma` / `NumpadEqual` 补齐。
    #[test]
    fn numpad_comma_and_equal_are_reachable() {
        assert_eq!(
            key_code_from_native(0x7E, 0x7E, false),
            KeyCode::NumpadComma
        );
        assert_eq!(
            key_code_from_native(0x59, 0x59, false),
            KeyCode::NumpadEqual
        );
        // `VK_HELP`（0x2F）没有扫描码时也能给出 `Help`。
        assert_eq!(key_code_from_native(0x2F, 0, false), KeyCode::Help);
    }

    #[test]
    fn pause_and_print_screen_use_the_vk_because_scancodes_collide() {
        assert_eq!(key_code_from_native(0x13, 0x45, false), KeyCode::Pause);
        assert_eq!(key_code_from_native(0x2C, 0x37, true), KeyCode::PrintScreen);
        assert_eq!(key_code_from_native(0x90, 0x45, false), KeyCode::NumLock);
    }

    #[test]
    fn focus_and_close_have_no_payload_variants() {
        assert_eq!(
            translate_raw(WM_SETFOCUS, 0, 0),
            Some(PlatformEvent::Focus(true))
        );
        assert_eq!(
            translate_raw(WM_KILLFOCUS, 0, 0),
            Some(PlatformEvent::Focus(false))
        );
        assert_eq!(translate_raw(WM_CLOSE, 0, 0), Some(PlatformEvent::Close));
    }

    /// R07-6：`VK_PROCESSKEY`（0xE5）是「这个键被输入法吃了」，`VK_PACKET`（0xE7）是注入文本
    /// 的载体 —— 两者都**不是**应用可观察的按键，不得派发 `Key::Other`。
    #[test]
    fn ime_processed_and_packet_keys_never_become_key_events() {
        // 组合期间按下/抬起拼音字母：wParam 恒为 VK_PROCESSKEY。
        assert_eq!(translate_raw(WM_KEYDOWN, 0xE5, 0), None);
        assert_eq!(translate_raw(WM_KEYUP, 0xE5, 0), None);
        // SendInput(KEYEVENTF_UNICODE) / 语音输入 / 辅助技术注入。
        assert_eq!(translate_raw(WM_KEYDOWN, 0xE7, 0), None);
        assert_eq!(translate_raw(WM_KEYUP, 0xE7, 0), None);
        // 高 8 位是系统填充的扩展信息：真实 wParam 只有低 8 位是键码。
        assert_eq!(translate_raw(WM_KEYDOWN, 0x01_00E5, 0), None);
        // 普通字母不受影响（'A' 的 VK 是 0x41，扫描码 0x1E）。
        assert!(matches!(
            translate_raw(WM_KEYDOWN, 0x41, key_lparam(0x1E, false, false)),
            Some(PlatformEvent::Key {
                code: KeyCode::KeyA,
                state: KeyState::Down,
                ..
            })
        ));
    }

    /// R07-5：提交去重按 **UTF-16 码元** 逐条比对；不匹配时队列自愈，绝不吞真实键入。
    #[test]
    fn committed_text_suppresses_only_the_matching_char_units() {
        let mut state = TranslateState::default();
        // 模拟 `ImeState` 在 `GCS_RESULTSTR` 提交后排队的内容：「🌍a」= 3 个码元。
        // 入队时间取 `context()` 的 `MSG.time`（同一批消息共享时间域）。
        state.ime_mut().queue_commit("🌍a", context().ticks_ms);
        // 系统随后补发 3 条 `WM_CHAR`（每码元一条）：全部被抑制。
        for unit in "🌍a".encode_utf16() {
            assert_eq!(
                translate(WM_CHAR, usize::from(unit), 0, context(), &mut state),
                None,
                "提交后补发的 WM_CHAR 必须被抑制"
            );
        }
        // 队列已空：真正的键入必须放行。
        let text = translate(
            WM_CHAR,
            usize::from(u16::from(b'x')),
            0,
            context(),
            &mut state,
        )
        .expect("WM_CHAR");
        assert_eq!(text.event, PlatformEvent::Text(SmolStr::new_static("x")));
    }

    /// AR3 §1.5（P2-2）：候选窗提交后系统**不**补发 `WM_CHAR`，用户随即键入与提交首码元相同
    /// 的字符 —— 真实键入 = `WM_KEYDOWN` + `WM_CHAR`，按键按下必须清空抑制队列。
    ///
    /// 失败面：残留队列留到下一次组合 / 失焦；用户键入同首码元字符时 `consume_char` 命中，
    /// 真实按键被吞掉一次（「提交后直接键入丢一个字符」）。
    #[test]
    fn a_real_key_press_after_a_commit_is_never_swallowed() {
        let mut state = TranslateState::default();
        // 提交字面量 "a"（微软拼音 a+Enter 的形态）但系统没有补发 WM_CHAR。
        state.ime_mut().queue_commit("a", context().ticks_ms);
        // 用户立刻真实键入 'a'：keydown 先到（0x41 = VK_A，扫描码 0x1E）。
        let key = translate(
            WM_KEYDOWN,
            0x41,
            key_lparam(0x1E, false, false),
            context(),
            &mut state,
        )
        .expect("按键");
        assert!(matches!(
            key.event,
            PlatformEvent::Key {
                state: KeyState::Down,
                ..
            }
        ));
        // 随后 TranslateMessage 产生的 WM_CHAR：不得被残留抑制队列吞掉。
        let text = translate(
            WM_CHAR,
            usize::from(u16::from(b'a')),
            0,
            context(),
            &mut state,
        )
        .expect("真实键入必须放行");
        assert_eq!(text.event, PlatformEvent::Text(SmolStr::new_static("a")));
    }

    /// AR3 §1.5：跨过「入队批次 + 紧随其后的一轮」后，残留队列（提交后系统没补发 `WM_CHAR`）
    /// 必须作废，不能吞掉更晚的真实键入（真实键入即使没有 keydown —— 例如粘贴后的 WM_CHAR）。
    #[test]
    fn a_queue_left_over_from_an_older_message_batch_is_dropped() {
        let mut state = TranslateState::default();
        state.ime_mut().queue_commit("a", context().ticks_ms);
        // 两轮消息排空之后：不是补发字符了。
        state.end_message_batch();
        state.end_message_batch();
        let text = translate(
            WM_CHAR,
            usize::from(u16::from(b'a')),
            0,
            context(),
            &mut state,
        )
        .expect("过期队列必须放行");
        assert_eq!(text.event, PlatformEvent::Text(SmolStr::new_static("a")));
    }

    /// 失焦（`reset_transient`）必须清空 IME 待抑制队列：否则残留码元会静默吞掉
    /// 之后真实键入的字符（R07-5 的核心缺陷）。
    #[test]
    fn losing_focus_drops_pending_ime_suppression() {
        let mut state = TranslateState::default();
        state.ime_mut().queue_commit("ni", context().ticks_ms);

        state.reset_transient();

        let event = translate(
            WM_CHAR,
            usize::from(u16::from(b'n')),
            0,
            context(),
            &mut state,
        );
        assert_eq!(
            event.map(|stamped| stamped.event),
            Some(PlatformEvent::Text(SmolStr::new_static("n"))),
            "失焦后第一次键入必须放行"
        );
    }
}
