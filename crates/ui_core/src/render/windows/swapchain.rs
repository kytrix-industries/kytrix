//! **组合**交换链（`CreateSwapChainForComposition`）/ `ResizeBuffers` / `Present1` 分类
//!
//! 窗口路径的交换链不是 HWND swapchain：它的内容经 `IDCompositionVisual::SetContent` 交给
//! DirectComposition（`platform/windows/composition.rs`），窗口的客户区由 DComp 合成。
//!
//! 三条实测硬约束（probe `caps.log` / `present.log` / `resize.log`）：
//! ① `DXGI_SCALING_STRETCH` 是**唯一**可用的缩放模式（`SCALING_NONE` 创建即 `0x887A0001`）；
//! ② 创建后的**第一次 present 必须全量**（`DirtyRectsCount = 0`）—— 由调用方的
//!    [`crate::render::windows::compositor::TargetState`] 保证，本类型不再自己记 `first_present`
//!    （R08-11：那曾是第三个「必须全量」状态源）；
//! ③ dirty rect 必须非空且完全落在 buffer 内（越界 / 负坐标都是 `DXGI_ERROR_INVALID_CALL`）——
//!    调用方在录制前对每个矩形做一次 `intersect(content)`（R08-10），本类型只做精确转换。
//!
//! **`ResizeBuffers` 是唯一的重建方式**（R08-1：旧注释断言「组合交换链第一次 present 后
//! `ResizeBuffers` 必失败」与 MSDN 不符 —— MSDN 列出的失败条件只有「未释放 back buffer 的
//! 直接/间接引用」「组合链 W/H 传 0」「参数非法」。旧 `grow` 路径做的正是它的前置条件：
//! `wait_idle` + `reset_in_flight_lists` 已经丢掉了命令列表的间接引用，`GetBuffer` 每帧重取、
//! 从不缓存 ⇒ 直接/间接引用都已释放）。因此尺寸变化只需要 `ResizeBuffers(3, w, h, UNKNOWN,
//! 原 flag)` 并重写 back buffer 的 RTV；整条交换链的替换只剩 `ResizeBuffers` 失败后的回退
//! 开关（[`Swapchain::resize`] 返回的原始 HRESULT 由调用方判定，见
//! `hresult::resize_fallback_worthwhile`）。
//!
//! 呈现节奏来自 `DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT` 的 waitable 对象
//! `SetMaximumFrameLatency(1)` + 每帧渲染前等一次
//! 该句柄。`SetBackgroundColor` 在组合交换链上是**惰性**的（probe：返回 OK 但不影响合成），
//! 因此背景色只走持久目标的 clear（`render/color.rs`）与 1×1 背景层。
//!
//! 格式：`B8G8R8A8_UNORM`（sRGB flip swapchain 在本机创建即失败，DEVIATIONS F-80）。back
//! buffer 的 RTV 用 `_UNORM_SRGB` **视图**（与持久目标同一套 relaxed format casting 前提，
//! R08-12）：全量帧直接画进 back buffer 时（R08-8 的直绘路径）必须让硬件做一次 sRGB 编码。

use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, RECT},
        Graphics::{
            Direct3D12::*,
            Dxgi::{
                Common::{
                    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM,
                    DXGI_FORMAT_B8G8R8A8_UNORM_SRGB, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
                },
                *,
            },
        },
    },
    core::Interface,
};

use super::hresult::{DXGI_ERROR_INVALID_CALL, PresentOutcome, classify_present};
use crate::{
    error::EngineError,
    render::{compositor::DamageRects, damage::MAX_DAMAGE_RECTS, windows::device::init_error_from},
};

/// flip-model 的 back buffer 数（主设计 §2.4 的 `BufferCount >= 2`；本设计取 3）。
pub(crate) const BUFFER_COUNT: u32 = 3;

/// 一条组合交换链 + 它的帧延迟 waitable + 自己那几个 back buffer 的 RTV。
///
/// RTV 堆**每条交换链自带**（不挤共享堆）：内容层与 1×1 背景层各有一条交换链，各自的 back
/// buffer 身份只在本链内有意义；`ResizeBuffers` 之后重写这一小撮描述符即可。
pub(crate) struct Swapchain {
    raw: IDXGISwapChain1,
    /// `GetFrameLatencyWaitableObject` 的句柄（auto-reset，每帧等一次）。
    waitable: HANDLE,
    /// 当前 back buffer 尺寸（物理像素）。`ResizeBuffers` 之后更新。
    buffer: (u32, u32),
    /// back buffer 的 RTV 堆（`BUFFER_COUNT` 个描述符）。
    rtv_heap: ID3D12DescriptorHeap,
    rtv_size: u32,
}

impl Swapchain {
    /// 建链 + 打开 waitable + 显式声明色彩空间 + 写 back buffer 的 RTV。
    ///
    /// `buffer` 是这条交换链的尺寸（物理像素，已按 [`crate::platform::windows::composition::buffer_size`]
    /// 取桶）；此后尺寸变化走 [`Swapchain::resize`]。
    pub(crate) fn for_composition(
        device: &ID3D12Device,
        queue: &ID3D12CommandQueue,
        buffer: (u32, u32),
    ) -> Result<Self, EngineError> {
        let factory = create_factory()?;
        let desc = composition_desc(buffer);
        // SAFETY: COM 调用：工厂 / 队列 / 描述符在调用期间有效；`NULL` output 表示不限制到
        // 某个显示器；失败按 HRESULT 处理。
        let raw: IDXGISwapChain1 =
            unsafe { factory.CreateSwapChainForComposition(queue, &desc, None) }.map_err(
                |error| EngineError::Swapchain {
                    detail: smol_str::format_smolstr!(
                        "CreateSwapChainForComposition({}x{}): {:#010X}",
                        desc.Width,
                        desc.Height,
                        error.code().0 as u32
                    ),
                },
            )?;
        let waitable = arm_frame_latency(&raw)?;
        apply_color_space(&raw)?;
        let rtv_heap = create_rtv_heap(device)?;
        // SAFETY: 设备 / 堆句柄由调用方持有，调用期间有效；查询无副作用。
        let rtv_size =
            unsafe { device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV) };
        let chain = Self {
            raw,
            waitable,
            buffer: (desc.Width.max(1), desc.Height.max(1)),
            rtv_heap,
            rtv_size,
        };
        chain.write_back_buffer_rtvs(device)?;
        Ok(chain)
    }

    /// 帧延迟 waitable（GPU 线程在每帧渲染前等它一次，§6.3 ①）。
    pub(crate) fn waitable(&self) -> HANDLE {
        self.waitable
    }

    /// 组合交换链本体（`IDCompositionVisual::SetContent` 的输入）。
    pub(crate) fn raw(&self) -> &IDXGISwapChain1 {
        &self.raw
    }

    /// 当前 back buffer 尺寸（= 最近一次 `Create`/`ResizeBuffers` 的 `buffer`）。
    pub(crate) fn buffer(&self) -> (u32, u32) {
        self.buffer
    }

    /// 当前 back buffer：`(索引, 资源)`。**每帧重新问 DXGI**（flip model 的 current index 会随
    /// present 轮转；不缓存引用也是 `ResizeBuffers` 的前置条件之一）。
    pub(crate) fn back_buffer(&self) -> Result<(u32, ID3D12Resource), EngineError> {
        let chain: IDXGISwapChain3 = self
            .raw
            .cast()
            .map_err(|error| init_error_from("cast IDXGISwapChain3", error))?;
        // SAFETY: swapchain 句柄由本结构持有；查询无副作用。
        let index = unsafe { chain.GetCurrentBackBufferIndex() };
        // SAFETY: COM 调用：swapchain 句柄有效，输出指针在调用期间存活，失败按 HRESULT 处理。
        let resource =
            unsafe { self.raw.GetBuffer(index) }.map_err(|error| EngineError::Swapchain {
                detail: smol_str::format_smolstr!(
                    "GetBuffer({index}): {:#010X}",
                    error.code().0 as u32
                ),
            })?;
        Ok((index, resource))
    }

    /// 第 `index` 个 back buffer 的 sRGB RTV（全量帧直绘路径的渲染目标）。
    pub(crate) fn rtv(&self, index: u32) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        debug_assert!(index < BUFFER_COUNT, "back buffer index out of range");
        // SAFETY: 堆由本结构持有，句柄在堆生命周期内有效；查询无副作用。
        let base = unsafe { self.rtv_heap.GetCPUDescriptorHandleForHeapStart() };
        D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: base.ptr + (u64::from(self.rtv_size) * u64::from(index)) as usize,
        }
    }

    /// `ResizeBuffers`：**唯一的**尺寸变化手段（R08-1）。
    ///
    /// 前置条件（全部由调用方满足）：
    /// 1. 本窗口的 GPU 工作已完成（`CommandStream::wait_idle`）且 in-flight 命令列表已重置
    ///    （`reset_in_flight_lists`）—— 这是「丢掉 back buffer 的间接引用」的唯一手段；
    /// 2. 不持有任何 `GetBuffer` 引用（本类型每帧重取、从不缓存）。
    ///
    /// 之后重设 `SetMaximumFrameLatency(1)`、重写全部 back buffer 的 RTV，并确认 waitable 句柄
    /// 仍然有效（句柄由同一个交换链对象拥有；`ResizeBuffers` 不换对象）。
    ///
    /// 失败时返回**原始** `windows::core::Error`：调用方据 `hresult::resize_fallback_worthwhile`
    /// 判断是否回退到整链替换（R08-1 的可回退开关），否则包装成 `EngineError::Swapchain`。
    pub(crate) fn resize(
        &mut self,
        device: &ID3D12Device,
        buffer: (u32, u32),
    ) -> Result<(), windows::core::Error> {
        let buffer = (buffer.0.max(1), buffer.1.max(1));
        if buffer == self.buffer {
            return Ok(());
        }
        // flag 必须与创建时一致（DXGI 明确禁止用 ResizeBuffers 增删 WAITABLE 标志）；
        // 格式传 UNKNOWN 表示保持不变（组合链不接受 sRGB 格式，见模块文档）。
        // SAFETY: 调用方已释放全部 back buffer 引用；参数是值。
        unsafe {
            self.raw.ResizeBuffers(
                BUFFER_COUNT,
                buffer.0,
                buffer.1,
                DXGI_FORMAT_UNKNOWN,
                DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT,
            )
        }?;
        self.buffer = buffer;
        // 句柄由同一个交换链对象拥有；仍取一次并以新值替换（不同则关闭旧句柄）。
        let waitable = arm_frame_latency(&self.raw).map_err(|error| match error {
            EngineError::Swapchain { detail } => windows::core::Error::new(
                DXGI_ERROR_INVALID_CALL,
                smol_str::format_smolstr!("frame latency waitable after resize: {detail}"),
            ),
            other => windows::core::Error::new(
                DXGI_ERROR_INVALID_CALL,
                smol_str::format_smolstr!("frame latency waitable after resize: {other}"),
            ),
        })?;
        if waitable != self.waitable {
            // SAFETY: 旧句柄由本结构独占；替换之后不再使用。
            unsafe {
                let _ = CloseHandle(self.waitable);
            }
            self.waitable = waitable;
        }
        self.write_back_buffer_rtvs(device).map_err(|error| {
            windows::core::Error::new(
                DXGI_ERROR_INVALID_CALL,
                smol_str::format_smolstr!("back buffer RTVs after resize: {error}"),
            )
        })?;
        Ok(())
    }

    /// `Present1`：`damage = None` 表示整面（`DirtyRectsCount = 0`），否则只呈现这些矩形。
    ///
    /// 返回值是**分类**而不是 `Result`（R08-7）：`DXGI_STATUS_OCCLUDED` 等正 HRESULT 不是
    /// 失败，调用方必须据此退避/重建，而不是把窗口判死。
    pub(crate) fn present1(&mut self, damage: Option<&DamageRects>, sync: u32) -> PresentOutcome {
        let mut rects = [RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        }; MAX_DAMAGE_RECTS];
        let count = match damage {
            // 空 dirty 集与整面等价（`Plan::Partial` 里不会出现空集，这里只是防御）。
            Some(damage) if !damage.is_empty() => fill_rects(damage, &mut rects),
            _ => 0,
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
        // SAFETY: COM 调用：swapchain 句柄有效；`rects` / `params` 在调用期间存活。
        let hr = unsafe { self.raw.Present1(sync, DXGI_PRESENT(0), &params) };
        classify_present(hr)
    }

    fn write_back_buffer_rtvs(&self, device: &ID3D12Device) -> Result<(), EngineError> {
        let desc = D3D12_RENDER_TARGET_VIEW_DESC {
            // **视图**格式：资源是 UNORM、视图是 sRGB（R08-12 的 relaxed format casting 前提），
            // 与持久目标同一套语义 —— 直绘路径写进 back buffer 的字节必须已经过 sRGB 编码。
            Format: DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
            ViewDimension: D3D12_RTV_DIMENSION_TEXTURE2D,
            Anonymous: D3D12_RENDER_TARGET_VIEW_DESC_0 {
                Texture2D: D3D12_TEX2D_RTV {
                    MipSlice: 0,
                    PlaneSlice: 0,
                },
            },
        };
        for index in 0..BUFFER_COUNT {
            // SAFETY: swapchain / 设备 / 堆句柄由调用方持有，调用期间有效；索引在堆容量内。
            let resource: ID3D12Resource =
                unsafe { self.raw.GetBuffer(index) }.map_err(|error| EngineError::Swapchain {
                    detail: smol_str::format_smolstr!(
                        "GetBuffer({index}) for RTVs: {:#010X}",
                        error.code().0 as u32
                    ),
                })?;
            // SAFETY: 同上；视图描述指向本函数的栈上值。
            unsafe { device.CreateRenderTargetView(&resource, Some(&desc), self.rtv(index)) };
            // 不保留 `resource`：RTV 由堆持有，交换链本身拥有 buffer 的生命周期。
            drop(resource);
        }
        Ok(())
    }
}

impl Drop for Swapchain {
    fn drop(&mut self) {
        // SAFETY: waitable 由本结构独占；`Drop` 只执行一次。
        unsafe {
            let _ = CloseHandle(self.waitable);
        }
    }
}

/// 打开帧延迟 waitable 并设 `SetMaximumFrameLatency(1)`（CPU 侧排队深度 1）。
///
/// `IDXGISwapChain2::SetMaximumFrameLatency` 是唯一正确入口（用 `IDXGIDevice1` 的版本在
/// waitable 模式下无效，0.1 第 3 条）。
fn arm_frame_latency(raw: &IDXGISwapChain1) -> Result<HANDLE, EngineError> {
    let chain2: IDXGISwapChain2 = raw
        .cast()
        .map_err(|error| init_error_from("cast IDXGISwapChain2", error))?;
    // SAFETY: swapchain 句柄有效；参数是值。
    unsafe { chain2.SetMaximumFrameLatency(1) }
        .map_err(|error| init_error_from("SetMaximumFrameLatency", error))?;
    // SAFETY: 句柄由交换链拥有，调用期间有效。
    let waitable = unsafe { chain2.GetFrameLatencyWaitableObject() };
    if waitable.is_invalid() {
        return Err(EngineError::Swapchain {
            detail: smol_str::SmolStr::new_static(
                "GetFrameLatencyWaitableObject returned a null handle",
            ),
        });
    }
    Ok(waitable)
}

/// R06-10：**显式声明**呈现面的色彩空间（sRGB / Rec.709）。三端共用
/// `render::colorspace` 的决定，这里只做 DXGI 翻译。
fn apply_color_space(raw: &IDXGISwapChain1) -> Result<(), EngineError> {
    let chain3: IDXGISwapChain3 = raw
        .cast()
        .map_err(|error| init_error_from("cast IDXGISwapChain3", error))?;
    let color_space = crate::render::colorspace::dxgi_color_space();
    // SAFETY: swapchain 句柄有效；参数是值。
    let support = unsafe { chain3.CheckColorSpaceSupport(color_space) }
        .map_err(|error| init_error_from("CheckColorSpaceSupport", error))?;
    if crate::render::colorspace::support_allows_present(support) {
        // SAFETY: 同上。
        unsafe { chain3.SetColorSpace1(color_space) }.map_err(|error| EngineError::Swapchain {
            detail: smol_str::format_smolstr!("SetColorSpace1: {:#010X}", error.code().0 as u32),
        })?;
    }
    Ok(())
}

fn create_rtv_heap(device: &ID3D12Device) -> Result<ID3D12DescriptorHeap, EngineError> {
    // SAFETY: 设备句柄由调用方持有；描述是值参数，失败按 HRESULT 返回。
    unsafe {
        device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
            Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
            NumDescriptors: BUFFER_COUNT,
            Flags: D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
            NodeMask: 0,
        })
    }
    .map_err(|error| init_error_from("CreateDescriptorHeap(back buffer RTVs)", error))
}

/// 组合交换链描述（probe `caps.log` 的「baseline」行）。
fn composition_desc(size: (u32, u32)) -> DXGI_SWAP_CHAIN_DESC1 {
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
        // 组合交换链只接受 STRETCH（`SCALING_NONE` / `SCALING` 其它值创建即 0x887A0001）。
        Scaling: DXGI_SCALING_STRETCH,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
        AlphaMode: DXGI_ALPHA_MODE_IGNORE,
        Flags: DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32,
    }
}

fn create_factory() -> Result<IDXGIFactory4, EngineError> {
    // SAFETY: COM 创建调用：工厂句柄是输出，失败按 HRESULT 处理。
    unsafe { CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)) }
        .map_err(|error| init_error_from("CreateDXGIFactory2", error))
}

/// `DamageRects` → `RECT`（半开区间 → Win32 的闭区间右/下边界）。返回矩形数。
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
        Common::{DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM},
        DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT,
        DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
    };

    use super::{BUFFER_COUNT, composition_desc, fill_rects};
    use crate::{
        math::PhysicalRect,
        render::{compositor::DamageRects, damage::MAX_DAMAGE_RECTS},
    };

    fn rects() -> [windows::Win32::Foundation::RECT; MAX_DAMAGE_RECTS] {
        [windows::Win32::Foundation::RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        }; MAX_DAMAGE_RECTS]
    }

    #[test]
    fn the_composition_desc_is_stretch_flip_sequential_with_three_buffers() {
        let desc = composition_desc((1000, 700));
        // 组合交换链只接受 STRETCH（probe `caps.log`：SCALING_NONE 创建即 0x887A0001）。
        assert_eq!(desc.Scaling, DXGI_SCALING_STRETCH);
        assert_eq!(desc.SwapEffect, DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL);
        assert_eq!(desc.BufferCount, BUFFER_COUNT);
        assert!(desc.BufferCount >= 2);
        assert_eq!(desc.Width, 1000);
        assert_eq!(desc.Height, 700);
        // sRGB flip swapchain 在本机创建即失败（DEVIATIONS F-80）：格式固定 UNORM。
        assert_eq!(desc.Format, DXGI_FORMAT_B8G8R8A8_UNORM);
        assert_eq!(desc.SampleDesc.Count, 1);
        assert_eq!(
            desc.AlphaMode, DXGI_ALPHA_MODE_IGNORE,
            "组合交换链上没有可用的 SetBackgroundColor，窗口不透明"
        );
        assert_eq!(
            desc.Flags, DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32,
            "waitable 是呈现节奏的唯一原语（SetMaximumFrameLatency 需要它）"
        );
    }

    #[test]
    fn the_desc_never_asks_for_a_zero_sized_buffer() {
        let desc = composition_desc((0, 0));
        assert_eq!((desc.Width, desc.Height), (1, 1));
    }

    /// dirty 集 → `RECT` 的边界语义：半开区间换成 Win32 闭区间，顺序不变、数量不丢。
    #[test]
    fn dirty_rects_map_to_win32_rects_in_order() {
        let mut damage = DamageRects::new();
        assert!(damage.push(PhysicalRect::new(0, 0, 32, 16)));
        assert!(damage.push(PhysicalRect::new(32, 16, 64, 32)));
        let mut out = rects();
        assert_eq!(fill_rects(&damage, &mut out), 2);
        assert_eq!(
            (out[0].left, out[0].top, out[0].right, out[0].bottom),
            (0, 0, 32, 16)
        );
        assert_eq!(
            (out[1].left, out[1].top, out[1].right, out[1].bottom),
            (32, 16, 64, 32)
        );
    }

    /// 没有任何 back buffer 引用被缓存：`back_buffer()` 每帧重取，`ResizeBuffers` 的前置条件
    /// 因此天然满足（源码级防回归：`Swapchain` 里不得出现 `GetBuffer` 结果的字段）。
    #[test]
    fn the_swapchain_never_caches_a_back_buffer_reference() {
        let source = include_str!("swapchain.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap_or_default();
        assert!(!source.contains("back: ID3D12Resource"));
        assert!(!source.contains("buffers: [ID3D12Resource"));
        assert!(
            source.contains("self.raw.GetBuffer(index)"),
            "back buffer 必须在每次需要时向 DXGI 重新获取"
        );
        assert!(
            source.contains("ResizeBuffers"),
            "尺寸变化必须走 ResizeBuffers（R08-1）"
        );
    }
}
