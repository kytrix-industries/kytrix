use std::sync::{
    Mutex, MutexGuard,
    atomic::{AtomicU64, Ordering},
};

use windows::{
    Win32::{
        Foundation::{HWND, RECT},
        Graphics::{
            DirectComposition::{
                DCOMPOSITION_BITMAP_INTERPOLATION_MODE_LINEAR, DCompositionCreateDevice2,
                IDCompositionDevice, IDCompositionRectangleClip, IDCompositionScaleTransform,
                IDCompositionTarget, IDCompositionVisual,
            },
            Dxgi::IDXGISwapChain1,
        },
        UI::WindowsAndMessaging::{GetClientRect, IsWindow},
    },
    core::{BOOL, IUnknown},
};

use crate::error::EngineError;

static DEVICE_CHECK_EPOCH: AtomicU64 = AtomicU64::new(0);

pub(crate) fn note_device_check() {
    DEVICE_CHECK_EPOCH.fetch_add(1, Ordering::AcqRel);
}

pub(crate) fn device_check_epoch() -> u64 {
    DEVICE_CHECK_EPOCH.load(Ordering::Acquire)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum CompositionState {
    #[default]
    Valid,
    Invalid,
    WindowClosed,
}

const MAX_BUFFER: u32 = 16384;

const BUFFER_HEADROOM: u32 = 64;

const BUFFER_ALIGN: u32 = 256;

pub(crate) struct Composition {
    hwnd: HWND,
    device: Option<IDCompositionDevice>,
    target: Option<IDCompositionTarget>,
    root: Option<IDCompositionVisual>,
    background: Option<IDCompositionVisual>,
    content: Option<IDCompositionVisual>,
    background_scale: Option<IDCompositionScaleTransform>,
    content_clip: Option<IDCompositionRectangleClip>,
    geometry: Option<((u32, u32), (u32, u32))>,
}

unsafe impl Send for Composition {}

pub(crate) fn lock(composition: &Mutex<Composition>) -> MutexGuard<'_, Composition> {
    composition
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Composition {
    pub(crate) fn new(hwnd: HWND) -> Result<Self, EngineError> {
        let device: IDCompositionDevice =
            unsafe { DCompositionCreateDevice2(None::<&IUnknown>) }
                .map_err(|error| init_error("DCompositionCreateDevice2", error))?;
        let target = unsafe { device.CreateTargetForHwnd(hwnd, false) }
            .map_err(|error| init_error("CreateTargetForHwnd", error))?;
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

    pub(crate) fn hwnd_raw(&self) -> isize {
        self.hwnd.0 as isize
    }

    pub(crate) fn check_device_state(&self) -> CompositionState {
        let Some(device) = &self.device else {
            return CompositionState::WindowClosed;
        };
        let result = unsafe { device.CheckDeviceState() };
        classify_device_state(result, self.window_alive())
    }

    fn window_alive(&self) -> bool {
        self.hwnd != HWND::default() && unsafe { IsWindow(Some(self.hwnd)).as_bool() }
    }

    pub(crate) fn client_size(&self) -> (u32, u32) {
        let mut rect = RECT::default();
        if self.hwnd == HWND::default() || unsafe { GetClientRect(self.hwnd, &mut rect) }.is_err() {
            return (1, 1);
        }
        (
            (rect.right - rect.left).max(1) as u32,
            (rect.bottom - rect.top).max(1) as u32,
        )
    }

    pub(crate) fn rebuild(&mut self) -> Result<(), EngineError> {
        let hwnd = self.hwnd;
        self.shutdown();
        *self = Self::new(hwnd)?;
        Ok(())
    }

    pub(crate) fn set_content(&self, swapchain: &IDXGISwapChain1) -> Result<(), EngineError> {
        self.set_visual_content(self.content.as_ref(), "Visual::SetContent", swapchain)
    }

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
        unsafe { visual.SetContent(swapchain) }.map_err(|error| runtime_error(stage, error))
    }

    pub(crate) fn commit(&self) -> Result<(), EngineError> {
        let device = self
            .device
            .as_ref()
            .ok_or_else(|| EngineError::Composition {
                detail: smol_str::SmolStr::new_static("Device::Commit after shutdown"),
            })?;
        unsafe { device.Commit() }.map_err(|error| runtime_error("Device::Commit", error))
    }

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
        let (Some(background_scale), Some(content_clip), Some(device)) =
            (&self.background_scale, &self.content_clip, &self.device)
        else {
            return Ok(());
        };
        let geometry = visual_geometry(content, client);
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

    pub(crate) fn shutdown(&mut self) {
        if let (Some(target), Some(device)) = (&self.target, &self.device) {
            unsafe {
                let _ = target.SetRoot(None::<&IDCompositionVisual>);
                let _ = device.Commit();
            }
        }
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
        self.shutdown();
    }
}

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

pub(crate) fn buffer_size(content: (u32, u32)) -> Result<(u32, u32), EngineError> {
    Ok((
        buffer_extent("width", content.0)?,
        buffer_extent("height", content.1)?,
    ))
}

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

fn classify_device_state(
    result: windows::core::Result<BOOL>,
    window_alive: bool,
) -> CompositionState {
    if !window_alive {
        return CompositionState::WindowClosed;
    }
    match result {
        Ok(valid) if valid.as_bool() => CompositionState::Valid,
        Ok(_) | Err(_) => CompositionState::Invalid,
    }
}

#[cfg(test)]
mod tests {
    use windows::{
        Win32::{
            Foundation::{HWND, LPARAM, LRESULT, WPARAM},
            System::LibraryLoader::GetModuleHandleW,
            UI::WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW, WINDOW_EX_STYLE,
                WNDCLASSW, WS_OVERLAPPEDWINDOW,
            },
        },
        core::{BOOL, PCWSTR},
    };

    use super::{
        Composition, CompositionState, VisualGeometry, buffer_size, classify_device_state,
        visual_geometry,
    };
    use crate::error::EngineError;

    #[test]
    fn device_state_is_classified_without_guessing_the_undocumented_error_code() {
        assert_eq!(
            classify_device_state(Ok(BOOL(1)), true),
            CompositionState::Valid
        );
        assert_eq!(
            classify_device_state(Ok(BOOL(0)), true),
            CompositionState::Invalid,
            "设备自报无效 ⇒ 可重建"
        );
        assert_eq!(
            classify_device_state(
                Err(windows::core::Error::from_hresult(windows::core::HRESULT(
                    0x8899_0000u32 as i32
                ))),
                true
            ),
            CompositionState::Invalid,
            "窗口仍存活但检查报错 ⇒ 可重建（不猜具体 DComp 错误码）"
        );
        assert_eq!(
            classify_device_state(Ok(BOOL(1)), false),
            CompositionState::WindowClosed,
            "窗口已销毁：恢复没有意义"
        );
        assert_eq!(
            classify_device_state(Ok(BOOL(0)), false),
            CompositionState::WindowClosed
        );
        assert_eq!(
            classify_device_state(
                Err(windows::core::Error::from_hresult(windows::core::HRESULT(
                    0x8899_0000u32 as i32
                ))),
                false
            ),
            CompositionState::WindowClosed
        );
    }

    unsafe extern "system" fn default_window_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn hidden_window(tag: &str) -> (HWND, Vec<u16>) {
        let class_name = wide(tag);
        let instance = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
        let class = WNDCLASSW {
            lpfnWndProc: Some(default_window_proc),
            hInstance: instance.into(),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        let atom = unsafe { RegisterClassW(&class) };
        assert_ne!(atom, 0, "RegisterClassW");
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR(class_name.as_ptr()),
                PCWSTR(class_name.as_ptr()),
                WS_OVERLAPPEDWINDOW,
                0,
                0,
                320,
                240,
                None,
                None,
                Some(instance.into()),
                None,
            )
        }
        .expect("CreateWindowExW");
        (hwnd, class_name)
    }

    #[test]
    fn the_composition_tree_can_be_rebuilt_for_a_live_window() {
        let (hwnd, _class) = hidden_window("kytrix-composition-rebuild-test");
        let mut composition = Composition::new(hwnd).expect("DCompositionCreateDevice2");
        assert_eq!(
            composition.check_device_state(),
            CompositionState::Valid,
            "新建设备必须报告有效"
        );
        assert!(composition.client_size().0 >= 1 && composition.client_size().1 >= 1);

        composition.rebuild().expect("rebuild visual tree");
        assert_eq!(
            composition.check_device_state(),
            CompositionState::Valid,
            "重建后的设备必须报告有效"
        );

        composition.shutdown();
        assert_eq!(
            composition.check_device_state(),
            CompositionState::WindowClosed,
            "shutdown 之后不再有可恢复的合成树"
        );
        composition.shutdown();

        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }

    #[test]
    fn a_closed_window_is_reported_as_window_closed() {
        let (hwnd, _class) = hidden_window("kytrix-composition-closed-test");
        let composition = Composition::new(hwnd).expect("DCompositionCreateDevice2");
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
        assert_eq!(
            composition.check_device_state(),
            CompositionState::WindowClosed,
            "窗口销毁后检查必须归为退出路径"
        );
    }

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
        let first = visual_geometry((800, 600), (1000, 700));
        assert_eq!(first, visual_geometry((800, 600), (1000, 700)));
        assert_ne!(first, visual_geometry((801, 600), (1000, 700)));
        assert_ne!(first, visual_geometry((800, 600), (1001, 700)));
    }

    #[test]
    fn the_buffer_is_content_plus_headroom_rounded_up_to_256() {
        assert_eq!(buffer_size((800, 600)).expect("fits"), (1024, 768));
        assert_eq!(buffer_size((768, 512)).expect("fits"), (1024, 768));
        assert_eq!(buffer_size((0, 0)).expect("fits"), (256, 256));
        assert_eq!(buffer_size((1, 1)).expect("fits"), (256, 256));
        assert_eq!(buffer_size((1200, 925)).expect("fits"), (1280, 1024));
        assert_eq!(
            buffer_size((800, 600)).expect("fits"),
            buffer_size((800, 600)).expect("fits")
        );
    }

    #[test]
    fn the_buffer_caps_at_16384_and_oversized_content_is_an_error() {
        assert_eq!(
            buffer_size((16384, 16384)).expect("the cap itself fits"),
            (16384, 16384)
        );
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
