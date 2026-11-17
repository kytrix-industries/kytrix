//! copy plan：`FramePlan` / `Plan` → back buffer 的拷贝矩形（全量 / damage 逐矩形）。
//! 自由函数由 `record` 与 `persist` 的实现体调用（零行为变更的结构拆分）。

use windows::Win32::Graphics::Direct3D12::*;

use super::{Persist, persist::transition};
use crate::{
    error::EngineError,
    render::{
        compositor::{FramePlan, Plan},
        windows::swapchain::Swapchain,
    },
};

/// 消费者：`copy_to_back`。
/// copy 到 back buffer 的范围：`fresh`（交换链刚建好）时强制整块 buffer。
///
/// 只有 D3D12 后端需要这个规则（F-121：Vulkan 的 present 目标跨帧内容无保证，每帧整面写入；
/// Metal 自 P2-6 起按 drawable 追踪落后区域，但 `fresh` 同样强制整面）。`present_dirty() == None`
/// 恰好同时覆盖 `fresh` 与 `Plan::Full`；`Idle` 必须单独先判，否则会被当成全量。
pub(super) fn copy_plan(frame: FramePlan) -> Plan {
    if frame.is_idle() {
        return Plan::Idle;
    }
    match frame.present_dirty() {
        Some(damage) => Plan::Partial(damage),
        None => Plan::Full,
    }
}

/// 消费者：`copy_to_back`。
/// 全量帧要 copy 的物理范围：`fresh`（交换链刚建好）⇒ 整块 buffer（back buffer 的内容未定义），
/// 其余全量帧 ⇒ 只有**内容**子矩形（buffer 的其余部分永远不可见）。
pub(super) fn copy_extent(frame: FramePlan, buffer: (u32, u32), content: (u32, u32)) -> (u32, u32) {
    if frame.is_fresh() { buffer } else { content }
}

/// 消费者：`FrameRecorder::record`。
/// copy 持久目标 → back buffer。
///
/// `buffer` / `content` 都是物理像素：只有 `fresh`（组合交换链刚建好、第一次 present 必须整面）
/// 才拷整块 buffer；其余全量帧只拷内容子矩形，局部帧逐 damage 矩形（永远落在内容之内）。
pub(super) fn copy_to_back(
    list: &ID3D12GraphicsCommandList,
    persist: &Persist,
    swapchain: &Swapchain,
    frame: FramePlan,
    buffer: (u32, u32),
    content: (u32, u32),
) -> Result<(), EngineError> {
    let back = swapchain.back_buffer()?;
    transition(
        list,
        &persist.texture,
        D3D12_RESOURCE_STATE_RENDER_TARGET,
        D3D12_RESOURCE_STATE_COPY_SOURCE,
    );
    transition(
        list,
        &back,
        D3D12_RESOURCE_STATE_PRESENT,
        D3D12_RESOURCE_STATE_COPY_DEST,
    );
    match copy_plan(frame) {
        // 全帧也用 `CopyTextureRegion`（与局部同一路径，逐矩形）：
        // swapchain back buffer 上 `CopyResource` 在 flip model 下会被拒绝（本机实测 DEVICE_REMOVED）。
        Plan::Full => {
            let whole = copy_extent(frame, buffer, content);
            let whole = crate::math::PhysicalRect::new(0, 0, whole.0 as i32, whole.1 as i32);
            copy_rect(list, &persist.texture, &back, &whole);
        }
        Plan::Partial(damage) => {
            for rect in damage.as_slice() {
                copy_rect(list, &persist.texture, &back, rect);
            }
        }
        Plan::Idle => {}
    }
    transition(
        list,
        &persist.texture,
        D3D12_RESOURCE_STATE_COPY_SOURCE,
        D3D12_RESOURCE_STATE_RENDER_TARGET,
    );
    transition(
        list,
        &back,
        D3D12_RESOURCE_STATE_COPY_DEST,
        D3D12_RESOURCE_STATE_PRESENT,
    );
    Ok(())
}

/// 消费者：`FrameRecorder::record`。
/// 缩容 prime 的内容保留：把 `source` 的 `(0, 0, content)` 拷到 `destination` 的左上角。
///
/// 两块持久目标在这一步之前都处于 `RENDER_TARGET`（旧目标刚被 wait_idle，新目标刚 clear），
/// 这里临时过渡到拷贝状态并在返回前转回：后续 `bind_frame` / `copy_to_back` 仍按「持久目标
/// 处于 RENDER_TARGET」记账。`destination` 的其余像素保持 clear 出来的背景色，永远不可见
/// （visual 的 clip 只到内容）。
pub(super) fn copy_content(
    list: &ID3D12GraphicsCommandList,
    source: &Persist,
    destination: &Persist,
    content: (u32, u32),
) {
    transition(
        list,
        &source.texture,
        D3D12_RESOURCE_STATE_RENDER_TARGET,
        D3D12_RESOURCE_STATE_COPY_SOURCE,
    );
    transition(
        list,
        &destination.texture,
        D3D12_RESOURCE_STATE_RENDER_TARGET,
        D3D12_RESOURCE_STATE_COPY_DEST,
    );
    let rect = crate::math::PhysicalRect::new(0, 0, content.0 as i32, content.1 as i32);
    copy_rect(list, &source.texture, &destination.texture, &rect);
    transition(
        list,
        &destination.texture,
        D3D12_RESOURCE_STATE_COPY_DEST,
        D3D12_RESOURCE_STATE_RENDER_TARGET,
    );
    transition(
        list,
        &source.texture,
        D3D12_RESOURCE_STATE_COPY_SOURCE,
        D3D12_RESOURCE_STATE_RENDER_TARGET,
    );
}

/// 消费者：`copy_to_back` / `copy_content`。
pub(super) fn copy_rect(
    list: &ID3D12GraphicsCommandList,
    source: &ID3D12Resource,
    destination: &ID3D12Resource,
    rect: &crate::math::PhysicalRect,
) {
    let mut destination_location = D3D12_TEXTURE_COPY_LOCATION {
        pResource: core::mem::ManuallyDrop::new(Some(destination.clone())),
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            SubresourceIndex: 0,
        },
    };
    let mut source_location = D3D12_TEXTURE_COPY_LOCATION {
        pResource: core::mem::ManuallyDrop::new(Some(source.clone())),
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            SubresourceIndex: 0,
        },
    };
    let region = D3D12_BOX {
        left: rect.x0 as u32,
        top: rect.y0 as u32,
        front: 0,
        right: rect.x1 as u32,
        bottom: rect.y1 as u32,
        back: 1,
    };
    // SAFETY: 两个 location 在本次调用期间有效，`region` 的边界由调用方夹紧到 surface 内。
    unsafe {
        list.CopyTextureRegion(
            &destination_location,
            rect.x0 as u32,
            rect.y0 as u32,
            0,
            &source_location,
            Some(&region),
        )
    };
    // SAFETY: `pResource` 是 `ManuallyDrop`，结构体 drop 不释放 COM 引用；不显式释放会让
    // swapchain back buffer 永远存活（DEVIATIONS F-86）。
    unsafe {
        core::mem::ManuallyDrop::drop(&mut destination_location.pResource);
        core::mem::ManuallyDrop::drop(&mut source_location.pResource);
    }
}
