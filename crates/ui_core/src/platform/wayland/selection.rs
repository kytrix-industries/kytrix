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

/// 进程内**所有**未完成 `wl_data_source.send` 的字节总量上限（R10-17 / AR3 §3.4）。
///
/// 对端可以永远不读 pipe，因此「每笔上限」还不够：挂起的对端会长期占住 fd 与内存。这个
/// 总预算把「未写出的数据」变成常量；放不下时按协议直接关闭 fd（对端读到 EOF）。
pub(crate) const MAX_PENDING_SEND_BYTES: usize = 4 * 1024 * 1024;

/// 单笔 `send` 的存活上限：超过之后即使对端还没读也放弃（关闭 fd，对端读到 EOF）。
///
/// 只覆盖「读得很慢」这类可观测情形 —— 对端**完全不读**时不会再有可写事件，截断由总预算
/// [`MAX_PENDING_SEND_BYTES`] 保证。消费者只有 Linux 的 `wl_data_source.send` 驱动，
/// 因此在开发机（Windows）的测试构建里不需要编译它。
#[cfg(target_os = "linux")]
pub(crate) const SEND_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);

/// 发布方向一个数据源的世代号（单调递增、进程内不重复）。
///
/// 用世代号而不是对象 id：`wl_data_source` 的对象 id 由 compositor 分配且可能复用（同一
/// 连接内 id 空间按 new_id 递增，实际上不复用，但世代号让状态机不依赖协议 id 的语义）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct SourceGeneration(u64);

/// 发布方向（`wl_data_source`）的状态机。
///
/// 调用顺序（由 [`publish_selection`] 驱动，生产实现见 `platform/linux/clipboard.rs`）：
///
/// ```text
/// publish(text) ──► SourceGeneration
///                     │  调用方：destroy(旧 source) → create_data_source → offer(mime)
///                     │          → set_selection(serial)
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
    /// 发布一段文本：分配新世代。
    ///
    /// **每次调用都必须新建一个 `wl_data_source`**：不能复用上一次发布的那个对象（它已经
    /// `cancelled` 失效，复用会让 compositor 把新选区的文本清掉）。这条纪律由
    /// [`publish_selection`] 执行、由它的单测钉住。
    pub(crate) fn publish(&mut self, text: SmolStr) -> SourceGeneration {
        let generation = SourceGeneration(self.next_generation);
        self.next_generation += 1;
        self.published = Some(PublishedSource { generation, text });
        generation
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

// ── 发布动作的注入接口（AR3 §3.1 / P2-7）────────────────────────────────

/// 发布一次选区所需的**协议动作**（`wl_data_source` 的创建与销毁）。
///
/// 抽成 trait 是为了让「一次复制一个新 source、旧的必须 `destroy`、绝不复用」这条协议纪律
/// 可以在开发机（Windows）上单测：真实实现由 `platform/linux/clipboard.rs` 提供（持有
/// `wl_data_device_manager` / `wl_data_device` 代理），测试实现只记录调用顺序。
pub(crate) trait SelectionBackend {
    /// 数据源代理类型（生产：`wl_data_source::WlDataSource`）。
    type Source;

    /// 新建一个 `wl_data_source` 并 `offer` 全部文本 mime（顺序即偏好）。
    fn create_source(&mut self) -> Self::Source;

    /// 销毁一个不再需要的 source（`wayland.xml` 的 `wl_data_source.cancelled`：*"The client
    /// should clean up and **destroy** this data source."*）。
    fn destroy_source(&mut self, source: Self::Source);

    /// 成为选区属主（`wl_data_device.set_selection`，`serial` 是最近的输入 serial）。
    fn set_selection(&mut self, source: &Self::Source, serial: u32);
}

/// 当前持有的 source：世代号与协议代理一一对应（`send` 按世代取文本，R10-1）。
#[derive(Debug)]
pub(crate) struct LiveSource<S> {
    /// 这个 source 对应的发布世代。
    pub generation: SourceGeneration,
    /// 协议代理（生产：`wl_data_source`）。
    pub source: S,
}

/// 发布一次选区：**总是**销毁旧 source 并新建一个（R10-1）。
///
/// 协议动作顺序固定（`destroy` → `create` → `offer` → `set_selection`），由 [`SelectionBackend`]
/// 注入执行；返回新世代号，`held` 换成新的 [`LiveSource`]。
///
/// 为什么不能复用旧代理：compositor 在选区被替换时给**旧** source 发 `cancelled`，而旧 source
/// 就是同一个代理 ⇒ 客户端把刚发布的文本清掉 ⇒ 对端 `send` 读到 EOF ⇒ 粘贴为空串。
pub(crate) fn publish_selection<B: SelectionBackend>(
    backend: &mut B,
    sources: &mut SelectionSources,
    held: &mut Option<LiveSource<B::Source>>,
    text: SmolStr,
    serial: u32,
) -> SourceGeneration {
    let generation = sources.publish(text);
    // 旧 source 已经失效：先销毁，再新建（顺序固定，测试按调用顺序断言）。
    if let Some(previous) = held.take() {
        backend.destroy_source(previous.source);
    }
    let source = backend.create_source();
    backend.set_selection(&source, serial);
    *held = Some(LiveSource { generation, source });
    generation
}

// ── 发出方向的字节预算（R10-17 / AR3 §3.4）────────────────────────────────

/// 未完成 `wl_data_source.send` 的字节预算（进程内唯一一份，见 [`MAX_PENDING_SEND_BYTES`]）。
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

    /// 为 `bytes` 字节预留预算；放不下返回 `false`（调用方应按协议关闭 fd，对端读到 EOF）。
    pub(crate) fn admit(&mut self, bytes: usize) -> bool {
        if bytes > self.limit.saturating_sub(self.used) {
            return false;
        }
        self.used += bytes;
        true
    }

    /// 一笔写出结束（写完 / 出错 / 被截止 / 被淘汰）时归还预算。
    pub(crate) fn release(&mut self, bytes: usize) {
        self.used = self.used.saturating_sub(bytes);
    }

    /// 当前占用（**只有测试需要**：生产路径只关心「能不能放下」，记账是内部的）。
    #[cfg(test)]
    pub(crate) fn used(&self) -> usize {
        self.used
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

    #[cfg(test)]
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
        LiveSource, MAX_PENDING_SEND_BYTES, MAX_SELECTION_BYTES, MIME_TEXT, OfferTable,
        SelectionBackend, SelectionSources, SendBudget, preferred_mime, publish_selection,
        too_large,
    };

    fn offered(list: &[&str]) -> Vec<SmolStr> {
        list.iter().map(|mime| SmolStr::new(*mime)).collect()
    }

    // ── 发布动作的注入接口（AR3 §3.1 / P2-7）────────────────────────────
    //
    // 记录调用顺序的假后端：`Source` 只是一个 id，真实实现（`clipboard.rs`）持有
    // `wl_data_source` 代理。这样「每次发布都新建、旧的一定 destroy」这条纪律就有断言，
    // 而不是「单测全绿、真实剪贴板在第二次复制后失效」。

    /// 测试用的假 source（真实实现是 `wl_data_source::WlDataSource`）。
    type FakeSource = u32;

    struct RecordingBackend {
        next: FakeSource,
        /// 按发生顺序记录的动作（`create(2)` / `destroy(1)` / `select(2, serial)`）。
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

    /// R10-1 的核心回归：第二次复制必须 `destroy` 旧 source 并 `create` 一个新的，
    /// 顺序固定为 `destroy → create → select`（旧实现复用代理 ⇒ 粘贴为空）。
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

    /// `cancelled` 之后（我们不是选区属主）再发布：没有旧 source 可销毁。
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
        // `wl_data_source.cancelled`：协议要求销毁这个 source，调用方清空持有项。
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

    // ── 发出方向的字节预算（AR3 §3.4）──────────────────────────────────

    /// 总预算把「对端永远不读」的内存占用变成常量：放不下的那一笔必须被拒绝
    /// （调用方关闭 fd，对端读到 EOF）。
    #[test]
    fn the_send_budget_rejects_a_payload_that_does_not_fit() {
        let mut budget = SendBudget::new(MAX_PENDING_SEND_BYTES);
        assert_eq!(budget.used(), 0);
        assert!(budget.admit(MAX_SELECTION_BYTES));
        assert_eq!(budget.used(), MAX_SELECTION_BYTES);

        // 再塞满到上限。
        let rest = MAX_PENDING_SEND_BYTES - MAX_SELECTION_BYTES;
        assert!(budget.admit(rest));
        assert_eq!(budget.used(), MAX_PENDING_SEND_BYTES);
        assert!(
            !budget.admit(1),
            "超出总预算的 send 必须被拒绝，而不是无限攒内存"
        );

        // 一笔结束（写完 / 出错 / 被截止）之后预算归还，下一笔可以进来。
        budget.release(MAX_SELECTION_BYTES);
        assert_eq!(budget.used(), MAX_PENDING_SEND_BYTES - MAX_SELECTION_BYTES);
        assert!(budget.admit(MAX_SELECTION_BYTES));
    }

    /// 单笔大于整个预算：直接拒绝（不能因为「还没占用」就放行，否则上界失效）。
    #[test]
    fn a_single_send_larger_than_the_budget_is_rejected() {
        let mut budget = SendBudget::new(1024);
        assert!(!budget.admit(2048));
        assert_eq!(budget.used(), 0, "被拒绝的预留不得记账");
        assert!(budget.admit(1024));
        assert_eq!(budget.used(), 1024);
    }

    /// 归还不会下溢（重复归还 / 未预留就归还都是安全的）。
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

    /// R10-1 的核心回归：第二次复制必须换一个新的 source，并且旧世代的数据**不再可用**。
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

    /// R10-1 的第三个失败面：自我替换时 compositor 会先给**旧** source 发 `cancelled`，
    /// 而那时新世代已经接管选区 —— 迟到的旧事件不得清掉新文本。
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

    /// `send` 的原始 text 必须按 source 世代匹配（R10-1 修复方案第 3 条）。
    #[test]
    fn send_data_is_routed_by_source_generation() {
        let mut sources = SelectionSources::default();
        let first = sources.publish(SmolStr::new_static("old"));
        let second = sources.publish(SmolStr::new_static("new"));
        // 两个 source 的 send 交错到达（对端可能同时粘贴两次）。
        assert_eq!(sources.take_text(second), Some(&SmolStr::new_static("new")));
        assert_eq!(sources.take_text(first), None);
        assert_ne!(first, second);
        assert!(second > first);
    }

    /// 世代号单调递增；「未发布」时 `take_text` 对任何世代都返回 `None`。
    #[test]
    fn an_unpublished_state_serves_no_text_for_any_generation() {
        let sources = SelectionSources::default();
        let mut probe = SelectionSources::default();
        let generation = probe.publish(SmolStr::new_static("x"));
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
