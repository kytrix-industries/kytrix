//! FLIP_SEQUENTIAL swapchain / `Present1` dirty rects / resize（`design/phase-08-d3d12.md` §5.3 / §6.4）。
//!
//! 三条实测硬约束（DEVIATIONS F-72）在这里集中实现：
//! ① 创建后 / `ResizeBuffers` 后**第一次 present 必须全量**（`DirtyRectsCount = 0`）；
//! ② dirty rect 必须非空且完全落在 surface 内（由共享层 `compositor::plan` 夹紧）；
//! ③ `pPresentParameters` 永不为 NULL。
//!
//! 格式：`B8G8R8A8_UNORM`（sRGB flip swapchain 在本机创建即失败，DEVIATIONS F-80）。

use windows::{
    Win32::{
        Foundation::{HWND, RECT},
        Graphics::{
            Direct3D12::*,
            Dxgi::{
                Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC},
                *,
            },
        },
    },
    core::Interface,
};

use crate::{
    error::EngineError,
    render::{
        compositor::{DamageRects, FramePlan},
        damage::MAX_DAMAGE_RECTS,
        windows::{device::init_error_from, queue::Queue},
    },
};

/// flip-model 的 back buffer 数（主设计 §2.4 的 `BufferCount ≥ 2`；本设计取 3）。
pub(crate) const BUFFER_COUNT: u32 = 3;

/// 一个 flip swapchain（`fresh` 记录「下一次 present 必须全量」）。
pub(crate) struct Swapchain {
    raw: IDXGISwapChain1,
    size: (u32, u32),
    fresh: bool,
}

impl Swapchain {
    /// 隐藏 / 真实 HWND 的 swapchain（P09 的窗口层与隐藏窗口 probe 共用）。
    pub(crate) fn for_hwnd(
        queue: &Queue,
        hwnd: isize,
        size: (u32, u32),
    ) -> Result<Self, EngineError> {
        let factory = create_factory()?;
        let desc = swap_chain_desc(size);
        let window = HWND(hwnd as *mut core::ffi::c_void);
        let raw: IDXGISwapChain1 =
            // SAFETY: COM 调用：设备 / 工厂 / swapchain 句柄有效，参数与输出指针在调用期间存活，失败按 HRESULT 处理。
            unsafe { factory.CreateSwapChainForHwnd(queue.raw(), window, &desc, None, None) }
                .map_err(|error| EngineError::UnsupportedGpu {
                    os: "windows",
                    required: "FLIP_SEQUENTIAL / BufferCount >= 2",
                    actual: smol_str::format_smolstr!(
                        "CreateSwapChainForHwnd: {:#010X}",
                        error.code().0 as u32
                    ),
                })?;
        Self::validate(raw, size)
    }

    fn validate(raw: IDXGISwapChain1, size: (u32, u32)) -> Result<Self, EngineError> {
        // SAFETY: swapchain / 适配器句柄由调用方持有；参数与输出结构在调用期间有效。
        let actual = unsafe { raw.GetDesc1() }
            .map_err(|error| init_error_from("SwapChain::GetDesc1", error))?;
        if actual.SwapEffect != DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL || actual.BufferCount < 2 {
            return Err(EngineError::UnsupportedGpu {
                os: "windows",
                required: "FLIP_SEQUENTIAL / BufferCount >= 2",
                actual: smol_str::format_smolstr!(
                    "SwapEffect={} / BufferCount={}",
                    actual.SwapEffect.0,
                    actual.BufferCount
                ),
            });
        }
        Ok(Self {
            raw,
            size,
            fresh: true,
        })
    }

    pub(crate) fn is_fresh(&self) -> bool {
        self.fresh
    }

    /// 当前 back buffer（**每帧重新问 DXGI**；flip model 的 current index 会随 present 轮转）。
    ///
    /// 调用方只在录制 copy 期间持有，**不得**跨帧保留（resize 前提）。
    pub(crate) fn back_buffer(&self) -> Result<ID3D12Resource, EngineError> {
        // flip model 的 current index 在 `IDXGISwapChain3` 上。
        let chain: IDXGISwapChain3 = self
            .raw
            .cast()
            .map_err(|error| init_error_from("cast IDXGISwapChain3", error))?;
        // SAFETY: swapchain / 适配器句柄由调用方持有；参数与输出结构在调用期间有效。
        let index = unsafe { chain.GetCurrentBackBufferIndex() };
        // SAFETY: COM 调用：设备 / 工厂 / swapchain 句柄有效，参数与输出指针在调用期间存活，失败按 HRESULT 处理。
        unsafe { self.raw.GetBuffer(index) }.map_err(|error| EngineError::Swapchain {
            detail: smol_str::format_smolstr!(
                "GetBuffer({index}): {:#010X}",
                error.code().0 as u32
            ),
        })
    }

    /// `Present1`：dirty 集由 [`FramePlan`] 决定（fresh ⇒ 全量，F-72 ①）。
    ///
    /// **成功之后**才清除 fresh 与 swapchain 自身的标记：失败时下一帧仍按首帧全量处理。
    pub(crate) fn present(&mut self, frame: &mut FramePlan, sync: u32) -> Result<(), EngineError> {
        let mut rects = [RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        }; MAX_DAMAGE_RECTS];
        let count = match frame.present_dirty() {
            Some(damage) => fill_rects(&damage, &mut rects),
            None => 0,
        };

        let params = DXGI_PRESENT_PARAMETERS {
            DirtyRectsCount: count,
            pDirtyRects: if count == 0 {
                std::ptr::null_mut()
            } else {
                rects.as_mut_ptr()
            },
            pScrollRect: std::ptr::null_mut(),
            pScrollOffset: std::ptr::null_mut(),
        };
        // SAFETY: swapchain / 适配器句柄由调用方持有；参数与输出结构在调用期间有效。
        let hr = unsafe { self.raw.Present1(sync, DXGI_PRESENT(0), &params) };
        if hr.is_ok() {
            frame.presented();
            self.fresh = false;
            Ok(())
        } else {
            Err(EngineError::Swapchain {
                detail: smol_str::format_smolstr!(
                    "Present1({count} dirty rects): {:#010X}",
                    hr.0 as u32
                ),
            })
        }
    }

    /// `ResizeBuffers(0,0,0,UNKNOWN,0)`：调用前必须已有 `queue.wait_idle()` 且不持有 back buffer 引用。
    pub(crate) fn resize(&mut self, size: (u32, u32)) -> Result<(), EngineError> {
        // SAFETY: COM 调用：设备 / 工厂 / swapchain 句柄有效，参数与输出指针在调用期间存活，失败按 HRESULT 处理。
        unsafe {
            self.raw.ResizeBuffers(
                0,
                0,
                0,
                windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_UNKNOWN,
                DXGI_SWAP_CHAIN_FLAG(0),
            )
        }
        .map_err(|error| EngineError::Swapchain {
            detail: smol_str::format_smolstr!("ResizeBuffers: {:#010X}", error.code().0 as u32),
        })?;
        self.size = size;
        self.fresh = true;
        Ok(())
    }
}

fn swap_chain_desc(size: (u32, u32)) -> DXGI_SWAP_CHAIN_DESC1 {
    DXGI_SWAP_CHAIN_DESC1 {
        Width: size.0,
        Height: size.1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        Stereo: false.into(),
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: BUFFER_COUNT,
        Scaling: DXGI_SCALING_STRETCH,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
        AlphaMode: windows::Win32::Graphics::Dxgi::Common::DXGI_ALPHA_MODE_IGNORE,
        Flags: 0,
    }
}

fn create_factory() -> Result<IDXGIFactory4, EngineError> {
    // SAFETY: COM 调用：设备 / 工厂 / swapchain 句柄有效，参数与输出指针在调用期间存活，失败按 HRESULT 处理。
    unsafe { CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)) }
        .map_err(|error| init_error_from("CreateDXGIFactory2", error))
}

/// `PhysicalRect` → `RECT`（半开区间 → Win32 的闭区间右/下边界）。
fn fill_rects(damage: &DamageRects, out: &mut [RECT; MAX_DAMAGE_RECTS]) -> u32 {
    let mut count = 0;
    for rect in damage.as_slice() {
        out[count] = RECT {
            left: rect.x0,
            top: rect.y0,
            right: rect.x1,
            bottom: rect.y1,
        };
        count += 1;
    }
    count as u32
}

#[cfg(test)]
mod tests {
    use windows::Win32::Graphics::Dxgi::{
        Common::DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
    };

    use super::{BUFFER_COUNT, swap_chain_desc};

    #[test]
    fn flip_sequential_contract_is_written_down() {
        let desc = swap_chain_desc((256, 256));
        assert_eq!(desc.SwapEffect, DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL);
        assert_eq!(desc.BufferCount, BUFFER_COUNT);
        assert!(desc.BufferCount >= 2);
        // sRGB flip swapchain 在本机创建即失败（DEVIATIONS F-80）：格式固定 UNORM。
        assert_eq!(desc.Format, DXGI_FORMAT_B8G8R8A8_UNORM);
        assert_eq!(desc.SampleDesc.Count, 1);
    }
}
