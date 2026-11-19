//! X1 真机证据的实现：真实可见窗口上的 owner / 模态 / 置顶原生语义检查。
//!
//! 检查全部由后台线程在**真实可见窗口**上做系统状态查询（不发输入、不抢焦点）：
//! - owner：`CreateWindowExW` 的 `hWndParent` 是否真的建立了 owner 关系（`GW_OWNER` 回指 +
//!   owned 在 z 序里位于 owner 之上）；
//! - always_on_top：`WS_EX_TOPMOST` 位 + 置顶窗口在 z 序里位于普通窗口之上；
//! - modal：打开期间 `IsWindowEnabled(owner) == false`，关闭后恢复；
//! - 最小化跟随：owner 最小化时 owned 隐藏，恢复后重新可见。
//!
//! 关闭窗口的顺序 = owned / modal / topmost → owner（本证据程序不实现 App 层 owner 级联，
//! owner 最后关避免平台级联先销毁 owned 原生窗口）。结果由 `on_frame` 钩子打印并判定。

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use ui_core::{
    app::{App, AppHandle, Application},
    math::{Point, Size},
    paint::color::Color,
    window::{WindowId, WindowOptions},
};
use windows::Win32::{
    Foundation::{HWND, RECT},
    UI::{
        Input::KeyboardAndMouse::IsWindowEnabled,
        WindowsAndMessaging::{
            FindWindowW, GW_HWNDNEXT, GW_OWNER, GWL_EXSTYLE, GetTopWindow, GetWindow,
            GetWindowLongPtrW, GetWindowRect, IsIconic, IsWindowVisible, SW_MINIMIZE, SW_RESTORE,
            ShowWindow, WS_EX_TOPMOST,
        },
    },
};

/// 证据窗口标题（`FindWindowW` 定位用；四个标题必须互不相同）。
const OWNER_TITLE: &str = "kytrix P1-B X1 evidence owner";
const OWNED_TITLE: &str = "kytrix P1-B X1 evidence owned";
const MODAL_TITLE: &str = "kytrix P1-B X1 evidence modal";
const TOPMOST_TITLE: &str = "kytrix P1-B X1 evidence topmost";
/// 证据窗口逻辑尺寸。
const SIZE: Size = Size::new(380.0, 220.0);
/// 等四个窗口全部出现的上限。
const FIND_TIMEOUT: Duration = Duration::from_secs(10);
/// 关闭模态窗口后等原生窗口销毁 + owner 恢复的上限。
const MODAL_CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
/// 最小化 / 恢复后等系统传播状态的时间。
const SETTLE: Duration = Duration::from_millis(450);
/// 整体超时（窗口没出现 / 钩子没被调度时不挂死）。
const RUN_TIMEOUT: Duration = Duration::from_secs(30);
/// 拉起检查线程前先让窗口完成几帧（保证四个原生窗口都已显示）。
const FRAMES_BEFORE_CHECK: u32 = 3;

/// 后台线程的检查结果（写进 `Arc<Mutex<Option<X1Result>>>`，由 `on_frame` 打印/判定）。
#[derive(Default)]
struct X1Result {
    /// 四个窗口都找到了。
    found: bool,
    /// 外框 `(left, top, width, height)`：owner / owned / modal / topmost（可见性证据）。
    rects: [Option<(i32, i32, i32, i32)>; 4],
    /// `GetWindow(owned, GW_OWNER) == owner`。
    owner_of_owned: bool,
    /// owned 在 z 序里位于 owner 之上（顶层窗口 z 序，0 = 最顶）。
    owned_above_owner: bool,
    /// 置顶窗口带 `WS_EX_TOPMOST`。
    topmost_ex_style: bool,
    /// 普通 owner 窗口**不带** `WS_EX_TOPMOST`。
    plain_not_topmost: bool,
    /// 置顶窗口在 z 序里位于普通 owner 窗口之上。
    topmost_above_owner: bool,
    /// 桌面顶层窗口 z 序（从顶到底）的总数（日志用）。
    z_total: usize,
    /// owner / owned / modal / topmost 在 z 序里的下标（0 = 最顶）。
    z_positions: [Option<usize>; 4],
    /// 模态窗口打开期间 owner 被系统禁用。
    modal_disables_owner: bool,
    /// 关闭模态窗口后 owner 恢复启用。
    modal_close_restores_owner: bool,
    /// 最小化 owner 后 `IsIconic(owner)`。
    owner_minimized: bool,
    /// owner 最小化时 owned 隐藏。
    owned_hidden_with_owner: bool,
    /// 恢复 owner 后 owned 重新可见。
    owned_visible_after_restore: bool,
    /// 全部断言通过。
    passed: bool,
}

/// 证据收集状态（只在 UI 线程访问；后台线程只碰 `detail` / `thread` 里的 `Arc`）。
struct Evidence {
    windows: [WindowId; 4],
    started: Instant,
    frames: u32,
    spawned: bool,
    finished: bool,
    passed: Cell<bool>,
    detail: Arc<Mutex<Option<X1Result>>>,
    thread: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
}

pub(crate) fn run() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let mut app = App::new(runtime.handle().clone());
    let owner = app.create_window(WindowOptions {
        title: smol_str::SmolStr::new_static(OWNER_TITLE),
        size: SIZE,
        position: Some(Point::new(60.0, 60.0)),
        background: Color::WHITE,
        ..WindowOptions::default()
    });
    let owned = app.create_window(WindowOptions {
        title: smol_str::SmolStr::new_static(OWNED_TITLE),
        owner: Some(owner),
        size: SIZE,
        position: Some(Point::new(130.0, 110.0)),
        background: Color::BLACK,
        ..WindowOptions::default()
    });
    let modal = app.create_window(WindowOptions {
        title: smol_str::SmolStr::new_static(MODAL_TITLE),
        owner: Some(owner),
        modal: true,
        size: SIZE,
        position: Some(Point::new(200.0, 160.0)),
        background: Color::WHITE,
        ..WindowOptions::default()
    });
    let topmost = app.create_window(WindowOptions {
        title: smol_str::SmolStr::new_static(TOPMOST_TITLE),
        always_on_top: true,
        size: SIZE,
        position: Some(Point::new(270.0, 210.0)),
        background: Color::BLACK,
        ..WindowOptions::default()
    });
    let windows = [owner, owned, modal, topmost];
    let state = Rc::new(RefCell::new(Evidence {
        windows,
        started: Instant::now(),
        frames: 0,
        spawned: false,
        finished: false,
        passed: Cell::new(false),
        detail: Arc::new(Mutex::new(None)),
        thread: Arc::new(Mutex::new(None)),
    }));
    let hook_state = Rc::clone(&state);
    app.on_frame(move |app| on_frame(app, &hook_state));

    let run_result = Application::new().run(app);
    // 先回收检查线程（`Application::run` 可能先返回），把 panic 暴露出来。
    let joined = state
        .borrow()
        .thread
        .lock()
        .ok()
        .and_then(|mut slot| slot.take())
        .map(std::thread::JoinHandle::join);
    run_result?;
    if joined.is_some_and(|joined| joined.is_err()) {
        return Err("X1 证据检查线程 panic（见上方输出）".into());
    }
    if state.borrow().passed.get() {
        Ok(())
    } else {
        Err("X1 真机证据未通过（见上方输出）".into())
    }
}

/// 每拍：数帧；第 [`FRAMES_BEFORE_CHECK`] 帧拉起检查线程；结果就绪时打印 / 判定 / 关窗。
fn on_frame(app: &mut App, state: &Rc<RefCell<Evidence>>) {
    let mut evidence = state.borrow_mut();
    if evidence.finished {
        return;
    }
    evidence.frames += 1;
    if !evidence.spawned && evidence.frames >= FRAMES_BEFORE_CHECK {
        evidence.spawned = true;
        let detail = Arc::clone(&evidence.detail);
        let thread_slot = Arc::clone(&evidence.thread);
        let handle = app.handle();
        let windows = evidence.windows;
        let thread = std::thread::Builder::new()
            .name("x1-evidence".to_owned())
            .spawn(move || {
                let result = check(&handle, windows);
                if let Ok(mut slot) = detail.lock() {
                    *slot = Some(result);
                }
                handle.wake();
            });
        match thread {
            Ok(thread) => {
                if let Ok(mut slot) = thread_slot.lock() {
                    *slot = Some(thread);
                }
            }
            Err(error) => {
                println!("FAIL: X1 证据检查线程创建失败：{error}");
                let windows = evidence.windows;
                evidence.finished = true;
                evidence.passed.set(false);
                close_windows(app, windows);
            }
        }
        return;
    }

    let result = evidence.detail.lock().ok().and_then(|mut slot| slot.take());
    let Some(result) = result else {
        if evidence.started.elapsed() > RUN_TIMEOUT {
            println!("FAIL: 检查线程未在 {RUN_TIMEOUT:?} 内完成（窗口没出现 / 泵没被唤醒）");
            evidence.finished = true;
            evidence.passed.set(false);
            let windows = evidence.windows;
            close_windows(app, windows);
        }
        return;
    };

    evidence.finished = true;
    evidence.passed.set(result.passed);
    print_result(&result);
    let windows = evidence.windows;
    close_windows(app, windows);
}

/// 后台检查线程：找四个真实窗口 → 逐项断言 → 返回结果。
fn check(handle: &AppHandle, windows: [WindowId; 4]) -> X1Result {
    let mut result = X1Result::default();
    let Some(hwnds) = wait_for_windows() else {
        println!("FAIL: {FIND_TIMEOUT:?} 内没找齐四个证据窗口");
        return result;
    };
    let [owner, owned, modal, topmost] = hwnds;
    result.found = true;
    result.rects = [
        window_rect(owner),
        window_rect(owned),
        window_rect(modal),
        window_rect(topmost),
    ];
    println!(
        "四个窗口已显示（外框：左上角 + 宽高）：owner={:?} owned={:?} modal={:?} topmost={:?}",
        result.rects[0], result.rects[1], result.rects[2], result.rects[3]
    );

    // ── owner：GW_OWNER 回指 + owned 在 owner 之上（z 序）────────────────────
    // SAFETY: 四个窗口由本进程持有且此刻存活；只读 / 系统状态查询。
    result.owner_of_owned =
        unsafe { GetWindow(owned, GW_OWNER) }.is_ok_and(|owner_of| owner_of == owner);
    // 顶层窗口 z 序（从顶到底）：owned 必须排在 owner 之前（更靠顶）；置顶窗口必须在 owner 之前。
    let z_order = z_order();
    result.z_total = z_order.len();
    result.z_positions = [
        z_order.iter().position(|&hwnd| hwnd == owner),
        z_order.iter().position(|&hwnd| hwnd == owned),
        z_order.iter().position(|&hwnd| hwnd == modal),
        z_order.iter().position(|&hwnd| hwnd == topmost),
    ];
    let [owner_z, owned_z, _, topmost_z] = result.z_positions;
    result.owned_above_owner =
        matches!((owned_z, owner_z), (Some(owned), Some(owner)) if owned < owner);

    // ── always_on_top：WS_EX_TOPMOST 位 + 置顶窗口位于 owner 之上 ────────────
    result.topmost_ex_style = has_topmost_ex_style(topmost);
    result.plain_not_topmost = !has_topmost_ex_style(owner);
    result.topmost_above_owner =
        matches!((topmost_z, owner_z), (Some(topmost), Some(owner)) if topmost < owner);

    // ── modal：打开期间禁用 owner，关闭后恢复 ────────────────────────────────
    result.modal_disables_owner = !is_enabled(owner) && is_enabled(modal);
    let modal_window = windows[2];
    handle.send(move |app| {
        let _ = app.close_window(modal_window);
    });
    result.modal_close_restores_owner =
        wait_until(Duration::from_millis(50), MODAL_CLOSE_TIMEOUT, || {
            find_window(MODAL_TITLE).is_none() && is_enabled(owner)
        });

    // ── 最小化跟随：owner 最小化 ⇒ owned 隐藏；恢复 ⇒ owned 可见 ─────────────
    // SAFETY: owner 此刻已启用且存活；`ShowWindow` 只改窗口显示状态。
    unsafe {
        let _ = ShowWindow(owner, SW_MINIMIZE);
    }
    std::thread::sleep(SETTLE);
    result.owner_minimized = is_iconic(owner);
    result.owned_hidden_with_owner = !is_visible(owned);
    // SAFETY: owner 仍存活；恢复显示。
    unsafe {
        let _ = ShowWindow(owner, SW_RESTORE);
    }
    std::thread::sleep(SETTLE);
    result.owned_visible_after_restore = is_visible(owned);

    result.passed = result.found
        && result.owner_of_owned
        && result.owned_above_owner
        && result.topmost_ex_style
        && result.plain_not_topmost
        && result.topmost_above_owner
        && result.modal_disables_owner
        && result.modal_close_restores_owner
        && result.owner_minimized
        && result.owned_hidden_with_owner
        && result.owned_visible_after_restore;
    result
}

/// 打印全部实测值与判定（UI 线程，结果就绪后调用一次）。
fn print_result(result: &X1Result) {
    println!("== X1 真机证据（Windows owner / 模态 / 置顶的原生语义） ==");
    println!(
        "owner：GetWindow(owned, GW_OWNER)==owner = {}；owned 在 z 序中位于 owner 之上 = {}；顶层窗口 z 序（从顶到底，共 {} 个）= owner #{:?} / owned #{:?} / modal #{:?} / topmost #{:?}",
        result.owner_of_owned,
        result.owned_above_owner,
        result.z_total,
        result.z_positions[0],
        result.z_positions[1],
        result.z_positions[2],
        result.z_positions[3]
    );
    println!(
        "always_on_top：置顶窗口 WS_EX_TOPMOST = {}；普通 owner 不带 WS_EX_TOPMOST = {}；置顶窗口在 z 序中位于 owner 之上 = {}",
        result.topmost_ex_style, result.plain_not_topmost, result.topmost_above_owner
    );
    println!(
        "modal：打开期间 IsWindowEnabled(owner)==false = {}；关闭模态窗口后 IsWindowEnabled(owner)==true = {}",
        result.modal_disables_owner, result.modal_close_restores_owner
    );
    println!(
        "最小化跟随：owner IsIconic = {}；owner 最小化时 owned IsWindowVisible==false = {}；恢复后 owned 可见 = {}",
        result.owner_minimized, result.owned_hidden_with_owner, result.owned_visible_after_restore
    );
    println!(
        "判定：owner={}；always_on_top={}；modal={}；最小化跟随={}",
        result.owner_of_owned && result.owned_above_owner,
        result.topmost_ex_style && result.plain_not_topmost && result.topmost_above_owner,
        result.modal_disables_owner && result.modal_close_restores_owner,
        result.owner_minimized
            && result.owned_hidden_with_owner
            && result.owned_visible_after_restore
    );
    println!("RESULT: {}", if result.passed { "PASS" } else { "FAIL" });
}

/// 关闭全部证据窗口：owned / modal / topmost 先关，owner 最后（本程序不实现 App 层
/// owner 级联；owner 最后关可避免平台 owner 级联先把 owned 原生窗口偷走）。
fn close_windows(app: &mut App, windows: [WindowId; 4]) {
    for id in [windows[1], windows[2], windows[3], windows[0]] {
        let _ = app.close_window(id);
    }
}

// ── Win32 小工具（只在检查线程 / UI 线程的短函数里用）────────────────────────────────────

/// `FindWindowW` 按标题找窗口。
fn find_window(title: &str) -> Option<HWND> {
    let wide = title
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<u16>>();
    // SAFETY: 标题是栈上 NUL 结尾 UTF-16 缓冲；`FindWindowW` 只读查询。
    unsafe { FindWindowW(None, windows::core::PCWSTR(wide.as_ptr())) }.ok()
}

/// 等四个证据窗口全部出现。
fn wait_for_windows() -> Option<[HWND; 4]> {
    let titles = [OWNER_TITLE, OWNED_TITLE, MODAL_TITLE, TOPMOST_TITLE];
    let deadline = Instant::now() + FIND_TIMEOUT;
    loop {
        let mut found = [HWND::default(); 4];
        let mut all = true;
        for (index, title) in titles.iter().enumerate() {
            match find_window(title) {
                Some(hwnd) => found[index] = hwnd,
                None => {
                    all = false;
                    break;
                }
            }
        }
        if all {
            return Some(found);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// 窗口外框 `(left, top, width, height)`。
fn window_rect(hwnd: HWND) -> Option<(i32, i32, i32, i32)> {
    let mut rect = RECT::default();
    // SAFETY: 窗口存活期内查询；`rect` 是栈上输出。
    unsafe { GetWindowRect(hwnd, &mut rect) }.ok()?;
    Some((
        rect.left,
        rect.top,
        rect.right - rect.left,
        rect.bottom - rect.top,
    ))
}

/// 桌面顶层窗口 z 序（从顶到底；`GetTopWindow(NULL)` → `GW_HWNDNEXT` 链，最多 10000 个）。
fn z_order() -> Vec<HWND> {
    let mut order = Vec::new();
    let mut current = (unsafe { GetTopWindow(None) }).ok();
    while let Some(hwnd) = current {
        if hwnd.is_invalid() || order.len() >= 10_000 {
            break;
        }
        order.push(hwnd);
        current = (unsafe { GetWindow(hwnd, GW_HWNDNEXT) })
            .ok()
            .filter(|next| !next.is_invalid());
    }
    order
}

/// 窗口是否带 `WS_EX_TOPMOST`。
fn has_topmost_ex_style(hwnd: HWND) -> bool {
    // SAFETY: 只读查询扩展样式。
    let ex_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
    ex_style & (WS_EX_TOPMOST.0 as isize) != 0
}

/// 窗口是否启用（`IsWindowEnabled`）。
fn is_enabled(hwnd: HWND) -> bool {
    // SAFETY: 只读查询。
    unsafe { IsWindowEnabled(hwnd) }.as_bool()
}

/// 窗口是否可见（`IsWindowVisible`）。
fn is_visible(hwnd: HWND) -> bool {
    // SAFETY: 只读查询。
    unsafe { IsWindowVisible(hwnd) }.as_bool()
}

/// 窗口是否最小化（`IsIconic`）。
fn is_iconic(hwnd: HWND) -> bool {
    // SAFETY: 只读查询。
    unsafe { IsIconic(hwnd) }.as_bool()
}

/// 轮询等待条件成立（每 `step` 检查一次，最多 `timeout`）。
fn wait_until(step: Duration, timeout: Duration, mut check: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if check() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(step);
    }
}
