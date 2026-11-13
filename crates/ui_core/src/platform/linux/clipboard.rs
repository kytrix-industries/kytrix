//! 剪贴板：`wl_data_device` / `wl_data_source`（`design/phase-12-macos-linux.md` §8.6）。
//!
//! Wayland 是**异步拉模型**：`receive` 之后数据由对端写 pipe。对外的
//! `App::clipboard_text()` 只能返回**缓存**（F-130）；发布方向要把文本经进程级 mailbox 交给
//! 主循环（`set_selection` 需要最近的输入 serial，只有循环持有，F-137）。
//!
//! 协议顺序（wl_data_offer 的 spec）：收到 `wl_data_device.selection` 之后**先** `accept(mime)`
//! **再** `receive(mime, fd)`；漏掉 `accept` 会被部分 compositor 直接忽略这次拉取。
//! mime 由 offer 自己声明（`wl_data_offer.offer`），不能写死。

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

/// 支持的 mime，**顺序即偏好**（客户端发布时按此顺序 offer，接收时按此顺序挑第一个对端也支持的）。
const MIME_TEXT: [&str; 3] = ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING"];

/// 一次 `receive` 最多接受的字节数：对端可以把 pipe 写到无穷，必须有上限。
const MAX_READ: u64 = 1024 * 1024;

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
    /// 当前 offer 声明的 mime（`wl_data_offer.offer` 逐条累积）。
    offer_mimes: Vec<SmolStr>,
    /// 已发布到 `source` 的文本（`Send` 时写 pipe）。
    published: Option<SmolStr>,
}

impl Clipboard {
    pub(crate) fn new(
        _connection: Connection,
        device: wl_data_device::WlDataDevice,
        source: wl_data_source::WlDataSource,
    ) -> Self {
        Self {
            device,
            source,
            offer: None,
            offer_mimes: Vec::with_capacity(MIME_TEXT.len()),
            published: None,
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
        self.published = Some(text);
        self.device.set_selection(Some(&self.source), serial);
    }

    /// `wl_data_device` 事件：新 offer / 选区变化。
    pub(crate) fn handle_device_event(&mut self, event: wl_data_device::Event) {
        match event {
            wl_data_device::Event::DataOffer { id } => {
                // mime 由紧随其后的 `wl_data_offer.offer` 逐条报告。
                self.offer = Some(id);
                self.offer_mimes.clear();
            }
            wl_data_device::Event::Selection { id } => {
                if id.is_none() {
                    self.offer = None;
                    self.offer_mimes.clear();
                    if let Ok(mut slot) = cached().lock() {
                        *slot = None;
                    }
                    return;
                }
                let Some(offer) = self.offer.as_ref() else {
                    return;
                };
                let Some(mime) = preferred_mime(&self.offer_mimes) else {
                    return;
                };
                // 拉取：spec 要求先 accept 再 receive（顺序反了部分 compositor 会忽略）。
                offer.accept(0, Some(mime.as_str().to_owned()));
                let Ok((reader, writer)) = std::io::pipe() else {
                    return;
                };
                spawn_reader(reader);
                offer.receive(mime.as_str().to_owned(), writer.as_fd());
            }
            wl_data_device::Event::Leave => {
                self.offer = None;
                self.offer_mimes.clear();
            }
            _ => {}
        }
    }

    /// `wl_data_offer` 事件：只关心 `Offer`（mime 声明）；其余不影响缓存。
    pub(crate) fn handle_offer_event(&mut self, event: wl_data_offer::Event) {
        if let wl_data_offer::Event::Offer { mime_type } = event {
            self.offer_mimes.push(SmolStr::new(mime_type));
        }
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

    /// `wl_data_source::Event::Cancelled`：对端换了选区，我们不再是属主 ⇒ 丢弃已发布文本。
    pub(crate) fn handle_cancelled(&mut self) {
        self.published = None;
    }
}

/// 按 `MIME_TEXT` 的偏好挑一个双方都支持的 mime。
fn preferred_mime(offered: &[SmolStr]) -> Option<SmolStr> {
    MIME_TEXT
        .iter()
        .find(|wanted| offered.iter().any(|mime| mime.as_str() == **wanted))
        .map(|wanted| SmolStr::new_static(wanted))
}

/// 读回线程：读到 EOF 后把文本写进缓存（Wayland 的接收端**不能**在主线程阻塞读）。
fn spawn_reader(reader: std::io::PipeReader) {
    std::thread::spawn(move || {
        let mut bytes = Vec::with_capacity(256);
        // 上限 1 MiB：对端写无穷数据时不能把内存吃满。
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
    use smol_str::SmolStr;

    use super::{MAX_READ, MIME_TEXT, preferred_mime};

    fn offered(list: &[&str]) -> Vec<SmolStr> {
        list.iter().map(|mime| SmolStr::new(*mime)).collect()
    }

    #[test]
    fn preferred_mime_follows_the_declared_priority() {
        assert_eq!(
            preferred_mime(&offered(&[
                "UTF8_STRING",
                "text/plain",
                "text/plain;charset=utf-8"
            ])),
            Some(SmolStr::new_static(MIME_TEXT[0]))
        );
        assert_eq!(
            preferred_mime(&offered(&["UTF8_STRING", "text/plain"])),
            Some(SmolStr::new_static(MIME_TEXT[1]))
        );
        assert_eq!(
            preferred_mime(&offered(&["UTF8_STRING"])),
            Some(SmolStr::new_static(MIME_TEXT[2]))
        );
    }

    #[test]
    fn preferred_mime_ignores_unknown_types() {
        assert_eq!(
            preferred_mime(&offered(&[
                "image/png",
                "text/html",
                "application/octet-stream"
            ])),
            None,
            "只认纯文本 mime，不能把图片当文本拉"
        );
        assert_eq!(preferred_mime(&[]), None);
    }

    #[test]
    fn read_cap_is_one_mebibyte() {
        assert_eq!(MAX_READ, 1024 * 1024);
    }
}
