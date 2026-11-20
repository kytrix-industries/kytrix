//! 平台无关的 IME 纯逻辑：
//!
//! - 组合串子句（highlight runs）与子句边界的归一化：Windows 的 `GCS_COMPATTR` /
//!   `GCS_COMPCLAUSE` 原始字节在这里被压成 [`crate::event::ImeEvent::Preedit`] 的 `clauses`
//!   区间（三平台共用一种表示，见 [`preedit_clauses`]）。
//! - Linux `zwp_text_input_v3.preedit_string` 的**字节偏移光标** → `Preedit.caret` /
//!   `Preedit.clauses`（UTF-16 码元；协议给的是 UTF-8 字节，见 [`preedit_carets`]）。
//! - 提交去重：连续相同字符 / 双空格是合法输入，只有**同一作用域内**的重复回调才吞掉。
//!   macOS 的 `NSTextInputClient` 按原生按键记账（[`ImeCommitScope`]），它额外负责
//!   「本按键的正文是否已由 IME 交付」这一判定（R09-3），因此作用域结束时清空**全部**记账。
//!   Linux 的 `zwp_text_input_v3` 一侧**没有**去重：协议按 `done` 双缓冲交付，一批只有一个
//!   `commit_string`，跨批的相同文本是合法输入（AR1b P3-1：旧的 `CommitDeduper` 在 Linux
//!   没有消费者，`begin_key + note` 恒为真）。
//! - AppKit `replacementRange`（UTF-16）→ 组合串内替换区间（macOS `NSTextInputClient` 专用，
//!   但换算规则是纯整数处理，与 AppKit 类型无关）。
//!
//! 放在平台无关层是为了让这些纯逻辑能在开发机（Windows）上被单测覆盖：
//! macOS 的 `NSTextInputClient` 包装器只在 macOS 目标上编译，测试跑不到。

// 子句区间（`Range`/`SmallVec`）的消费者：Windows 的 `GCS_COMPATTR`/`GCS_COMPCLAUSE`、
// Linux 的 `preedit_string` 高亮区间（`cursor_begin != cursor_end`）与测试构建；macOS 不产生
// 子句区间，无条件 import 会变成死导入（-D warnings）。macOS 侧唯一用到 `Range` 的地方
// （`replacement_range` 的返回类型）用的是全路径。
#[cfg(any(windows, target_os = "linux", test))]
use std::ops::Range;

#[cfg(any(windows, target_os = "linux", test))]
use smallvec::SmallVec;

/// 组合串子句区间的最大条数（`SmallVec` 内联容量；超出部分堆分配）。
///
/// 实测输入法的组合串子句数是个位数（日文一次変換常见 1–3 段），4 条内联覆盖绝大多数按键，
/// 多出来的走堆分配不影响正确性。
///
/// 只在 Windows（`GCS_COMPATTR` / `GCS_COMPCLAUSE` 的消费者）、Linux（`preedit_string` 的
/// 高亮区间）+ 测试构建里编译：macOS 不产生子句区间，留着就是死代码。
#[cfg(any(windows, target_os = "linux", test))]
pub(crate) const PREEDIT_CLAUSE_INLINE: usize = 4;

/// 组合串子句区间（UTF-16 码元，相对组合串起点）。
#[cfg(any(windows, target_os = "linux", test))]
pub(crate) type ClauseRanges = SmallVec<[Range<usize>; PREEDIT_CLAUSE_INLINE]>;

/// 每码元一个属性字节的原始属性数组 → 「非默认属性的连续区间」。
///
/// `plain` 是「仍在输入中」的属性值（Win32 `ATTR_INPUT`）。输入法的属性字节是**每 UTF-16
/// 码元**一个，直接把属性交给文本层没有意义（一码元一个值），必须压成区间。
///
/// 纯函数：`attrs` 长度即组合串码元数；越界的 `attrs`（比文本长）被忽略，不会产出越界区间。
#[cfg(any(windows, test))]
pub(crate) fn highlight_runs(attrs: &[u8], plain: u8) -> ClauseRanges {
    let mut runs = ClauseRanges::new();
    let mut start: Option<usize> = None;
    for (index, &attr) in attrs.iter().enumerate() {
        if attr == plain {
            if let Some(from) = start.take() {
                runs.push(from..index);
            }
        } else if start.is_none() {
            start = Some(index);
        }
    }
    if let Some(from) = start {
        runs.push(from..attrs.len());
    }
    runs
}

/// `GCS_COMPCLAUSE` 的子句边界（N+1 个 UTF-16 偏移，末项 = 组合串长度）→ 子句区间。
///
/// 输入法给的数据不一定严格单调（转换中的候选重排、第三方 IME 的越界值都会出现），因此这里
/// 做三件事而不是直接 `windows(2)`：**丢弃非单调项**、**夹到 `0..=len`**、**丢弃空区间**。
/// 结果是升序、互不重叠、全部落在 `0..=len` 的子句列表。
#[cfg(any(windows, test))]
pub(crate) fn clause_ranges(offsets: &[u32], len: usize) -> ClauseRanges {
    let mut ranges = ClauseRanges::new();
    let mut start: Option<usize> = None;
    for &raw in offsets {
        let end = (raw as usize).min(len);
        match start {
            // 非单调（比上一个边界还小）：忽略这一项，保持当前子句继续延伸。
            Some(from) if end < from => {}
            Some(from) => {
                if end > from {
                    ranges.push(from..end);
                }
                start = Some(end);
            }
            None => start = Some(end),
        }
    }
    ranges
}

/// 组合串的子句/高亮区间：优先用属性字节（更精确），退到子句边界，都没有则空。
///
/// Win32 的 `WM_IME_COMPOSITION` 是 bitmask：`GCS_COMPATTR` 与 `GCS_COMPCLAUSE` 可能只到其一
/// （第三方 IME 常见只给 `COMPSTR`+`COMPCLAUSE`）。引擎统一成同一种表示，文本层不需要知道
/// 输入法给了哪几项。
#[cfg(any(windows, test))]
pub(crate) fn preedit_clauses(
    attrs: Option<&[u8]>,
    clause_offsets: Option<&[u32]>,
    len: usize,
    plain: u8,
) -> ClauseRanges {
    if let Some(attrs) = attrs
        && !attrs.is_empty()
    {
        return highlight_runs(&attrs[..attrs.len().min(len)], plain);
    }
    if let Some(offsets) = clause_offsets
        && !offsets.is_empty()
    {
        return clause_ranges(offsets, len);
    }
    ClauseRanges::new()
}

/// AppKit `NSNotFound`：`replacementRange` 用它表示「没有显式替换区间」。
///
/// **值是 `NSIntegerMax`（`isize::MAX`），不是 `NSUInteger::MAX`**：objc2-foundation 的
/// `NSNotFound: NSInteger = NSIntegerMax`，AppKit 传给 `NSUInteger` 形参时是
/// `0x7FFF_FFFF_FFFF_FFFF`（64 位）。写 `usize::MAX` 会让真实输入法的正常路径
/// `{NSNotFound, 0}` 不匹配、退化成「在组合串尾部插入」（第三轮复审 P1）。
///
/// 只有 macOS 包装器、macOS 绑定测试（`platform/macos/ime.rs`）与跨平台单测用得到；
/// Linux 的 `zwp_text_input_v3` 是整串 preedit，没有区间替换概念，因此编译期排除，
/// 避免 `dead_code`。
#[cfg(any(target_os = "macos", test))]
pub(crate) const NS_NOT_FOUND: usize = isize::MAX as usize;

/// AppKit `replacementRange`（UTF-16 码元，**文档坐标**）→ 组合串内要替换的旧文本区间。
///
/// `setMarkedText:selectedRange:replacementRange:` / `insertText:replacementRange:` 的
/// `replacementRange` 语义是「用新文本替换掉该区间里的旧文本」；`{NSNotFound, 0}` 表示没有
/// 显式区间 —— 替换当前的整个 marked text（没有 marked text 时在插入点插入）。
///
/// `location` / `length` 是 AppKit 的**文档坐标**（与 `markedRange` 同一坐标系），不是组合串
/// 内坐标：真实的 `NSTextInputClient` 文本存储覆盖整个文档，Qt 的实现同样先减去
/// `markedRange.location` 再切片。本 view 只把组合串暴露为文本存储，所以
/// `marked_location` 传 `ImeState::marked_range().0`（当前恒为 0）。**该参数必须与引擎真实
/// 文本里组合串的起点一致**，否则替换区间会整体偏移；未来接通 surrounding text 时把真实
/// 起点传进来即可，本函数不用改。
///
/// 先换算成组合串内相对坐标，再按现有规则 clamp：
///
/// - `location` 在组合串之前：相对起点饱和到 `0`（起点 clamp 到组合串开头；区间本身无法
///   表达，属 surrounding-text 未实现的已知限制）；
/// - 越界 clamp：`start` / `end` 都收敛到 `0..=marked.len()`（输入法在候选/重转换路径上
///   可能给出与当前组合串不一致的区间，直接按入参切片会越界）；
/// - 码点边界吸附：区间端点落在代理对中间时向外扩到整个码点（拆开代理对会得到非法 UTF-16，
///   应用侧无法安全切片）。
///
/// 返回的区间满足 `start <= end <= marked.len()`，且两个端点都是**码点边界**。
///
/// 平台无关（入参是 `usize` 而不是 `NSRange`）：macOS 的包装器只在 macOS 目标上编译，
/// 换算规则放这里才能在开发机（Windows）上单测。
#[cfg(any(target_os = "macos", test))]
pub(crate) fn replacement_range(
    location: usize,
    length: usize,
    marked: &[u16],
    marked_location: usize,
) -> Option<std::ops::Range<usize>> {
    if location == NS_NOT_FOUND {
        return None;
    }
    // `marked_location` 是 `markedRange.location`（组合串起点的文档坐标），不可能是
    // `NSNotFound`：真没有组合串时长度也为 0，起点仍是合法位置。
    debug_assert_ne!(
        marked_location, NS_NOT_FOUND,
        "marked_location 必须是组合串起点的文档坐标，不是 NSNotFound"
    );
    // 文档坐标 → 组合串内坐标：起点在组合串之前时饱和到 0（其余 clamp 在下面统一处理）。
    let location = location.saturating_sub(marked_location);
    let len = marked.len();
    let mut start = location.min(len);
    let mut end = location.saturating_add(length).min(len);
    // UTF-16 代理对只占一个码点，但拆成两个码元后不是合法 UTF-16。
    let is_high_surrogate = |unit: u16| (0xD800..=0xDBFF).contains(&unit);
    let is_low_surrogate = |unit: u16| (0xDC00..=0xDFFF).contains(&unit);
    if start > 0 && start < len && is_low_surrogate(marked[start]) {
        // 起点落在低代理码元上 ⇒ 边界回到它前面的高代理（低代理前面必有高代理，不会下溢）。
        start -= 1;
    }
    if end > 0 && end < len && is_high_surrogate(marked[end - 1]) {
        // 终点停在高代理码元之后（= 落在代理对中间）⇒ 收进后面的低代理。
        end += 1;
    }
    Some(start..end)
}

/// `zwp_text_input_v3.preedit_string` 的字节偏移光标 → (`Preedit.caret`, 高亮子句)。
///
/// 协议（`text-input-unstable-v3.xml`）：
///
/// > The parameters cursor_begin and cursor_end are counted in bytes relative to the beginning
/// > of the submitted text buffer. Cursor should be hidden when both are equal to -1.
/// > They could be represented by the client as a line if both values are the same, or as a
/// > text highlight otherwise.
///
/// 引擎的 `caret` / `clauses` 单位是 **UTF-16 码元**（与 Win32 `GCS_CURSORPOS`、AppKit
/// `selectedRange.location` 同一单位），所以这里做 字节 → UTF-16 码元 的换算：
///
/// - 偏移落在多字节字符中间、或越过串尾 ⇒ 收敛到**最近的合法码点边界**（`0..=len`）；
/// - 两者相等 ⇒ 插入点，`caret = Some(begin)`，无高亮；
/// - 两者不等 ⇒ 高亮区间，`caret = Some(begin)` 且 `clauses = [begin..end)`；
/// - 任一为负（协议只有「两者都是 -1」的合法情形）⇒ `caret = None` + 空子句：引擎没有
///   「隐藏光标」的表示，`None` 让文本层退化为「插入点在末尾」（已在 `ImeEvent` 文档写明）。
#[cfg(any(target_os = "linux", test))]
pub(crate) fn preedit_carets(
    text: &str,
    cursor_begin: i32,
    cursor_end: i32,
) -> (Option<u32>, ClauseRanges) {
    if cursor_begin < 0 || cursor_end < 0 {
        return (None, ClauseRanges::new());
    }
    // 字节偏移 → UTF-16 码元偏移：逐码点累加，偏移落在字符中间时停在**该字符之前**。
    let unit_at = |byte: i32| -> usize {
        let limit = byte as usize;
        let mut units = 0;
        let mut bytes = 0;
        for ch in text.chars() {
            let width = ch.len_utf8();
            if bytes + width > limit {
                break;
            }
            bytes += width;
            units += ch.len_utf16();
        }
        units
    };
    let start = unit_at(cursor_begin);
    let end = unit_at(cursor_end);
    let mut clauses = ClauseRanges::new();
    if end > start {
        clauses.push(start..end);
    }
    (Some(start as u32), clauses)
}

/// IME 组合期 `InputEvent::Key` 的可见性判定（三端统一，B4）。
///
/// 语义（与 Win32 的 `VK_PROCESSKEY` 过滤一致，macOS / Wayland 对齐）：
///
/// - **组合期**（mac `hasMarkedText` / Linux 已有非空 preedit）不投递裸 `InputEvent::Key`：
///   组合中的字母/数字/空格会被输入法翻译成候选，应用若同时收到裸键就会命中快捷键
///   （下单误触级别）；被 IME 消费的按键（mac `handleEvent == true`）同样不投递；
/// - **带 Ctrl/Alt/Meta 的命令组合始终投递**：Win32 在这些键上通常不产生 `VK_PROCESSKEY`
///   （`Ctrl+W` / `Cmd+C` 必须可用）；AppKit 的菜单键等价在 `keyDown:` 之前处理，不受影响；
///   Shift 不算命令修饰键（`Shift+字母` 是正常的上屏输入）。
///
/// 提交键的 key-up **始终投递**（不受本函数约束）：抬起不产生正文，应用只用它清
/// `held_keys`；组合期连 up 一起吞会让「按下 → 组合开始 → 抬起」把按住状态永久残留
/// （RB-02）。Win32 同样只把按下替换成 `VK_PROCESSKEY`，抬起以真实 `VK_*` 到达。
///
/// `ImeEvent::Commit/Preedit` 通路不受本函数影响（组合结果必须交付）。
#[cfg(any(target_os = "macos", target_os = "linux", test))]
pub(crate) const fn key_visible(
    ime_consumed: bool,
    composing: bool,
    command_modifiers: bool,
) -> bool {
    command_modifiers || (!ime_consumed && !composing)
}

/// IME 组合「恰好一条 `End`」的显式记账（RB-09）。
///
/// # 为什么不能用 `hasMarkedText` 推断
///
/// 清场路径（禁用 IME / 窗口失焦）调用 `NSTextInputContext.discardMarkedText` 让 AppKit 丢弃
/// 转换会话，但 AppKit **没有承诺**它会回调哪个 `NSTextInputClient` 方法：
///
/// | `discardMarkedText` 的回调形态 | 修复前按 `hasMarkedText` 推断的后果 |
/// |---|---|
/// | 同步 `unmarkText:`（回调自己投递 `End`） | 0 条补充 → 恰好一条（唯一侥幸成立的形态） |
/// | `setMarkedText:@""`（清空组合串、不投 `End`） | 推断为「已结束」→ **0 条 `End`**（preedit 残留） |
/// | 异步 `unmarkText:`（返回后才回调） | 推断为「仍在组合」→ 补一条 + 回调一条 = **2 条 `End`** |
/// | `insertText:`（提交正文，投 `Commit`） | 推断为「已结束」→ 0 条 `End`（可接受，但 Commit 不是 End） |
///
/// 因此收尾不能由「组合串是否还在」推断，必须由**回调记账**：应用收到过 `Preedit` 就欠一条
/// 收尾；`End` 只在欠账时投递一次，`Commit` 同样销账（正文已上屏，不需要 `End`）。迟到的
/// `unmarkText:` 看到账已销 ⇒ 吞掉，不会出现第二条。
#[cfg(any(target_os = "macos", test))]
#[derive(Debug, Default)]
pub(crate) struct ImeCompositionEnd {
    /// 应用收到过 `Preedit` 且尚未收到收尾（`End` 或 `Commit`）。
    pending: bool,
}

#[cfg(any(target_os = "macos", test))]
impl ImeCompositionEnd {
    /// 组合串已交给应用（`Preedit`）：从现在起欠一条收尾。
    pub(crate) fn note_preedit(&mut self) {
        self.pending = true;
    }

    /// 投递 `End` 的决策：返回 `true` = 现在该投（并销账）；`false` = 已经收尾过，吞掉。
    ///
    /// 同一个函数既服务回调路径（`unmarkText:`）也服务清场兜底（`discardMarkedText` 之后）：
    /// 谁先到谁销账，后到的一方看到已销账就不再投 —— 这正是「恰好一条」的全部内容。
    pub(crate) fn note_end(&mut self) -> bool {
        std::mem::take(&mut self.pending)
    }

    /// 提交（`insertText:` → `Commit`）：组合以正文结束，收尾不再补 `End`。
    pub(crate) fn note_commit(&mut self) {
        self.pending = false;
    }

    /// 是否欠一条收尾（清场入口据此决定要不要真的 `discardMarkedText`）。
    pub(crate) fn is_pending(&self) -> bool {
        self.pending
    }
}

/// 一次**原生输入事件**的作用域令牌（macOS 的每个 `keyDown:` 一个）。
///
/// 用令牌而不是裸 `bool` 的原因：`insertText:` 可能由非按键路径触发（鼠标点候选窗、听写、
/// 表情面板、Option+码位），此时 AppKit **没有**对应的 `keyDown:`。裸 `bool` 会被这类提交
/// 置位并一直留到下一次按键（旧实现吞掉下一个真实字符的根因）。令牌把「谁的作用域」绑定到
/// 具体事件：作用域结束后令牌失效，迟到的提交被判定为「作用域外」，**不可能**影响任何按键。
#[cfg(any(target_os = "macos", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct KeyScopeToken(u64);

/// 一次 IME 提交的裁决。
#[cfg(any(target_os = "macos", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommitVerdict {
    /// 作用域内的首次提交：派发 `ImeEvent::Commit`，并抑制**同一作用域**的 `Text`。
    DeliverInScope,
    /// 作用域外（听写 / 鼠标点候选窗 / 粘贴 / 上一按键迟到的回调）的提交：只派发 `Commit`，
    /// **不**影响任何按键的 `Text`。
    DeliverOutsideScope,
    /// 同一作用域内对同一文本的重复回调（输入法会重复投递）：吞掉。
    Duplicate,
}

/// 按键作用域内的提交记账（R09-3）。
///
/// 不变式：**所有状态的生命周期 ⊂ 一个作用域**。`begin_key` 打开作用域并返回令牌，
/// `end_key` 关闭并清空；`takes_text` 只对**当前**令牌为真。于是「本次按键的正文已由 IME
/// 交付」这件事在结构上不可能泄漏到下一次按键。
///
/// 匹配单位是 **UTF-16 码元**（与 `NSRange` / `AppKit` 的文本单位一致，也就能正确处理代理对
/// 与 `NSAttributedString` 的等价文本）。
#[cfg(any(target_os = "macos", test))]
#[derive(Debug, Default)]
pub(crate) struct ImeCommitScope {
    /// 当前打开的作用域（`None` = 不在按键内）。
    open: Option<KeyScopeToken>,
    /// 本作用域内已交付的提交文本（UTF-16 码元）。
    delivered: Option<Vec<u16>>,
    /// 令牌发号器（单调递增；`0` 从不使用，避免与 `Default` 混淆）。
    next: u64,
}

#[cfg(any(target_os = "macos", test))]
impl ImeCommitScope {
    /// 打开一个作用域（一次 `keyDown:`），顺手丢弃上一个作用域的残留状态。
    pub(crate) fn begin_key(&mut self) -> KeyScopeToken {
        self.next += 1;
        let token = KeyScopeToken(self.next);
        self.open = Some(token);
        self.delivered = None;
        token
    }

    /// 关闭作用域：**清空**本作用域的所有记账（旧令牌从此不再生效）。
    ///
    /// 只认当前令牌：嵌套/迟到的作用域收尾不会误清别人的状态。
    pub(crate) fn end_key(&mut self, token: KeyScopeToken) {
        if self.open == Some(token) {
            self.open = None;
            self.delivered = None;
        }
    }

    /// 记录一次提交并给出裁决。
    pub(crate) fn note_commit(&mut self, text: &str) -> CommitVerdict {
        if self.open.is_none() {
            // 按键之外（听写 / 鼠标点候选窗 / 粘贴）：只交付正文，不参与任何按键的抑制。
            return CommitVerdict::DeliverOutsideScope;
        }
        let utf16: Vec<u16> = text.encode_utf16().collect();
        if utf16.is_empty() {
            // 空提交不携带正文：不能记成「本按键已交付」，否则同一次按键的 `Text` 会被无端抑制。
            return CommitVerdict::DeliverInScope;
        }
        if self.delivered.as_deref() == Some(utf16.as_slice()) {
            // 同一次按键内输入法重复投递同一段文本：只交付一次。
            return CommitVerdict::Duplicate;
        }
        self.delivered = Some(utf16);
        CommitVerdict::DeliverInScope
    }

    /// 本作用域内是否已经交付过正文（`keyDown:` 据此抑制同一次按键的 `Text`）。
    pub(crate) fn takes_text(&self, token: KeyScopeToken) -> bool {
        self.open == Some(token) && self.delivered.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ClauseRanges, NS_NOT_FOUND, clause_ranges, highlight_runs, key_visible, preedit_carets,
        preedit_clauses, replacement_range,
    };

    /// `ATTR_INPUT`（Win32 值 0）：仍在输入、不需要高亮的那一段。
    const PLAIN: u8 = 0x00;

    #[test]
    fn highlight_runs_merges_adjacent_units_of_the_same_kind() {
        // 5 个码元：输入中 输入中 已转换 已转换 输入中 ⇒ 唯一高亮段 [2, 4)。
        let attrs = [PLAIN, PLAIN, 0x01, 0x01, PLAIN];

        assert_eq!(
            highlight_runs(&attrs, PLAIN)
                .into_iter()
                .collect::<Vec<_>>(),
            vec![2..4]
        );
    }

    #[test]
    fn highlight_runs_ignores_the_plain_attribute_and_keeps_edge_runs() {
        // 整串都在输入中：没有任何高亮段（不能返回 0..len）。
        assert!(highlight_runs(&[PLAIN, PLAIN, PLAIN], PLAIN).is_empty());
        // 首尾都是高亮段：两个区间分别产出，不会合并。
        assert_eq!(
            highlight_runs(&[0x02, PLAIN, 0x02, 0x02], PLAIN)
                .into_iter()
                .collect::<Vec<_>>(),
            vec![0..1, 2..4]
        );
        // 空串不留区间。
        assert!(highlight_runs(&[], PLAIN).is_empty());
    }

    #[test]
    fn clause_ranges_drops_empty_and_non_monotonic_offsets() {
        // 0,0,2,2,4：空区间（0..0、2..2）被丢掉，只留 [0,2) 与 [2,4)。
        assert_eq!(
            clause_ranges(&[0, 0, 2, 2, 4], 4)
                .into_iter()
                .collect::<Vec<_>>(),
            vec![0..2, 2..4]
        );
        // 非单调项被忽略：0,3,1,4 ⇒ [0,3) 之后 1 比 3 小 ⇒ 丢掉，再由 4 收尾成 [3,4)。
        assert_eq!(
            clause_ranges(&[0, 3, 1, 4], 4)
                .into_iter()
                .collect::<Vec<_>>(),
            vec![0..3, 3..4]
        );
    }

    #[test]
    fn clause_ranges_clamps_to_the_composition_length() {
        // 越界边界夹到组合串长度；末项缺失也不越界。
        assert_eq!(
            clause_ranges(&[0, 2, 99], 3)
                .into_iter()
                .collect::<Vec<_>>(),
            vec![0..2, 2..3]
        );
        assert!(clause_ranges(&[], 3).is_empty());
    }

    #[test]
    fn preedit_clauses_prefers_attributes_over_clause_boundaries() {
        let attrs = [PLAIN, 0x01, PLAIN];
        let offsets = [0, 3];

        // 属性可用时用它：高亮段是 [1,2)，不是子句边界给的 [0,3)。
        assert_eq!(
            preedit_clauses(Some(&attrs), Some(&offsets), 3, PLAIN)
                .into_iter()
                .collect::<Vec<_>>(),
            vec![1..2]
        );
        // 属性缺失（第三方 IME 只给 COMPCLAUSE）时退到边界。
        assert_eq!(
            preedit_clauses(None, Some(&offsets), 3, PLAIN)
                .into_iter()
                .collect::<Vec<_>>(),
            vec![0..3]
        );
        // 两者都没有 ⇒ 空（整串仍在输入中）。
        assert!(preedit_clauses(None, None, 3, PLAIN).is_empty());
        assert!(preedit_clauses(Some(&[]), Some(&[]), 3, PLAIN).is_empty());
    }

    /// UTF-16 码元视图（AppKit 的 `NSRange` 单位）。
    fn utf16(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    #[test]
    fn ns_not_found_means_replace_the_current_marked_text() {
        // AppKit 的 `{NSNotFound, 0}`：没有显式区间，调用方替换整个组合串。
        // 真实哨兵是 `NSIntegerMax`，不是 `NSUInteger::MAX`（复审 P1 的回归点）。
        assert_eq!(NS_NOT_FOUND, isize::MAX as usize);
        assert_eq!(replacement_range(NS_NOT_FOUND, 0, &utf16("ni"), 0), None);
        assert_eq!(replacement_range(NS_NOT_FOUND, 3, &utf16("你好"), 0), None);
        assert_eq!(replacement_range(NS_NOT_FOUND, 0, &[], 0), None);
        // `NSNotFound` 与组合串起点无关：文档坐标起点非 0 时同样表示整段替换。
        assert_eq!(replacement_range(NS_NOT_FOUND, 0, &utf16("ni"), 5), None);
        assert_eq!(replacement_range(1, 0, &utf16("ni"), 0), Some(1..1));
        assert_eq!(replacement_range(2, 0, &utf16("ni"), 0), Some(2..2));
    }

    #[test]
    fn an_empty_range_is_an_insertion_point_inside_the_composition() {
        assert_eq!(replacement_range(0, 0, &utf16("ni"), 0), Some(0..0));
        assert_eq!(replacement_range(2, 0, &utf16("ni"), 0), Some(2..2));
    }

    #[test]
    fn a_range_inside_the_marked_text_is_kept() {
        assert_eq!(replacement_range(1, 2, &utf16("shili"), 0), Some(1..3));
        assert_eq!(replacement_range(0, 4, &utf16("shili"), 0), Some(0..4));
    }

    #[test]
    fn out_of_bounds_ranges_are_clamped_to_the_marked_text() {
        let marked = utf16("ni");
        assert_eq!(replacement_range(9, 1, &marked, 0), Some(2..2));
        assert_eq!(replacement_range(1, 99, &marked, 0), Some(1..2));
        assert_eq!(replacement_range(0, usize::MAX, &marked, 0), Some(0..2));
        assert_eq!(
            replacement_range(NS_NOT_FOUND - 1, 2, &marked, 0),
            Some(2..2)
        );
        // 没有组合串时（例如 `insertText:` 在提交前清空）任何区间都收敛成插入点。
        assert_eq!(replacement_range(0, 0, &[], 0), Some(0..0));
        assert_eq!(replacement_range(3, 2, &[], 0), Some(0..0));
    }

    /// `replacementRange` 是 AppKit 的**文档坐标**：组合串起点不是 0 时（surrounding text
    /// 接通后的形态），先减去 `markedRange.location` 再 clamp/吸附。
    #[test]
    fn document_coordinates_are_rebased_onto_the_marked_text_start() {
        let marked = utf16("shili");
        // 组合串在文档 [5, 10)；文档区间 [7, 8) ⇒ 组合串内 [2, 3)。
        assert_eq!(replacement_range(7, 1, &marked, 5), Some(2..3));
        // 文档位置正好等于组合串起点。
        assert_eq!(replacement_range(5, 2, &marked, 5), Some(0..2));
        // 区间起点在组合串之前：相对起点饱和到 0（clamp 到组合串开头，不越界）。
        assert_eq!(replacement_range(3, 0, &marked, 5), Some(0..0));
        assert_eq!(replacement_range(0, 0, &marked, 5), Some(0..0));
        // 文档区间跨过组合串末尾：终点 clamp 到组合串长度。
        assert_eq!(replacement_range(8, 99, &marked, 5), Some(3..5));
        // 指向组合串之后的已提交文本：收敛成末尾插入点（surrounding text 未实现，已知限制）。
        assert_eq!(replacement_range(12, 0, &marked, 5), Some(5..5));
        // `NSNotFound` 与起点无关：仍然表示「替换整个组合串」。
        assert_eq!(replacement_range(NS_NOT_FOUND, 0, &marked, 5), None);
    }

    #[test]
    fn cjk_ranges_are_counted_in_utf16_units() {
        // 每个汉字都是 1 个 UTF-16 码元（与 `str::len` 的 3 字节不同）。
        let marked = utf16("你好");
        assert_eq!(marked.len(), 2);
        assert_eq!(replacement_range(0, 1, &marked, 0), Some(0..1));
        assert_eq!(replacement_range(1, 1, &marked, 0), Some(1..2));
        assert_eq!(replacement_range(2, 0, &marked, 0), Some(2..2));
    }

    #[test]
    fn surrogate_pairs_are_never_split() {
        // 🌍 = U+1F30D = 高代理 D83C + 低代理 DF0D（2 个码元），'a' 1 个码元。
        let marked = utf16("🌍a");
        assert_eq!(marked.len(), 3);

        // 起点落在低代理上：边界回到高代理；长度 0 也要覆盖整个代理对。
        assert_eq!(replacement_range(1, 0, &marked, 0), Some(0..2));
        // 终点落在高代理之后：收进低代理。
        assert_eq!(replacement_range(0, 1, &marked, 0), Some(0..2));
        // 整个 emoji 的组合串内替换区间。
        assert_eq!(replacement_range(0, 2, &marked, 0), Some(0..2));
        // 代理对之后的普通字符不受影响。
        assert_eq!(replacement_range(2, 1, &marked, 0), Some(2..3));
        // emoji 在结尾：终点越界 clamp 后再吸附回码点边界。
        let trailing = utf16("a🌍");
        assert_eq!(replacement_range(1, 1, &trailing, 0), Some(1..3));
        assert_eq!(replacement_range(1, 9, &trailing, 0), Some(1..3));
    }

    #[test]
    fn replacement_range_is_idempotent_for_already_valid_ranges() {
        let marked = utf16("🌍你好");
        assert_eq!(marked.len(), 4);
        for range in [0..0, 0..2, 2..3, 2..4, 4..4] {
            let start = range.start;
            let len = range.end - range.start;
            assert_eq!(
                replacement_range(start, len, &marked, 0),
                Some(range),
                "合法区间必须原样返回"
            );
        }
    }

    /// Linux `preedit_string` 的字节光标在纯 ASCII 串上就是 UTF-16 码元偏移。
    #[test]
    fn preedit_caret_is_counted_in_utf16_units() {
        assert_eq!(preedit_carets("ni", 0, 0), (Some(0), ClauseRanges::new()));
        assert_eq!(preedit_carets("ni", 1, 1), (Some(1), ClauseRanges::new()));
        assert_eq!(preedit_carets("ni", 2, 2), (Some(2), ClauseRanges::new()));
    }

    /// 汉字是 3 字节 / 1 码元：字节偏移必须先换算成码元，否则插入点会越过串尾。
    #[test]
    fn preedit_caret_converts_cjk_byte_offsets() {
        let (caret, clauses) = preedit_carets("你好", 0, 0);
        assert_eq!((caret, clauses), (Some(0), ClauseRanges::new()));
        assert_eq!(preedit_carets("你好", 3, 3).0, Some(1));
        assert_eq!(preedit_carets("你好", 6, 6).0, Some(2));
        // 停在第一个汉字之后（字节 4 落在第二个汉字中间）⇒ 吸附到码元 1。
        assert_eq!(preedit_carets("你好", 4, 4).0, Some(1));
    }

    /// 代理对：emoji 占 4 字节 / 2 码元，插入点必须按码元计数（否则文本层会切错）。
    #[test]
    fn preedit_caret_handles_astral_characters() {
        // "🌍a" = 5 字节 / 3 码元。
        assert_eq!(preedit_carets("🌍a", 4, 4).0, Some(2));
        assert_eq!(preedit_carets("🌍a", 5, 5).0, Some(3));
        // 落在代理对中间（字节 2）⇒ 收敛到 emoji 之前。
        assert_eq!(preedit_carets("🌍a", 2, 2).0, Some(0));
    }

    /// `cursor_begin != cursor_end` ⇒ 高亮区间；协议说「两者不同时客户端可以画成文本高亮」。
    #[test]
    fn a_selected_preedit_range_becomes_a_clause() {
        let (caret, clauses) = preedit_carets("你好", 3, 6);
        assert_eq!(caret, Some(1), "插入点在选区起点");
        assert_eq!(
            clauses.into_iter().collect::<Vec<_>>(),
            vec![1..2],
            "高亮区间是 UTF-16 码元区间"
        );
        // 反向区间（越界的第三方实现）：不产出高亮，只保留插入点。
        let (caret, clauses) = preedit_carets("ni", 2, 1);
        assert_eq!(caret, Some(2));
        assert!(clauses.is_empty());
    }

    /// 协议：`cursor_begin == cursor_end == -1` 表示「隐藏光标」。
    ///
    /// 引擎的 `Preedit.caret` 没有「隐藏」这个状态，`None` 让文本层退化为「插入点在末尾」
    /// （已在 `ImeEvent` 文档写明）。任一端为负同样按不可用处理。
    #[test]
    fn a_hidden_cursor_yields_no_caret() {
        assert_eq!(preedit_carets("ni", -1, -1), (None, ClauseRanges::new()));
        assert_eq!(preedit_carets("ni", -1, 2), (None, ClauseRanges::new()));
        assert_eq!(preedit_carets("ni", 0, -1), (None, ClauseRanges::new()));
    }

    /// 越界偏移（组合串已变短 / 第三方 compositor）：夹到 `0..=len`，不 panic、不越界。
    #[test]
    fn out_of_range_preedit_offsets_are_clamped() {
        assert_eq!(preedit_carets("ni", 99, 99).0, Some(2));
        assert_eq!(preedit_carets("ni", i32::MAX, i32::MAX).0, Some(2));
        assert_eq!(preedit_carets("", 0, 0), (Some(0), ClauseRanges::new()));
    }

    /// B4 真值表（1/4）：组合期 + 无命令修饰键 ⇒ 不投递裸 `Key`（拼音中按 B/S/数字）。
    #[test]
    fn composing_without_command_modifiers_hides_the_bare_key() {
        assert!(!key_visible(false, true, false));
        assert!(!key_visible(true, true, false));
    }

    /// B4 真值表（2/4）：组合期 + Ctrl/Alt/Meta ⇒ **仍然投递**（`Cmd+C` / `Ctrl+W` 不能被吞）。
    /// Shift 不算命令修饰键（调用方传入的 `command_modifiers` 不含 Shift），组合中 Shift+数字
    /// 仍按输入法语义处理。
    #[test]
    fn composing_with_a_command_modifier_still_delivers() {
        assert!(key_visible(true, true, true));
        assert!(key_visible(false, true, true));
    }

    /// B4 真值表（3/4）：按键被 IME 消费（mac `handleEvent == true`）⇒ 不投递，即使没有组合串。
    #[test]
    fn an_ime_consumed_key_is_hidden_even_without_marked_text() {
        assert!(!key_visible(true, false, false));
        assert!(key_visible(true, false, true));
    }

    /// B4 真值表（4/4）：普通键入（无组合、未消费）⇒ 投递。
    #[test]
    fn an_ordinary_key_is_visible() {
        assert!(key_visible(false, false, false));
    }
}

/// IME 组合收尾的「恰好一条 `End`」记账单测（RB-09）。
///
/// 三种回调形态各一条用例：它们必须在**同一条**记账规则下都得到「恰好一条收尾」——
/// 同步 `unmarkText:`（回调投 End）、`setMarkedText:@""`（无 End，清场补）、异步
/// `unmarkText:`（清场先补、回调迟到）。修复前按 `hasMarkedText` 推断的实现在后两种形态下
/// 分别是 0 条与 2 条 End。
#[cfg(test)]
mod end_bookkeeping_tests {
    use super::ImeCompositionEnd;

    /// 形态 1：`discardMarkedText` 同步回调 `unmarkText:` —— 回调投递 End，清场不得再补。
    #[test]
    fn a_synchronous_unmark_during_discard_emits_exactly_one_end() {
        let mut end = ImeCompositionEnd::default();
        end.note_preedit();
        assert!(end.note_end(), "unmarkText: 回调应当投递 End");
        assert!(!end.note_end(), "清场兜底不得再补第二条 End");
        assert!(!end.is_pending());
    }

    /// 形态 2：`discardMarkedText` 只清空组合串（`setMarkedText:@""`）—— 没有 End 回调，
    /// 清场必须补一条，否则应用侧 preedit 永远收不到收尾。
    #[test]
    fn a_discard_that_only_clears_the_marked_text_still_emits_one_end() {
        let mut end = ImeCompositionEnd::default();
        end.note_preedit();
        // `setMarkedText:@""` 只清组合串，不调用 `note_end`。
        assert!(end.is_pending());
        assert!(end.note_end(), "清场必须补一条 End（否则 preedit 残留）");
        assert!(!end.note_end(), "补过之后不得再补");
    }

    /// 形态 3：`discardMarkedText` 异步回调 `unmarkText:` —— 清场先补一条，迟到的回调被吞，
    /// 总数恰好一条（修复前的推断会补一条 + 回调一条 = 两条）。
    #[test]
    fn a_late_unmark_after_the_teardown_fallback_is_swallowed() {
        let mut end = ImeCompositionEnd::default();
        end.note_preedit();
        assert!(end.note_end(), "discardMarkedText 返回时还没收尾 ⇒ 补一条");
        assert!(!end.note_end(), "迟到的 unmarkText: 不得再投第二条 End");
    }

    /// 形态 4：`discardMarkedText` 走 `insertText:`（Commit）—— 组合以正文结束，不补 End。
    #[test]
    fn a_commit_during_discard_emits_no_end() {
        let mut end = ImeCompositionEnd::default();
        end.note_preedit();
        end.note_commit();
        assert!(!end.note_end(), "Commit 已经销账，清场不得再补 End");
        assert!(!end.is_pending());
    }

    /// 记账不能被上一次清场永久置为已销账：下一次组合仍要恰好一条 End。
    #[test]
    fn a_new_composition_gets_its_own_end() {
        let mut end = ImeCompositionEnd::default();
        end.note_preedit();
        assert!(end.note_end());
        end.note_preedit();
        assert!(end.note_end(), "下一次组合仍要恰好一条 End");
    }

    /// 没有 `Preedit` 就没有 `End`：`unmarkText:` 可能在没有组合时被 AppKit 调用（应用侧
    /// 从未看到 preedit，凭空补一条 End 会让文本层清掉别的状态）。
    #[test]
    fn an_unmark_without_a_preedit_never_emits_an_end() {
        let mut end = ImeCompositionEnd::default();
        assert!(!end.note_end());
        assert!(!end.note_end());
    }
}

/// 按键作用域（macOS `keyDown:`）的提交记账单测（R09-3）。
///
/// 与 Linux 无关：Linux 的 `zwp_text_input_v3` 按 `done` 双缓冲交付、一批只有一个
/// `commit_string`，没有「同一按键内重复提交」这回事（去重器已删除，见文件头）。这里是
/// 原生按键作用域，**必须在作用域结束时清空全部状态**。
#[cfg(test)]
mod scope_tests {
    use super::{CommitVerdict, ImeCommitScope};

    /// R09-3 的核心回归：鼠标点候选窗提交（不在任何 `keyDown:` 内）之后，下一次按键的正文
    /// **不得**被吞掉。旧实现用一个裸 `suppress_text: Cell<bool>` 记录这件事，候选窗点击置位后
    /// 再也没有消费点，于是下一个真实字符丢失（交易终端的「输入 10 变成 0」）。
    #[test]
    fn a_commit_outside_a_key_never_suppresses_the_next_key() {
        let mut scope = ImeCommitScope::default();

        // 鼠标点候选窗：没有 `begin_key`，提交直接到达。
        assert_eq!(scope.note_commit("你"), CommitVerdict::DeliverOutsideScope);

        // 下一次真实按键：不得继承任何抑制状态。
        let token = scope.begin_key();
        assert!(!scope.takes_text(token), "作用域外提交不得影响本按键");
        scope.end_key(token);
    }

    /// 同一次按键内 `insertText:` 交付正文 ⇒ 本按键的 `Text` 被抑制。
    #[test]
    fn a_commit_inside_the_key_suppresses_that_keys_text() {
        let mut scope = ImeCommitScope::default();
        let token = scope.begin_key();
        assert_eq!(scope.note_commit("ni"), CommitVerdict::DeliverInScope);
        assert!(scope.takes_text(token));
        scope.end_key(token);
    }

    /// 作用域结束后令牌失效：迟到的（或跨按键重复的）提交回到「作用域外」。
    #[test]
    fn a_stale_token_never_sees_or_sets_suppression() {
        let mut scope = ImeCommitScope::default();
        let first = scope.begin_key();
        assert_eq!(scope.note_commit("ni"), CommitVerdict::DeliverInScope);
        scope.end_key(first);

        // 令牌已失效：`takes_text` 必须为 false，且旧令牌的 `end_key` 不能清别人的状态。
        assert!(!scope.takes_text(first));
        let second = scope.begin_key();
        scope.end_key(first);
        assert!(scope.open == Some(second), "旧令牌的收尾不得关闭新作用域");
        assert!(!scope.takes_text(second), "新作用域是干净的");
        scope.end_key(second);
    }

    /// 输入法在同一按键内重复投递同一段文本：只交付一次（否则会插入两遍）。
    #[test]
    fn a_duplicate_commit_within_one_key_is_swallowed() {
        let mut scope = ImeCommitScope::default();
        let token = scope.begin_key();
        assert_eq!(scope.note_commit("你"), CommitVerdict::DeliverInScope);
        assert_eq!(scope.note_commit("你"), CommitVerdict::Duplicate);
        assert!(scope.takes_text(token));
        scope.end_key(token);
    }

    /// 跨按键的相同文本是合法输入（`"ll"`、双空格）：每次按键各自交付。
    #[test]
    fn the_same_text_in_the_next_key_is_delivered_again() {
        let mut scope = ImeCommitScope::default();
        for _ in 0..3 {
            let token = scope.begin_key();
            assert_eq!(scope.note_commit("l"), CommitVerdict::DeliverInScope);
            scope.end_key(token);
        }
    }

    /// 同一按键内的不同文本都交付（逐子句提交：一次按键可能先提交 A 再提交 B）。
    #[test]
    fn different_texts_in_one_key_are_both_delivered() {
        let mut scope = ImeCommitScope::default();
        let token = scope.begin_key();
        assert_eq!(scope.note_commit("a"), CommitVerdict::DeliverInScope);
        assert_eq!(scope.note_commit("b"), CommitVerdict::DeliverInScope);
        assert!(scope.takes_text(token));
        scope.end_key(token);
    }

    /// 空文本提交不产生抑制（`insertText:@""` 是输入法的合法回调）。
    #[test]
    fn an_empty_commit_does_not_suppress_text() {
        let mut scope = ImeCommitScope::default();
        let token = scope.begin_key();
        assert_eq!(scope.note_commit(""), CommitVerdict::DeliverInScope);
        // 空提交本身不携带正文，同一次按键仍会走 `Text` 通路。
        assert!(!scope.takes_text(token));
        scope.end_key(token);
    }

    /// 匹配单位是 UTF-16 码元：代理对（emoji）与 CJK 都按 `AppKit` 的 `NSRange` 单位比较。
    #[test]
    fn commits_are_compared_in_utf16_units() {
        let mut scope = ImeCommitScope::default();
        let token = scope.begin_key();
        assert_eq!(scope.note_commit("🌍"), CommitVerdict::DeliverInScope);
        assert_eq!(scope.note_commit("🌍"), CommitVerdict::Duplicate);
        // 「🌍」与「🌍a」是不同文本 ⇒ 都要交付。
        assert_eq!(scope.note_commit("🌍a"), CommitVerdict::DeliverInScope);
        scope.end_key(token);
    }
}
