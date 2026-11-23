use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[cfg(debug_assertions)]
use windows::Win32::Graphics::Direct3D12::*;
#[cfg(debug_assertions)]
use windows::core::Interface;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct DebugMessageCounts {
    pub warning: u64,
    pub error: u64,
    pub corruption: u64,
}

static WARNINGS: AtomicU64 = AtomicU64::new(0);
static ERRORS: AtomicU64 = AtomicU64::new(0);
static CORRUPTION: AtomicU64 = AtomicU64::new(0);
static LAYER_AVAILABLE: AtomicBool = AtomicBool::new(false);

#[cfg(debug_assertions)]
const MAX_DRAIN: u64 = 256;

#[cfg(debug_assertions)]
const MAX_LOGGED: usize = 4;

pub(crate) fn layer_available() -> bool {
    LAYER_AVAILABLE.load(Ordering::Relaxed)
}

pub(crate) fn message_counts() -> DebugMessageCounts {
    DebugMessageCounts {
        warning: WARNINGS.load(Ordering::Relaxed),
        error: ERRORS.load(Ordering::Relaxed),
        corruption: CORRUPTION.load(Ordering::Relaxed),
    }
}

#[cfg(debug_assertions)]
pub(crate) fn enable_debug_layer() -> bool {
    if LAYER_AVAILABLE.load(Ordering::Relaxed) {
        return true;
    }
    let mut debug: Option<ID3D12Debug> = None;
    if unsafe { D3D12GetDebugInterface(&mut debug) }.is_err() {
        return false;
    }
    let Some(debug) = debug else {
        return false;
    };
    unsafe { debug.EnableDebugLayer() };
    LAYER_AVAILABLE.store(true, Ordering::Relaxed);
    true
}

#[cfg(not(debug_assertions))]
pub(crate) fn enable_debug_layer() -> bool {
    false
}

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
    unsafe {
        let _ = queue.PushEmptyStorageFilter();
        let _ = queue.AddStorageFilterEntries(&filter);
    }
}

#[cfg(not(debug_assertions))]
pub(crate) fn configure_info_queue(_device: &windows::Win32::Graphics::Direct3D12::ID3D12Device) {}

#[cfg(debug_assertions)]
pub(crate) fn drain_messages(device: &ID3D12Device) -> DebugMessageCounts {
    let Ok(queue) = device.cast::<ID3D12InfoQueue>() else {
        return DebugMessageCounts::default();
    };
    let mut counts = DebugMessageCounts::default();
    let mut samples: Vec<smol_str::SmolStr> = Vec::new();
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

#[cfg(not(debug_assertions))]
pub(crate) fn drain_messages(
    _device: &windows::Win32::Graphics::Direct3D12::ID3D12Device,
) -> DebugMessageCounts {
    DebugMessageCounts::default()
}

#[cfg(debug_assertions)]
unsafe fn read_message(
    queue: &ID3D12InfoQueue,
    index: u64,
) -> Option<(D3D12_MESSAGE_SEVERITY, smol_str::SmolStr)> {
    let mut length = 0usize;
    if unsafe { queue.GetMessage(index, None, &mut length) }.is_err()
        || length < std::mem::size_of::<D3D12_MESSAGE>()
    {
        return None;
    }
    let mut buffer = vec![0u8; length];
    let mut written = length;
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

    #[test]
    fn the_release_build_has_no_debug_layer() {
        if cfg!(debug_assertions) {
            return;
        }
        assert!(!super::enable_debug_layer());
        assert!(!super::layer_available());
        assert_eq!(message_counts(), DebugMessageCounts::default());
    }

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
        let second = drain_messages(device.raw());
        assert_eq!(second, DebugMessageCounts::default());
    }
}
