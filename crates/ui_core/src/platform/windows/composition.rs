//! DirectComposition 合成目标：`WS_EX_NOREDIRECTIONBITMAP` 窗口的全部像素都由这里合成。
//!
//! 语义与 Chrome / Zed 相同：**内容 visual 恒 1:1，不随窗口 resize 缩放**。客户区永远显示
//! 「上一次呈现的内容 + 独立背景层」：拖动中旧内容保持左上角 1:1，露出的新区域由背景层
//! （窗口背景色）填充；新内容由 GPU 线程按新尺寸重画后 1:1 覆盖。因此没有「上一帧被拉伸」
//! 的中间态，也就没有静止时收回 1:1 的 snap / 回弹（`design/DEVIATIONS.md` §36 F-205 / S-152）。
//!
//! 视觉树（自下而上）：
//! - `root`：`SetRoot` 的容器，没有内容；
//! - `background`：内容 = **1×1** 组合交换链（纯背景色），transform = `background_scale`
//!   （中心 `(0, 0)`、`scale = client_px`，把 1×1 铺满客户区）；
//! - `content`：内容 = 真实组合交换链，**没有缩放变换**（恒等 ⇒ 1:1），clip = `content_clip`
//!   `(0, 0, content_px)`。
//!
//! 四条实测约束（probe `dpi.log` / `resize.log` / `caps.log`）：
//! 1. 组合交换链的 `ResizeBuffers` 只在**第一次 `Present` 之前**可用，之后必然 `0x887A0001`；
//!    所以 buffer 是 **grow-only 高水位**（[`buffer_size`] 按内容 + 64 余量、256 对齐取桶；
//!    GPU 侧 `grow` 再与当前 buffer 逐轴取大），只有内容越过它时才重建「持久目标 + 交换链」并
//!    把 visual 切到新交换链（`GpuRenderer::grow`）；内容静止 `SHRINK_IDLE` 后 `maybe_shrink`
//!    把高水位缩回当前内容桶（缩容先拷旧内容，F-207）。
//! 2. visual 的单位是**物理像素**（与 DPI 无关）：内容 clip 写内容坐标 `(0, 0, content)`；
//!    背景层 1×1 内容靠 `scale = client_px` 放大到客户区，两者都不再乘 1/dpi。
//! 3. 内容 visual 没有 transform，所以 clip 就是客户区里的 `(0, 0, content)`：客户区变大时
//!    露出背景层，变小时裁掉右侧/下侧。
//! 4. `WS_EX_NOREDIRECTIONBITMAP` 的客户区在第一次 `Present` 之前完全透明（不是黑色），
//!    所以内容**和背景**都必须在 `ShowWindow` 之前提交并 present 一次（见
//!    `GpuRenderer::new_for_composition`）—— 否则启动期/新区域会露出桌面。
//!
//! 线程：DComp 对象**不绑定创建线程**，但同一 visual 的并发 `Commit` 与属性写入批次归属未定义
//! （文档要求应用自己同步），所以本结构用 `Arc<Mutex<Composition>>` 把**所有** DComp 调用串行化：
//! UI 线程做 visual 几何 / 关闭，GPU 线程只在内容越过 buffer（`set_content`）、高水位缩容
//! （`maybe_shrink`）或背景色变化（`set_background_content`）时各做一次 `SetContent` + `Commit`。
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
/// 64 px 的物理余量（120 DPI 下约 51 逻辑 px）远大于一次 `WM_SIZE` 的步长；桶同时向上对齐到
/// 256，因此真实余量在 64…319 px 之间。**不再用 512**：它让 1200×925 的内容常驻 1792×1536 的
/// 缓冲（≈2.4× 面积），而缩容只回到同一个带余量的桶（DEVIATIONS F-209）。
const BUFFER_HEADROOM: u32 = 64;

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
    /// 根 visual：`SetRoot` 的容器，没有内容。
    root: Option<IDCompositionVisual>,
    /// 背景层：内容是一条 1×1 的组合交换链（纯色），由 [`background_scale`] 放大到客户区。
    background: Option<IDCompositionVisual>,
    /// 内容层：真实组合交换链，**没有缩放变换**（恒等 = 1:1）。
    content: Option<IDCompositionVisual>,
    /// 背景层唯一的变换：绕 `(0, 0)` 把 1×1 内容放大到客户区。
    background_scale: Option<IDCompositionScaleTransform>,
    /// 内容层唯一的裁剪：内容坐标里的 `(0, 0, content)`；内容 visual 无 transform，故不缩放。
    content_clip: Option<IDCompositionRectangleClip>,
    /// 最近一次成功应用的 `(content, client)`（物理像素）；同值 `set_geometry` 是 no-op。
    geometry: Option<((u32, u32), (u32, u32))>,
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
    /// 为一个**尚未显示**的窗口建立合成树（`design/DEVIATIONS.md` §36 F-202）。
    ///
    /// 本方法不设置裁剪边界 / 背景缩放（`CreateRectangleClip` 的初始边界没有语义保证）：
    /// 调用方必须在 `ShowWindow` 之前先挂上两条交换链（`set_content` / `set_background_content`，
    /// 都已 prime）并 `set_geometry` —— 几何之前内容可见范围未定义，背景层也还只是 1×1。
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
        // SAFETY: 同上的 COM 创建调用；以下对象由本结构持有到窗口销毁。
        let root =
            unsafe { device.CreateVisual() }.map_err(|error| init_error("CreateVisual", error))?;
        let background =
            unsafe { device.CreateVisual() }.map_err(|error| init_error("CreateVisual", error))?;
        let content =
            unsafe { device.CreateVisual() }.map_err(|error| init_error("CreateVisual", error))?;
        let background_scale = unsafe { device.CreateScaleTransform() }
            .map_err(|error| init_error("CreateScaleTransform", error))?;
        let content_clip = unsafe { device.CreateRectangleClip() }
            .map_err(|error| init_error("CreateRectangleClip", error))?;
        // SAFETY: 六个对象都由本函数创建、尚未交给任何其它所有者；这一批属性在最后一次
        // `Commit` 落盘（中间失败时对象随 `Err` 一起释放）。
        //
        // `AddVisual(…, true, None)` 在 `referenceVisual == NULL` 时把新 child 放在**所有 sibling
        // 之下**（MSDN），所以背景先加、内容再明确插到背景之上。两者都挂好之后 `SetRoot`。
        unsafe {
            background_scale
                .SetCenterX2(0.0)
                .and_then(|()| background_scale.SetCenterY2(0.0))
                .and_then(|()| background.SetTransform(&background_scale))
                .and_then(|()| {
                    background
                        .SetBitmapInterpolationMode(DCOMPOSITION_BITMAP_INTERPOLATION_MODE_LINEAR)
                })
                .and_then(|()| content.SetClip(&content_clip))
                .and_then(|()| {
                    content
                        .SetBitmapInterpolationMode(DCOMPOSITION_BITMAP_INTERPOLATION_MODE_LINEAR)
                })
                .and_then(|()| root.AddVisual(&background, true, None::<&IDCompositionVisual>))
                .and_then(|()| root.AddVisual(&content, true, Some(&background)))
                .and_then(|()| target.SetRoot(&root))
                .and_then(|()| device.Commit())
        }
        .map_err(|error| init_error("initial visual tree", error))?;
        Ok(Self {
            hwnd,
            device: Some(device),
            target: Some(target),
            root: Some(root),
            background: Some(background),
            content: Some(content),
            background_scale: Some(background_scale),
            content_clip: Some(content_clip),
            geometry: None,
        })
    }

    /// HWND 的 `isize` 形式（GPU 线程 `PostMessageW` 的目标）。
    pub(crate) fn hwnd_raw(&self) -> isize {
        self.hwnd.0 as isize
    }

    /// 把内容交换链挂到**内容 visual** 上（恒 1:1，不缩放）。
    ///
    /// 调用点：首帧 setup（UI 线程、`ShowWindow` 之前）与 `GpuRenderer::grow`（GPU 线程、内容
    /// 越过 buffer 时；旧的交换链在 `Commit` 之后才丢弃）。
    pub(crate) fn set_content(&self, swapchain: &IDXGISwapChain1) -> Result<(), EngineError> {
        self.set_visual_content(self.content.as_ref(), "Visual::SetContent", swapchain)
    }

    /// 把 1×1 的背景交换链挂到**背景 visual** 上（由 `background_scale` 放大到客户区；
    /// DEVIATIONS §36 F-205 / S-152）。
    ///
    /// 调用点：首帧 setup 与背景色变化（`GpuRenderer::refresh_background`）。
    pub(crate) fn set_background_content(
        &self,
        swapchain: &IDXGISwapChain1,
    ) -> Result<(), EngineError> {
        self.set_visual_content(
            self.background.as_ref(),
            "BackgroundVisual::SetContent",
            swapchain,
        )
    }

    fn set_visual_content(
        &self,
        visual: Option<&IDCompositionVisual>,
        stage: &'static str,
        swapchain: &IDXGISwapChain1,
    ) -> Result<(), EngineError> {
        let visual = visual.ok_or_else(|| EngineError::Composition {
            detail: smol_str::format_smolstr!("{stage} after shutdown"),
        })?;
        // SAFETY: visual 由本结构持有；swapchain 属于 `GpuRenderer`，调用方保证它在本次
        // `Commit` 之前不释放。
        unsafe { visual.SetContent(swapchain) }.map_err(|error| runtime_error(stage, error))
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

    /// 按「有内容的尺寸 / 客户区尺寸」重设内容裁剪与背景缩放（同值 no-op；DEVIATIONS §36 F-205）。
    ///
    /// 内容 visual **不做任何缩放**：`content` 是上一帧的实际尺寸，clip 到 `(0, 0, content)`；
    /// 客户区比它大时露出背景层，比它小时裁掉右侧/下侧。背景层是 1×1 的纯色交换链，所以
    /// `scale = client` 恰好把它铺满客户区，和内容尺寸无关。
    pub(crate) fn set_geometry(
        &mut self,
        content: (u32, u32),
        client: (u32, u32),
    ) -> Result<(), EngineError> {
        let content = (content.0.max(1), content.1.max(1));
        let client = (client.0.max(1), client.1.max(1));
        if self.geometry == Some((content, client)) {
            return Ok(());
        }
        // 窗口正在销毁：没有 visual 可改（`shutdown` 之后的调用是 no-op，不是错误）。
        let (Some(background_scale), Some(content_clip), Some(device)) =
            (&self.background_scale, &self.content_clip, &self.device)
        else {
            return Ok(());
        };
        let geometry = visual_geometry(content, client);
        // SAFETY: 三个 DComp 对象的生命周期见 `new`；参数都是栈上的 f32，且一次 `Commit`
        // 把整批属性作为同一帧提交（避免「只有裁剪、没有背景缩放」的中间态）。
        unsafe {
            background_scale
                .SetScaleX2(geometry.background_scale.0)
                .and_then(|()| background_scale.SetScaleY2(geometry.background_scale.1))
                .and_then(|()| content_clip.SetLeft2(0.0))
                .and_then(|()| content_clip.SetTop2(0.0))
                .and_then(|()| content_clip.SetRight2(geometry.content_clip.0))
                .and_then(|()| content_clip.SetBottom2(geometry.content_clip.1))
                .and_then(|()| device.Commit())
        }
        .map_err(|error| runtime_error("set_geometry", error))?;
        self.geometry = Some((content, client));
        Ok(())
    }

    /// 把合成树从窗口上摘下来并释放全部 DComp 对象（`NativeWindow::destroy` 在**销毁 HWND
    /// 之前**调用）。
    ///
    /// 顺序：`SetRoot(NULL)` + `Commit` 让 DWM 不再把这棵树关联到窗口；随后在 HWND 仍然
    /// 存活时按依赖顺序释放 root → 两个子 visual → transform / clip → device → target。
    /// 窗口正在销毁，失败无法补救（COM 引用仍然随字段置空释放），因此忽略 `Commit` 的错误。
    /// 重复调用是 no-op。
    pub(crate) fn shutdown(&mut self) {
        if let (Some(target), Some(device)) = (&self.target, &self.device) {
            // SAFETY: target / device 由本结构持有到本次调用；窗口仍然存活。
            unsafe {
                let _ = target.SetRoot(None::<&IDCompositionVisual>);
                let _ = device.Commit();
            }
        }
        // 依赖顺序：root 引用 background / content，content 引用 content_clip，background 引用
        // background_scale；`Option` 逐个置空即释放。
        self.root = None;
        self.background = None;
        self.content = None;
        self.background_scale = None;
        self.content_clip = None;
        self.device = None;
        self.target = None;
        self.geometry = None;
    }
}

impl Drop for Composition {
    fn drop(&mut self) {
        // 字段的默认 drop 顺序是声明顺序：device 会先于 target / visual 释放。统一走
        // `shutdown`（先 `SetRoot(NULL)`，再按依赖顺序置空）；窗口销毁路径已经显式调过
        // `shutdown`，这里是 no-op。
        self.shutdown();
    }
}

/// 纯函数：由内容尺寸与客户区尺寸算两层 visual 的几何（DEVIATIONS §36 F-205；probe `dpi.log`
/// 21–25 是旧 `scale = client/content` 的历史数据）。
///
/// - `content_clip`：内容坐标里的 `(right, bottom)`，与内容尺寸相等；内容 visual 没有缩放
///   变换，所以它永远是 1:1（**不是** `client / content`）。
/// - `background_scale`：背景层是 1×1 纯色内容，放大到客户区就是 `client` 像素。
#[derive(Clone, Copy, Debug, PartialEq)]
struct VisualGeometry {
    content_clip: (f32, f32),
    background_scale: (f32, f32),
}

fn visual_geometry(content: (u32, u32), client: (u32, u32)) -> VisualGeometry {
    let content = (content.0.max(1), content.1.max(1));
    let client = (client.0.max(1), client.1.max(1));
    VisualGeometry {
        content_clip: (content.0 as f32, content.1 as f32),
        background_scale: (client.0 as f32, client.1 as f32),
    }
}

/// 组合交换链 / 持久目标的 buffer 尺寸（内容 + 余量、256 对齐的桶）。
///
/// `round_up_256(content + 64)`，至多 [`MAX_BUFFER`]：64 px 余量吸收拖动中的小幅增长，256 对齐
/// 把桶限制在有限档位。**不再读虚拟屏幕** —— 旧的固定 buffer 在 6000×3840 的桌面上每个窗口
/// 白占 ~352 MiB，且显示配置变大后内容必然越界、无法恢复。
///
/// GPU 侧的 `grow` 会把这个桶与当前 buffer **逐轴取大**（历史高水位）：`content + 64` 只是
/// 「当前内容需要的桶」，不是常驻 VRAM 的精确值。内容静止 `SHRINK_IDLE` 后
/// `GpuRenderer::maybe_shrink` 把高水位缩回当前内容桶（新目标先 clear、再拷旧内容，然后才切
/// visual；F-207），所以 VRAM 会回落而不是永久占用。grow 的逐轴取大还有一个正确性原因：视觉
/// 切换那一帧的旧 clip 可能仍指向旧内容，新交换链必须至少覆盖旧 buffer，否则会露出未定义像素
/// （P1-A）。
///
/// # Errors
/// `EngineError::Swapchain`：内容的任一边 > [`MAX_BUFFER`]（DXGI 纹理上限），显式失败而不是
/// 静默裁剪（裁掉的右下角会露出背景层）。
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
    use super::{VisualGeometry, buffer_size, visual_geometry};
    use crate::error::EngineError;

    #[test]
    fn matching_sizes_are_a_pixel_exact_one_to_one() {
        let geometry = visual_geometry((800, 600), (800, 600));
        assert_eq!(
            geometry.content_clip,
            (800.0, 600.0),
            "内容 clip = 内容尺寸；内容 visual 无缩放 ⇒ 1:1，不引入重采样"
        );
        assert_eq!(
            geometry.background_scale,
            (800.0, 600.0),
            "1×1 背景层按客户区放大"
        );
    }

    #[test]
    fn growing_the_client_never_scales_the_content_and_only_scales_the_background() {
        // probe `dpi.log` 21 的旧语义是 client/content = 1.25 的拉伸；新语义必须是 Chrome 式
        // 1:1 + 背景层。
        let geometry = visual_geometry((800, 600), (1000, 700));
        assert_eq!(
            geometry.content_clip,
            (800.0, 600.0),
            "内容矩形不随客户区变化：grow 不缩放旧内容"
        );
        assert_eq!(
            geometry.background_scale,
            (1000.0, 700.0),
            "只有背景层被放大到客户区"
        );
        // 与静止状态相比，内容几何完全不变（内容 visual 的 scale 恒为 1,1）。
        assert_eq!(
            geometry.content_clip,
            visual_geometry((800, 600), (800, 600)).content_clip
        );
        assert_ne!(
            geometry.background_scale,
            visual_geometry((800, 600), (800, 600)).background_scale
        );
    }

    #[test]
    fn shrinking_the_client_clips_the_content_without_rescaling_it() {
        let geometry = visual_geometry((800, 600), (400, 300));
        assert_eq!(
            geometry.content_clip,
            (800.0, 600.0),
            "内容不被压缩：只由 clip 裁掉右侧/下侧"
        );
        assert_eq!(
            geometry.background_scale,
            (400.0, 300.0),
            "背景层仍然铺满（变小后的）客户区"
        );
    }

    #[test]
    fn zero_sizes_are_clamped_to_one_pixel() {
        // 最小化时客户区是 0x0；0 的 scale 会让背景层消失，直接 clamp 掉。
        let geometry = visual_geometry((0, 0), (0, 0));
        assert_eq!(
            geometry,
            VisualGeometry {
                content_clip: (1.0, 1.0),
                background_scale: (1.0, 1.0),
            }
        );
    }

    #[test]
    fn the_geometry_is_deterministic_so_reapplying_the_same_sizes_is_a_no_op() {
        // `Composition::set_geometry` 以 `(content, client)` 为缓存键：同值直接跳过 DComp 调用。
        // 纯函数必须对同一组输入给出逐位相同的几何，否则缓存会吞掉一次真实更新。
        let first = visual_geometry((800, 600), (1000, 700));
        assert_eq!(first, visual_geometry((800, 600), (1000, 700)));
        // 任一轴变化都必须产生不同的几何（缓存键的每个分量都参与几何）。
        assert_ne!(first, visual_geometry((801, 600), (1000, 700)));
        assert_ne!(first, visual_geometry((800, 600), (1001, 700)));
    }

    #[test]
    fn the_buffer_is_content_plus_headroom_rounded_up_to_256() {
        // 800×600 的内容 ⇒ 1024×768（64 px 余量后向上取整到 256 网格）。
        assert_eq!(buffer_size((800, 600)).expect("fits"), (1024, 768));
        // 精确落在 256 网格上的内容 + 余量：768+64 = 832 → 1024、512+64 = 576 → 768。
        assert_eq!(buffer_size((768, 512)).expect("fits"), (1024, 768));
        // 空内容也至少有一格 256。
        assert_eq!(buffer_size((0, 0)).expect("fits"), (256, 256));
        assert_eq!(buffer_size((1, 1)).expect("fits"), (256, 256));
        // demo 的真实内容（1200×925）：桶 1280×1024，而不是旧 512 余量的 1792×1536。
        assert_eq!(buffer_size((1200, 925)).expect("fits"), (1280, 1024));
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
