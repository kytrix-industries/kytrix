//! D3D12 调试层 + `ID3D12InfoQueue`（R08-5）。
//!
//! 与 Vulkan 后端对称（`render/linux/instance.rs` 在 `debug_assertions` 下打开
//! `VK_LAYER_KHRONOS_validation`）：dev 构建必须让 D3D12 的校验真正跑起来，release 恒关。
//!
//! 三步，顺序不能错：
//! 1. [`enable_debug_layer`] 必须在 `D3D12CreateDevice` **之前**调用（进程级幂等）；
//! 2. [`configure_info_queue`] 在设备创建**之后**调用：`PushEmptyStorageFilter` + 拒绝
//!    `INFO` / `MESSAGE` 两级消息 —— 否则每帧的 `SetDescriptorHeaps` / barrier 提示会刷屏并
//!    吃满 InfoQueue 的默认额度，把最早的、真正有价值的那条 ERROR 挤掉；
//! 3. [`drain_messages`] 把排队里的 WARNING/ERROR/CORRUPTION 计数进
//!    [`crate::render::windows::diagnostics`]（`Diagnostics::debug_*`），把最严重的几条原文
//!    写进日志，然后**清空**队列：长跑数天的进程不能让 InfoQueue 自己截断历史。
//!
//! release 构建：`enable_debug_layer` 返回 `false`、其余是空实现，能力行里打印
//! `debug_layer=false`（见 `device.rs` 的 `GpuCaps::describe`）。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[cfg(debug_assertions)]
use windows::Win32::Graphics::Direct3D12::*;
#[cfg(debug_assertions)]
use windows::core::Interface;

/// InfoQueue 里我们关心的三类消息的累计计数（进程级）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct DebugMessageCounts {
    pub warning: u64,
    pub error: u64,
    pub corruption: u64,
}

static WARNINGS: AtomicU64 = AtomicU64::new(0);
static ERRORS: AtomicU64 = AtomicU64::new(0);
static CORRUPTION: AtomicU64 = AtomicU64::new(0);
/// 调试层是否真的可用（`D3D12GetDebugInterface` 成功；零售机缺「图形工具」时为 `false`）。
static LAYER_AVAILABLE: AtomicBool = AtomicBool::new(false);

/// 单次排空最多处理多少条消息（避免设备挂死时在 InfoQueue 里二次挂死）。
#[cfg(debug_assertions)]
const MAX_DRAIN: u64 = 256;

/// 一次排空里最多把多少条 ERROR/CORRUPTION 的原文写进日志。
#[cfg(debug_assertions)]
const MAX_LOGGED: usize = 4;

/// 调试层是否可用（能力行 `debug_layer=`）。
pub(crate) fn layer_available() -> bool {
    LAYER_AVAILABLE.load(Ordering::Relaxed)
}

/// 累计消息计数（`Diagnostics` / 日志用）。
pub(crate) fn message_counts() -> DebugMessageCounts {
    DebugMessageCounts {
        warning: WARNINGS.load(Ordering::Relaxed),
        error: ERRORS.load(Ordering::Relaxed),
        corruption: CORRUPTION.load(Ordering::Relaxed),
    }
}

/// 打开 D3D12 调试层；**必须**在 `D3D12CreateDevice` 之前调用。返回是否可用。
///
/// 幂等：设备恢复（R08-4）会重新建设备，届时再调一次是无害的（调试层是进程级开关）。
/// 不打开 GPU-based validation（那是另一个数量级的开销，不在本后端的构建契约里）。
#[cfg(debug_assertions)]
pub(crate) fn enable_debug_layer() -> bool {
    if LAYER_AVAILABLE.load(Ordering::Relaxed) {
        return true;
    }
    let mut debug: Option<ID3D12Debug> = None;
    // SAFETY: 调试层是可选能力：零售机缺「图形工具」时返回错误，被静默降级为「层不可用」。
    if unsafe { D3D12GetDebugInterface(&mut debug) }.is_err() {
        return false;
    }
    let Some(debug) = debug else {
        return false;
    };
    // SAFETY: 接口句柄在本次调用期间有效；`EnableDebugLayer` 无返回值、无失败面。
    unsafe { debug.EnableDebugLayer() };
    LAYER_AVAILABLE.store(true, Ordering::Relaxed);
    true
}

/// release：调试层恒关（`D3D12Debug` 不参与发布物的任何分支）。
#[cfg(not(debug_assertions))]
pub(crate) fn enable_debug_layer() -> bool {
    false
}

/// 配好信息队列：只留 WARNING 及更严重的消息（每帧 INFO 级提示全拒）。
///
/// 拒绝 INFO 用**存储过滤器**（storage filter）而不是检索过滤器：被拒的消息不会进队列，
/// InfoQueue 的额度（默认 1024 条 / 1 MiB）留给真正的错误。
#[cfg(debug_assertions)]
pub(crate) fn configure_info_queue(device: &ID3D12Device) {
    let Ok(queue) = device.cast::<ID3D12InfoQueue>() else {
        return;
    };
    let mut deny = [D3D12_MESSAGE_SEVERITY_INFO, D3D12_MESSAGE_SEVERITY_MESSAGE];
    let filter = D3D12_INFO_QUEUE_FILTER {
        AllowList: D3D12_INFO_QUEUE_FILTER_DESC::default(),
        DenyList: D3D12_INFO_QUEUE_FILTER_DESC {
            NumCategories: 0,
            pCategoryList: std::ptr::null_mut(),
            NumSeverities: deny.len() as u32,
            pSeverityList: deny.as_mut_ptr(),
            NumIDs: 0,
            pIDList: std::ptr::null_mut(),
        },
    };
    // SAFETY: 过滤器描述与它指向的 severity 数组在本次调用期间有效。
    unsafe {
        // 先压一个空过滤器作为基线，再加拒绝项（叠加语义，与 Vulkan 的 message filter 同构）。
        let _ = queue.PushEmptyStorageFilter();
        let _ = queue.AddStorageFilterEntries(&filter);
    }
}

/// release：无 InfoQueue 可配。
#[cfg(not(debug_assertions))]
pub(crate) fn configure_info_queue(_device: &windows::Win32::Graphics::Direct3D12::ID3D12Device) {}

/// 排空 InfoQueue：累计计数、把最严重的几条写进日志、清空队列。返回本次新到的计数。
///
/// 消费者：设备丢失（`Device::note_failure` 的 DRED 日志）与进程 teardown
/// （`SharedGpu::drop`）；每帧调用会把 `GetNumStoredMessages` 变成热路径开销，不需要。
#[cfg(debug_assertions)]
pub(crate) fn drain_messages(device: &ID3D12Device) -> DebugMessageCounts {
    let Ok(queue) = device.cast::<ID3D12InfoQueue>() else {
        return DebugMessageCounts::default();
    };
    let mut counts = DebugMessageCounts::default();
    let mut samples: Vec<smol_str::SmolStr> = Vec::new();
    // SAFETY: 队列句柄由设备持有；`read_message` 自己完成「问长度 → 分配 → 取内容 → 拷文本」。
    unsafe {
        let stored = queue.GetNumStoredMessages().min(MAX_DRAIN);
        for index in 0..stored {
            let Some((severity, text)) = read_message(&queue, index) else {
                continue;
            };
            if severity == D3D12_MESSAGE_SEVERITY_CORRUPTION {
                counts.corruption += 1;
            } else if severity == D3D12_MESSAGE_SEVERITY_ERROR {
                counts.error += 1;
            } else if severity == D3D12_MESSAGE_SEVERITY_WARNING {
                counts.warning += 1;
            }
            if (severity == D3D12_MESSAGE_SEVERITY_ERROR
                || severity == D3D12_MESSAGE_SEVERITY_CORRUPTION)
                && samples.len() < MAX_LOGGED
            {
                samples.push(text);
            }
        }
        // 计数与取样都完成之后才清空：日志要读到原文。
        queue.ClearStoredMessages();
    }
    WARNINGS.fetch_add(counts.warning, Ordering::Relaxed);
    ERRORS.fetch_add(counts.error, Ordering::Relaxed);
    CORRUPTION.fetch_add(counts.corruption, Ordering::Relaxed);
    for text in samples {
        eprintln!("kytrix: d3d12 debug message: {text}");
    }
    counts
}

/// release：没有消息队列。
#[cfg(not(debug_assertions))]
pub(crate) fn drain_messages(
    _device: &windows::Win32::Graphics::Direct3D12::ID3D12Device,
) -> DebugMessageCounts {
    DebugMessageCounts::default()
}

/// 读一条消息的 severity 与原文（文本在返回前拷出，内部缓冲随即释放）。
///
/// # Safety
/// `queue` 必须是设备的 `ID3D12InfoQueue`；每次 GET 都先问长度再自行分配，缓冲区至少
/// `size_of::<D3D12_MESSAGE>()` 字节、首部就是一个 `D3D12_MESSAGE`（InfoQueue 的契约），
/// `pDescription` 指向同一块缓冲内的 NUL 结尾串。
#[cfg(debug_assertions)]
unsafe fn read_message(
    queue: &ID3D12InfoQueue,
    index: u64,
) -> Option<(D3D12_MESSAGE_SEVERITY, smol_str::SmolStr)> {
    let mut length = 0usize;
    // SAFETY: 见本函数的安全契约（只问长度）。
    if unsafe { queue.GetMessage(index, None, &mut length) }.is_err()
        || length < std::mem::size_of::<D3D12_MESSAGE>()
    {
        return None;
    }
    let mut buffer = vec![0u8; length];
    let mut written = length;
    // SAFETY: 缓冲区按查询到的长度分配，`written` 是它的字节数。
    if unsafe {
        queue.GetMessage(
            index,
            Some(buffer.as_mut_ptr().cast::<D3D12_MESSAGE>()),
            &mut written,
        )
    }
    .is_err()
    {
        return None;
    }
    // SAFETY: 缓冲区首部是 `D3D12_MESSAGE`；描述串在同一块缓冲内，离开本函数前拷出。
    unsafe {
        let message = *buffer.as_ptr().cast::<D3D12_MESSAGE>();
        let text = if message.pDescription.is_null() {
            smol_str::SmolStr::new_static("<no description>")
        } else {
            std::ffi::CStr::from_ptr(message.pDescription.cast())
                .to_string_lossy()
                .into_owned()
                .into()
        };
        Some((message.Severity, text))
    }
}

#[cfg(test)]
mod tests {
    use super::{DebugMessageCounts, drain_messages, message_counts};
    use crate::render::windows::device::Device;

    /// release 构建下一切都是空实现（不入日志、不计数、不报错）。
    #[test]
    fn the_release_build_has_no_debug_layer() {
        if cfg!(debug_assertions) {
            return;
        }
        assert!(!super::enable_debug_layer());
        assert!(!super::layer_available());
        assert_eq!(message_counts(), DebugMessageCounts::default());
    }

    /// dev 构建：能建出 WARP 设备就说明「开层 + 配 InfoQueue」不会阻止设备创建；
    /// 排空不 panic，二次排空是空操作（队列已清空）。
    #[test]
    fn the_debug_layer_does_not_block_device_creation_and_drains_safely() {
        let _guard = crate::render::windows::device::gpu_test_lock();
        let Ok(device) = Device::new_warp() else {
            eprintln!("skip: no WARP device");
            return;
        };
        let first = drain_messages(device.raw());
        let total = message_counts();
        assert!(total.warning >= first.warning);
        assert!(total.error >= first.error);
        assert!(total.corruption >= first.corruption);
        // 二次排空必须是幂等的（队列已清空）。
        let second = drain_messages(device.raw());
        assert_eq!(second, DebugMessageCounts::default());
    }
}
