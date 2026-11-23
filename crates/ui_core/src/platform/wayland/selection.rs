use smol_str::SmolStr;

pub(crate) const MIME_TEXT: [&str; 3] = ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING"];

pub(crate) const MAX_SELECTION_BYTES: usize = 1024 * 1024;

pub(crate) const MAX_PENDING_SEND_BYTES: usize = 4 * 1024 * 1024;

#[cfg(target_os = "linux")]
pub(crate) const SEND_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct SourceGeneration(u64);

#[derive(Debug, Default)]
pub(crate) struct SelectionSources {
    published: Option<PublishedSource>,
    next_generation: u64,
}

#[derive(Debug)]
struct PublishedSource {
    generation: SourceGeneration,
    text: SmolStr,
}

impl SelectionSources {
    pub(crate) fn publish(&mut self, text: SmolStr) -> SourceGeneration {
        let generation = SourceGeneration(self.next_generation);
        self.next_generation += 1;
        self.published = Some(PublishedSource { generation, text });
        generation
    }

    pub(crate) fn cancelled(&mut self, generation: SourceGeneration) -> bool {
        if self
            .published
            .as_ref()
            .map(|published| published.generation)
            != Some(generation)
        {
            return false;
        }
        self.published = None;
        true
    }

    #[must_use]
    pub(crate) fn take_text(&self, generation: SourceGeneration) -> Option<&SmolStr> {
        self.published
            .as_ref()
            .filter(|published| published.generation == generation)
            .map(|published| &published.text)
    }

    pub(crate) fn clear(&mut self) -> Option<SourceGeneration> {
        self.published.take().map(|published| published.generation)
    }
}

pub(crate) trait SelectionBackend {
    type Source;

    fn create_source(&mut self) -> Self::Source;

    fn destroy_source(&mut self, source: Self::Source);

    fn set_selection(&mut self, source: &Self::Source, serial: u32);
}

#[derive(Debug)]
pub(crate) struct LiveSource<S> {
    pub generation: SourceGeneration,
    pub source: S,
}

pub(crate) fn publish_selection<B: SelectionBackend>(
    backend: &mut B,
    sources: &mut SelectionSources,
    held: &mut Option<LiveSource<B::Source>>,
    text: SmolStr,
    serial: u32,
) -> SourceGeneration {
    let generation = sources.publish(text);
    if let Some(previous) = held.take() {
        backend.destroy_source(previous.source);
    }
    let source = backend.create_source();
    backend.set_selection(&source, serial);
    *held = Some(LiveSource { generation, source });
    generation
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct SendBudget {
    limit: usize,
    used: usize,
}

impl SendBudget {
    #[must_use]
    pub(crate) const fn new(limit: usize) -> Self {
        Self { limit, used: 0 }
    }

    pub(crate) fn admit(&mut self, bytes: usize) -> bool {
        if bytes > self.limit.saturating_sub(self.used) {
            return false;
        }
        self.used += bytes;
        true
    }

    pub(crate) fn release(&mut self, bytes: usize) {
        self.used = self.used.saturating_sub(bytes);
    }

    #[cfg(test)]
    pub(crate) fn used(&self) -> usize {
        self.used
    }
}

#[derive(Clone, Debug)]
struct OfferEntry {
    id: OfferId,
    mimes: Vec<SmolStr>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct OfferId(u32);

#[derive(Debug, Default)]
pub(crate) struct OfferTable {
    offers: Vec<OfferEntry>,
    next_id: u32,
}

impl OfferTable {
    pub(crate) fn begin(&mut self) -> OfferId {
        let id = OfferId(self.next_id);
        self.next_id += 1;
        self.offers.push(OfferEntry {
            id,
            mimes: Vec::with_capacity(MIME_TEXT.len()),
        });
        id
    }

    pub(crate) fn add_mime(&mut self, id: OfferId, mime: SmolStr) {
        if let Some(entry) = self.offers.iter_mut().find(|entry| entry.id == id) {
            entry.mimes.push(mime);
        }
    }

    #[must_use]
    pub(crate) fn preferred_mime(&self, id: OfferId) -> Option<SmolStr> {
        let entry = self.offers.iter().find(|entry| entry.id == id)?;
        preferred_mime(&entry.mimes)
    }

    #[cfg(test)]
    pub(crate) fn take(&mut self, id: OfferId) -> Option<Vec<SmolStr>> {
        let index = self.offers.iter().position(|entry| entry.id == id)?;
        Some(self.offers.remove(index).mimes)
    }

    pub(crate) fn drain(&mut self) -> Vec<OfferId> {
        self.offers.drain(..).map(|entry| entry.id).collect()
    }
}

pub(crate) fn preferred_mime(offered: &[SmolStr]) -> Option<SmolStr> {
    MIME_TEXT
        .iter()
        .find(|wanted| offered.iter().any(|mime| mime.as_str() == **wanted))
        .map(|wanted| SmolStr::new_static(wanted))
}

#[must_use]
pub(crate) fn too_large(text: &str) -> bool {
    text.len() > MAX_SELECTION_BYTES
}

#[cfg(test)]
mod tests {
    use smol_str::SmolStr;

    use super::{
        LiveSource, MAX_PENDING_SEND_BYTES, MAX_SELECTION_BYTES, MIME_TEXT, OfferTable,
        SelectionBackend, SelectionSources, SendBudget, preferred_mime, publish_selection,
        too_large,
    };

    fn offered(list: &[&str]) -> Vec<SmolStr> {
        list.iter().map(|mime| SmolStr::new(*mime)).collect()
    }

    type FakeSource = u32;

    struct RecordingBackend {
        next: FakeSource,
        log: Vec<String>,
    }

    impl RecordingBackend {
        fn new() -> Self {
            Self {
                next: 1,
                log: Vec::with_capacity(8),
            }
        }
    }

    impl SelectionBackend for RecordingBackend {
        type Source = FakeSource;

        fn create_source(&mut self) -> Self::Source {
            let source = self.next;
            self.next += 1;
            self.log.push(format!("create({source})"));
            source
        }

        fn destroy_source(&mut self, source: Self::Source) {
            self.log.push(format!("destroy({source})"));
        }

        fn set_selection(&mut self, source: &Self::Source, serial: u32) {
            self.log.push(format!("select({source},{serial})"));
        }
    }

    #[test]
    fn every_publish_destroys_the_previous_source_and_creates_a_new_one() {
        let mut backend = RecordingBackend::new();
        let mut sources = SelectionSources::default();
        let mut held: Option<LiveSource<FakeSource>> = None;

        let first = publish_selection(
            &mut backend,
            &mut sources,
            &mut held,
            SmolStr::new_static("A"),
            7,
        );
        assert_eq!(backend.log, vec!["create(1)", "select(1,7)"]);
        assert_eq!(
            held.as_ref().map(|live| (live.generation, live.source)),
            Some((first, 1)),
            "持有项的世代号必须就是这次发布的世代"
        );

        let second = publish_selection(
            &mut backend,
            &mut sources,
            &mut held,
            SmolStr::new_static("B"),
            8,
        );
        assert_eq!(
            backend.log,
            vec![
                "create(1)",
                "select(1,7)",
                "destroy(1)",
                "create(2)",
                "select(2,8)"
            ],
            "旧 source 必须先销毁、新 source 必须新建（绝不复用代理）"
        );
        assert_ne!(first, second, "每次发布都是新世代");
        assert_eq!(
            held.as_ref().map(|live| (live.generation, live.source)),
            Some((second, 2))
        );
        assert_eq!(sources.take_text(second), Some(&SmolStr::new_static("B")));
        assert_eq!(
            sources.take_text(first),
            None,
            "旧世代的 send 不得再取到文本"
        );
    }

    #[test]
    fn publishing_after_a_cancel_does_not_destroy_anything() {
        let mut backend = RecordingBackend::new();
        let mut sources = SelectionSources::default();
        let mut held: Option<LiveSource<FakeSource>> = None;

        let first = publish_selection(
            &mut backend,
            &mut sources,
            &mut held,
            SmolStr::new_static("A"),
            3,
        );
        assert!(sources.cancelled(first));
        if let Some(previous) = held.take() {
            backend.destroy_source(previous.source);
        }
        backend.log.clear();

        publish_selection(
            &mut backend,
            &mut sources,
            &mut held,
            SmolStr::new_static("B"),
            4,
        );
        assert_eq!(
            backend.log,
            vec!["create(2)", "select(2,4)"],
            "已被 cancelled 的 source 早就销毁过，不能重复 destroy"
        );
    }

    #[test]
    fn the_send_budget_rejects_a_payload_that_does_not_fit() {
        let mut budget = SendBudget::new(MAX_PENDING_SEND_BYTES);
        assert_eq!(budget.used(), 0);
        assert!(budget.admit(MAX_SELECTION_BYTES));
        assert_eq!(budget.used(), MAX_SELECTION_BYTES);

        let rest = MAX_PENDING_SEND_BYTES - MAX_SELECTION_BYTES;
        assert!(budget.admit(rest));
        assert_eq!(budget.used(), MAX_PENDING_SEND_BYTES);
        assert!(
            !budget.admit(1),
            "超出总预算的 send 必须被拒绝，而不是无限攒内存"
        );

        budget.release(MAX_SELECTION_BYTES);
        assert_eq!(budget.used(), MAX_PENDING_SEND_BYTES - MAX_SELECTION_BYTES);
        assert!(budget.admit(MAX_SELECTION_BYTES));
    }

    #[test]
    fn a_single_send_larger_than_the_budget_is_rejected() {
        let mut budget = SendBudget::new(1024);
        assert!(!budget.admit(2048));
        assert_eq!(budget.used(), 0, "被拒绝的预留不得记账");
        assert!(budget.admit(1024));
        assert_eq!(budget.used(), 1024);
    }

    #[test]
    fn releasing_the_budget_never_underflows() {
        let mut budget = SendBudget::new(1024);
        budget.release(4096);
        assert_eq!(budget.used(), 0);
        assert!(budget.admit(1024));
        budget.release(1024);
        budget.release(1024);
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn a_second_copy_uses_a_new_generation() {
        let mut sources = SelectionSources::default();
        let first = sources.publish(SmolStr::new_static("A"));
        assert_eq!(sources.take_text(first), Some(&SmolStr::new_static("A")));

        let second = sources.publish(SmolStr::new_static("B"));
        assert!(second > first, "世代号必须单调递增");
        assert_eq!(
            sources.take_text(first),
            None,
            "旧世代的 source 已经失效，send 不得再取到文本"
        );
        assert_eq!(sources.take_text(second), Some(&SmolStr::new_static("B")));
    }

    #[test]
    fn a_late_cancelled_for_the_replaced_source_keeps_the_new_text() {
        let mut sources = SelectionSources::default();
        let first = sources.publish(SmolStr::new_static("A"));
        let second = sources.publish(SmolStr::new_static("B"));

        assert!(
            !sources.cancelled(first),
            "旧世代的 cancelled 不得清掉当前发布"
        );
        assert_eq!(
            sources.take_text(second),
            Some(&SmolStr::new_static("B")),
            "新选区的数据必须仍然可发送（旧实现就是在这里丢掉了 B）"
        );

        assert!(sources.cancelled(second), "当前世代被取消");
        assert_eq!(sources.take_text(second), None);
        assert_eq!(sources.clear(), None, "取消之后已经不再是选区属主");
    }

    #[test]
    fn cancelling_the_current_generation_is_idempotent() {
        let mut sources = SelectionSources::default();
        let published = sources.publish(SmolStr::new_static("x"));
        assert!(sources.cancelled(published));
        assert!(
            !sources.cancelled(published),
            "第二次 cancelled（或迟到的重复事件）不得再报告「清掉了当前发布」"
        );
    }

    #[test]
    fn clearing_reports_the_generation_so_the_callers_can_destroy_it() {
        let mut sources = SelectionSources::default();
        let published = sources.publish(SmolStr::new_static("x"));
        assert_eq!(sources.clear(), Some(published));
        assert_eq!(sources.clear(), None, "没有发布时 clear 是 no-op");
        assert_eq!(sources.take_text(published), None);
    }

    #[test]
    fn send_data_is_routed_by_source_generation() {
        let mut sources = SelectionSources::default();
        let first = sources.publish(SmolStr::new_static("old"));
        let second = sources.publish(SmolStr::new_static("new"));
        assert_eq!(sources.take_text(second), Some(&SmolStr::new_static("new")));
        assert_eq!(sources.take_text(first), None);
        assert_ne!(first, second);
        assert!(second > first);
    }

    #[test]
    fn an_unpublished_state_serves_no_text_for_any_generation() {
        let sources = SelectionSources::default();
        let mut probe = SelectionSources::default();
        let generation = probe.publish(SmolStr::new_static("x"));
        assert_eq!(sources.take_text(generation), None);
    }

    #[test]
    fn selection_uses_the_offer_carried_by_the_event() {
        let mut table = OfferTable::default();
        let dropped = table.begin();
        table.add_mime(dropped, SmolStr::new_static("image/png"));
        let selection = table.begin();
        table.add_mime(selection, SmolStr::new_static("text/plain"));

        assert_eq!(
            table.preferred_mime(selection),
            Some(SmolStr::new_static("text/plain")),
            "选区必须按自己的 mime 协商"
        );
        assert_eq!(
            table.preferred_mime(dropped),
            None,
            "拖放的 offer 只有图片，不能当文本拉"
        );
    }

    #[test]
    fn an_unknown_offer_id_has_no_mime() {
        let mut table = OfferTable::default();
        let known = table.begin();
        table.add_mime(known, SmolStr::new_static("text/plain"));
        let mut other = OfferTable::default();
        let unknown = other.begin();
        assert_eq!(
            table.preferred_mime(unknown),
            Some(SmolStr::new_static("text/plain"))
        );
        let mut fresh = OfferTable::default();
        fresh.begin();
        let beyond = fresh.begin();
        assert_eq!(fresh.preferred_mime(beyond), None);
    }

    #[test]
    fn taking_an_offer_hands_over_its_lifetime() {
        let mut table = OfferTable::default();
        let id = table.begin();
        table.add_mime(id, SmolStr::new_static(MIME_TEXT[0]));
        let mimes = table.take(id).expect("登记过的 offer");
        assert_eq!(mimes.len(), 1);
        assert_eq!(
            table.preferred_mime(id),
            None,
            "取出后不再持有 ⇒ 调用方负责 destroy"
        );
        assert_eq!(table.take(id), None, "重复 take 是 no-op");
    }

    #[test]
    fn leave_drains_every_offer_so_none_leaks() {
        let mut table = OfferTable::default();
        let first = table.begin();
        let second = table.begin();
        table.add_mime(second, SmolStr::new_static("text/plain"));
        let mut ids = table.drain();
        ids.sort_unstable();
        let mut expected = vec![first, second];
        expected.sort_unstable();
        assert_eq!(ids, expected);
        assert!(table.drain().is_empty(), "drain 之后是空的");
        assert_eq!(table.preferred_mime(first), None);
    }

    #[test]
    fn every_data_offer_allocates_a_fresh_id() {
        let mut table = OfferTable::default();
        let first = table.begin();
        table.add_mime(first, SmolStr::new_static("text/plain"));
        let second = table.begin();
        assert_ne!(first, second);
        assert_eq!(
            table.preferred_mime(second),
            None,
            "新 offer 的 mime 要重新累积"
        );
        table.add_mime(second, SmolStr::new_static("image/png"));
        assert_eq!(
            table.preferred_mime(first),
            Some(SmolStr::new_static("text/plain")),
            "老 offer 的 mime 不受新 offer 影响"
        );
    }

    #[test]
    fn preferred_mime_follows_the_declared_priority() {
        assert_eq!(
            preferred_mime(&offered(&["UTF8_STRING", "text/plain", MIME_TEXT[0]])),
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
        assert_eq!(
            preferred_mime(&offered(&["image/png", "text/html"])),
            None,
            "只认纯文本 mime，不能把图片当文本拉"
        );
        assert_eq!(preferred_mime(&[]), None);
    }

    #[test]
    fn oversized_payloads_are_rejected_not_truncated() {
        assert_eq!(MAX_SELECTION_BYTES, 1024 * 1024);
        assert!(!too_large("hello"));
        assert!(!too_large(&"a".repeat(MAX_SELECTION_BYTES)));
        assert!(too_large(&"a".repeat(MAX_SELECTION_BYTES + 1)));
    }
}
