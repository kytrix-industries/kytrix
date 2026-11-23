use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use ui_core::{
    app::{App, AppHandle, Application},
    math::{LogicalPoint, LogicalSize, px},
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

const OWNER_TITLE: &str = "kytrix P1-B X1 evidence owner";
const OWNED_TITLE: &str = "kytrix P1-B X1 evidence owned";
const MODAL_TITLE: &str = "kytrix P1-B X1 evidence modal";
const TOPMOST_TITLE: &str = "kytrix P1-B X1 evidence topmost";
const SIZE: LogicalSize = LogicalSize::new(px(380.0), px(220.0));
const FIND_TIMEOUT: Duration = Duration::from_secs(10);
const MODAL_CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
const SETTLE: Duration = Duration::from_millis(450);
const RUN_TIMEOUT: Duration = Duration::from_secs(30);
const FRAMES_BEFORE_CHECK: u32 = 3;

#[derive(Default)]
struct X1Result {
    found: bool,
    rects: [Option<(i32, i32, i32, i32)>; 4],
    owner_of_owned: bool,
    owned_above_owner: bool,
    topmost_ex_style: bool,
    plain_not_topmost: bool,
    topmost_above_owner: bool,
    z_total: usize,
    z_positions: [Option<usize>; 4],
    modal_disables_owner: bool,
    modal_close_restores_owner: bool,
    owner_minimized: bool,
    owned_hidden_with_owner: bool,
    owned_visible_after_restore: bool,
    passed: bool,
}

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
        position: Some(LogicalPoint::new(px(60.0), px(60.0))),
        background: Color::WHITE,
        ..WindowOptions::default()
    });
    let owned = app.create_window(WindowOptions {
        title: smol_str::SmolStr::new_static(OWNED_TITLE),
        owner: Some(owner),
        size: SIZE,
        position: Some(LogicalPoint::new(px(130.0), px(110.0))),
        background: Color::BLACK,
        ..WindowOptions::default()
    });
    let modal = app.create_window(WindowOptions {
        title: smol_str::SmolStr::new_static(MODAL_TITLE),
        owner: Some(owner),
        modal: true,
        size: SIZE,
        position: Some(LogicalPoint::new(px(200.0), px(160.0))),
        background: Color::WHITE,
        ..WindowOptions::default()
    });
    let topmost = app.create_window(WindowOptions {
        title: smol_str::SmolStr::new_static(TOPMOST_TITLE),
        always_on_top: true,
        size: SIZE,
        position: Some(LogicalPoint::new(px(270.0), px(210.0))),
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

    result.owner_of_owned =
        unsafe { GetWindow(owned, GW_OWNER) }.is_ok_and(|owner_of| owner_of == owner);
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

    result.topmost_ex_style = has_topmost_ex_style(topmost);
    result.plain_not_topmost = !has_topmost_ex_style(owner);
    result.topmost_above_owner =
        matches!((topmost_z, owner_z), (Some(topmost), Some(owner)) if topmost < owner);

    result.modal_disables_owner = !is_enabled(owner) && is_enabled(modal);
    let modal_window = windows[2];
    handle.send(move |app| {
        let _ = app.close_window(modal_window);
    });
    result.modal_close_restores_owner =
        wait_until(Duration::from_millis(50), MODAL_CLOSE_TIMEOUT, || {
            find_window(MODAL_TITLE).is_none() && is_enabled(owner)
        });

    unsafe {
        let _ = ShowWindow(owner, SW_MINIMIZE);
    }
    std::thread::sleep(SETTLE);
    result.owner_minimized = is_iconic(owner);
    result.owned_hidden_with_owner = !is_visible(owned);
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

fn close_windows(app: &mut App, windows: [WindowId; 4]) {
    for id in [windows[1], windows[2], windows[3], windows[0]] {
        let _ = app.close_window(id);
    }
}

fn find_window(title: &str) -> Option<HWND> {
    let wide = title
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<u16>>();
    unsafe { FindWindowW(None, windows::core::PCWSTR(wide.as_ptr())) }.ok()
}

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

fn window_rect(hwnd: HWND) -> Option<(i32, i32, i32, i32)> {
    let mut rect = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut rect) }.ok()?;
    Some((
        rect.left,
        rect.top,
        rect.right - rect.left,
        rect.bottom - rect.top,
    ))
}

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

fn has_topmost_ex_style(hwnd: HWND) -> bool {
    let ex_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
    ex_style & (WS_EX_TOPMOST.0 as isize) != 0
}

fn is_enabled(hwnd: HWND) -> bool {
    unsafe { IsWindowEnabled(hwnd) }.as_bool()
}

fn is_visible(hwnd: HWND) -> bool {
    unsafe { IsWindowVisible(hwnd) }.as_bool()
}

fn is_iconic(hwnd: HWND) -> bool {
    unsafe { IsIconic(hwnd) }.as_bool()
}

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
