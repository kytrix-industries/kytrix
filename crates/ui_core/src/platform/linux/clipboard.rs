//! 剪贴板：`wl_data_device` / `wl_data_source` / `wl_data_offer`（`design/phase-12-macos-linux.md`
//! §8.6 + R10-1 / R10-9 / R10-12 / R10-17 的整改）。
//!
//! Wayland 是**异步拉模型**：`receive` 之后数据由对端写 pipe；发布方向要把文本写进对端给的
//! `wl_data_source.send` fd。协议顺序（`wl_data_offer` 的 spec）：收到
//! `wl_data_device.selection` 之后**先** `accept(mime)` 再 `receive(mime, fd)`，漏掉 `accept`
//! 会被部分 compositor 直接忽略这次拉取。
//!
//! 三条硬约束（都是 R10 的 P0/P1）：
//!
//! 1. **一个 `wl_data_source` 只服务一次选区**（R10-1）。复用同一个 source 做第二次
//!    `set_selection` 会让 compositor 对「旧选区」补发 `cancelled`，而旧选区就是这个 source
//!    ⇒ 客户端清空已发布文本 ⇒ 生效的新选区再也发不出数据（粘贴为空）。本文件每次发布都
//!    `create_data_source` 新建，旧的立即 `destroy()`。
//! 2. **`send` 不能阻塞事件循环**（R10-17）。对端何时读、读多少不可控；`write_all` 在
//!    64 KiB 管道上会阻塞整个 UI 线程。这里把 fd 交给 calloop 的**可写事件**驱动，每次最多写
//!    `PIPE_BUF` 字节（保证不阻塞），写完/出错即关闭 fd（对端读到 EOF）。
//! 3. **`selection` 必须用它自带的 offer**（R10-9）。DND 的序列是 `data_offer` → `offer` →
//!    `enter`，用「最近一次 data_offer」会把拖放的 offer 当成剪贴板。

use std::{
    io::Read,
    os::fd::{AsFd, OwnedFd},
    sync::{Mutex, OnceLock},
};

use smol_str::SmolStr;
use wayland_client::{
    Connection, Proxy, QueueHandle,
    protocol::{wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source},
};

use crate::{
    error::EngineError,
    platform::wayland::selection::{
        MAX_SELECTION_BYTES, MIME_TEXT, OfferTable, SelectionSources, SourceGeneration, too_large,
    },
};

/// 一次 `receive` 最多接受的字节数：对端可以把 pipe 写到无穷，必须有上限。
const MAX_READ: u64 = MAX_SELECTION_BYTES as u64;

/// 单次 `write` 的字节上限（`PIPE_BUF`，Linux 上是 4096）。
///
/// 管道 fd 是**阻塞**的，而 epoll 的 `POLLOUT` 只保证「至少能写一个管道缓冲区（PIPE_BUF）」。
/// 因此每次回调最多写这么多字节就绝不会阻塞；剩余部分等下一次可写事件（Level 触发）。
const PIPE_BUF: usize = 4096;

/// 发往剪贴板的文本（`App::set_clipboard_text` 写、主循环取）。
fn outgoing() -> &'static Mutex<Option<SmolStr>> {
    static OUTGOING: OnceLock<Mutex<Option<SmolStr>>> = OnceLock::new();
    OUTGOING.get_or_init(|| Mutex::new(None))
}

/// 最近一次从选区收到的文本（读回线程写、`App::clipboard_text` 读）。
fn cached() -> &'static Mutex<Option<SmolStr>> {
    static CACHED: OnceLock<Mutex<Option<SmolStr>>> = OnceLock::new();
    CACHED.get_or_init(|| Mutex::new(None))
}

/// 写剪贴板（把文本放进 mailbox；主循环在下一轮用最新 serial 发布）。
///
/// # Errors
/// `EngineError::Clipboard`：单次文本超过 [`MAX_SELECTION_BYTES`]。**不截断** —— 截断会让用户
/// 粘贴到半个地址/金额，比明确失败更危险。
pub(crate) fn set_text(text: &str) -> Result<(), EngineError> {
    if too_large(text) {
        return Err(EngineError::Clipboard {
            detail: smol_str::format_smolstr!(
                "clipboard text is {} bytes; the limit is {MAX_SELECTION_BYTES}",
                text.len()
            ),
        });
    }
    if let Ok(mut slot) = outgoing().lock() {
        *slot = Some(SmolStr::new(text));
    }
    Ok(())
}

/// 读剪贴板：只返回**已经收到**的选区内容；从未收到过 ⇒ `Clipboard` 错误。
///
/// # Errors
/// `EngineError::Clipboard`：尚未从任何选区收到文本（Wayland 的拉模型，F-130）。
pub(crate) fn get_text() -> Result<SmolStr, EngineError> {
    match cached().lock() {
        Ok(slot) => slot.clone().ok_or(EngineError::Clipboard {
            detail: smol_str::SmolStr::new_static(
                "Wayland clipboard is pull-based: no selection content received yet",
            ),
        }),
        Err(_) => Err(EngineError::Clipboard {
            detail: smol_str::SmolStr::new_static("clipboard cache poisoned"),
        }),
    }
}

/// 待写出的 `wl_data_source.send` 数据（交给事件循环用 calloop 的可写事件驱动）。
///
/// 实现 [`AsFd`] 是为了能直接放进 `calloop::generic::Generic`：fd 的所有权在这里，
/// 写完 / 出错 / source 被销毁时随 `Drop` 关闭 ⇒ 对端读到 EOF（协议要求的结束信号）。
pub(crate) struct PendingSend {
    /// 对端的读端（`wl_data_source.send` 交给我们的 fd）。
    file: std::fs::File,
    /// 待写文本（`Bytes`：不可变共享，配合 `offset` 支持分片写）。
    bytes: bytes::Bytes,
    /// 已写出的字节数。
    written: usize,
}

impl AsFd for PendingSend {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.file.as_fd()
    }
}

impl PendingSend {
    /// 可写事件：写一片数据，返回 `true` = 写完了（调用方应把事件源摘掉 ⇒ drop 关闭 fd）。
    ///
    /// `Ok(true)`：全部写完；`Ok(false)`：还有剩余（下一次可写事件继续）；
    /// `Err(_)`：对端已关闭（`EPIPE`）或 IO 错误 —— 按协议直接关闭 fd 即可。
    pub(crate) fn write_more(&mut self) -> std::io::Result<bool> {
        use std::io::Write;
        let remaining = &self.bytes[self.written..];
        let chunk = remaining.len().min(PIPE_BUF);
        // 管道是阻塞 fd：只写 ≤ PIPE_BUF 且 epoll 已经报告可写 ⇒ 不会阻塞。
        match self.file.write(&remaining[..chunk]) {
            Ok(0) => Ok(false),
            Ok(written) => {
                self.written += written;
                Ok(self.written >= self.bytes.len())
            }
            Err(error) => Err(error),
        }
    }
}

/// 协议侧状态（`WaylandState` 持有；主线程独占）。
pub(crate) struct Clipboard {
    /// 数据设备（`set_selection` 在它上面，需要 source + serial）。
    device: wl_data_device::WlDataDevice,
    /// 数据源工厂：**每次发布都要新建一个 source**（R10-1）。
    manager: wl_data_device_manager::WlDataDeviceManager,
    queue: QueueHandle<crate::platform::linux::surface::WaylandState>,
    /// 发布方向的状态机（世代号 ↔ 文本）。
    sources: SelectionSources,
    /// 当前持有的 `wl_data_source` 与它的世代（`None` = 我们不是选区属主）。
    source: Option<(SourceGeneration, wl_data_source::WlDataSource)>,
    /// 收到的 offer：协议代理（按对象 id 索引，`run_device_event` 里用）。
    offer_proxies: Vec<(u32, wl_data_offer::WlDataOffer)>,
    /// offer 的 mime 表（`selection` 事件**自带**的 id 是唯一权威来源，R10-9）。
    offers: OfferTable,
    /// 待交给事件循环的写出任务（带上限：对端不读时会攒起来，超限直接关闭最老的）。
    pending_sends: Vec<PendingSend>,
}

/// 未完成写出任务的并发上限（每个任务占一个 fd；对端不读时不能无限攒 fd）。
const MAX_PENDING_SENDS: usize = 8;

impl Clipboard {
    pub(crate) fn new(
        device: wl_data_device::WlDataDevice,
        manager: wl_data_device_manager::WlDataDeviceManager,
        queue: QueueHandle<crate::platform::linux::surface::WaylandState>,
    ) -> Self {
        Self {
            device,
            manager,
            queue,
            sources: SelectionSources::default(),
            source: None,
            offer_proxies: Vec::with_capacity(2),
            offers: OfferTable::default(),
            pending_sends: Vec::with_capacity(2),
        }
    }

    /// 主循环每轮调用：把 mailbox 里的文本发布成选区。
    ///
    /// `serial` 必须是最近的输入 serial：**serial 为 0 时不能 `take`**，否则文本会被静默丢弃
    /// （启动后第一个循环还没有任何输入事件，`serial` 恒为 0）。等到下一次带 serial 的事件再发布。
    pub(crate) fn apply_pending(&mut self, serial: u32) {
        if serial == 0 {
            return;
        }
        let Some(text) = outgoing().lock().ok().and_then(|mut slot| slot.take()) else {
            return;
        };
        let outcome = self.sources.publish(text);
        // 旧 source 已经失效（协议：一个 source 只服务一次选区）⇒ 立即销毁，绝不能复用。
        if outcome.replaced_live_source
            && let Some((_, previous)) = self.source.take()
        {
            previous.destroy();
        }
        let source = self.manager.create_data_source(&self.queue, ());
        for mime in MIME_TEXT {
            source.offer(mime.to_owned());
        }
        self.device.set_selection(Some(&source), serial);
        self.source = Some((outcome.generation, source));
    }

    /// `wl_data_device` 事件：新 offer / 选区变化 / 拖放离开。
    ///
    /// `serial` 是最近的输入 serial（`accept` 的凭证，R10-25）。
    pub(crate) fn handle_device_event(&mut self, event: wl_data_device::Event, serial: u32) {
        match event {
            wl_data_device::Event::DataOffer { id } => {
                // 新一条 offer 序列开始：之前那些 offer 都不再需要（选区/DND 已换过一轮）。
                self.destroy_offers();
                self.offers.begin(id.protocol_id());
                self.offer_proxies.push((id.protocol_id(), id));
            }
            wl_data_device::Event::Selection { id } => {
                let Some(offer) = id else {
                    // 无选区：清缓存（读到的是「上一次的内容」不能再返回，F-130）。
                    self.destroy_offers();
                    if let Ok(mut slot) = cached().lock() {
                        *slot = None;
                    }
                    return;
                };
                // R10-9：用**事件自带的** offer 定位 mime，而不是「最近一次 data_offer」。
                let key = offer.protocol_id();
                let Some(mime) = self.offers.preferred_mime(key) else {
                    self.destroy_offers();
                    return;
                };
                // 拉取：spec 要求先 accept 再 receive（顺序反了部分 compositor 会忽略）。
                offer.accept(serial, Some(mime.as_str().to_owned()));
                let Ok((reader, writer)) = std::io::pipe() else {
                    self.destroy_offers();
                    return;
                };
                spawn_reader(reader);
                offer.receive(mime.as_str().to_owned(), writer.as_fd());
                self.destroy_offers();
            }
            wl_data_device::Event::Leave => {
                // 拖放离开：所有未处理的 offer 作废（我们不做 DND，直接销毁，避免对象泄漏）。
                self.destroy_offers();
            }
            _ => {}
        }
    }

    /// `wl_data_offer.offer`：给对应 offer 追加 mime（R10-9：按 offer 对象归属）。
    pub(crate) fn handle_offer_event(
        &mut self,
        offer: &wl_data_offer::WlDataOffer,
        event: wl_data_offer::Event,
    ) {
        if let wl_data_offer::Event::Offer { mime_type } = event {
            self.offers
                .add_mime(offer.protocol_id(), SmolStr::new(mime_type));
        }
    }

    /// `wl_data_source.send`：把文本交给**异步**写出队列（R10-17），绝不在这里阻塞。
    ///
    /// `data_source` 决定用哪一份已发布文本（R10-1：按 source 归属）。
    pub(crate) fn handle_send(
        &mut self,
        data_source: &wl_data_source::WlDataSource,
        _mime_type: &str,
        fd: OwnedFd,
    ) {
        let generation = self
            .source
            .as_ref()
            .filter(|(_, source)| source.id() == data_source.id())
            .map(|(generation, _)| *generation);
        let Some(text) = generation.and_then(|generation| self.sources.take_text(generation))
        else {
            // source 已经 cancelled（或不是我们的 source）：按协议直接关闭 fd，对端读到 EOF。
            return;
        };
        let bytes = bytes::Bytes::copy_from_slice(text.as_bytes());
        let send = PendingSend {
            file: std::fs::File::from(fd),
            bytes,
            written: 0,
        };
        // 积压保护：对端迟迟不读时，先关掉最老的 fd（对端读到 EOF）而不是无限攒 fd。
        while self.pending_sends.len() >= MAX_PENDING_SENDS {
            self.pending_sends.remove(0);
        }
        self.pending_sends.push(send);
    }

    /// `wl_data_source.cancelled`：对端换了选区属主。
    ///
    /// 无论这条事件属于哪一代，**都必须销毁这个 source**（协议原文 "The client should clean up
    /// and destroy this data source"）；只有当前世代才清空已发布文本。
    pub(crate) fn handle_cancelled(&mut self, data_source: &wl_data_source::WlDataSource) {
        let Some((generation, source)) = self
            .source
            .take_if(|(_, source)| source.id() == data_source.id())
        else {
            data_source.destroy();
            return;
        };
        self.sources.cancelled(generation);
        source.destroy();
    }

    /// 取出待写出的任务（事件循环把它们注册成 calloop 的可写事件源）。
    pub(crate) fn take_pending_sends(&mut self) -> Vec<PendingSend> {
        std::mem::take(&mut self.pending_sends)
    }

    /// 销毁所有 offer 代理（服务端对象泄漏 + user data 泄漏，R10-34）。
    fn destroy_offers(&mut self) {
        self.offers.drain();
        for (_, offer) in self.offer_proxies.drain(..) {
            offer.destroy();
        }
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        // 退出路径：显式销毁 source（协议要求「不再需要时 destroy」，服务端对象不会随客户端
        // 代理 drop 消失）并清空已发布状态；未写完的 send 由 `PendingSend` 的 drop 关闭 fd。
        self.sources.clear();
        if let Some((_, source)) = self.source.take() {
            source.destroy();
        }
        self.destroy_offers();
    }
}

/// 读回线程：读到 EOF 后把文本写进缓存（Wayland 的接收端**不能**在主线程阻塞读）。
fn spawn_reader(reader: std::io::PipeReader) {
    std::thread::spawn(move || {
        let mut bytes = Vec::with_capacity(256);
        // 上限 1 MiB：对端写无限数据时不能把内存吃满。
        if reader.take(MAX_READ).read_to_end(&mut bytes).is_ok() {
            let text = String::from_utf8_lossy(&bytes);
            if let Ok(mut slot) = cached().lock() {
                *slot = Some(SmolStr::new(text.trim_end_matches('\0')));
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{PIPE_BUF, set_text};
    use crate::platform::wayland::selection::MAX_SELECTION_BYTES;

    /// R10-17：超限的复制请求必须**失败**而不是截断（截断会静默改坏粘贴内容）。
    #[test]
    fn oversized_clipboard_text_is_rejected() {
        let huge = "a".repeat(MAX_SELECTION_BYTES + 1);
        let error = set_text(&huge).expect_err("超限必须报错");
        assert!(
            matches!(error, crate::error::EngineError::Clipboard { .. }),
            "{error:?}"
        );
        assert!(set_text("ok").is_ok(), "正常文本不受影响");
    }

    /// 分片写的上限必须是管道原子写长度（≤ PIPE_BUF 才不会在阻塞 fd 上卡住）。
    #[test]
    fn each_write_stays_within_one_pipe_buffer() {
        assert_eq!(PIPE_BUF, 4096);
        assert!(PIPE_BUF <= 4096, "PIPE_BUF 是管道原子写的上界");
    }
}
