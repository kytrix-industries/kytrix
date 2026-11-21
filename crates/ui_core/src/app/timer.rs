//! 一次性定时器：注册 / 取消 / 到期派发 / 平台唤醒截止时刻（E10）。
//!
//! # 语义
//!
//! - **一次性**：到期派发一次即从表里移除；回调拿 `&mut App`，可建树 / 写信号 / 再注册定时器。
//! - **不新建线程**：到期派发走引擎每轮一次的钩子步骤（[`App::run_frame_hook`]；headless
//!   自驱帧宿主由 [`App::frame`] 入口兜底）。平台泵只负责把**阻塞等待**截止到
//!   [`next_deadline`]（Windows 一次性 `SetTimer` / macOS 一次性 `CFRunLoopTimer` /
//!   Linux calloop timeout），到点**唤醒一次**。
//! - **I3**：注册 / 取消 / 等待都不请求帧、不标脏，也不让平台进入连续帧模式
//!   （`needs_continuous_frames` 只看动画）；等待期间引擎处于空闲零帧状态。
//! - **生命周期**（与 E8 scoped 键绑定同源）：窗口关闭自动取消该窗口的全部定时器；
//!   [`App::after_scoped`] 的 `scope` 子树卸载时自动取消；应用也可显式
//!   [`App::cancel_timer`]（幂等）。定时器不引用信号值，因此 `release_scope` 不会留下悬垂
//!   句柄 —— 组件契约仍是「先卸载节点、再释放作用域」，节点卸载已完成取消。
//!
//! # 时钟与精度
//!
//! 到期判定用**引擎时钟**（平台泵每轮 `sync_clock_live`；测试用 `TestApp::advance`）。
//!
//! **登记基准**（E10c）：生产构建（`App::new` 即活时钟，`App::clock_live`）用**登记瞬间的
//! 真实时间** `Instant::now()` 作基准 —— 包括**首次泵启动之前**登记的定时器（E10c-R01：
//! `Application::run` 之前的建窗/初始化耗时不折算进 `delay`）；测试宿主 `TestApp` 用引擎时钟
//! （`TestApp::advance` 是唯一推进源，deadline 必须精确等于 `clock + delay` 才可断言）。
//!
//! 为什么不能用泵起点同步的引擎时钟当基准：泵在**开始**同步时钟，之后还有事件派发、窗口状态
//! 同步、渲染器轮询等本帧前置工作；应用在 `on_frame` 里登记定时器时，引擎时钟已经落后真实
//! 时间那段工作。拿它做基准会把这段前置工作算进 `delay` ⇒ 平台定时器在「登记后 delay」之前
//! 到点（macOS 真机实测 `398.648 ms < 400 ms`）。`run_due` 的到点判定用**泵起点**的引擎时钟，
//! 因此也不会把平台定时器的提前唤醒当作到期（未到期就下一轮重新挂表，只晚不早）。
//!
//! 平台唤醒本身受系统定时器粒度影响（Windows `WM_TIMER` 约 15.6 ms 粒度、Linux calloop
//! 毫秒级、macOS `CFRunLoopTimer`），唤醒**晚到**只是让回调晚一轮派发，不会漏派发。

use std::time::{Duration, Instant};

use super::{App, TimerId};
use crate::{error::EngineError, node::id::NodeId, window::WindowId};

/// 一条定时器（`App::timers`；按注册顺序，id 单调）。
pub(crate) struct TimerEntry {
    /// 单调 id（`TimerId` 的代际）。
    id: u32,
    /// 所属窗口（关窗清理 / 多窗口隔离）。
    window: WindowId,
    /// 节点作用域（`App::after_scoped`）：子树卸载 ⇒ 自动取消。
    scope: Option<NodeId>,
    /// 到期时刻（引擎时钟）。
    deadline: Instant,
    /// 到期回调（一次性；派发后条目已从表里移除，再调用）。
    callback: Box<dyn FnOnce(&mut App)>,
}

/// 注册一个一次性定时器。
///
/// 消费者：`App::after` / `App::after_scoped`。
///
/// # Errors
/// - [`EngineError::UnknownWindow`]：窗口不存在（关窗后再注册的定时器永远没有派发轮次）。
/// - [`EngineError::StaleNode`]：`scope` 不在该窗口树中（从未挂载 / 已卸载 / 属于另一窗口）。
pub(crate) fn after(
    app: &mut App,
    window: WindowId,
    scope: Option<NodeId>,
    delay: Duration,
    callback: impl FnOnce(&mut App) + 'static,
) -> Result<TimerId, EngineError> {
    let Some(target) = app.windows.get(&window) else {
        return Err(EngineError::UnknownWindow { window });
    };
    if let Some(scope) = scope
        && (scope.window() != Some(window) || target.tree().get(scope).is_err())
    {
        return Err(EngineError::StaleNode { node: scope });
    }
    let id = app.next_timer;
    app.next_timer = app.next_timer.wrapping_add(1);
    // E10c：登记基准 = 生产（活时钟，`App::new` 即置位 ⇒ 首次泵启动前登记同样成立）取
    // **登记瞬间**真实时间；测试宿主取引擎时钟（可 `advance` 精确断言）。见模块文档
    // 「时钟与精度」。
    let reference = if app.clock_live {
        Instant::now()
    } else {
        app.clock
    };
    // 超出 `Instant` 表示范围的延时按「立即到期」处理（不 panic）：`delay` 是应用给的
    // `Duration`，几十年的延时在 UI 语义上与「不再醒来」等价，但立即到期至少是确定性的，
    // 且下一轮就会被派发掉、不会在表里永久驻留。
    let deadline = reference.checked_add(delay).unwrap_or(reference);
    app.timers.push(TimerEntry {
        id,
        window,
        scope,
        deadline,
        callback: Box::new(callback),
    });
    Ok(TimerId::new(window, id))
}

/// 取消一个定时器（幂等）。返回 `false` = 该 id 已到期派发 / 已被取消 / 窗口已关闭。
///
/// 消费者：`App::cancel_timer`。
pub(crate) fn cancel(app: &mut App, id: &TimerId) -> bool {
    let before = app.timers.len();
    app.timers
        .retain(|timer| !(timer.window == id.window() && timer.id == id.generation()));
    app.timers.len() != before
}

/// 最近一个定时器的到期时刻（平台泵据此决定阻塞等待的截止时刻）。
///
/// `None` = 没有待派发的定时器：平台泵保持原有的空闲阻塞（I3：没有定时器就没有额外唤醒）。
pub(crate) fn next_deadline(app: &App) -> Option<Instant> {
    app.timers.iter().map(|timer| timer.deadline).min()
}

/// 派发**已到期**的定时器（平台泵每轮在应用钩子之前调用；`App::frame` 入口兜底）。
///
/// 先快照到期 id 再逐个「查表 → 移除 → 调用」：
/// - 派发顺序 = `(deadline, id)`（同一时刻到期的按注册顺序），确定性；
/// - 回调里取消**其它**待派发定时器 ⇒ 该条已不在表里，本轮不会再被调用；
/// - **同一次派发调用内**不会内联派发回调里新注册的定时器（哪怕 `delay == 0`）：快照语义，
///   因此不存在「0 ms 定时器在回调里自我续期」的不返回循环。新注册的定时器至多在**同一引擎
///   轮次的后半段**（`App::frame` 入口的兜底派发点）被派发一次，否则下一轮派发。
pub(crate) fn run_due(app: &mut App) {
    let now = app.clock;
    let mut due: Vec<(Instant, u32)> = app
        .timers
        .iter()
        .filter(|timer| timer.deadline <= now)
        .map(|timer| (timer.deadline, timer.id))
        .collect();
    if due.is_empty() {
        return;
    }
    due.sort_unstable();
    for (_, id) in due {
        let Some(index) = app.timers.iter().position(|timer| timer.id == id) else {
            continue;
        };
        let timer = app.timers.swap_remove(index);
        // 防御：窗口 / 作用域在注册之后被销毁但清理遗漏（不该发生）⇒ 丢弃，不调用应用回调。
        let Some(target) = app.windows.get(&timer.window) else {
            continue;
        };
        if let Some(scope) = timer.scope
            && target.tree().get(scope).is_err()
        {
            continue;
        }
        (timer.callback)(app);
    }
}

/// 关窗清理：丢弃该窗口的全部定时器（`App::close_window`）。
pub(crate) fn cancel_window(app: &mut App, window: WindowId) {
    app.timers.retain(|timer| timer.window != window);
}

/// 节点卸载清理：`scope` 落在被卸载子树内的定时器自动取消（E8 scoped 语义）。
///
/// 返回取消的定时器条数（[`crate::app::RemoveOutcome::timers`] 的证据面）。判据是**树里不再
/// 存在该 scope**（而不是「属于被删集合」）：重复调用 / 其它路径留下的死 scope 一并收口。
///
/// 消费者：`App::remove_node`。
pub(crate) fn retain_live_scopes(app: &mut App, window: WindowId) -> usize {
    let Some(target) = app.windows.get(&window) else {
        return 0;
    };
    let tree = target.tree();
    let before = app.timers.len();
    app.timers.retain(|timer| {
        timer.window != window || timer.scope.is_none_or(|scope| tree.get(scope).is_ok())
    });
    before - app.timers.len()
}

/// 待派发定时器条数（测试 / 诊断的泄漏证据面）。
#[cfg(all(test, feature = "test-support"))]
pub(crate) fn count(app: &App) -> usize {
    app.timers.len()
}
