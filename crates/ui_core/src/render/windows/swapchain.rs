//! FLIP_SEQUENTIAL swapchain / `Present1` dirty rects / GPU 线程侧的 resize
//! （`design/phase-08-d3d12.md` §5.3 / §6.4、`design/async-render-pipeline.md` §6.2 / §6.4）。
//!
//! 三条实测硬约束（DEVIATIONS F-72）在这里集中实现：
//! ① 创建后 / `ResizeBuffers` 后**第一次 present 必须全量**（`DirtyRectsCount = 0`）；
//! ② dirty rect 必须非空且完全落在 surface 内（由共享层 `compositor::plan` 夹紧）；
//! ③ `pPresentParameters` 永不为 NULL。
//!
//! 呈现节奏来自 `DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT` 的 waitable 对象
//! （`design/async-render-pipeline.md` §6.2）：`SetMaximumFrameLatency(2)` + 每帧渲染前等一次
//! 该句柄。flag 是 swapchain 自己的帧延迟信号量，不与同进程其它 swapchain 共享隐式计数。
//!
//! 格式：`B8G8R8A8_UNORM`（sRGB flip swapchain 在本机创建即失败，DEVIATIONS F-80）。

use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, HWND, RECT},
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

/// flip-model 的 back buffer 数（主设计 §2.4 的 `BufferCount >= 2`；本设计取 3）。
pub(crate) const BUFFER_COUNT: u32 = 3;

/// 一个 flip swapchain + 它的帧延迟 waitable（auto-reset，每帧等一次）。
pub(crate) struct Swapchain {
    raw: IDXGISwapChain1,
    chain2: IDXGISwapChain2,
    /// `GetFrameLatencyWaitableObject` 的句柄；本结构独占，`Drop` 时关闭。
    waitable: HANDLE,
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
        let chain2: IDXGISwapChain2 = raw
            .cast()
            .map_err(|error| init_error_from("cast IDXGISwapChain2", error))?;
        // 默认 1；2 = 一帧给合成器、一帧给 GPU（§6.2）。必须在 `IDXGISwapChain2` 上调。
        // SAFETY: swapchain / 适配器句柄由调用方持有；参数与输出结构在调用期间有效。
        unsafe { chain2.SetMaximumFrameLatency(2) }
            .map_err(|error| init_error_from("SetMaximumFrameLatency", error))?;
        let swapchain = Self::validate(raw, chain2)?;
        Ok(swapchain)
    }

    fn validate(raw: IDXGISwapChain1, chain2: IDXGISwapChain2) -> Result<Self, EngineError> {
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
        let waitable = frame_latency_waitable(&chain2)?;
        Ok(Self {
            raw,
            chain2,
            waitable,
        })
    }

    /// 帧延迟 waitable（GPU 线程在每帧渲染前等它一次，§6.3 ①）。
    pub(crate) fn waitable(&self) -> HANDLE {
        self.waitable
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

    /// `Present1`：dirty 集由 [`FramePlan`] 决定（`fresh` ⇒ 全量，F-72 ①）。
    ///
    /// **成功之后**才清除 `FramePlan` 的 fresh：失败时下一帧仍按首帧全量处理。
    pub(crate) fn present1(&mut self, frame: &mut FramePlan, sync: u32) -> Result<(), EngineError> {
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

    /// `ResizeBuffers(0, w, h, UNKNOWN, FRAME_LATENCY_WAITABLE_OBJECT)`（§6.4）。
    ///
    /// 前置条件（调用方保证）：GPU 已完成全部在飞提交、命令列表已复位（不再持有 back buffer
    /// 引用）。flag **必须原样传入**，否则重建后 waitable 失效 / 帧延迟语义丢失。
    pub(crate) fn resize(&mut self, size: (u32, u32)) -> Result<(), EngineError> {
        // SAFETY: COM 调用：设备 / 工厂 / swapchain 句柄有效，参数与输出指针在调用期间存活，失败按 HRESULT 处理。
        unsafe {
            self.raw.ResizeBuffers(
                0,
                size.0.max(1),
                size.1.max(1),
                windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_UNKNOWN,
                DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT,
            )
        }
        .map_err(|error| EngineError::Swapchain {
            detail: smol_str::format_smolstr!("ResizeBuffers: {:#010X}", error.code().0 as u32),
        })?;
        // 探针 2（§11）：某些驱动在 resize 后会换一个 waitable 句柄。不同则关掉旧的。
        let waitable = frame_latency_waitable(&self.chain2)?;
        if waitable != self.waitable {
            // SAFETY: 旧句柄由本结构独占；新句柄已取到并即将替换。
            unsafe {
                let _ = CloseHandle(self.waitable);
            }
            self.waitable = waitable;
        }
        Ok(())
    }
}

impl Drop for Swapchain {
    fn drop(&mut self) {
        // SAFETY: waitable 由本结构独占（resize 后已换成新句柄）；`Drop` 只执行一次。
        unsafe {
            let _ = CloseHandle(self.waitable);
        }
    }
}

/// 取帧延迟 waitable；失败（NULL）按强类型 swapchain 错误上报，绝不拿 NULL 去 `INFINITE` 等待。
fn frame_latency_waitable(chain: &IDXGISwapChain2) -> Result<HANDLE, EngineError> {
    // SAFETY: swapchain / 适配器句柄由调用方持有；参数与输出结构在调用期间有效。
    let waitable = unsafe { chain.GetFrameLatencyWaitableObject() };
    if waitable.is_invalid() {
        return Err(EngineError::Swapchain {
            detail: smol_str::SmolStr::new_static(
                "GetFrameLatencyWaitableObject returned a null handle",
            ),
        });
    }
    Ok(waitable)
}

fn swap_chain_desc(size: (u32, u32)) -> DXGI_SWAP_CHAIN_DESC1 {
    DXGI_SWAP_CHAIN_DESC1 {
        Width: size.0.max(1),
        Height: size.1.max(1),
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
        Flags: DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32,
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
        Common::DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT,
        DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
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

    #[test]
    fn the_frame_latency_waitable_flag_is_always_created_and_resized_with_it() {
        let desc = swap_chain_desc((64, 64));
        assert_eq!(
            desc.Flags, DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32,
            "flag 让 swapchain 拥有独立的帧延迟信号量（§6.2）"
        );
        // `resize` 里同一条 flag 原样传给 `ResizeBuffers`（§6.4）；这里断言常量不会漂移。
        assert_eq!(
            DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0, 0x40,
            "DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT"
        );
    }
}
