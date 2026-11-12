//! 剪贴板：`wl_data_device` / `wl_data_source`（`design/phase-12-macos-linux.md` §8.6）。
//!
//! Wayland 是**异步拉模型**：`receive` 之后数据由对端写 pipe。对外的
//! `App::clipboard_text()` 只能返回**缓存**（F-130）；发布方向要把文本经进程级 mailbox 交给
//! 主循环（`set_selection` 需要最近的输入 serial，只有循环持有，F-137）。

use std::{
    io::Read,
    os::fd::{AsFd, OwnedFd},
    sync::{Mutex, OnceLock},
};

use smol_str::SmolStr;
use wayland_client::{
    Connection,
    protocol::{wl_data_device, wl_data_offer, wl_data_source},
};

use crate::error::EngineError;

/// 支持的 mime（顺序即偏好）。
const MIME_TEXT: [&str; 3] = ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING"];

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
/// 不产生错误（发布阶段的失败由主循环记诊断，不影响调用方语义）。
pub(crate) fn set_text(text: &str) -> Result<(), EngineError> {
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

/// 协议侧状态（`WaylandState` 持有；主线程独占）。
pub(crate) struct Clipboard {
    /// 数据设备（`set_selection` 在它上面，需要 source + serial）。
    device: wl_data_device::WlDataDevice,
    /// 我们的数据源（发布方向）。
    source: wl_data_source::WlDataSource,
    /// 当前可用的选区 offer（粘贴方向）。
    offer: Option<wl_data_offer::WlDataOffer>,
    /// 已发布到 `source` 的文本（`Send` 时写 pipe）。
    published: Option<SmolStr>,
    /// 连接（供将来做非阻塞读；v1 的读回在独立线程里）。
    connection: Connection,
}

impl Clipboard {
    pub(crate) fn new(
        connection: Connection,
        device: wl_data_device::WlDataDevice,
        source: wl_data_source::WlDataSource,
    ) -> Self {
        Self {
            device,
            source,
            offer: None,
            published: None,
            connection,
        }
    }

    /// 主循环每轮调用：把 mailbox 里的文本发布成选区（`serial` 必须是最近的输入 serial）。
    pub(crate) fn apply_pending(&mut self, serial: u32) {
        let Some(text) = outgoing().lock().ok().and_then(|mut slot| slot.take()) else {
            return;
        };
        self.published = Some(text);
        if serial != 0 {
            self.device.set_selection(Some(&self.source), serial);
        }
    }

    /// `wl_data_device` 事件：新 offer / 选区变化。
    pub(crate) fn handle_device_event(&mut self, event: wl_data_device::Event) {
        match event {
            wl_data_device::Event::DataOffer { id } => {
                // 可用的 mime 由后续 `wl_data_offer::Event::Offer` 报告；v1 只记住 offer。
                self.offer = Some(id);
            }
            wl_data_device::Event::Selection { id } => {
                if id.is_none() {
                    self.offer = None;
                    if let Ok(mut slot) = cached().lock() {
                        *slot = None;
                    }
                    return;
                }
                let Some(offer) = self.offer.as_ref() else {
                    return;
                };
                // 拉取：交给读回线程（`receive` 把 pipe 的写端交给数据源）。
                let Ok((reader, writer)) = std::io::pipe() else {
                    return;
                };
                spawn_reader(reader);
                offer.receive(MIME_TEXT[0].to_owned(), writer.as_fd());
            }
            wl_data_device::Event::Leave => {
                self.offer = None;
            }
            _ => {}
        }
    }

    /// `wl_data_offer` 事件（v1 只关心 `Offer` / `SourceActions`，都不影响缓存）。
    pub(crate) fn handle_offer_event(&mut self, _event: wl_data_offer::Event) {
        let _ = &self.connection;
    }

    /// `wl_data_source::Event::Send`：把已发布的文本写进 pipe，然后关闭（EOF）。
    pub(crate) fn handle_send(
        &mut self,
        _source: &wl_data_source::WlDataSource,
        _mime_type: &str,
        fd: OwnedFd,
    ) {
        let Some(text) = self.published.clone() else {
            return;
        };
        // SAFETY: `fd` 由协议交给本回调，所有权在此转移给 `File`；写完即关闭（对端读到 EOF）。
        let mut file = std::fs::File::from(fd);
        use std::io::Write;
        let _ = file.write_all(text.as_bytes());
    }
}

/// 读回线程：读到 EOF 后把文本写进缓存（Wayland 的接收端**不能**在主线程阻塞读）。
fn spawn_reader(mut reader: std::io::PipeReader) {
    std::thread::spawn(move || {
        let mut bytes = Vec::with_capacity(256);
        if reader.read_to_end(&mut bytes).is_ok() {
            let text = String::from_utf8_lossy(&bytes);
            if let Ok(mut slot) = cached().lock() {
                *slot = Some(SmolStr::new(text.trim_end_matches('\0')));
            }
        }
    });
}
