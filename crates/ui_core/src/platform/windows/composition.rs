//! DirectComposition 合成目标：`WS_EX_NOREDIRECTIONBITMAP` 窗口的全部像素都由这里合成。
//!
//! 为什么是「组合交换链 + visual 变换」：窗口 resize 时 DWM 直接按新客户区拉伸 visual
//! 里的**上一帧内容**（一次 `SetScale` + `Commit`，不等 GPU、不重建交换链），新内容由 GPU
//! 线程按新尺寸重画后 1:1 覆盖。内容因此不再追着窗口边缘跑，缩小/恢复也没有先黑一帧的空窗。
//!
//! 四条实测约束（probe `dpi.log` / `resize.log` / `caps.log`）：
//! 1. 组合交换链的 `ResizeBuffers` 只在**第一次 `Present` 之前**可用，之后必然 `0x887A0001`；
//!    所以 buffer 是 **grow-only 高水位**（[`buffer_size`] 按内容 + 512 余量、256 对齐取桶；
//!    GPU 侧 `grow` 再与当前 buffer 逐轴取大），只有内容越过它时才重建「持久目标 + 交换链」并
//!    把 visual 切到新交换链（`GpuRenderer::grow`）。
//! 2. visual 的单位是**物理像素**（与 DPI 无关）：`scale = client_px / content_px`，不能再乘 1/dpi。
//! 3. clip 会被 visual 的 scale 变换：clip 写**内容坐标** `(0, 0, content)` 即可盖满客户区。
//! 4. `WS_EX_NOREDIRECTIONBITMAP` 的客户区在第一次 `Present` 之前完全透明（不是黑色），
//!    所以内容必须在 `ShowWindow` 之前提交并 present 一次（见 `GpuRenderer::new_for_composition`）。
//!
//! 线程：DComp 对象**不绑定创建线程**，但同一 visual 的并发 `Commit` 与属性写入批次归属未定义
//! （文档要求应用自己同步），所以本结构用 `Arc<Mutex<Composition>>` 把**所有** DComp 调用串行化：
//! UI 线程做 visual 几何 / 关闭，GPU 线程只在内容越过 buffer 时做一次 `SetContent` + `Commit`。
//!
//! 两个错误面：[`Composition::new`] 的 COM 创建阶段失败是 [`EngineError::AppInit`]（窗口还建不
//! 出来）；**运行时**调用（几何 / 提交 / 交换链切换，含首帧 `SetContent` + `Commit`）失败是
//! [`EngineError::Composition`]，并带失败操作名。

use std::sync::{Mutex, MutexGuard};

use windows::{
    Win32::{
        Foundation::HWND,
        Graphics::{
            DirectComposition::{
                DCOMPOSITION_BITMAP_INTERPOLATION_MODE_LINEAR, DCompositionCreateDevice2,
                IDCompositionDevice, IDCompositionRectangleClip, IDCompositionScaleTransform,
                IDCompositionTarget, IDCompositionVisual,
            },
            Dxgi::IDXGISwapChain1,
        },
    },
    core::IUnknown,
};

use crate::error::EngineError;

/// buffer 的硬上限（物理像素；DXGI 纹理上限）。内容超过它显式失败，不静默裁剪。
const MAX_BUFFER: u32 = 16384;

/// 内容之外的余量：吸收拖动中的小幅增长，避免每越过一像素就重建一次交换链。
///
/// 它同时把 buffer 的下限抬到 512 px（≥ 需求的 256 px）。
const BUFFER_HEADROOM: u32 = 512;

/// buffer 的对齐粒度：把桶限制在有限档位（也符合 DComp / DXGI 的 256 对齐习惯）。
const BUFFER_ALIGN: u32 = 256;

/// 一个窗口的 DirectComposition 合成树。
///
/// 由 `NativeWindow` 与窗口的 `GpuRenderer` 通过 `Arc<Mutex<Composition>>` 共享；所有 DComp
/// 调用的串行化见模块文档。`shutdown` 之后所有 COM 字段为 `None`。
pub(crate) struct Composition {
    hwnd: HWND,
    /// `shutdown` 之后为 `None`：全部 COM 引用在 HWND 销毁**之前**释放。
    device: Option<IDCompositionDevice>,
    target: Option<IDCompositionTarget>,
    visual: Option<IDCompositionVisual>,
    /// 唯一的变换：绕 `(0, 0)` 的缩放；静止时 `(1, 1)`。
    scale: Option<IDCompositionScaleTransform>,
    /// 唯一的裁剪：内容坐标里的 `(0, 0, content)`；随 `scale` 一起变换。
    clip: Option<IDCompositionRectangleClip>,
    /// 最近一次 `set_geometry` 的内容尺寸（物理像素）。
    content: (u32, u32),
    /// 最近一次 `set_geometry` 的客户区尺寸（物理像素）。
    client: (u32, u32),
}

// SAFETY: `Composition` 只经 `Arc<Mutex<Composition>>` 共享，且下面每个方法都要求调用方先持锁
// （[`lock`]），因此同一时刻只有一个线程访问这些 COM 对象：没有并发 `SetContent` / 属性写入 /
// `Commit`。DirectComposition 对象不绑定创建线程（`IDCompositionDevice::CreateVisual` 等返回的
// 对象可从任意线程使用；文档只把「同一批属性 + Commit」的竞态留给应用同步，互斥锁就是那个同步）。
// `Send` 足够：`Mutex<T>: Send + Sync` 只要求 `T: Send`，而 `Arc<Mutex<T>>` 的跨线程能力由这两条
// 自动派生。HWND / `IDCompositionTarget` 的存活期由 `NativeWindow` 覆盖 `GpuRenderer`（关闭顺序：
// 先 drop `App`（join `kytrix-d3d12` 线程）再 `NativeWindow::destroy`）。
unsafe impl Send for Composition {}

/// 取合成目标的互斥锁。
///
/// 毒化不致命：所有 DComp 调用都返回 `Result`，本结构没有「失败后不可用」的跨调用不变量；
/// 与 `render/windows/renderer.rs` 的共享槽同一策略（继续使用内部值）。
pub(crate) fn lock(composition: &Mutex<Composition>) -> MutexGuard<'_, Composition> {
    composition
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Composition {
    /// 为一个**尚未显示**的窗口建立合成目标（`design/DEVIATIONS.md` §36 F-202）。
    ///
    /// 本方法不设置裁剪边界（`CreateRectangleClip` 的初始边界没有语义保证）：内容在第一次
    /// `set_geometry` 之前不应被当作可见 —— 调用方必须在 `ShowWindow` 之前设置几何。
    pub(crate) fn new(hwnd: HWND) -> Result<Self, EngineError> {
        // SAFETY: COM 创建调用：不传 rendering device（probe `caps.log`：传 D3D12 设备、或请求
        // `IDCompositionDevice2`/`IDCompositionDesktopDevice` 的 IID 都不会得到可用对象）；
        // 输出指针在调用期间存活，失败按 HRESULT 处理。
        let device: IDCompositionDevice =
            unsafe { DCompositionCreateDevice2(None::<&IUnknown>) }
                .map_err(|error| init_error("DCompositionCreateDevice2", error))?;
        // SAFETY: `hwnd` 由 `NativeWindow` 持有且晚于本结构销毁（`destroy` 先 `shutdown` 再
        // `DestroyWindow`）；`topmost = false`：本窗口没有子 HWND，resize 交给自定义边框。
        let target = unsafe { device.CreateTargetForHwnd(hwnd, false) }
            .map_err(|error| init_error("CreateTargetForHwnd", error))?;
        // SAFETY: 同上的 COM 创建调用；三个对象由本结构持有到窗口销毁。
        let visual =
            unsafe { device.CreateVisual() }.map_err(|error| init_error("CreateVisual", error))?;
        let scale = unsafe { device.CreateScaleTransform() }
            .map_err(|error| init_error("CreateScaleTransform", error))?;
        let clip = unsafe { device.CreateRectangleClip() }
            .map_err(|error| init_error("CreateRectangleClip", error))?;
        // SAFETY: 四个对象都由本函数创建、尚未交给任何其它所有者；这一批属性在最后一次
        // `Commit` 落盘（中间失败时对象随 `Err` 一起释放）。
        unsafe {
            scale
                .SetCenterX2(0.0)
                .and_then(|()| scale.SetCenterY2(0.0))
                .and_then(|()| visual.SetTransform(&scale))
                .and_then(|()| visual.SetClip(&clip))
                .and_then(|()| {
                    visual.SetBitmapInterpolationMode(DCOMPOSITION_BITMAP_INTERPOLATION_MODE_LINEAR)
                })
                .and_then(|()| target.SetRoot(&visual))
                .and_then(|()| device.Commit())
        }
        .map_err(|error| init_error("initial visual tree", error))?;
        Ok(Self {
            hwnd,
            device: Some(device),
            target: Some(target),
            visual: Some(visual),
            scale: Some(scale),
            clip: Some(clip),
            content: (0, 0),
            client: (0, 0),
        })
    }

    /// HWND 的 `isize` 形式（GPU 线程 `PostMessageW` 的目标）。
    pub(crate) fn hwnd_raw(&self) -> isize {
        self.hwnd.0 as isize
    }

    /// 把组合交换链挂到 visual 上。
    ///
    /// 调用点有两处：首帧 setup（UI 线程、`ShowWindow` 之前）与 `GpuRenderer::grow`
    /// （GPU 线程、内容越过 buffer 时；旧的交换链在 `Commit` 之后才丢弃）。
    pub(crate) fn set_content(&self, swapchain: &IDXGISwapChain1) -> Result<(), EngineError> {
        let visual = self
            .visual
            .as_ref()
            .ok_or_else(|| EngineError::Composition {
                detail: smol_str::SmolStr::new_static("Visual::SetContent after shutdown"),
            })?;
        // SAFETY: visual 由本结构持有；swapchain 属于 `GpuRenderer`，调用方保证它在本次
        // `Commit` 之前不释放。
        unsafe { visual.SetContent(swapchain) }
            .map_err(|error| runtime_error("Visual::SetContent", error))
    }

    /// 提交本批次的属性变更（`Commit` 之后才进入合成器的下一帧）。
    pub(crate) fn commit(&self) -> Result<(), EngineError> {
        let device = self
            .device
            .as_ref()
            .ok_or_else(|| EngineError::Composition {
                detail: smol_str::SmolStr::new_static("Device::Commit after shutdown"),
            })?;
        // SAFETY: device 由本结构持有到 `shutdown`。
        unsafe { device.Commit() }.map_err(|error| runtime_error("Device::Commit", error))
    }

    /// 按「有内容的尺寸 / 客户区尺寸」重设 visual 的缩放与裁剪（同值 no-op）。
    ///
    /// 拖动中 `content` 是上一帧内容（比客户区大或小）⇒ 拉伸/压缩旧内容铺满客户区，
    /// 静止时两者相等 ⇒ `(1, 1)` 的 1:1 清晰画面。
    pub(crate) fn set_geometry(
        &mut self,
        content: (u32, u32),
        client: (u32, u32),
    ) -> Result<(), EngineError> {
        let content = (content.0.max(1), content.1.max(1));
        let client = (client.0.max(1), client.1.max(1));
        if content == self.content && client == self.client {
            return Ok(());
        }
        // 窗口正在销毁：没有 visual 可改（`shutdown` 之后的调用是 no-op，不是错误）。
        let (Some(scale), Some(clip), Some(device)) = (&self.scale, &self.clip, &self.device)
        else {
            return Ok(());
        };
        let ((scale_x, scale_y), (clip_x, clip_y)) = visual_geometry(content, client);
        // SAFETY: 三个 DComp 对象的生命周期见 `new`；参数都是栈上的 f32，且一次 `Commit`
        // 把整批属性作为同一帧提交（避免「只有缩放、没有裁剪」的中间态）。
        unsafe {
            scale
                .SetScaleX2(scale_x)
                .and_then(|()| scale.SetScaleY2(scale_y))
                .and_then(|()| clip.SetLeft2(0.0))
                .and_then(|()| clip.SetTop2(0.0))
                .and_then(|()| clip.SetRight2(clip_x))
                .and_then(|()| clip.SetBottom2(clip_y))
                .and_then(|()| device.Commit())
        }
        .map_err(|error| runtime_error("set_geometry", error))?;
        self.content = content;
        self.client = client;
        Ok(())
    }

    /// 把合成树从窗口上摘下来并释放全部 DComp 对象（`NativeWindow::destroy` 在**销毁 HWND
    /// 之前**调用）。
    ///
    /// 顺序：`SetRoot(NULL)` + `Commit` 让 DWM 不再把这个 visual 关联到窗口；随后在 HWND 仍然
    /// 存活时逐个释放 visual / transform / clip / target / device。窗口正在销毁，失败无法补救
    /// （COM 引用仍然随字段置空释放），因此忽略 `Commit` 的错误。重复调用是 no-op。
    pub(crate) fn shutdown(&mut self) {
        if let (Some(target), Some(device)) = (&self.target, &self.device) {
            // SAFETY: target / device 由本结构持有到本次调用；窗口仍然存活。
            unsafe {
                let _ = target.SetRoot(None::<&IDCompositionVisual>);
                let _ = device.Commit();
            }
        }
        // 依赖顺序：target 引用 visual，visual 引用 scale / clip；`Option` 逐个置空即释放。
        self.target = None;
        self.visual = None;
        self.scale = None;
        self.clip = None;
        self.device = None;
        self.content = (0, 0);
        self.client = (0, 0);
    }
}

impl Drop for Composition {
    fn drop(&mut self) {
        // 字段的默认 drop 顺序是 device → target → visual（声明顺序）：device 会先于 target /
        // visual 释放。统一走 `shutdown`（先 `SetRoot(NULL)`，再按依赖顺序置空）；窗口销毁路径
        // 已经显式调过 `shutdown`，这里是 no-op。
        self.shutdown();
    }
}

/// 纯函数：由内容尺寸与客户区尺寸算 visual 的 `(scale, clip)`（probe `dpi.log` 21–25）。
///
/// `scale = client / content`（物理像素）；clip 是**内容坐标**里的整块内容 —— clip 会被
/// visual 的 scale 变换，所以放大、缩小都恰好盖住客户区，内容不会露出一半。
fn visual_geometry(content: (u32, u32), client: (u32, u32)) -> ((f32, f32), (f32, f32)) {
    let content = (content.0.max(1), content.1.max(1));
    let client = (client.0.max(1), client.1.max(1));
    (
        (
            client.0 as f32 / content.0 as f32,
            client.1 as f32 / content.1 as f32,
        ),
        (content.0 as f32, content.1 as f32),
    )
}

/// 组合交换链 / 持久目标的 buffer 尺寸（内容 + 余量、256 对齐的桶）。
///
/// `round_up_256(content + 512)`，至多 [`MAX_BUFFER`]：512 px 余量吸收拖动中的小幅增长（也把
/// 下限抬到 512 px），256 对齐把桶限制在有限档位。**不再读虚拟屏幕** —— 旧的固定 buffer 在
/// 6000×3840 的桌面上每个窗口白占 ~352 MiB，且显示配置变大后内容必然越界、无法恢复。
///
/// GPU 侧的 `grow` 会把这个桶与当前 buffer **逐轴取大**（grow-only 高水位）：每窗口 VRAM 的
/// 上界是历史最大桶，而不是当前内容的公式值 —— **缩小时不重建、不释放显存**，`content + 512`
/// 只是「当前内容需要的桶」，不是常驻 VRAM 的精确值。逐轴取大还有一个正确性原因：视觉切换
/// 那一帧的旧 clip 可能仍指向旧内容，新交换链必须至少覆盖旧 buffer，否则会露出未定义像素
/// （P1-A）。
///
/// # Errors
/// `EngineError::Swapchain`：内容的任一边 > [`MAX_BUFFER`]（DXGI 纹理上限），显式失败而不是
/// 静默裁剪（裁掉的右下角会被 visual 当成内容拉伸）。
pub(crate) fn buffer_size(content: (u32, u32)) -> Result<(u32, u32), EngineError> {
    Ok((
        buffer_extent("width", content.0)?,
        buffer_extent("height", content.1)?,
    ))
}

/// 单边的桶尺寸：`content` 超过上限即错误；否则 `content + headroom` 向上取整到 256，封顶 16384
/// （内容本身 ≤ 16384，因此封顶后仍然覆盖内容）。
fn buffer_extent(axis: &'static str, content: u32) -> Result<u32, EngineError> {
    if content > MAX_BUFFER {
        return Err(EngineError::Swapchain {
            detail: smol_str::format_smolstr!(
                "content {axis} {content} exceeds the DirectComposition buffer limit {MAX_BUFFER}"
            ),
        });
    }
    let headroom = content.saturating_add(BUFFER_HEADROOM);
    Ok(headroom
        .div_ceil(BUFFER_ALIGN)
        .saturating_mul(BUFFER_ALIGN)
        .min(MAX_BUFFER))
}

fn init_error(stage: &'static str, error: windows::core::Error) -> EngineError {
    EngineError::AppInit {
        detail: smol_str::format_smolstr!(
            "DirectComposition {stage}: {:#010X}",
            error.code().0 as u32
        ),
    }
}

fn runtime_error(stage: &'static str, error: windows::core::Error) -> EngineError {
    EngineError::Composition {
        detail: smol_str::format_smolstr!("{stage}: {:#010X}", error.code().0 as u32),
    }
}

#[cfg(test)]
mod tests {
    use super::{buffer_size, visual_geometry};
    use crate::error::EngineError;

    fn assert_close(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() < 1e-6,
            "{actual} should be {expected}"
        );
    }

    #[test]
    fn matching_sizes_are_a_pixel_exact_identity() {
        let (scale, clip) = visual_geometry((800, 600), (800, 600));
        assert_eq!(scale, (1.0, 1.0), "静止时 1:1，不引入重采样");
        assert_eq!(clip, (800.0, 600.0));
    }

    #[test]
    fn growing_the_client_stretches_the_last_content_to_fill_it() {
        // probe `dpi.log` 21：client 1000x700 / content 800x600 恰好铺满（不是 (1.25, 1.25)）。
        let (scale, clip) = visual_geometry((800, 600), (1000, 700));
        assert_close(scale.0, 1.25);
        assert_close(scale.1, 700.0 / 600.0);
        assert_eq!(
            clip,
            (800.0, 600.0),
            "clip 用内容坐标，由 scale 放大到客户区"
        );
    }

    #[test]
    fn shrinking_the_client_compresses_the_clip_with_the_content() {
        let (scale, clip) = visual_geometry((800, 600), (400, 300));
        assert_eq!(scale, (0.5, 0.5));
        assert_eq!(clip, (800.0, 600.0), "内容被压到 400x300，不露边界");
    }

    #[test]
    fn zero_sizes_are_clamped_to_one_pixel() {
        // 最小化时客户区是 0x0；除零会得到 inf，直接 clamp 掉。
        let (scale, clip) = visual_geometry((0, 0), (0, 0));
        assert_eq!(scale, (1.0, 1.0));
        assert_eq!(clip, (1.0, 1.0));
    }

    #[test]
    fn the_buffer_is_content_plus_headroom_rounded_up_to_256() {
        // 800×600 的内容 ⇒ 1536×1280 —— 不再是 6000×3840 的虚屏。
        assert_eq!(buffer_size((800, 600)).expect("fits"), (1536, 1280));
        // 精确落在 256 网格上的内容 + 余量：768+512 = 1280、512+512 = 1024。
        assert_eq!(buffer_size((768, 512)).expect("fits"), (1280, 1024));
        // 空内容也有 512 px（余量把下限抬到 512，向上取整到 256 网格）。
        assert_eq!(buffer_size((0, 0)).expect("fits"), (512, 512));
        assert_eq!(buffer_size((1, 1)).expect("fits"), (768, 768));
        // 同一内容必须给出同一个桶（桶是确定性的，不随时间/桌面漂移）。
        assert_eq!(
            buffer_size((800, 600)).expect("fits"),
            buffer_size((800, 600)).expect("fits")
        );
    }

    #[test]
    fn the_buffer_caps_at_16384_and_oversized_content_is_an_error() {
        // 上限本身仍然覆盖内容（内容 16384 + 余量会被封顶回 16384）。
        assert_eq!(
            buffer_size((16384, 16384)).expect("the cap itself fits"),
            (16384, 16384)
        );
        // 接近上限的内容不需要精确的 headroom，但必须仍然被覆盖。
        let near = buffer_size((16000, 9000)).expect("fits");
        assert!(near.0 >= 16000 && near.1 >= 9000, "{near:?}");
        assert!(near.0 <= 16384 && near.1 <= 16384, "{near:?}");

        match buffer_size((16385, 100)) {
            Err(EngineError::Swapchain { detail }) => {
                assert!(detail.contains("16385"), "{detail}");
                assert!(detail.contains("16384"), "{detail}");
            }
            other => panic!("unexpected result: {other:?}"),
        }
    }
}
