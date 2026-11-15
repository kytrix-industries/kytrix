//! **组合**交换链（`CreateSwapChainForComposition`）/ `Present1` dirty rects
//! （`design/phase-08-d3d12.md` §5.3 / §6.4、`design/DEVIATIONS.md` §36 F-199）。
//!
//! 窗口路径的交换链不是 HWND swapchain：它的内容经 `IDCompositionVisual::SetContent` 交给
//! DirectComposition（`platform/windows/composition.rs`），窗口的客户区由 DComp 合成。
//!
//! 四条实测硬约束（probe `caps.log` / `present.log` / `resize.log`）：
//! ① `DXGI_SCALING_STRETCH` 是**唯一**可用的缩放模式（`SCALING_NONE` 创建即 `0x887A0001`）；
//! ② 组合交换链在**第一次 `Present` 之后不能 `ResizeBuffers`**（`0x887A0001`），所以 buffer 是
//!    grow-only 高水位桶（`platform/windows/composition.rs` 的 `buffer_size`）：内容越过它时
//!    整条交换链被替换（`render/windows/compositor.rs` 的 `GpuRenderer::grow`），内容静止 2 s
//!    后 `maybe_shrink` 换到更小的当前内容桶（先拷旧内容）；窗口 resize 本身只改 visual 几何；
//! ③ 创建后的**第一次 present 必须全量**（`DirtyRectsCount = 0`）；
//! ④ dirty rect 必须非空且完全落在 buffer 内（越界 / 负坐标都是 `DXGI_ERROR_INVALID_CALL`）。
//!
//! 呈现节奏来自 `DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT` 的 waitable 对象
//! （`design/async-render-pipeline.md` §6.2）：`SetMaximumFrameLatency(1)` + 每帧渲染前等一次
//! 该句柄。`SetBackgroundColor` 在组合交换链上是**惰性**的（probe：返回 OK 但不影响合成），
//! 因此背景色只走 persist 的 clear（`render/color.rs`）。
//!
//! 格式：`B8G8R8A8_UNORM`（sRGB flip swapchain 在本机创建即失败，DEVIATIONS F-80）。

use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, RECT},
        Graphics::{
            Direct3D12::*,
            Dxgi::{
                Common::{DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC},
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

/// 一条组合交换链 + 它的帧延迟 waitable（auto-reset，每帧等一次）。
pub(crate) struct Swapchain {
    raw: IDXGISwapChain1,
    /// `GetFrameLatencyWaitableObject` 的句柄；由 swapchain 拥有，本结构 `Drop` 时关闭。
    waitable: HANDLE,
    /// 首个 present 必须是全量（`DirtyRectsCount = 0`）：back buffer 的内容在第一次 present
    /// 之前未定义，给局部矩形会让其余区域露出垃圾（F-72 ①）。成功后清除。
    first_present: bool,
}

impl Swapchain {
    /// `buffer` 是这条交换链的尺寸（高水位桶，物理像素）。
    ///
    /// 不能再 `ResizeBuffers`：内容越过 `buffer` 时调用方整条替换交换链（新的
    /// [`Swapchain::for_composition`]）；内容静止 2 s 后缩容也会替换成更小的桶（先拷旧内容），
    /// 旧链都在新 `SetContent` + `Commit` 之后丢弃。
    pub(crate) fn for_composition(queue: &Queue, buffer: (u32, u32)) -> Result<Self, EngineError> {
        let factory = create_factory()?;
        let desc = composition_desc(buffer);
        // SAFETY: COM 调用：工厂 / 队列 / 描述符在调用期间有效；`NULL` output 表示不限制到
        // 某个显示器；失败按 HRESULT 处理。
        let raw: IDXGISwapChain1 =
            unsafe { factory.CreateSwapChainForComposition(queue.raw(), &desc, None) }.map_err(
                |error| EngineError::Swapchain {
                    detail: smol_str::format_smolstr!(
                        "CreateSwapChainForComposition({}x{}): {:#010X}",
                        desc.Width,
                        desc.Height,
                        error.code().0 as u32
                    ),
                },
            )?;
        let chain2: IDXGISwapChain2 = raw
            .cast()
            .map_err(|error| init_error_from("cast IDXGISwapChain2", error))?;
        // flag 是创建参数（probe：没有它 `SetMaximumFrameLatency` 直接失败）：CPU 侧排队深度 1。
        // SAFETY: swapchain 句柄由 `raw` 持有；参数是值。
        unsafe { chain2.SetMaximumFrameLatency(1) }
            .map_err(|error| init_error_from("SetMaximumFrameLatency", error))?;
        // `chain2` 只是访问 `IDXGISwapChain2` 的临时引用：waitable 由 swapchain 本体拥有，
        // 句柄在 `raw` 存活期间有效，因此这里之后不再需要保存接口指针。
        let waitable = frame_latency_waitable(&chain2)?;
        Ok(Self {
            raw,
            waitable,
            first_present: true,
        })
    }

    /// 帧延迟 waitable（GPU 线程在每帧渲染前等它一次，§6.3 ①）。
    pub(crate) fn waitable(&self) -> HANDLE {
        self.waitable
    }

    /// 组合交换链本体（`IDCompositionVisual::SetContent` 的输入）。
    pub(crate) fn raw(&self) -> &IDXGISwapChain1 {
        &self.raw
    }

    /// 当前 back buffer（**每帧重新问 DXGI**；flip model 的 current index 会随 present 轮转）。
    ///
    /// 调用方只在录制 copy 期间持有，**不得**跨帧保留。
    pub(crate) fn back_buffer(&self) -> Result<ID3D12Resource, EngineError> {
        // flip model 的 current index 在 `IDXGISwapChain3` 上。
        let chain: IDXGISwapChain3 = self
            .raw
            .cast()
            .map_err(|error| init_error_from("cast IDXGISwapChain3", error))?;
        // SAFETY: swapchain 句柄由本结构持有；查询无副作用。
        let index = unsafe { chain.GetCurrentBackBufferIndex() };
        // SAFETY: COM 调用：swapchain 句柄有效，输出指针在调用期间存活，失败按 HRESULT 处理。
        unsafe { self.raw.GetBuffer(index) }.map_err(|error| EngineError::Swapchain {
            detail: smol_str::format_smolstr!(
                "GetBuffer({index}): {:#010X}",
                error.code().0 as u32
            ),
        })
    }

    /// `Present1`：dirty 集由 [`frame_dirty_rects`] 决定（首次全量、全量帧只给内容矩形）。
    ///
    /// **成功之后**才清除 `first_present` / `FramePlan` 的 fresh：失败时下一帧仍按首帧全量处理。
    pub(crate) fn present1(
        &mut self,
        frame: &mut FramePlan,
        sync: u32,
        content: (u32, u32),
    ) -> Result<(), EngineError> {
        let mut rects = [RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        }; MAX_DAMAGE_RECTS];
        let count = frame_dirty_rects(self.first_present, *frame, content, &mut rects);
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
        if hr.is_ok() {
            frame.presented();
            self.first_present = false;
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
}

impl Drop for Swapchain {
    fn drop(&mut self) {
        // SAFETY: waitable 由本结构独占；`Drop` 只执行一次（swapchain 不重建，句柄不会变）。
        unsafe {
            let _ = CloseHandle(self.waitable);
        }
    }
}

/// 本帧的 `Present1` dirty 集（返回矩形数；`0` = 整面）。
///
/// 首次 present 必须整面（约束 ③）；之后的全量帧只呈现**内容子矩形** —— buffer 是内容 + 余量
/// 的桶，其余部分永远不可见（也就是不该为了它做一次全 buffer 的合成）。
fn frame_dirty_rects(
    first_present: bool,
    frame: FramePlan,
    content: (u32, u32),
    out: &mut [RECT; MAX_DAMAGE_RECTS],
) -> u32 {
    if first_present {
        return 0;
    }
    if frame.is_full() {
        out[0] = RECT {
            left: 0,
            top: 0,
            right: content.0.max(1) as i32,
            bottom: content.1.max(1) as i32,
        };
        return 1;
    }
    match frame.present_dirty() {
        Some(damage) => fill_rects(&damage, out),
        // 非全量、非 fresh 的空帧不会走到 present（GPU 循环把它按 Idle 拦下）。
        None => 0,
    }
}

/// 取帧延迟 waitable；失败（NULL）按强类型 swapchain 错误上报，绝不拿 NULL 去 `INFINITE` 等待。
fn frame_latency_waitable(chain: &IDXGISwapChain2) -> Result<HANDLE, EngineError> {
    // SAFETY: swapchain 句柄由调用方持有；返回值按值取出。
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

/// `DamageRects` → `RECT`（半开区间 → Win32 的闭区间右/下边界）。
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

    use super::{BUFFER_COUNT, composition_desc, frame_dirty_rects};
    use crate::{
        math::PhysicalRect,
        render::{
            compositor::{DamageRects, FramePlan, Plan},
            damage::MAX_DAMAGE_RECTS,
        },
    };

    fn rects() -> [windows::Win32::Foundation::RECT; MAX_DAMAGE_RECTS] {
        [windows::Win32::Foundation::RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        }; MAX_DAMAGE_RECTS]
    }

    fn partial(rects: &[PhysicalRect]) -> Plan {
        let mut damage = DamageRects::new();
        for rect in rects {
            assert!(damage.push(*rect));
        }
        Plan::Partial(damage)
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

    #[test]
    fn the_first_present_is_the_whole_surface() {
        let mut out = rects();
        // 即便本帧是局部 damage，首次 present 也必须是整面（约束 ③）。
        let frame = FramePlan::new(partial(&[PhysicalRect::new(0, 0, 10, 10)]), false);
        assert_eq!(frame_dirty_rects(true, frame, (800, 600), &mut out), 0);
    }

    #[test]
    fn a_full_frame_presents_only_the_content_sub_rect() {
        let mut out = rects();
        let frame = FramePlan::new(Plan::Full, false);
        assert_eq!(frame_dirty_rects(false, frame, (800, 600), &mut out), 1);
        assert_eq!(
            (out[0].left, out[0].top, out[0].right, out[0].bottom),
            (0, 0, 800, 600),
            "buffer 比客户区大，全量帧也只呈现内容那部分"
        );
    }

    #[test]
    fn a_partial_frame_presents_its_damage_rects() {
        let mut out = rects();
        let damage = [
            PhysicalRect::new(0, 0, 32, 16),
            PhysicalRect::new(32, 16, 64, 32),
        ];
        let frame = FramePlan::new(partial(&damage), false);
        assert_eq!(frame_dirty_rects(false, frame, (800, 600), &mut out), 2);
        assert_eq!(
            (out[0].left, out[0].top, out[0].right, out[0].bottom),
            (0, 0, 32, 16)
        );
        assert_eq!(
            (out[1].left, out[1].top, out[1].right, out[1].bottom),
            (32, 16, 64, 32)
        );
    }
}
