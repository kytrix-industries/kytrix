//! 选区（剪贴板）的协议无关状态机：`wl_data_source` 的一次性生命周期（R10-1）、
//! `wl_data_offer` 的归属（R10-9）、mime 协商与大小上限（R10-17 的发送侧）。
//!
//! # 为什么必须「一次复制一个新 source」
//!
//! `wayland.xml` 的 `wl_data_source.cancelled` 原文：*"This data source is no longer valid …
//! The client should clean up and **destroy** this data source."* 一个 `wl_data_source` 只服务
//! **一次**「成为选区属主」的操作；compositor 在选区被替换时会给旧 source 发 `cancelled`。
//!
//! 复用同一个 source 做第二次 `set_selection` 的后果（R10-1 的现场）：
//!
//! 1. 第一次复制 A：`set_selection(source)`；
//! 2. 第二次复制 B：对**同一个** source 再 `set_selection` ⇒ compositor 先给「旧选区」发
//!    `cancelled`，而旧选区的 source 就是同一个代理 ⇒ 客户端把已发布文本清成 `None`；
//! 3. 此刻生效的选区是 B，但对端 `send` 到达时客户端已经没有文本 ⇒ 关闭 fd ⇒ 对端读到 EOF，
//!    **粘贴得到空串**。
//!
//! 因此本模块按「世代号（generation）」建模：每次发布分配新的世代，`send` 必须按**发起它的
//! source 世代**取文本；`cancelled` 只对「当前世代」清文本，迟到的旧世代 `cancelled` 不得
//! 清掉新世代的数据（自我替换时 compositor 会先给旧 source 发 `cancelled`，那条事件在新世代
//! 建立**之后**才到达客户端）。
//!
//! GTK4（`gtk_clipboard_set_content` 每次 `create_data_source`、`cancelled` 里 `destroy`）、
//! Chromium（`WaylandDataSource` one-shot）、Qt（`QWaylandDataSource`）、SDL3 都是这个模型。

use smol_str::SmolStr;

/// 支持的 mime，**顺序即偏好**（发布时按此顺序 offer，接收时按此顺序挑第一个对端也支持的）。
pub(crate) const MIME_TEXT: [&str; 3] = ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING"];

/// 一次选区传输的字节上限（读方向对端可能写无穷；写方向我们自己限，避免主线程被大段文本
/// 拖住，见 R10-17）。
pub(crate) const MAX_SELECTION_BYTES: usize = 1024 * 1024;

/// 发布方向一个数据源的世代号（单调递增、进程内不重复）。
///
/// 用世代号而不是对象 id：`wl_data_source` 的对象 id 由 compositor 分配且可能复用（同一
/// 连接内 id 空间按 new_id 递增，实际上不复用，但世代号让状态机不依赖协议 id 的语义）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct SourceGeneration(u64);

/// [`SelectionSources::publish`] 的结果：调用方据此建/销毁协议对象。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PublishOutcome {
    /// 新数据源的世代号（`wl_data_source` 从此与它一一对应）。
    pub generation: SourceGeneration,
    /// `true` ⇒ 上一次发布的数据源仍然存活，调用方**必须**先 `destroy()` 它
    /// （协议要求一个 source 只服务一次选区）。
    pub replaced_live_source: bool,
}

/// 发布方向（`wl_data_source`）的状态机。
///
/// 调用顺序（由 `platform/linux/clipboard.rs` 驱动）：
///
/// ```text
/// publish(text) ──► PublishOutcome { generation, replaced_live_source }
///                     │  调用方：if replaced ⇒ 旧 source.destroy()
///                     │           create_data_source + offer(mime) + set_selection(serial)
/// cancelled(gen) ───► 只对当前世代清文本；调用方永远 destroy 该世代对应的 source
/// take_text(gen) ───► wl_data_source.send 的数据源（世代不匹配 ⇒ 没有数据）
/// ```
#[derive(Debug, Default)]
pub(crate) struct SelectionSources {
    /// 当前有效的发布（`None` = 我们不是选区属主）。
    published: Option<PublishedSource>,
    /// 下一个世代号（`publish` 自增）。
    next_generation: u64,
}

#[derive(Debug)]
struct PublishedSource {
    generation: SourceGeneration,
    /// 已发布文本（`send` 时写进 pipe）。
    text: SmolStr,
}

impl SelectionSources {
    /// 发布一段文本：分配新世代，返回调用方需要知道的动作。
    ///
    /// **每次调用都必须新建一个 `wl_data_source`**：不能复用 `replaced_live_source` 为
    /// `false` 的那个旧对象（那个对象已经 `cancelled` 失效）。
    pub(crate) fn publish(&mut self, text: SmolStr) -> PublishOutcome {
        let replaced_live_source = self.published.is_some();
        let generation = SourceGeneration(self.next_generation);
        self.next_generation += 1;
        self.published = Some(PublishedSource { generation, text });
        PublishOutcome {
            generation,
            replaced_live_source,
        }
    }

    /// `wl_data_source.cancelled`：返回 `true` 表示这条事件属于**当前**世代（已发布文本被清掉）。
    ///
    /// 迟到的旧世代事件返回 `false`：自我替换时 compositor 会对旧 source 补发 `cancelled`，
    /// 而那时新世代已经接管选区 —— 不能顺手把新文本清掉（R10-1 的第三个失败面）。
    ///
    /// **无论返回值是什么，调用方都必须 `destroy()` 该事件对应的 source**。
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

    /// `wl_data_source.send` 要写的文本：按**发起 send 的 source 世代**取，世代不匹配 ⇒ `None`
    /// （说明那个 source 已经 `cancelled`，此时按协议直接关闭 fd，对端读到 EOF）。
    #[must_use]
    pub(crate) fn take_text(&self, generation: SourceGeneration) -> Option<&SmolStr> {
        self.published
            .as_ref()
            .filter(|published| published.generation == generation)
            .map(|published| &published.text)
    }

    /// 我们不再是选区属主（例如 `wl_data_device.selection` 指向别人的 offer、或窗口全部关闭）。
    ///
    /// 返回被丢弃的世代（调用方据此销毁对应 source）。
    pub(crate) fn clear(&mut self) -> Option<SourceGeneration> {
        self.published.take().map(|published| published.generation)
    }
}

/// 一条 `wl_data_offer` 的登记项。
#[derive(Clone, Debug)]
struct OfferEntry {
    /// 本进程自增的内部 offer id（**不是**协议 object id）。
    id: OfferId,
    /// `wl_data_offer.offer` 逐条累积的 mime。
    mimes: Vec<SmolStr>,
}

/// 本进程自增的 offer id。
///
/// 为什么不用协议 object id：`ObjectId::protocol_id()` 确实存在
/// （`wayland-backend-0.3.17/src/client_api.rs:128`），但它的文档明确写着
/// *"Protocol IDs are reused after object destruction, so this should not be used as a unique
/// identifier, instead use the [`ObjectId`] directly"*。而纯逻辑层不能持有 `ObjectId`（那会把
/// `wayland-client` 依赖带进这个「可在开发机单测」的模块），所以由本表自己发号；Linux 侧用
/// **代理相等性**把事件里的 `wl_data_offer` 映射回内部 id —— wayland-scanner 生成的
/// `impl PartialEq for <iface>` 正是 `self.id == other.id`
/// （`wayland-scanner-0.31.11/src/client_gen.rs:80-84`），语义等价且不依赖未公开 API。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct OfferId(u32);

/// 收到的 offer 表。
///
/// `wl_data_device.selection` 事件**自带**当前选区的 offer（R10-9）：旧实现用「最近一次
/// `data_offer` 事件」当选区来源，只要发生过一次拖放（`data_offer` → `offer` → `enter`，
/// 而不是 `selection`）就会把拖放的 offer 当成剪贴板，于是 `receive` 到与选区无关的数据。
#[derive(Debug, Default)]
pub(crate) struct OfferTable {
    offers: Vec<OfferEntry>,
    /// 下一个内部 id（单调递增，一次运行内不重复）。
    next_id: u32,
}

impl OfferTable {
    /// `wl_data_device.data_offer`：登记新 offer 并返回它的内部 id。
    pub(crate) fn begin(&mut self) -> OfferId {
        let id = OfferId(self.next_id);
        self.next_id += 1;
        self.offers.push(OfferEntry {
            id,
            mimes: Vec::with_capacity(MIME_TEXT.len()),
        });
        id
    }

    /// `wl_data_offer.offer`：给对应 offer 追加一个 mime。
    ///
    /// 找不到登记项时忽略（协议保证 `data_offer` 先到；防御乱序/已清理的 offer）。
    pub(crate) fn add_mime(&mut self, id: OfferId, mime: SmolStr) {
        if let Some(entry) = self.offers.iter_mut().find(|entry| entry.id == id) {
            entry.mimes.push(mime);
        }
    }

    /// 该 offer 里按 [`MIME_TEXT`] 偏好选出的 mime（`None` = 没有可用的文本类型，
    /// 或这个 id 不在表里）。
    #[must_use]
    pub(crate) fn preferred_mime(&self, id: OfferId) -> Option<SmolStr> {
        let entry = self.offers.iter().find(|entry| entry.id == id)?;
        preferred_mime(&entry.mimes)
    }

    /// 移除并返回该 offer 的 mime 列表（`selection` 处理完 / 拖放结束 / 离开时调用）。
    ///
    /// 返回 `Some` ⇒ 调用方分管这个 offer 的生命周期（处理完 `receive` 后 `destroy()`）。
    pub(crate) fn take(&mut self, id: OfferId) -> Option<Vec<SmolStr>> {
        let index = self.offers.iter().position(|entry| entry.id == id)?;
        Some(self.offers.remove(index).mimes)
    }

    /// `wl_data_device.leave`：拖放离开，之后所有未处理的 offer 都作废。
    ///
    /// 返回被丢弃的内部 id（调用方据此 `destroy()` 对应代理，否则服务端对象泄漏，R10-34）。
    pub(crate) fn drain(&mut self) -> Vec<OfferId> {
        self.offers.drain(..).map(|entry| entry.id).collect()
    }
}

/// 按 [`MIME_TEXT`] 的偏好挑一个双方都支持的 mime。
pub(crate) fn preferred_mime(offered: &[SmolStr]) -> Option<SmolStr> {
    MIME_TEXT
        .iter()
        .find(|wanted| offered.iter().any(|mime| mime.as_str() == **wanted))
        .map(|wanted| SmolStr::new_static(wanted))
}

/// 发布文本是否超出 [MAX_SELECTION_BYTES]（发送侧上限，R10-17）。
///
/// 超出时**不截断**：截断会让用户粘贴到半个地址/金额（交易终端的语义错误比失败更危险），
/// 调用方应报 `EngineError::Clipboard` 并保持旧选区不变。
#[must_use]
pub(crate) fn too_large(text: &str) -> bool {
    text.len() > MAX_SELECTION_BYTES
}

#[cfg(test)]
mod tests {
    use smol_str::SmolStr;

    use super::{
        MAX_SELECTION_BYTES, MIME_TEXT, OfferTable, SelectionSources, preferred_mime, too_large,
    };

    fn offered(list: &[&str]) -> Vec<SmolStr> {
        list.iter().map(|mime| SmolStr::new(*mime)).collect()
    }

    /// R10-1 的核心回归：第二次复制必须换一个新的 source，并且旧世代的数据**不再可用**。
    #[test]
    fn a_second_copy_uses_a_new_generation() {
        let mut sources = SelectionSources::default();
        let first = sources.publish(SmolStr::new_static("A"));
        assert!(!first.replaced_live_source, "第一次发布没有旧 source");
        assert_eq!(
            sources.take_text(first.generation),
            Some(&SmolStr::new_static("A"))
        );

        let second = sources.publish(SmolStr::new_static("B"));
        assert!(
            second.replaced_live_source,
            "旧 source 还活着 ⇒ 调用方必须销毁它（协议：一个 source 只服务一次选区）"
        );
        assert!(second.generation > first.generation, "世代号必须单调递增");
        assert_eq!(
            sources.take_text(first.generation),
            None,
            "旧世代的 source 已经失效，send 不得再取到文本"
        );
        assert_eq!(
            sources.take_text(second.generation),
            Some(&SmolStr::new_static("B"))
        );
    }

    /// R10-1 的第三个失败面：自我替换时 compositor 会先给**旧** source 发 `cancelled`，
    /// 而那时新世代已经接管选区 —— 迟到的旧事件不得清掉新文本。
    #[test]
    fn a_late_cancelled_for_the_replaced_source_keeps_the_new_text() {
        let mut sources = SelectionSources::default();
        let first = sources.publish(SmolStr::new_static("A"));
        let second = sources.publish(SmolStr::new_static("B"));

        assert!(
            !sources.cancelled(first.generation),
            "旧世代的 cancelled 不得清掉当前发布"
        );
        assert_eq!(
            sources.take_text(second.generation),
            Some(&SmolStr::new_static("B")),
            "新选区的数据必须仍然可发送（旧实现就是在这里丢掉了 B）"
        );

        assert!(sources.cancelled(second.generation), "当前世代被取消");
        assert_eq!(sources.take_text(second.generation), None);
        assert_eq!(sources.clear(), None, "取消之后已经不再是选区属主");
    }

    #[test]
    fn cancelling_the_current_generation_is_idempotent() {
        let mut sources = SelectionSources::default();
        let published = sources.publish(SmolStr::new_static("x"));
        assert!(sources.cancelled(published.generation));
        assert!(
            !sources.cancelled(published.generation),
            "第二次 cancelled（或迟到的重复事件）不得再报告「清掉了当前发布」"
        );
    }

    #[test]
    fn clearing_reports_the_generation_so_the_callers_can_destroy_it() {
        let mut sources = SelectionSources::default();
        let published = sources.publish(SmolStr::new_static("x"));
        assert_eq!(sources.clear(), Some(published.generation));
        assert_eq!(sources.clear(), None, "没有发布时 clear 是 no-op");
        assert_eq!(sources.take_text(published.generation), None);
    }

    /// `send` 的原始 text 必须按 source 世代匹配（R10-1 修复方案第 3 条）。
    #[test]
    fn send_data_is_routed_by_source_generation() {
        let mut sources = SelectionSources::default();
        let first = sources.publish(SmolStr::new_static("old"));
        let second = sources.publish(SmolStr::new_static("new"));
        // 两个 source 的 send 交错到达（对端可能同时粘贴两次）。
        assert_eq!(
            sources.take_text(second.generation),
            Some(&SmolStr::new_static("new"))
        );
        assert_eq!(sources.take_text(first.generation), None);
        assert_ne!(first.generation, second.generation);
        assert!(second.generation > first.generation);
    }

    /// 世代号单调递增；「未发布」时 `take_text` 对任何世代都返回 `None`。
    #[test]
    fn an_unpublished_state_serves_no_text_for_any_generation() {
        let sources = SelectionSources::default();
        let mut probe = SelectionSources::default();
        let generation = probe.publish(SmolStr::new_static("x")).generation;
        assert_eq!(sources.take_text(generation), None);
    }

    /// R10-9：`selection` 事件按它**自带的** offer 匹配，而不是「最近一次 data_offer」。
    ///
    /// 现场：拖放先到（offer B），随后剪贴板选区到（offer A）。旧实现会去 `receive` 最近那条，
    /// 拿到与选区无关的数据。
    #[test]
    fn selection_uses_the_offer_carried_by_the_event() {
        let mut table = OfferTable::default();
        // 先到的拖放 offer（只有图片），后到的剪贴板 offer（文本）。
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

    /// 未登记的内部 id 一律 `None`（`selection` 指向一个我们没有 `data_offer` 的代理）。
    #[test]
    fn an_unknown_offer_id_has_no_mime() {
        let mut table = OfferTable::default();
        let known = table.begin();
        table.add_mime(known, SmolStr::new_static("text/plain"));
        // 内部 id 单调递增：再造一个 id 就是「表里没有」的那个。
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

    /// 每条 `data_offer` 都分配新的内部 id：旧条目由调用方销毁，不会与新条目混淆。
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

    /// R10-17：发送侧的上限与读侧同量级，超限不截断（截断会静默改坏粘贴内容）。
    #[test]
    fn oversized_payloads_are_rejected_not_truncated() {
        assert_eq!(MAX_SELECTION_BYTES, 1024 * 1024);
        assert!(!too_large("hello"));
        assert!(!too_large(&"a".repeat(MAX_SELECTION_BYTES)));
        assert!(too_large(&"a".repeat(MAX_SELECTION_BYTES + 1)));
    }
}
