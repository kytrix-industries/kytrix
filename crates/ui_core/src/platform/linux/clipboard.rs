use std::{
    cell::Cell,
    io::Read,
    os::fd::{AsFd, OwnedFd},
    sync::{Mutex, OnceLock},
    time::Instant,
};

use smol_str::SmolStr;
use wayland_client::{
    Proxy, QueueHandle,
    protocol::{wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source},
};

use crate::{
    error::EngineError,
    platform::wayland::selection::{
        LiveSource, MAX_PENDING_SEND_BYTES, MAX_SELECTION_BYTES, MIME_TEXT, OfferId, OfferTable,
        SEND_DEADLINE, SelectionBackend, SelectionSources, SendBudget, publish_selection,
        too_large,
    },
};

const MAX_READ: u64 = MAX_SELECTION_BYTES as u64;

const PIPE_BUF: usize = 4096;

fn outgoing() -> &'static Mutex<Option<SmolStr>> {
    static OUTGOING: OnceLock<Mutex<Option<SmolStr>>> = OnceLock::new();
    OUTGOING.get_or_init(|| Mutex::new(None))
}

fn send_budget() -> &'static Mutex<SendBudget> {
    static BUDGET: OnceLock<Mutex<SendBudget>> = OnceLock::new();
    BUDGET.get_or_init(|| Mutex::new(SendBudget::new(MAX_PENDING_SEND_BYTES)))
}

fn cached() -> &'static Mutex<Option<SmolStr>> {
    static CACHED: OnceLock<Mutex<Option<SmolStr>>> = OnceLock::new();
    CACHED.get_or_init(|| Mutex::new(None))
}

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

pub(crate) struct PendingSend {
    file: std::fs::File,
    bytes: bytes::Bytes,
    written: Cell<usize>,
    deadline: Instant,
}

impl AsFd for PendingSend {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.file.as_fd()
    }
}

impl PendingSend {
    pub(crate) fn write_more(&self) -> std::io::Result<bool> {
        use std::io::Write;
        let written = self.written.get();
        if written >= self.bytes.len() {
            return Ok(true);
        }
        if Instant::now() >= self.deadline {
            return Ok(true);
        }
        let remaining = &self.bytes[written..];
        let chunk = remaining.len().min(PIPE_BUF);
        match (&self.file).write(&remaining[..chunk]) {
            Ok(0) => Ok(true),
            Ok(step) => {
                let written = written + step;
                self.written.set(written);
                Ok(written >= self.bytes.len())
            }
            Err(error) => Err(error),
        }
    }
}

impl Drop for PendingSend {
    fn drop(&mut self) {
        if let Ok(mut budget) = send_budget().lock() {
            budget.release(self.bytes.len());
        }
    }
}

pub(crate) struct Clipboard {
    device: wl_data_device::WlDataDevice,
    manager: wl_data_device_manager::WlDataDeviceManager,
    queue: QueueHandle<crate::platform::linux::surface::WaylandState>,
    sources: SelectionSources,
    source: Option<LiveSource<wl_data_source::WlDataSource>>,
    offer_proxies: Vec<(OfferId, wl_data_offer::WlDataOffer)>,
    offers: OfferTable,
    pending_sends: Vec<PendingSend>,
}

const MAX_PENDING_SENDS: usize = 8;

struct SourceOps<'a> {
    manager: &'a wl_data_device_manager::WlDataDeviceManager,
    device: &'a wl_data_device::WlDataDevice,
    queue: &'a QueueHandle<crate::platform::linux::surface::WaylandState>,
}

impl SelectionBackend for SourceOps<'_> {
    type Source = wl_data_source::WlDataSource;

    fn create_source(&mut self) -> Self::Source {
        let source = self.manager.create_data_source(self.queue, ());
        for mime in MIME_TEXT {
            source.offer(mime.to_owned());
        }
        source
    }

    fn destroy_source(&mut self, source: Self::Source) {
        source.destroy();
    }

    fn set_selection(&mut self, source: &Self::Source, serial: u32) {
        self.device.set_selection(Some(source), serial);
    }
}

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

    pub(crate) fn apply_pending(&mut self, serial: u32) {
        if serial == 0 {
            return;
        }
        let Some(text) = outgoing().lock().ok().and_then(|mut slot| slot.take()) else {
            return;
        };
        let mut ops = SourceOps {
            manager: &self.manager,
            device: &self.device,
            queue: &self.queue,
        };
        publish_selection(&mut ops, &mut self.sources, &mut self.source, text, serial);
    }

    pub(crate) fn handle_device_event(&mut self, event: wl_data_device::Event, serial: u32) {
        match event {
            wl_data_device::Event::DataOffer { id } => {
                self.destroy_offers();
                let offer_id = self.offers.begin();
                self.offer_proxies.push((offer_id, id));
            }
            wl_data_device::Event::Selection { id } => {
                let Some(offer) = id else {
                    self.destroy_offers();
                    if let Ok(mut slot) = cached().lock() {
                        *slot = None;
                    }
                    return;
                };
                let Some(offer_id) = self.offer_id_for(&offer) else {
                    self.destroy_offers();
                    return;
                };
                let Some(mime) = self.offers.preferred_mime(offer_id) else {
                    self.destroy_offers();
                    return;
                };
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
                self.destroy_offers();
            }
            _ => {}
        }
    }

    pub(crate) fn handle_offer_event(
        &mut self,
        offer: &wl_data_offer::WlDataOffer,
        event: wl_data_offer::Event,
    ) {
        if let wl_data_offer::Event::Offer { mime_type } = event
            && let Some(offer_id) = self.offer_id_for(offer)
        {
            self.offers.add_mime(offer_id, SmolStr::new(mime_type));
        }
    }

    fn offer_id_for(&self, offer: &wl_data_offer::WlDataOffer) -> Option<OfferId> {
        let id = offer.id();
        self.offer_proxies
            .iter()
            .find(|(_, proxy)| proxy.id() == id)
            .map(|(offer_id, _)| *offer_id)
    }

    pub(crate) fn handle_send(
        &mut self,
        data_source: &wl_data_source::WlDataSource,
        _mime_type: &str,
        fd: OwnedFd,
    ) {
        let generation = self
            .source
            .as_ref()
            .filter(|live| live.source.id() == data_source.id())
            .map(|live| live.generation);
        let Some(text) = generation.and_then(|generation| self.sources.take_text(generation))
        else {
            return;
        };
        let bytes = bytes::Bytes::copy_from_slice(text.as_bytes());
        let admitted = send_budget()
            .lock()
            .map(|mut budget| budget.admit(bytes.len()))
            .unwrap_or(false);
        if !admitted {
            return;
        }
        let send = PendingSend {
            file: std::fs::File::from(fd),
            bytes,
            written: Cell::new(0),
            deadline: Instant::now() + SEND_DEADLINE,
        };
        while self.pending_sends.len() >= MAX_PENDING_SENDS {
            self.pending_sends.remove(0);
        }
        self.pending_sends.push(send);
    }

    pub(crate) fn handle_cancelled(&mut self, data_source: &wl_data_source::WlDataSource) {
        let Some(live) = self
            .source
            .take_if(|live| live.source.id() == data_source.id())
        else {
            data_source.destroy();
            return;
        };
        self.sources.cancelled(live.generation);
        live.source.destroy();
    }

    pub(crate) fn take_pending_sends(&mut self) -> Vec<PendingSend> {
        std::mem::take(&mut self.pending_sends)
    }

    fn destroy_offers(&mut self) {
        self.offers.drain();
        for (_, offer) in self.offer_proxies.drain(..) {
            offer.destroy();
        }
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        self.sources.clear();
        if let Some(live) = self.source.take() {
            live.source.destroy();
        }
        self.destroy_offers();
    }
}

fn spawn_reader(reader: std::io::PipeReader) {
    std::thread::spawn(move || {
        let mut bytes = Vec::with_capacity(256);
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

    const _: () = assert!(PIPE_BUF <= 4096, "PIPE_BUF 是管道原子写的上界");
}
