//! 无 GPU 的测试引擎（I7 / 设计 §6.6）：`TestApp` / `TestWindow`。
//!
//! `TestApp::new()` 的 renderer 恒为 `None`：`Scene` 照常生成、记录照常抓取、present 是 no-op。
//! `drain()` 是**唯一**帧推进原语：一次最多一帧，不做「等到没事干」的隐式阻塞（设计 §6.2）。
//!
//! **按平台泵的顺序推进**：`drain` = 帧 → a11y 交付 → 跨线程回投任务（R13-12）。三者同序是
//! 有意的：生产里「任务写状态 ⇒ 下一轮才出帧」的时序 bug 只有在测试泵也走 `Dispatcher` 的
//! push → drain → run 三层时才会现形（旧实现 `TestApp` 根本没有任务泵，a11y 用例全走
//! `dispatch_request` 直调，把这一层整个跳过了）。

use std::time::Duration;

use super::{
    a11y::ShadowTree,
    probe::Probe,
    record::{FrameRecord, RenderedFrame},
    snapshot::{SnapshotCase, SnapshotError, SnapshotImage},
};
use crate::{
    app::{App, AppHandle},
    error::EngineError,
    event::{Event, InputEvent, InputOutcome},
    window::{FrameOutcome, WindowId, WindowOptions},
};

/// 无 GPU 的测试引擎：持有 runtime + `App` + 一个窗口。
pub struct TestApp {
    // `App::new` 需要 `Handle`；current_thread runtime 足够（P06 不 spawn）。
    // 字段只为保活（`App` 只持 `Handle`），不提供访问器。
    _runtime: tokio::runtime::Runtime,
    app: App,
    window: WindowId,
    /// 最近一次**提交**的绘制快照（空闲帧不会产生 `frame`，oracle 的增量侧要回退到这里）。
    last_rendered: Option<RenderedFrame>,
    /// 是否在每帧之后交付一笔 a11y 快照（`enable_a11y_capture` 打开）。
    a11y_capture: bool,
    /// 最近一笔 a11y 交付对应的 **`TreeUpdate`**（与生产 `build_update` 同一条组装路径）。
    ///
    /// R13-10：全仓此前没有一处 `TreeUpdate` 断言，语义 bug 全部停在"快照"这一层。
    last_a11y_update: Option<accesskit::TreeUpdate>,
    /// a11y 影子树（逐笔应用 `TreeUpdate` 并校验 AccessKit 契约）。
    a11y_shadow: ShadowTree,
    /// 上一笔 `TreeUpdate` 的契约校验错误（`Some` ⇒ 本帧的更新违反 AccessKit 契约）。
    a11y_contract_error: Option<String>,
    /// 已执行的回投任务数（R13-12 的断言面：push → drain → run 三层真的走通）。
    tasks_run: usize,
}

impl Default for TestApp {
    fn default() -> Self {
        Self::new()
    }
}

impl TestApp {
    /// 默认窗口（800×600、`Scale::ONE`）的 headless 引擎。
    #[must_use]
    pub fn new() -> Self {
        Self::with_options(WindowOptions::default())
    }

    /// 指定建窗参数的 headless 引擎。
    ///
    /// # Panics
    /// 当前线程的 tokio runtime 建不起来时 panic（测试环境的基础设施故障，不是引擎行为）。
    #[must_use]
    pub fn with_options(options: WindowOptions) -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let mut app = App::new(runtime.handle().clone());
        let window = app.create_window(options);
        Self {
            _runtime: runtime,
            app,
            window,
            last_rendered: None,
            a11y_capture: false,
            last_a11y_update: None,
            a11y_shadow: ShadowTree::new(),
            a11y_contract_error: None,
            tasks_run: 0,
        }
    }

    /// 当前窗口句柄。
    #[must_use]
    pub fn window_id(&self) -> WindowId {
        self.window
    }

    /// 只读 `App`（写信号 / 建树需要 `app_mut`）。
    #[must_use]
    pub fn app(&self) -> &App {
        &self.app
    }

    /// 可变 `App`（`NodeBuilder::build` / `App::write` / 字体栈）。
    #[must_use]
    pub fn app_mut(&mut self) -> &mut App {
        &mut self.app
    }

    /// 推进**一帧**（空闲帧也返回记录：`rendered == false`）。
    ///
    /// 一次 `drain` = 生产泵的一轮：帧 → a11y 交付 → 跨线程回投任务（R13-12）。
    ///
    /// 捕获打开时，本帧的 a11y 交付**立即**挂到记录上（不看 `A11Y_THROTTLE`）：
    /// 这是断言面最直接的语义（「本帧有没有 a11y 工作」）。
    ///
    /// # Errors
    /// `EngineError::InvalidMeasure` / `TextShape` / `AtlasExhausted`：本帧已中止（未提交），
    /// damage/dirty 保留，下一次 `drain` 重试。
    pub fn drain(&mut self) -> Result<&FrameRecord, EngineError> {
        self.step(A11yGate::Immediate)
    }

    /// 推进一帧，a11y 交付走**平台泵**的闸门：单点数据增量在 `A11Y_THROTTLE`（100 ms）内不
    /// 交付；焦点变化 / 消费端重置 / 分批未走完（首树、DPI 重建）走不节流旁路（E5）。
    ///
    /// 帧脚本（[`super::script`]）用它断言「第 k 帧的 a11y 交付被节流」——节流是平台泵的行为，
    /// [`Self::drain`] 的立即交付抓不到它。
    ///
    /// # Errors
    /// 同 [`Self::drain`]。
    pub fn drain_pump(&mut self) -> Result<&FrameRecord, EngineError> {
        self.step(A11yGate::Pump)
    }

    /// 两条 drain 路径的共同实现（a11y 交付闸门是唯一差异）。
    fn step(&mut self, gate: A11yGate) -> Result<&FrameRecord, EngineError> {
        let outcome = self
            .app
            .frame(self.window)
            .expect("TestApp's window must exist");
        outcome?;
        // P11：a11y 交付在**帧之后**（与平台泵的 `update_a11y` 同序）。纯 `A11Y` 变化不提交帧，
        // 所以这一步与 `FrameOutcome` 无关（`FrameRecord.a11y` 在空闲帧也存在）。
        let deliver = self.a11y_capture
            && match gate {
                A11yGate::Immediate => true,
                A11yGate::Pump => self.app.a11y_pending(self.window),
            };
        self.last_a11y_update = None;
        self.a11y_contract_error = None;
        if deliver && let Some(snapshot) = self.app.a11y_take(self.window) {
            // 与生产**完全同一条**组装路径：`to_tree_update` 是平台泵闭包里的那个函数。
            let update = crate::a11y::adapter::to_tree_update(&snapshot);
            if let Err(error) = self.a11y_shadow.apply(&update) {
                self.a11y_contract_error = Some(error);
            }
            self.last_a11y_update = Some(update);
            self.app.attach_a11y(self.window, snapshot);
        }
        // ⑤ 跨线程回投的任务（R13-12）：与平台泵**同序**（帧 → a11y → 任务）。任务里写下的
        // 状态因此在本轮帧之后才可见 —— 依赖 dispatcher 时序的 bug（"任务写状态但当轮不出帧"
        // 或"任务在帧之前跑"）在这条路径上会直接现形。
        self.pump_tasks();
        if let Some(frame) = self.frame_record().frame.clone() {
            self.last_rendered = Some(frame);
        }
        Ok(self.frame_record())
    }

    /// 执行本轮回投的任务（消费者：`step`；与平台泵的 ⑤ 一一对应）。
    fn pump_tasks(&mut self) {
        let dispatcher = self.app.dispatcher();
        for task in dispatcher.drain_tasks() {
            self.tasks_run += 1;
            task.run(&mut self.app);
        }
    }

    /// 跨线程句柄（`App::handle` 的转发）：测试可以用 `send` 真走
    /// `Dispatcher::push → drain_tasks → UserTask::run` 三层（R13-12）。
    #[must_use]
    pub fn handle(&self) -> AppHandle {
        self.app.handle()
    }

    /// 累计执行过的回投任务数（R13-12 的断言面）。
    #[must_use]
    pub fn tasks_run(&self) -> usize {
        self.tasks_run
    }

    /// 最近一次 `drain` 的调度记录。
    #[must_use]
    pub fn frame_record(&self) -> &FrameRecord {
        self.app
            .window(self.window)
            .expect("TestApp's window must exist")
            .record()
            .expect("drain() must have produced a frame record")
    }

    /// 最近一次提交的绘制快照（空闲帧 → `None`）。
    #[must_use]
    pub fn rendered(&self) -> Option<&RenderedFrame> {
        self.frame_record().frame.as_ref()
    }

    /// 探针（节点树 + 场景只读）。
    #[must_use]
    pub fn probe(&self) -> Probe<'_> {
        let window = self
            .app
            .window(self.window)
            .expect("TestApp's window must exist");
        Probe::new(window.tree())
    }

    /// 打开 a11y 快照捕获：之后每次 `drain` 都把本帧的交付写进 `FrameRecord.a11y`。
    ///
    /// 首帧恒为全树（`A11yBridge.full_pending`），捕获开关只允许一个 drainer（生产里是平台
    /// adapter）：同一时刻只允许一个 drainer。
    pub fn enable_a11y_capture(&mut self) {
        self.a11y_capture = true;
        self.app.set_a11y_capture(self.window, true);
    }

    /// 最近一帧的 a11y 交付（`None` = 本帧没有 a11y 工作，或捕获未打开）。
    #[must_use]
    pub fn a11y(&self) -> Option<&crate::a11y::A11yTreeSnapshot> {
        self.frame_record().a11y.as_ref()
    }

    /// 最近一帧的 **`TreeUpdate`**（生产 `build_update` 的同一条组装路径）。
    ///
    /// 与 [`Self::a11y`] 的区别是"测试面与生产面统一"：AT 实际收到的是 `TreeUpdate`，
    /// 快照只是引擎侧的中间表示（R13-5/R13-10）。
    #[must_use]
    pub fn a11y_update(&self) -> Option<&accesskit::TreeUpdate> {
        self.last_a11y_update.as_ref()
    }

    /// 最近一帧 `TreeUpdate` 的契约校验错误（`None` = 合法）。
    #[must_use]
    pub fn a11y_contract_error(&self) -> Option<&str> {
        self.a11y_contract_error.as_deref()
    }

    /// 最近一帧的 `TreeUpdate` 必须是**合法**的 AccessKit 更新。
    ///
    /// # Panics
    /// 违反契约（错误信息指出哪个 id、哪条规则）。
    pub fn assert_a11y_contract(&self) {
        if let Some(error) = &self.a11y_contract_error {
            panic!("TreeUpdate violates the AccessKit contract: {error}");
        }
    }

    /// a11y 影子树的摘要（`id -> 语义摘要`；全量/增量一致性见 [`Self::a11y_oracle_diff`]）。
    #[must_use]
    pub fn a11y_shadow_summaries(&self) -> ahash::HashMap<accesskit::NodeId, String> {
        self.a11y_shadow.summaries()
    }

    /// a11y oracle：**增量累积**出来的树 vs **强制全树**重建的树。
    ///
    /// 这是 a11y 版的 `assert_oracle`（R13-11）：所有"忘记 `mark_a11y`"的 bug 都会让两条路径
    /// 分叉，变成红灯。比较项：节点集合 / 每节点语义摘要 / 父子结构。
    ///
    /// 需要先 `enable_a11y_capture()`（否则没有交付可以比较）。
    pub fn a11y_oracle_diff(&mut self) -> Option<String> {
        assert!(
            self.a11y_capture,
            "a11y oracle requires enable_a11y_capture()"
        );
        // ① 增量侧：当前状态照常交付，影子树是"逐笔累积"的结果。
        let _ = self.drain().expect("incremental a11y frame");
        let incremental = self.a11y_shadow.summaries();
        let incremental_structure = self.a11y_shadow.structure();
        // ② 全量侧：强制全树交付，用一份新的影子树描述"从头建"的结果。
        self.app
            .window_mut(self.window)
            .expect("TestApp's window must exist")
            .a11y_force_full();
        let _ = self.drain().expect("full a11y frame");
        let Some(update) = self.last_a11y_update.clone() else {
            return Some(String::from(
                "forced full a11y delivery produced no TreeUpdate",
            ));
        };
        let mut full = ShadowTree::new();
        if let Err(error) = full.apply(&update) {
            return Some(format!(
                "forced full TreeUpdate violates the contract: {error}"
            ));
        }
        let complete = full.summaries();
        if let Some(error) = self.a11y_contract_error.as_ref() {
            return Some(format!(
                "incremental TreeUpdate violates the contract: {error}"
            ));
        }
        if incremental.len() != complete.len() {
            return Some(format!(
                "incremental shadow has {} node(s), full rebuild has {}",
                incremental.len(),
                complete.len()
            ));
        }
        let mut ids: Vec<&accesskit::NodeId> = complete.keys().collect();
        ids.sort_by_key(|id| id.0);
        for id in ids {
            match incremental.get(id) {
                None => {
                    return Some(format!(
                        "node {id:?} exists in the full rebuild but not in the incremental tree"
                    ));
                }
                Some(actual) if actual != &complete[id] => {
                    return Some(format!(
                        "node {id:?} differs:\n  incremental: {actual}\n  full:        {}",
                        complete[id]
                    ));
                }
                Some(_) => {}
            }
        }
        if incremental_structure != full.structure() {
            return Some(String::from(
                "parent/children structure differs between incremental and full delivery",
            ));
        }
        None
    }

    /// 差异即 panic（a11y 版 `assert_oracle`）。
    ///
    /// # Panics
    /// 增量与全量描述的状态不一致（打印第一处差异）。
    pub fn assert_a11y_oracle(&mut self) {
        if let Some(diff) = self.a11y_oracle_diff() {
            panic!("inconsistent a11y tree: {diff}");
        }
    }

    /// 注入一个输入事件：走**与平台泵相同**的 `App::dispatch_input`（命中 / 焦点 / 拖拽 / 动作 / 滚动）。
    ///
    /// 事件时间戳被换成**可控时钟**（`App::clock`）：双击窗口、连击计数与拖拽判定都以事件时刻
    /// 为基准（R02-14），不这样做测试就无法用 `advance` 模拟「两次点击间隔 1 秒」。
    pub fn inject(&mut self, event: InputEvent) -> InputOutcome {
        let stamp = self.app.clock();
        self.app
            .dispatch_input(self.window, event.with_timestamp(stamp))
    }

    /// 投递一个**平台事件**（`Event`：输入 / IME / DPI / 外观 / 焦点 / 尺寸…）。
    ///
    /// 走与平台泵**完全相同**的 `App::dispatch`（`crate::app::resources::dispatch`）：
    /// ① `Event::Input` 立刻走一次交互分发；② 事件同时进有界观测队列（[`App::drain_events`]），
    /// 应用侧因此真的能「读到」字符输入 / IME 提交（`TestApp::inject` 只做 ①，不排队）。
    /// 时间戳同样换成可控时钟（双击 / 连击 / 拖拽判定以它为基准）。
    pub fn dispatch_event(&mut self, event: Event) {
        let stamp = self.app.clock();
        let event = match event {
            Event::Input(input) => Event::Input(input.with_timestamp(stamp)),
            other => other,
        };
        self.app.dispatch(self.window, event);
    }

    /// 推进测试时钟（双击窗口 / 动画 / 拖拽时间都读它；不改任何状态）。
    pub fn advance(&mut self, by: Duration) {
        self.app.advance_clock(by);
    }

    /// 强制整棵树的 lane 失效（oracle 的「全量」路径）。
    pub fn invalidate_all_lanes(&mut self) {
        if let Some(window) = self.app.window_mut(self.window) {
            window.force_full_repaint();
        }
    }

    /// 增量帧 vs 全量帧的差异（`None` = 一致；任何差异即 stale drawing）。
    pub fn oracle_diff(&mut self) -> Option<String> {
        self.drain().expect("incremental frame");
        // 增量侧可能整帧空闲（lane 全部复用）——那时用最近一次提交的快照。
        let incremental = self
            .rendered()
            .cloned()
            .or_else(|| self.last_rendered.clone())
            .expect("incremental frame must have committed pixels before");
        self.invalidate_all_lanes();
        self.drain().expect("full frame");
        let full = self
            .rendered()
            .cloned()
            .expect("full frame must commit pixels");
        if incremental.primitives != full.primitives {
            return Some(format!(
                "primitive sequence differs: incremental {} vs full {} entries",
                incremental.primitives.len(),
                full.primitives.len()
            ));
        }
        if incremental.quads != full.quads {
            return Some(format!(
                "quad sequence differs: incremental {} vs full {} entries",
                incremental.quads.len(),
                full.quads.len()
            ));
        }
        None
    }

    /// 差异即 panic（CI 必跑；设计 §3.8 的 full-refresh oracle）。
    ///
    /// # Panics
    /// 两条路径的 `(primitive, order)` 序列或解析后的 quad 不同。
    pub fn assert_oracle(&mut self) {
        if let Some(diff) = self.oracle_diff() {
            panic!("stale drawing: {diff}");
        }
    }

    /// 像素快照：Windows 上用 WARP 真渲染当前 `Scene`；macOS 用离屏 Metal；Linux 恒 `Gpu`
    /// （`new_offscreen` 在 Wayland 下需要真实 `wl_surface`，因此 Linux 上没有离屏渲染器）。
    ///
    /// 两张图集页都会上传：字形页恒上传，图片页只在 `App::resources` 非空时上传（P13 §8.3）。
    ///
    /// # Errors
    /// [`SnapshotError::NoRenderer`]（本构建没有渲染器）或 [`SnapshotError::Gpu`]
    /// （设备 / 管线 / 渲染 / 读回失败）。
    pub fn snapshot(&self, case: SnapshotCase) -> Result<SnapshotImage, SnapshotError> {
        debug_assert!(case.scale.factor() > 0.0, "snapshot scale must be positive");
        let window = self.app.window(self.window).ok_or(SnapshotError::Gpu {
            detail: smol_str::SmolStr::new_static("snapshot: window was closed"),
        })?;
        let surface = crate::math::Rect::new(0.0, 0.0, case.size.width, case.size.height)
            .to_physical_covering(case.scale);
        let size = (
            surface.width().max(1) as u32,
            surface.height().max(1) as u32,
        );
        // 快照的渲染器是临时建的：页尺寸随整页上传一起给出（DEVIATIONS F-211）。
        let (glyph_page, glyph_pixels) = self.app.glyph_page_pixels();
        let mut renderer = Self::offscreen_renderer(size).map_err(gpu_error)?;
        renderer.queue_atlas_upload(
            crate::paint::atlas::AtlasKind::Glyph,
            glyph_page.0,
            &glyph_pixels,
            crate::render::DirtyRects::full(),
        );
        // 图片页：只有插过资源（页已分配）才上传。
        if let Some((image_page, image_pixels)) = self.app.resource_page_pixels() {
            renderer.queue_atlas_upload(
                crate::paint::atlas::AtlasKind::Image,
                image_page.0,
                image_pixels,
                crate::render::DirtyRects::full(),
            );
        }
        let image = renderer
            .render_snapshot(
                window
                    .instance_buffer()
                    .expect("render_snapshot requires an inline (headless) window"),
            )
            .map_err(gpu_error)?;
        Ok(SnapshotImage {
            width: image.width,
            height: image.height,
            rgba8: image.rgba8,
        })
    }

    /// 离屏渲染器（Windows：WARP；macOS：真实 Metal；Linux：构造期返回 `UnsupportedGpu`）。
    #[cfg(windows)]
    fn offscreen_renderer(
        size: (u32, u32),
    ) -> Result<crate::render::backend::Renderer, EngineError> {
        crate::render::backend::Renderer::new_warp_offscreen(size)
    }

    #[cfg(not(windows))]
    fn offscreen_renderer(
        size: (u32, u32),
    ) -> Result<crate::render::backend::Renderer, EngineError> {
        crate::render::backend::Renderer::new_offscreen(size)
    }

    /// 当前窗口的**物理读回尺寸**（headless 窗口 = `size × scale` 的 covering）。
    fn readback_surface(&self) -> ((u32, u32), crate::math::Scale) {
        let window = self
            .app
            .window(self.window)
            .expect("TestApp's window must exist");
        let scale = window.scale();
        let size = window.size();
        let surface =
            crate::math::Rect::new(0.0, 0.0, size.width, size.height).to_physical_covering(scale);
        (
            (
                surface.width().max(1) as u32,
                surface.height().max(1) as u32,
            ),
            scale,
        )
    }

    /// 当前窗口的像素读回（真实 GPU 离屏渲染；**不应用**合成端滚动偏移）。
    ///
    /// 与 [`Self::snapshot`] 同一条读回链路，但尺寸取自窗口本身（不是 `SnapshotCase`），
    /// 供端到端冒烟测试断言「输入 → 绘制 → 呈现」的最后一跳。
    ///
    /// # Errors
    /// 同 [`Self::snapshot`]。
    pub fn read_pixels(&self) -> Result<SnapshotImage, SnapshotError> {
        self.read_pixels_with_scroll(&[])
    }

    /// 当前窗口的像素读回，并**应用调用方给出的合成端滚动槽表**（见
    /// [`super::pixels::render_offscreen`] 的「表从哪来」）。
    ///
    /// # Errors
    /// 同 [`Self::snapshot`]。
    pub fn read_pixels_with_scroll(
        &self,
        scroll: &[super::pixels::ScrollSlot],
    ) -> Result<SnapshotImage, SnapshotError> {
        let (size, scale) = self.readback_surface();
        let window = self
            .app
            .window(self.window)
            .ok_or_else(|| SnapshotError::Gpu {
                detail: smol_str::SmolStr::new_static("readback: window was closed"),
            })?;
        super::pixels::render_offscreen(&self.app, window, size, scale, scroll).map_err(gpu_error)
    }

    /// 单窗口借用视图（多窗口驱动的入口）。
    pub fn window(&mut self) -> TestWindow<'_> {
        TestWindow {
            app: &mut self.app,
            id: self.window,
        }
    }
}

fn gpu_error(error: EngineError) -> SnapshotError {
    SnapshotError::Gpu {
        detail: smol_str::SmolStr::new(error.to_string()),
    }
}

/// 一帧的 a11y 交付闸门（平台泵与测试捕获是两条消费路径）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum A11yGate {
    /// 捕获打开即交付（`TestApp::drain` 的既有语义）。
    Immediate,
    /// 与平台泵一致：`App::a11y_pending` 为假（含 `A11Y_THROTTLE` 未到期）就不交付。
    Pump,
}

/// 多窗口驱动：借用 `App` + 一个 `WindowId`。
pub struct TestWindow<'a> {
    app: &'a mut App,
    id: WindowId,
}

impl TestWindow<'_> {
    /// 该窗口推进一帧。
    ///
    /// # Errors
    /// 同 [`TestApp::drain`]。
    pub fn drain(&mut self) -> Result<FrameOutcome, EngineError> {
        match self.app.frame(self.id) {
            Some(result) => result,
            None => Ok(FrameOutcome::Idle),
        }
    }

    /// 最近一次帧记录。
    #[must_use]
    pub fn frame_record(&self) -> &FrameRecord {
        self.app
            .window(self.id)
            .expect("window must exist")
            .record()
            .expect("drain() must have produced a frame record")
    }

    /// 探针。
    #[must_use]
    pub fn probe(&self) -> Probe<'_> {
        let window = self.app.window(self.id).expect("window must exist");
        Probe::new(window.tree())
    }
}

#[cfg(test)]
mod tests {
    use super::TestApp;
    #[cfg(all(not(windows), not(target_os = "macos")))]
    use crate::test_support::snapshot::SnapshotError;
    use crate::{
        math::{Point, Rect, Scale},
        node::builder::NodeBuilder,
        paint::{
            color::{Color, ColorResolver, LinearPremulRgba},
            style::{PaintSpec, StylePatch},
        },
        test_support::{
            probe::Selector,
            record::{LaneOutcome, PrimitiveKind},
            snapshot::SnapshotCase,
        },
        text::TextStyle,
    };

    const RECT: Rect = Rect::new(4.0, 6.0, 40.0, 16.0);

    fn white() -> PaintSpec {
        PaintSpec {
            background: Color::WHITE,
            ..PaintSpec::default()
        }
    }

    fn mount_painted(app: &mut TestApp, id: &'static str) -> crate::node::id::NodeId {
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口").tree().root();
        NodeBuilder::boxed()
            .layout_override(RECT)
            .paint(white())
            .id(id)
            .build(app.app_mut().window_mut(window).unwrap().tree_mut(), root)
            .expect("挂载成功")
    }

    #[test]
    fn probe_by_id_and_bounds() {
        let mut app = TestApp::new();
        let node = mount_painted(&mut app, "panel");
        let _ = app.drain().expect("帧");
        let probe = app.probe();
        assert_eq!(probe.by_selector(Selector::Id("panel")), Some(node));
        assert!(probe.exists(Selector::Id("panel")));
        assert_eq!(probe.bounds(node), Some(RECT));
        assert!(probe.lane_of(node).is_some());
    }

    #[test]
    fn probe_by_text_and_path() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口").tree().root();
        let panel = NodeBuilder::boxed()
            .id("panel")
            .build(app.app_mut().window_mut(window).unwrap().tree_mut(), root)
            .expect("挂载");
        let _label = NodeBuilder::text("price", TextStyle::new(12.0))
            .layout_override(RECT)
            .id("price-label")
            .build(app.app_mut().window_mut(window).unwrap().tree_mut(), panel)
            .expect("挂载");
        let _ = app.drain().expect("帧");

        let probe = app.probe();
        assert!(probe.exists(Selector::Text("price")));
        assert_eq!(probe.count(Selector::Text("price")), 1);
        assert_eq!(
            probe.by_selector(Selector::Path(&["panel", "price-label"])),
            probe.by_selector(Selector::Id("price-label"))
        );
        assert_eq!(probe.by_selector(Selector::Path(&["missing"])), None);
    }

    #[test]
    fn probe_missing_and_hit() {
        let mut app = TestApp::new();
        let node = mount_painted(&mut app, "panel");
        let _ = app.drain().expect("帧");
        let probe = app.probe();
        assert_eq!(probe.by_selector(Selector::Id("nope")), None);
        let hit = probe.hit(Point::new(10.0, 10.0)).expect("命中");
        assert_eq!(hit.node, node);
        assert!(probe.hit(Point::new(-5.0, -5.0)).is_none());
    }

    #[test]
    fn drain_records_quad_fields() {
        let mut app = TestApp::new();
        let _node = mount_painted(&mut app, "panel");
        let _ = app.drain().expect("帧");
        let rendered = app.rendered().expect("已提交");
        assert_eq!(rendered.frame_index, app.frame_record().frame_index);
        assert_eq!(rendered.quads.len(), 1);
        let quad = rendered.quads[0];
        assert_eq!(quad.bounds, RECT);
        let expected: LinearPremulRgba = ColorResolver.resolve(Color::WHITE);
        assert_eq!(quad.fill, expected);
        assert!(quad.border.is_none());
        assert!(
            rendered
                .primitives
                .iter()
                .any(|primitive| primitive.kind == PrimitiveKind::Quad)
        );
        assert!(!app.frame_record().damage.is_empty());
    }

    /// A1（脏子树剪枝）之后 `PaintLog` 只记录**被访问**的 lane：第二帧的脏闭包只有绑定节点
    /// 及其祖先链，未变化的兄弟子树整棵跳过。语义从「未变化节点必须复用 lane」收敛为
    /// 「未变化节点不被访问、被访问且未重建的 lane 全部复用」。
    #[test]
    fn second_drain_visits_only_the_changed_closure() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口").tree().root();
        let color = app.app_mut().signal(Color::WHITE);
        {
            let _ = NodeBuilder::boxed()
                .layout_override(RECT)
                .bind_style(move |cx| StylePatch {
                    background: Some(*cx.read(&color)),
                    ..StylePatch::default()
                })
                .id("a")
                .build(app.app_mut().window_mut(window).unwrap().tree_mut(), root)
                .expect("挂载");
        }
        let static_node = mount_painted(&mut app, "b");
        let _ = app.drain().expect("首帧");

        app.app_mut().write(&color, Color::BLACK);
        let record = app.drain().expect("第二帧").clone();
        assert!(record.rendered);
        assert!(
            !record
                .paint_log
                .lanes
                .iter()
                .any(|entry| entry.node == static_node),
            "未变化的兄弟节点不得被访问（脏子树剪枝）：{:?}",
            record.paint_log
        );
        assert!(
            !record.dirty_nodes.contains(&static_node),
            "未变化的兄弟节点不得被重建：{:?}",
            record.dirty_nodes
        );
        assert!(
            record
                .paint_log
                .lanes
                .iter()
                .all(|entry| entry.outcome == LaneOutcome::Reused
                    || record.dirty_nodes.contains(&entry.node)),
            "被访问的 lane 中，未重建者必须整桶复用：{:?}",
            record.paint_log
        );
    }

    #[test]
    fn oracle_matches_full_refresh() {
        let mut app = TestApp::new();
        let _ = mount_painted(&mut app, "panel");
        assert!(app.oracle_diff().is_none());
    }

    /// 快照在各平台的**类型化**结果（S-124：mac 真读回、Linux 真读回、Windows WARP 真渲染）。
    #[test]
    fn snapshot_result_is_typed_per_platform() {
        let app = TestApp::new();
        let result = app.snapshot(SnapshotCase {
            scene: crate::test_support::snapshot::SnapshotScene::Shapes,
            theme: crate::test_support::snapshot::SnapshotTheme::Light,
            size: crate::math::Size::new(64.0, 64.0),
            scale: Scale::new(1.0).expect("scale"),
        });
        #[cfg(windows)]
        {
            // Windows：WARP 真渲染（空场景 ⇒ 全透明的 64×64）。
            let image = result.expect("WARP snapshot");
            assert_eq!((image.width, image.height), (64, 64));
            assert_eq!(image.rgba8.len(), 64 * 64 * 4);
            assert!(image.rgba8.iter().all(|byte| *byte == 0));
        }
        #[cfg(target_os = "macos")]
        {
            // macOS：真实 Metal 离屏渲染（F-137：`new_offscreen` 可用，`Readback` 真实读回）。
            let image = result.expect("Metal offscreen snapshot");
            assert_eq!((image.width, image.height), (64, 64));
            assert_eq!(image.rgba8.len(), 64 * 64 * 4);
        }
        #[cfg(target_os = "linux")]
        {
            // Linux：Vulkan 离屏渲染器（无 surface；F-150）。宿主没有 Vulkan 设备时是
            // `SnapshotError::Gpu`（构造期失败），有设备时必须真读回一张 64×64。
            match result {
                Ok(image) => {
                    assert_eq!((image.width, image.height), (64, 64));
                    assert_eq!(image.rgba8.len(), 64 * 64 * 4);
                    assert!(
                        image.rgba8.iter().all(|byte| *byte == 0),
                        "empty scene must read back fully transparent"
                    );
                }
                Err(SnapshotError::Gpu { detail }) => {
                    eprintln!("skip: no usable Vulkan device for the offscreen renderer: {detail}");
                }
                Err(other) => panic!("unexpected snapshot error: {other:?}"),
            }
        }
    }

    /// **文本像素级断言**（V-64 的回归面）：黑底白字，离屏读回后必须真有亮像素，
    /// 且覆盖率既不是 0 也不是整屏。只看 `GlyphRun` 实例/UV 字段抓不到上传或采样的错位。
    ///
    /// 仅 Windows（WARP）：这是字形页上传/采样的回归面，宿主无关的断言在 `frame_record` 里。
    #[cfg(windows)]
    #[test]
    fn rendered_text_produces_visible_glyph_pixels() {
        let _guard = crate::render::windows::device::gpu_test_lock();
        let mut app = TestApp::with_options(crate::window::WindowOptions {
            title: "text pixels".into(),
            size: crate::math::Size::new(200.0, 60.0),
            scale: Scale::ONE,
            ..crate::window::WindowOptions::default()
        });
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口").tree().root();
        let _backplate = NodeBuilder::boxed()
            .layout_override(Rect::new(0.0, 0.0, 200.0, 60.0))
            .paint(PaintSpec {
                background: Color::BLACK,
                ..PaintSpec::default()
            })
            .build(
                app.app_mut().window_mut(window).expect("窗口").tree_mut(),
                root,
            )
            .expect("backplate");
        let _text = NodeBuilder::text(
            "WWWW",
            TextStyle {
                size: 24.0,
                color: Color::WHITE,
                ..TextStyle::default()
            },
        )
        .layout_override(Rect::new(10.0, 8.0, 180.0, 40.0))
        .build(
            app.app_mut().window_mut(window).expect("窗口").tree_mut(),
            root,
        )
        .expect("text");
        let _ = app.drain().expect("text frame");

        let image = app
            .snapshot(SnapshotCase {
                scene: crate::test_support::snapshot::SnapshotScene::Shapes,
                theme: crate::test_support::snapshot::SnapshotTheme::Light,
                size: crate::math::Size::new(200.0, 60.0),
                scale: Scale::ONE,
            })
            .expect("WARP snapshot");
        let bright = image
            .rgba8
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|pixel| pixel[0] > 128 && pixel[1] > 128 && pixel[2] > 128)
            .count();
        let coverage = bright as f32 / f32::from((image.width * image.height) as u16);
        assert!(bright > 0, "text must produce non-background pixels");
        assert!(
            coverage > 0.001 && coverage < 0.5,
            "glyph coverage {coverage} is implausible (0 or whole screen)"
        );
    }

    #[test]
    fn resize_forces_full_damage() {
        let mut app = TestApp::new();
        let _node = mount_painted(&mut app, "panel");
        let _ = app.drain().expect("首帧");

        let window = app.window_id();
        app.app_mut()
            .window_mut(window)
            .expect("窗口")
            .set_size(crate::math::Size::new(400.0, 300.0));
        let record = app.drain().expect("resize 帧").clone();
        assert!(record.damage_full, "resize 必须退化为全帧");
        let expected =
            Rect::new(0.0, 0.0, 400.0, 300.0).to_physical_covering(Scale::new(1.0).expect("scale"));
        assert_eq!(record.damage, vec![expected]);
    }

    #[test]
    fn text_glyphs_use_border_and_padding_offset() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口").tree().root();
        let mut inner = taffy::Style::<smol_str::SmolStr>::DEFAULT;
        inner.size = taffy::geometry::Size {
            width: taffy::style::Dimension::length(80.0),
            height: taffy::style::Dimension::length(40.0),
        };
        let pad = taffy::style::LengthPercentage::length(4.0);
        inner.padding = taffy::geometry::Rect {
            left: pad,
            right: pad,
            top: pad,
            bottom: pad,
        };
        let style = crate::paint::style::Style {
            inner,
            clip: crate::paint::style::Clip::None,
        };
        let node = NodeBuilder::text("MMM", TextStyle::new(12.0))
            .style(style)
            .id("label")
            .build(app.app_mut().window_mut(window).unwrap().tree_mut(), root)
            .expect("挂载");

        let _ = app.drain().expect("帧");
        assert_eq!(
            app.app()
                .window(window)
                .expect("窗口")
                .tree()
                .content_offset(node),
            Point::new(4.0, 4.0),
            "content_offset = border + padding"
        );
        let rendered = app.rendered().expect("已提交");
        let glyph_bounds = rendered
            .primitives
            .iter()
            .find(|primitive| primitive.kind == PrimitiveKind::GlyphRun)
            .map(|primitive| primitive.bounds)
            .expect("必须有字形");
        assert!(
            glyph_bounds.origin.x >= 3.0,
            "字形绝对坐标必须加上 padding 偏移（实际 {:?}）",
            glyph_bounds
        );
    }
}

/// P10 的端到端用例（`TestApp::inject` 走与平台泵相同的分发路径）。
#[cfg(test)]
mod p10_tests {
    use super::TestApp;
    use crate::{
        event::{Action, InputEvent, KeyCode, Modifiers, PointerButton},
        math::{Point, Rect},
        node::builder::NodeBuilder,
        paint::style::{Clip, PaintSpec, Style, StylePatch},
    };

    fn app_with_button() -> (TestApp, crate::node::id::NodeId) {
        let mut app = TestApp::new();
        let window = app.window_id();
        let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
        let root = tree.root();
        let node = NodeBuilder::boxed()
            .layout_override(Rect::new(10.0, 10.0, 60.0, 40.0))
            .paint(PaintSpec {
                background: crate::paint::color::Color::from_rgba8([10, 20, 30, 255]),
                ..PaintSpec::default()
            })
            .on_click(Action::custom("press"))
            .build(tree, root)
            .expect("mount");
        app.app_mut().on_action(Action::custom("press"), |_ctx| {});
        app.drain().expect("first frame");
        (app, node)
    }

    #[test]
    fn inject_click_reports_the_action_and_the_target() {
        let (mut app, node) = app_with_button();
        app.inject(InputEvent::pointer_down(
            Point::new(20.0, 20.0),
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        let outcome = app.inject(InputEvent::pointer_up(
            Point::new(20.0, 20.0),
            PointerButton::Primary,
            Modifiers::empty(),
        ));

        assert_eq!(outcome.hit.map(|hit| hit.node), Some(node));
        assert_eq!(outcome.action, Some(Action::custom("press")));
        assert_eq!(outcome.target, Some(node));
        assert_eq!(outcome.click_count, 1);
    }

    #[test]
    fn inject_scroll_only_damages_the_viewport() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
        let root = tree.root();
        let mut style = Style::default();
        style.inner.size = taffy::geometry::Size {
            width: taffy::style::Dimension::length(100.0),
            height: taffy::style::Dimension::length(100.0),
        };
        style.clip = Clip::Bounds;
        let container = NodeBuilder::boxed()
            .style(style)
            .build(tree, root)
            .expect("mount");
        for index in 0..4 {
            let _ = NodeBuilder::boxed()
                .layout_override(Rect::new(0.0, index as f32 * 40.0, 100.0, 40.0))
                .paint(PaintSpec {
                    background: crate::paint::color::Color::from_rgba8([
                        index as u8 * 10 + 1,
                        0,
                        0,
                        255,
                    ]),
                    ..PaintSpec::default()
                })
                .build(tree, container);
        }
        app.drain().expect("first frame");
        let handle = app
            .app_mut()
            .make_scroll(
                window,
                container,
                crate::interaction::ScrollConfig::default(),
            )
            .expect("scroll");
        app.app_mut()
            .set_scroll_content(&handle, crate::math::Size::new(100.0, 160.0))
            .expect("content");
        app.app_mut()
            .scroll_to(&handle, Point::new(0.0, 40.0))
            .expect("scroll");
        let record = app.drain().expect("scroll frame");

        // 视口 100×100（物理像素）+ AA 外扩 1 px（`DamageAccumulator::push` 的固定外扩）。
        let allowed = crate::math::PhysicalRect::new(-1, -1, 101, 101);
        assert!(!record.damage.is_empty(), "滚动必须产生 damage");
        for rect in &record.damage {
            assert!(
                rect.intersect(allowed) == *rect,
                "damage 必须落在视口（含 1px AA 外扩）内：{rect:?}"
            );
        }
    }

    #[test]
    fn inject_double_click_reports_two() {
        let (mut app, node) = app_with_button();
        let click = |app: &mut TestApp| {
            app.inject(InputEvent::pointer_down(
                Point::new(20.0, 20.0),
                PointerButton::Primary,
                Modifiers::empty(),
            ));
            app.inject(InputEvent::pointer_up(
                Point::new(20.0, 20.0),
                PointerButton::Primary,
                Modifiers::empty(),
            ))
        };

        assert_eq!(click(&mut app).click_count, 1);
        app.advance(std::time::Duration::from_millis(200));
        assert_eq!(click(&mut app).click_count, 2, "200ms 内同点 = 双击");
        app.advance(std::time::Duration::from_millis(600));
        assert_eq!(click(&mut app).click_count, 1, "超时后重新计数");
        let _ = node;
    }

    #[test]
    fn animation_reaches_idle_after_the_last_frame() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let transient = app.app_mut().transient(0.0_f32);
        app.app_mut()
            .animate(
                window,
                crate::interaction::AnimatedValue::Scalar {
                    target: transient,
                    to: 1.0,
                },
                &[],
                crate::interaction::AnimationSpec {
                    duration: std::time::Duration::from_millis(50),
                    easing: crate::interaction::Easing::Linear,
                },
            )
            .expect("animate");
        app.drain().expect("animation frame");
        app.advance(std::time::Duration::from_millis(60));
        app.drain().expect("final frame");

        assert_eq!(*app.app().read_transient(&transient), 1.0);
        assert!(!app.app().animations_running(window));
        // 再推进一帧：没有脏位 ⇒ Idle（不提交、FrameIndex 不变）。
        let before = app.app().window(window).expect("window").frame_index();
        app.drain().expect("idle frame");
        let after = app.app().window(window).expect("window").frame_index();
        assert_eq!(before, after, "动画结束后必须回到空闲零帧");
    }

    #[test]
    fn tab_focus_is_visible_through_the_probe() {
        let (_app_unused, _node_unused) = app_with_button();
        let (mut app, node) = app_with_button();
        app.inject(InputEvent::key_down(KeyCode::Tab, Modifiers::empty()));
        assert_eq!(app.app().focused(app.window_id()), Some(node));
        assert!(
            !app.app()
                .window(app.window_id())
                .expect("window")
                .tree()
                .get(node)
                .expect("node")
                .dirty()
                .contains(crate::node::dirty::DirtyFlags::PAINT)
        );
        let _ = StylePatch::default();
    }
    #[test]
    fn nested_scroll_damage_is_in_screen_coordinates() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
        let root = tree.root();
        let mut outer_style = Style::default();
        outer_style.inner.size = taffy::geometry::Size {
            width: taffy::style::Dimension::length(200.0),
            height: taffy::style::Dimension::length(200.0),
        };
        outer_style.clip = Clip::Bounds;
        let outer = NodeBuilder::boxed()
            .style(outer_style)
            .build(tree, root)
            .expect("outer");
        let mut inner_style = Style::default();
        inner_style.inner.size = taffy::geometry::Size {
            width: taffy::style::Dimension::length(200.0),
            height: taffy::style::Dimension::length(200.0),
        };
        inner_style.inner.margin = taffy::geometry::Rect {
            left: taffy::style::LengthPercentageAuto::length(0.0),
            right: taffy::style::LengthPercentageAuto::length(0.0),
            top: taffy::style::LengthPercentageAuto::length(100.0),
            bottom: taffy::style::LengthPercentageAuto::length(0.0),
        };
        inner_style.clip = Clip::Bounds;
        let inner = NodeBuilder::boxed()
            .style(inner_style)
            .build(tree, outer)
            .expect("inner");
        for index in 0..6 {
            let _ = NodeBuilder::boxed()
                .layout_override(Rect::new(0.0, index as f32 * 40.0, 200.0, 40.0))
                .paint(PaintSpec {
                    background: crate::paint::color::Color::from_rgba8([
                        index as u8 + 1,
                        0,
                        0,
                        255,
                    ]),
                    ..PaintSpec::default()
                })
                .build(tree, inner);
        }
        app.drain().expect("first frame");

        let outer_handle = app
            .app_mut()
            .make_scroll(window, outer, crate::interaction::ScrollConfig::default())
            .expect("outer scroll");
        app.app_mut()
            .set_scroll_content(&outer_handle, crate::math::Size::new(200.0, 400.0))
            .expect("outer content");
        let inner_handle = app
            .app_mut()
            .make_scroll(window, inner, crate::interaction::ScrollConfig::default())
            .expect("inner scroll");
        app.app_mut()
            .set_scroll_content(&inner_handle, crate::math::Size::new(200.0, 240.0))
            .expect("inner content");
        // 外层先滚 30（内层屏幕 y = 100 − 30 = 70），把这一帧的 damage 消费掉。
        app.app_mut()
            .scroll_to(&outer_handle, Point::new(0.0, 30.0))
            .expect("outer scroll");
        let _ = app.drain().expect("outer frame");

        app.app_mut()
            .scroll_to(&inner_handle, Point::new(0.0, 10.0))
            .expect("inner scroll");
        let record = app.drain().expect("inner frame");

        // 内层视口在屏幕上 = (0, 70, 200, 70+200) + 1 px AA 外扩。
        let allowed = crate::math::PhysicalRect::new(-1, 69, 201, 271);
        assert!(!record.damage.is_empty(), "内层滚动必须产生 damage");
        for rect in &record.damage {
            assert!(
                rect.intersect(allowed) == *rect,
                "嵌套滚动的 damage 必须在**屏幕**坐标内：{rect:?}"
            );
        }
        assert!(
            record
                .damage
                .iter()
                .any(|rect| (69..=71).contains(&rect.y0)),
            "damage 顶部必须落在内层视口的屏幕位置：{:?}",
            record.damage
        );
    }

    #[test]
    fn clearing_focus_damages_the_ring_outset() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
        let root = tree.root();
        let ringed = NodeBuilder::boxed()
            .layout_override(Rect::new(10.0, 10.0, 40.0, 40.0))
            .paint(PaintSpec {
                background: crate::paint::color::Color::from_rgba8([9, 9, 9, 255]),
                ..PaintSpec::default()
            })
            .on_click(Action::custom("a"))
            .focus_ring(crate::paint::style::FocusRing::new(
                2.0,
                2.0,
                crate::paint::color::Color::from_rgba8([0, 120, 255, 255]),
            ))
            .build(tree, root)
            .expect("ringed");
        let other = NodeBuilder::boxed()
            .layout_override(Rect::new(100.0, 10.0, 40.0, 40.0))
            .on_click(Action::custom("b"))
            .build(tree, root)
            .expect("other");
        app.drain().expect("first frame");
        app.app_mut()
            .set_focus(window, Some(ringed))
            .then_some(())
            .expect("focus");
        let _ = app.drain().expect("focus frame");

        app.app_mut()
            .set_focus(window, Some(other))
            .then_some(())
            .expect("move focus");
        let record = app.drain().expect("unfocus frame");

        // 环外扩 = bounds.outset(offset 2 + width 2) ⇒ (6, 6, 48, 48) → 物理像素覆盖 (6,6)-(54,54)。
        assert!(
            record
                .damage
                .iter()
                .any(|rect| rect.x0 <= 6 && rect.y0 <= 6 && rect.x1 >= 54 && rect.y1 >= 54),
            "清焦点时必须把环的外扩并入 damage（否则残留 1px）：{:?}",
            record.damage
        );
    }
}

/// P11 的端到端用例：`TestApp` 在每帧之后交付一笔 a11y 快照（与平台泵同序）。
#[cfg(test)]
mod p11_tests {
    use smol_str::SmolStr;

    use super::TestApp;
    use crate::{
        a11y::{A11yAction, A11yDiagnostic, A11yOverride, A11yPatch, A11yRole, A11yTreeSnapshot},
        event::Action,
        math::{Point, Rect, Scale},
        node::{builder::NodeBuilder, dirty::DirtyFlags, id::NodeId},
        paint::{
            canvas::Canvas,
            color::Color,
            style::{Clip, PaintSpec, Style, StylePatch},
        },
        text::TextStyle,
        window::WindowOptions,
    };

    fn white() -> PaintSpec {
        PaintSpec {
            background: Color::WHITE,
            ..PaintSpec::default()
        }
    }

    fn mount(
        app: &mut TestApp,
        build: impl FnOnce(&mut crate::node::tree::NodeTree, NodeId) -> NodeId,
    ) -> NodeId {
        let window = app.window_id();
        let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
        let root = tree.root();
        build(tree, root)
    }

    fn snapshot(app: &TestApp) -> &A11yTreeSnapshot {
        app.a11y().expect("本帧必须有一笔 a11y 交付")
    }

    #[test]
    fn a11y_roles_labels_and_values() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let root = {
            let window = app.window_id();
            app.app().window(window).expect("window").tree().root()
        };
        let label = mount(&mut app, |tree, root| {
            NodeBuilder::text("价格", TextStyle::new(12.0))
                .layout_override(Rect::new(0.0, 0.0, 40.0, 20.0))
                .id("price")
                .build(tree, root)
                .expect("text")
        });
        let button = mount(&mut app, |tree, root| {
            NodeBuilder::boxed()
                .layout_override(Rect::new(0.0, 24.0, 60.0, 24.0))
                .paint(white())
                .on_click(Action::custom("submit"))
                .a11y(A11yOverride {
                    label: Some(SmolStr::new_static("提交")),
                    ..A11yOverride::default()
                })
                .id("submit")
                .build(tree, root)
                .expect("button")
        });
        let canvas = mount(&mut app, |tree, root| {
            NodeBuilder::canvas(Canvas::new(Rect::new(0.0, 52.0, 60.0, 30.0)))
                .a11y(A11yOverride {
                    label: Some(SmolStr::new_static("K 线图")),
                    ..A11yOverride::default()
                })
                .id("chart")
                .build(tree, root)
                .expect("canvas")
        });
        let _ = app.drain().expect("frame");

        let snap = snapshot(&app);
        assert!(snap.full, "首笔必须是全树");
        let root_node = snap.node_for(root).expect("root");
        assert_eq!(root_node.role, A11yRole::Window);
        assert_eq!(root_node.label_text(), None, "默认窗口没有标题 ⇒ 根无名");
        let label_node = snap.node_for(label).expect("label");
        assert_eq!(label_node.role, A11yRole::Label);
        assert_eq!(
            label_node.value.as_deref(),
            Some("价格"),
            "文本走 value（Role::Label）"
        );
        let button_node = snap.node_for(button).expect("button");
        assert_eq!(button_node.role, A11yRole::Button);
        assert_eq!(button_node.label_text(), Some("提交"));
        assert!(button_node.actions.contains(&A11yAction::Click));
        assert!(button_node.actions.contains(&A11yAction::Focus));
        let canvas_node = snap.node_for(canvas).expect("canvas");
        assert_eq!(canvas_node.role, A11yRole::Image);
        assert_eq!(canvas_node.label_text(), Some("K 线图"));
        assert!(!canvas_node.hidden);
    }

    #[test]
    fn a11y_box_is_a_transparent_container() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let panel = mount(&mut app, |tree, root| {
            NodeBuilder::boxed()
                .id("panel")
                .build(tree, root)
                .expect("panel")
        });
        let _ = app.drain().expect("frame");
        assert_eq!(
            snapshot(&app).node_for(panel).expect("panel").role,
            A11yRole::GenericContainer,
            "没有语义的 Box 必须是 GenericContainer（AccessKit 侧透明）"
        );
    }

    #[test]
    fn a11y_increment_contains_only_the_change_and_its_ancestors() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let text = app.app_mut().signal(SmolStr::new_static("旧的"));
        let label_signal = text;
        let button = mount(&mut app, move |tree, root| {
            NodeBuilder::boxed()
                .layout_override(Rect::new(0.0, 0.0, 60.0, 24.0))
                .paint(white())
                .on_click(Action::custom("press"))
                .bind_a11y(move |cx| A11yPatch {
                    label: Some(cx.read(&label_signal).clone()),
                    ..A11yPatch::default()
                })
                .id("press")
                .build(tree, root)
                .expect("button")
        });
        let _ = mount(&mut app, |tree, root| {
            NodeBuilder::boxed()
                .layout_override(Rect::new(0.0, 30.0, 60.0, 24.0))
                .id("sibling")
                .build(tree, root)
                .expect("sibling")
        });
        let _ = app.drain().expect("first");
        assert!(snapshot(&app).full);

        app.app_mut().write(&text, SmolStr::new_static("新的"));
        let _ = app.drain().expect("second");
        let snap = snapshot(&app);
        assert!(!snap.full, "第二笔必须是增量");
        let ids: Vec<NodeId> = snap.nodes().iter().map(|node| node.engine).collect();
        assert_eq!(
            ids,
            vec![root, button],
            "增量 = 变更节点 + 祖先链（不含兄弟）"
        );
        assert_eq!(
            snap.node_for(button).expect("button").label_text(),
            Some("新的")
        );
    }

    #[test]
    fn a11y_focus_is_always_present_even_on_an_idle_frame() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let window = app.window_id();
        let button = mount(&mut app, |tree, root| {
            NodeBuilder::boxed()
                .layout_override(Rect::new(0.0, 0.0, 60.0, 24.0))
                .on_click(Action::custom("press"))
                .id("press")
                .build(tree, root)
                .expect("button")
        });
        let _ = app.drain().expect("first");

        app.app_mut().set_focus(window, Some(button));
        let record = app.drain().expect("focus frame");
        assert!(!record.rendered, "纯 A11Y 变化必须让本帧保持 Idle");
        let snap = record.a11y.as_ref().expect("Idle 帧也要有 a11y 交付");
        assert_eq!(
            snap.focus(),
            snap.id_for_engine(button).expect("焦点节点的稳定 id"),
            "TreeUpdate.focus 必须是稳定 id"
        );
        assert_eq!(snap.focus_node().engine, button, "焦点节点必须在快照里");
    }

    #[test]
    fn a11y_structure_matches_the_tree() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let panel = mount(&mut app, |tree, root| {
            NodeBuilder::boxed()
                .id("panel")
                .build(tree, root)
                .expect("panel")
        });
        let _ = app.drain().expect("first");

        let child = mount(&mut app, |tree, _root| {
            NodeBuilder::boxed()
                .layout_override(Rect::new(0.0, 0.0, 20.0, 20.0))
                .id("child")
                .build(tree, panel)
                .expect("child")
        });
        let _ = app.drain().expect("second");
        let snap = snapshot(&app);
        let panel_id = snap.id_for_engine(panel).expect("panel 的稳定 id");
        let child_id = snap.node_for(child).expect("新子节点必须出现").id;
        assert_eq!(
            snap.children_of(panel_id),
            vec![child_id],
            "父的 children 必须与节点树一致（稳定 id）"
        );
        // 同一笔里父的 children 引用的 id 必须与子节点自身的 id 相同（不是两套编码）。
        assert_eq!(
            snap.node_for(panel).expect("panel").children,
            vec![child_id]
        );
    }

    #[test]
    fn a11y_bounds_equal_probe_times_scale() {
        let scale = Scale::new(1.25).expect("scale");
        let mut app = TestApp::with_options(WindowOptions {
            title: SmolStr::new_static("scale"),
            size: crate::math::Size::new(400.0, 300.0),
            scale,
            ..WindowOptions::default()
        });
        app.enable_a11y_capture();
        let node = mount(&mut app, |tree, root| {
            NodeBuilder::boxed()
                .layout_override(Rect::new(8.0, 12.0, 40.0, 24.0))
                .paint(white())
                .id("box")
                .build(tree, root)
                .expect("box")
        });
        let _ = app.drain().expect("frame");
        let logical = app.probe().bounds(node).expect("bounds");
        let expected = logical.to_physical(scale);
        let snap = snapshot(&app);
        assert_eq!(
            snap.node_for(node).expect("box").bounds,
            expected,
            "a11y bounds 必须是 逻辑 × scale 逐边 round"
        );
    }

    #[test]
    fn a11y_bind_a11y_disabled_is_not_layout() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let window = app.window_id();
        let disabled = app.app_mut().signal(false);
        let signal = disabled;
        let button = mount(&mut app, move |tree, root| {
            NodeBuilder::boxed()
                .layout_override(Rect::new(0.0, 0.0, 60.0, 24.0))
                .paint(white())
                .on_click(Action::custom("press"))
                .bind_a11y(move |cx| A11yPatch {
                    disabled: Some(*cx.read(&signal)),
                    ..A11yPatch::default()
                })
                .id("press")
                .build(tree, root)
                .expect("button")
        });
        let _ = app.drain().expect("first");
        assert!(!snapshot(&app).node_for(button).expect("button").disabled);

        app.app_mut().write(&disabled, true);
        let _ = app.drain().expect("second");
        let snap = snapshot(&app);
        assert!(snap.node_for(button).expect("button").disabled);
        let flags = app
            .app()
            .window(window)
            .expect("window")
            .tree()
            .get(button)
            .expect("button")
            .dirty();
        assert!(
            !flags.contains(DirtyFlags::LAYOUT),
            "`bind_a11y` 的禁用态不得触发重排（A11Y 不产生视觉 damage）"
        );
    }

    #[test]
    fn a11y_canvas_requires_alt_text() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let canvas = mount(&mut app, |tree, root| {
            NodeBuilder::canvas(Canvas::new(Rect::new(0.0, 0.0, 60.0, 30.0)))
                .id("chart")
                .build(tree, root)
                .expect("canvas")
        });
        let _ = app.drain().expect("frame");
        let snap = snapshot(&app);
        assert!(
            snap.node_for(canvas).expect("canvas").hidden,
            "缺替代文本的自绘节点必须 hidden（不静默丢弃）"
        );
        assert!(
            app.app()
                .a11y_diagnostics()
                .iter()
                .any(|diagnostic| matches!(
                    diagnostic,
                    A11yDiagnostic::MissingAltText { node, .. } if *node == canvas
                )),
            "必须记 MissingAltText 诊断"
        );
    }

    #[test]
    fn a11y_hidden_nodes_are_excluded() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let window = app.window_id();
        let hidden = mount(&mut app, |tree, root| {
            NodeBuilder::boxed()
                .layout_override(Rect::new(0.0, 0.0, 20.0, 20.0))
                .id("hidden")
                .build(tree, root)
                .expect("hidden")
        });
        let _ = app.drain().expect("first");

        app.app_mut()
            .window_mut(window)
            .expect("window")
            .tree_mut()
            .set_style_patch(
                hidden,
                StylePatch {
                    visible: Some(false),
                    ..StylePatch::default()
                },
            );
        // 直接写 patch 不经过 `refresh_styles`，这里显式标脏（生产路径由 `refresh_styles` 负责）。
        app.app_mut()
            .window_mut(window)
            .expect("window")
            .tree_mut()
            .mark_a11y(hidden);
        let _ = app.drain().expect("second");
        let snap = snapshot(&app);
        assert!(
            snap.node_for(hidden).expect("hidden").hidden,
            "`visible: false` 必须传播到 a11y 的 hidden"
        );
        assert!(
            snap.visible_nodes_in_batch()
                .all(|node| node.engine != hidden),
            "hidden 节点不得出现在可见集合里"
        );
    }

    #[test]
    fn a11y_scale_change_resends_the_full_tree() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let node = mount(&mut app, |tree, root| {
            NodeBuilder::boxed()
                .layout_override(Rect::new(0.0, 0.0, 20.0, 20.0))
                .paint(white())
                .build(tree, root)
                .expect("box")
        });
        let _ = app.drain().expect("first");
        assert!(snapshot(&app).full);

        let window = app.window_id();
        app.app_mut()
            .window_mut(window)
            .expect("window")
            .set_scale(Scale::new(1.5).expect("scale"));
        let _ = app.drain().expect("second");
        let snap = snapshot(&app);
        assert!(snap.full, "DPI 变化必须整棵重发（bounds 全是物理像素）");
        assert_eq!(
            snap.node_for(node).expect("box").bounds,
            Rect::new(0.0, 0.0, 20.0, 20.0).to_physical(Scale::new(1.5).expect("scale"))
        );
    }

    #[test]
    fn a11y_virtual_list_exposes_only_mounted_rows() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let window = app.window_id();
        let mut inner = taffy::Style::<SmolStr>::DEFAULT;
        inner.size = taffy::geometry::Size {
            width: taffy::style::Dimension::length(100.0),
            height: taffy::style::Dimension::length(80.0),
        };
        inner.position = taffy::style::Position::Relative;
        let container = mount(&mut app, move |tree, root| {
            NodeBuilder::boxed()
                .style(Style {
                    inner,
                    clip: Clip::Bounds,
                })
                .id("rows")
                .build(tree, root)
                .expect("container")
        });
        let list = app
            .app_mut()
            .make_virtual_list(
                window,
                container,
                crate::interaction::VirtualListConfig {
                    extent: 20.0,
                    overscan: 1,
                },
            )
            .expect("virtual list");
        app.app_mut()
            .set_virtual_items(&list, 100, |index| {
                NodeBuilder::boxed()
                    .a11y(A11yOverride {
                        label: Some(smol_str::format_smolstr!("row {index}")),
                        ..A11yOverride::default()
                    })
                    .id("row")
            })
            .expect("items");
        let _ = app.drain().expect("frame");

        let snap = snapshot(&app);
        let rows: Vec<&crate::a11y::A11yNode> = snap
            .nodes()
            .iter()
            .filter(|node| node.role == A11yRole::ListBoxOption)
            .collect();
        let mounted = app.app().virtual_list_state(&list).expect("state").mounted;
        assert_eq!(rows.len(), mounted, "a11y 只暴露已挂载的行（可见窗口）");
        assert_eq!(snap.node_for(container).expect("list").role, A11yRole::List);
        assert!(mounted < 100, "100 行的列表不得整表挂载");
        let _ = Point::ZERO;
    }
}

#[cfg(test)]
mod p11_fix_tests {
    use smol_str::SmolStr;

    use super::TestApp;
    use crate::{
        a11y::{A11yOverride, A11yPatch, A11yRole},
        event::Action,
        interaction::ScrollConfig,
        layout::LayoutOverride,
        math::{Rect, Scale, Size},
        node::{builder::NodeBuilder, dirty::DirtyFlags, id::NodeId},
        paint::{
            color::Color,
            style::{Clip, PaintSpec, Style},
        },
        text::{TextDirection, TextStyle},
        window::FrameOutcome,
    };

    fn white() -> PaintSpec {
        PaintSpec {
            background: Color::WHITE,
            ..PaintSpec::default()
        }
    }

    fn percent_style() -> Style {
        let mut inner = taffy::Style::<SmolStr>::DEFAULT;
        inner.size = taffy::geometry::Size {
            width: taffy::style::Dimension::percent(1.0),
            height: taffy::style::Dimension::percent(1.0),
        };
        Style {
            inner,
            clip: Clip::None,
        }
    }

    fn mount(
        app: &mut TestApp,
        build: impl FnOnce(&mut crate::node::tree::NodeTree, NodeId) -> NodeId,
    ) -> NodeId {
        let window = app.window_id();
        let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
        let root = tree.root();
        build(tree, root)
    }

    /// 审查 #1：窗口 resize 后子孙的布局盒变了，增量笔必须把它们重发。
    #[test]
    fn a11y_bounds_follow_window_resize() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let window = app.window_id();
        let child = mount(&mut app, |tree, root| {
            NodeBuilder::boxed()
                .style(percent_style())
                .paint(white())
                .id("child")
                .build(tree, root)
                .expect("child")
        });
        let _ = app.drain().expect("first");
        let before = app.probe().bounds(child).expect("bounds");

        app.app_mut()
            .window_mut(window)
            .expect("window")
            .set_size(Size::new(400.0, 300.0));
        let _ = app.drain().expect("second");

        let after = app.probe().bounds(child).expect("bounds");
        assert_ne!(before, after, "resize 必须真的改变子节点的布局盒");
        let snap = app.a11y().expect("resize 后必须有 a11y 交付");
        assert_eq!(
            snap.node_for(child)
                .expect("布局盒变了的子节点必须重发")
                .bounds,
            after.to_physical(app.app().window(window).expect("window").scale()),
            "a11y bounds 必须等于 逻辑 × scale"
        );
    }

    /// 审查 #2：`set_layout_override` / `set_style` 等走 `invalidate_layout` 的路径必须标 `A11Y`。
    #[test]
    fn a11y_bounds_follow_layout_override() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let window = app.window_id();
        let node = mount(&mut app, |tree, root| {
            NodeBuilder::boxed()
                .layout_override(Rect::new(0.0, 0.0, 20.0, 20.0))
                .paint(white())
                .id("box")
                .build(tree, root)
                .expect("box")
        });
        let _ = app.drain().expect("first");

        app.app_mut()
            .window_mut(window)
            .expect("window")
            .tree_mut()
            .set_layout_override(
                node,
                Some(LayoutOverride::new(Rect::new(0.0, 0.0, 44.0, 24.0))),
            )
            .expect("override");
        let _ = app.drain().expect("second");

        let logical = app.probe().bounds(node).expect("bounds");
        assert_eq!(logical.size, Size::new(44.0, 24.0), "覆盖必须生效");
        let snap = app.a11y().expect("覆盖后必须有 a11y 交付");
        assert_eq!(
            snap.node_for(node).expect("必须重发").bounds,
            logical.to_physical(Scale::ONE)
        );
    }

    /// 审查 #3：普通滚动（非 a11y action）也必须让容器的 a11y 子树失效。
    #[test]
    fn a11y_bounds_follow_scroll() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let window = app.window_id();
        let mut inner = taffy::Style::<SmolStr>::DEFAULT;
        inner.size = taffy::geometry::Size {
            width: taffy::style::Dimension::length(200.0),
            height: taffy::style::Dimension::length(100.0),
        };
        inner.position = taffy::style::Position::Relative;
        let container = mount(&mut app, move |tree, root| {
            NodeBuilder::boxed()
                .style(Style {
                    inner,
                    clip: Clip::Bounds,
                })
                .id("viewport")
                .build(tree, root)
                .expect("container")
        });
        let row = mount(&mut app, |tree, _root| {
            NodeBuilder::boxed()
                .layout_override(Rect::new(0.0, 0.0, 100.0, 40.0))
                .paint(white())
                .id("row")
                .build(tree, container)
                .expect("row")
        });
        let handle = app
            .app_mut()
            .make_scroll(window, container, ScrollConfig::default())
            .expect("scroll");
        app.app_mut()
            .set_scroll_content(&handle, Size::new(200.0, 400.0))
            .expect("content");
        let _ = app.drain().expect("first");
        let before = app
            .a11y()
            .expect("first a11y")
            .node_for(row)
            .expect("row")
            .bounds;

        app.app_mut()
            .scroll_by(&handle, crate::math::Point::new(0.0, 60.0))
            .expect("scroll");
        let _ = app.drain().expect("second");

        let logical = app.probe().bounds(row).expect("bounds");
        let snap = app.a11y().expect("滚动后必须有 a11y 交付");
        let after = snap.node_for(row).expect("行必须重发").bounds;
        assert_ne!(before, after, "滚动必须改变行的 a11y 位置");
        assert_eq!(
            after,
            crate::interaction::scroll::to_screen(
                app.app().window(window).expect("window").tree(),
                row,
                logical
            )
            .to_physical(Scale::ONE)
        );
    }

    /// E2 端到端：分批首树尚未跑完时滚动一次，**已交付**的行必须在后续笔重发新 bounds。
    ///
    /// 修前行为：滚动把整棵子树标脏（`a11y_note_scroll` → `invalidate_subtree`），但已交付行
    /// 仍占着 `queued` 去重名额 ⇒ 后续笔只有尚未交付的行，读屏的高亮框/命中区域停在旧位置。
    ///
    /// 变异守卫：删掉交付后的 `queued.remove` ⇒ `node_for(rows[0])` 变 None 或 bounds 不变。
    #[test]
    fn a11y_scroll_during_a_batched_first_tree_is_not_lost() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let window = app.window_id();
        let mut inner = taffy::Style::<SmolStr>::DEFAULT;
        inner.size = taffy::geometry::Size {
            width: taffy::style::Dimension::length(200.0),
            height: taffy::style::Dimension::length(100.0),
        };
        inner.position = taffy::style::Position::Relative;
        let container = mount(&mut app, move |tree, root| {
            NodeBuilder::boxed()
                .style(Style {
                    inner,
                    clip: Clip::Bounds,
                })
                .id("viewport")
                .build(tree, root)
                .expect("container")
        });
        // 3000 行 ⇒ 首笔 2048 节点后队列还剩 953 个未交付节点（分批运行**未结束**）。
        let rows: Vec<NodeId> = {
            let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
            (0..3000)
                .map(|index| {
                    NodeBuilder::boxed()
                        .layout_override(Rect::new(0.0, index as f32, 50.0, 10.0))
                        .id(smol_str::format_smolstr!("row-{index}"))
                        .build(tree, container)
                        .expect("row")
                })
                .collect()
        };
        let handle = app
            .app_mut()
            .make_scroll(window, container, ScrollConfig::default())
            .expect("scroll");
        app.app_mut()
            .set_scroll_content(&handle, Size::new(200.0, 4000.0))
            .expect("content");

        let _ = app.drain().expect("首帧");
        let first = app.a11y().expect("首笔 a11y").clone();
        assert!(first.full, "首笔必须是全树");
        let before = first.node_for(rows[0]).expect("行 0 必须已交付").bounds;

        // 分批运行中滚动：整棵子树 bounds 变化。
        app.app_mut()
            .scroll_to(&handle, crate::math::Point::new(0.0, 120.0))
            .expect("scroll to");
        let _ = app.drain().expect("滚动帧");
        let snapshot = app.a11y().expect("滚动后必须有 a11y 交付");
        let after = snapshot
            .node_for(rows[0])
            .expect("已交付行必须在滚动后重发")
            .bounds;
        assert_ne!(
            before, after,
            "滚动后已交付行的 bounds 必须更新（修前它永久停在旧位置）"
        );
        app.assert_a11y_contract();
    }

    /// 审查 #6：写 `bind_a11y` 的信号**不得**产生任何像素（Idle、无 damage、无 PAINT）。
    #[test]
    fn a11y_bind_a11y_write_is_idle_without_damage() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let window = app.window_id();
        let disabled = app.app_mut().signal(false);
        let signal = disabled;
        let button = mount(&mut app, move |tree, root| {
            NodeBuilder::boxed()
                .layout_override(Rect::new(0.0, 0.0, 60.0, 24.0))
                .paint(white())
                .on_click(Action::custom("press"))
                .bind_a11y(move |cx| A11yPatch {
                    disabled: Some(*cx.read(&signal)),
                    ..A11yPatch::default()
                })
                .id("press")
                .build(tree, root)
                .expect("button")
        });
        let first = app.drain().expect("first");
        assert!(first.rendered, "首帧必须提交（有背景要画）");
        assert!(
            !app.a11y()
                .expect("first a11y")
                .node_for(button)
                .expect("button")
                .disabled
        );

        app.app_mut().write(&disabled, true);
        let record = app.drain().expect("second");
        assert!(
            !record.rendered,
            "只有 a11y 变化 ⇒ 帧末门禁必须退回 Idle（FrameOutcome::{:?}）",
            FrameOutcome::Idle
        );
        assert!(record.damage.is_empty(), "a11y 更新不产生 damage");
        let flags = app
            .app()
            .window(window)
            .expect("window")
            .tree()
            .get(button)
            .expect("button")
            .dirty();
        assert!(
            !flags.intersects(DirtyFlags::LAYOUT | DirtyFlags::PAINT),
            "`bind_a11y` 不得标 LAYOUT/PAINT：{flags:?}"
        );
        assert!(
            app.a11y()
                .expect("a11y 必须交付")
                .node_for(button)
                .expect("button")
                .disabled,
            "补丁照常生效"
        );
    }

    /// E1 端到端：AT 重新激活（消费端重置）后，**不推进时钟**也必须立即交付全树。
    ///
    /// 修前行为：平台激活回调拿到 `None` 后 AccessKit 停在 `Pending`；`drain_pump` 走生产闸门
    /// `App::a11y_pending`，重置后既没有脏位、也不节流旁路 ⇒ 永远不交付（AT 永久空树）；
    /// 若有脏位则交付增量（`tree: None`）⇒ `accesskit_consumer::Tree::new` panic。
    ///
    /// 变异守卫：删掉 `App::a11y_pending` 的 epoch 旁路（泵不交付）或 `take` 的全树升级
    /// （`tree: None`）⇒ 本用例红。
    #[test]
    fn a_consumer_epoch_reset_delivers_a_full_tree_without_advancing_the_clock() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let window = app.window_id();
        let button = mount(&mut app, |tree, root| {
            NodeBuilder::boxed()
                .layout_override(Rect::new(0.0, 0.0, 60.0, 24.0))
                .paint(white())
                .on_click(Action::custom("press"))
                .id("press")
                .build(tree, root)
                .expect("button")
        });
        // 首笔全树（生产闸门：首笔不受节流影响）。
        let _ = app.drain_pump().expect("首帧");
        assert!(
            app.a11y_update().expect("首笔").tree.is_some(),
            "首笔必须全树"
        );
        app.assert_a11y_contract();

        // 标脏一笔增量：时钟不动 ⇒ 仍在 `A11Y_THROTTLE` 窗口内，普通脏位不得交付
        // （先证明节流仍然有效，否则"不推进时钟"这个前提没有意义）。
        app.app_mut()
            .window_mut(window)
            .expect("window")
            .tree_mut()
            .mark_a11y(button);
        assert!(
            !app.app().a11y_pending(window),
            "限流窗口内的普通脏位不得立即交付"
        );

        // 消费端重置（平台 adapter 的激活/停用回调）：必须立即、全量交付。
        let epoch = app
            .app()
            .window(window)
            .expect("window")
            .a11y_consumer_epoch();
        epoch.bump();
        let _ = app.drain_pump().expect("重置后的泵轮");
        let update = app
            .a11y_update()
            .expect("消费端重置后必须交付（不推进时钟、不依赖脏位）")
            .clone();
        assert!(
            update.tree.is_some(),
            "重置后的第一笔必须携带 TreeInfo（否则 consumer panic / AT 空树）"
        );
        app.assert_a11y_contract();
        // 额外用一份**空**影子树应用该笔：模拟 AT 重新接入时消费端从零开始。
        let mut fresh = crate::test_support::a11y::ShadowTree::new();
        fresh
            .apply(&update)
            .expect("AT 重新接入时全树笔必须能初始化一棵空树");
        assert!(fresh.len() >= 2, "全树笔必须覆盖根 + 按钮");
    }

    /// 造一个「输入框 + 单行子文本」的夹具，返回 TextRun 的引擎句柄。
    fn text_run_fixture(app: &mut TestApp, text: &'static str, style: TextStyle) -> NodeId {
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        // 输入框必须有真实布局（可容纳子文本节点）：用显式尺寸的 Style，不能用
        // `layout_override`（那是叶子节点的绝对定位覆盖，禁止带子节点）。
        let mut inner = taffy::Style::<SmolStr>::DEFAULT;
        inner.size = taffy::geometry::Size {
            width: taffy::style::Dimension::length(240.0),
            height: taffy::style::Dimension::length(24.0),
        };
        let input = NodeBuilder::boxed()
            .style(Style {
                inner,
                clip: Clip::None,
            })
            .a11y(A11yOverride {
                role: Some(A11yRole::TextInput),
                label: Some(SmolStr::new_static("搜索")),
                ..A11yOverride::default()
            })
            .id("input")
            .build(
                app.app_mut().window_mut(window).expect("window").tree_mut(),
                root,
            )
            .expect("input");
        NodeBuilder::text(text, style)
            .layout_override(Rect::new(0.0, 0.0, 160.0, 24.0))
            .id("run")
            .build(
                app.app_mut().window_mut(window).expect("window").tree_mut(),
                input,
            )
            .expect("text run")
    }

    /// E4b：单行显式 LTR 的 TextRun 必须输出 `character_positions` / `character_widths` +
    /// `text_direction`，且与真实 caret 一致（AccessKit 的 caret 高亮 / 屏幕放大镜依赖它）。
    ///
    /// 两条 shape 路径各测一遍：`"价🌍ab"`（parley，CJK+emoji）与 `"42150"`（数字快速路径）。
    #[test]
    fn text_run_exposes_character_metrics_for_a_single_ltr_line() {
        for (text, expected_lengths, caret_byte, position_index) in [
            ("价🌍ab", vec![3_u8, 4, 1, 1], 3_usize, 1_usize),
            ("42150", vec![1_u8, 1, 1, 1, 1], 2, 2),
        ] {
            let mut app = TestApp::new();
            app.enable_a11y_capture();
            let window = app.window_id();
            let run = text_run_fixture(
                &mut app,
                text,
                TextStyle {
                    direction: TextDirection::Ltr,
                    ..TextStyle::new(20.0)
                },
            );
            let _ = app.drain().expect("frame");
            app.assert_a11y_contract();

            let snapshot = app.a11y().expect("a11y delivery");
            let record = snapshot.node_for(run).expect("text run record");
            assert_eq!(record.role, A11yRole::TextRun);
            assert_eq!(record.character_lengths, expected_lengths, "{text:?}");
            assert_eq!(
                record.character_positions.len(),
                expected_lengths.len(),
                "{text:?}：positions 必须与 character_lengths 等长"
            );
            assert_eq!(
                record.character_widths.len(),
                expected_lengths.len(),
                "{text:?}：widths 必须与 character_lengths 等长"
            );
            assert!(
                record
                    .character_positions
                    .windows(2)
                    .all(|pair| pair[0] <= pair[1]),
                "{text:?}：LTR 位置必须单调：{:?}",
                record.character_positions
            );
            assert!(
                record.character_widths.iter().all(|width| *width > 0.0),
                "{text:?}：每个 grapheme 的 advance 必须为正：{:?}",
                record.character_widths
            );

            let update = app.a11y_update().expect("TreeUpdate").clone();
            let ak = update
                .nodes
                .iter()
                .find(|(id, _)| *id == accesskit::NodeId(record.id.get()))
                .map(|(_, node)| node)
                .expect("accesskit node");
            assert_eq!(
                ak.text_direction(),
                Some(accesskit::TextDirection::LeftToRight),
                "{text:?}"
            );
            assert_eq!(
                ak.character_positions(),
                Some(record.character_positions.as_slice()),
                "{text:?}"
            );
            assert_eq!(
                ak.character_widths(),
                Some(record.character_widths.as_slice()),
                "{text:?}"
            );

            // 与真实 caret 交叉验证：caret 物理 x − 节点 bounds 原点 == positions[i]。
            let caret = app
                .app()
                .window(window)
                .expect("window")
                .text_caret_rect(run, caret_byte)
                .expect("caret");
            let bounds = app.probe().bounds(run).expect("bounds");
            let scale = app.app().window(window).expect("window").scale().factor();
            let expected = (caret.min().x - bounds.origin.x) * scale;
            let actual = record.character_positions[position_index];
            assert!(
                (actual - expected).abs() <= 1.0,
                "{text:?}：positions[{position_index}]={actual} 必须与真实 caret x={expected} 一致"
            );
        }
    }

    /// E4b：多行 TextRun **省略**字符度量（AccessKit 的位置模型忽略 y，多行发出去会命中错行）
    /// —— 策略是"省略而不是发错值"，AT 回退到无 caret 高亮。
    #[test]
    fn multi_line_text_run_omits_character_metrics() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let run = text_run_fixture(
            &mut app,
            "价🌍ab\n第二行",
            TextStyle {
                direction: TextDirection::Ltr,
                ..TextStyle::new(20.0)
            },
        );
        let _ = app.drain().expect("frame");
        app.assert_a11y_contract();
        let snapshot = app.a11y().expect("a11y delivery");
        let record = snapshot.node_for(run).expect("text run record");
        assert!(
            !record.character_lengths.is_empty(),
            "多行文本仍必须有 character_lengths（文本接口本身可用）"
        );
        assert!(
            record.character_positions.is_empty() && record.character_widths.is_empty(),
            "多行文本必须省略 character_positions/widths（否则 caret 会命中错行）"
        );
        let update = app.a11y_update().expect("TreeUpdate").clone();
        let ak = update
            .nodes
            .iter()
            .find(|(id, _)| *id == accesskit::NodeId(record.id.get()))
            .map(|(_, node)| node)
            .expect("accesskit node");
        assert!(
            ak.character_positions().is_none()
                && ak.character_widths().is_none()
                && ak.text_direction().is_none(),
            "省略时不得写 character_positions/widths/text_direction（AT 回退路径）"
        );
    }

    /// E4b：`TextDirection::Auto`（未显式声明 LTR）时也**省略**字符度量 —— 方向由首个强方向
    /// 字符决定，consumer 只处理单一 `text_direction`（D6-A），引擎不猜。
    #[test]
    fn auto_direction_text_run_omits_character_metrics() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let run = text_run_fixture(
            &mut app,
            "价🌍ab",
            // 默认 `TextStyle::new` 的方向就是 `Auto`。
            TextStyle::new(20.0),
        );
        let _ = app.drain().expect("frame");
        app.assert_a11y_contract();
        let snapshot = app.a11y().expect("a11y delivery");
        let record = snapshot.node_for(run).expect("text run record");
        assert!(
            !record.character_lengths.is_empty(),
            "文本接口本身仍可用（character_lengths 与方向无关）"
        );
        assert!(
            record.character_positions.is_empty() && record.character_widths.is_empty(),
            "方向为 Auto 时必须省略字符度量（不猜方向、不发错轴的值）"
        );
    }

    /// RE-02：TextRun 的 `A11yPatch::value` 覆盖与真实 shape 文本不一致时，`character_lengths`
    /// 描述的是 value 串、而位置只能描述真实文本 —— 两者不一致 ⇒ **省略**位置/宽度（AT 回退），
    /// 绝不再发出「重复位置 + 零宽尾部」的错位 caret 表。
    #[test]
    fn a_value_override_that_disagrees_with_the_shaped_text_omits_character_metrics() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let run = text_run_fixture(
            &mut app,
            "ab",
            TextStyle {
                direction: TextDirection::Ltr,
                ..TextStyle::new(20.0)
            },
        );
        let window = app.window_id();
        app.app_mut()
            .window_mut(window)
            .expect("window")
            .tree_mut()
            .set_a11y_patch(
                run,
                A11yPatch {
                    value: Some(SmolStr::new_static("ABCDEFGH")),
                    ..A11yPatch::default()
                },
            );
        let _ = app.drain().expect("frame");
        app.assert_a11y_contract();

        let snapshot = app.a11y().expect("a11y delivery");
        let record = snapshot.node_for(run).expect("text run record");
        assert_eq!(
            record.value.as_deref(),
            Some("ABCDEFGH"),
            "value 覆盖本身仍然生效"
        );
        assert_eq!(
            record.character_lengths.len(),
            8,
            "character_lengths 仍描述 value 串（AccessKit 的 Σ == value.len() 约束成立）"
        );
        assert!(
            record.character_positions.is_empty() && record.character_widths.is_empty(),
            "value 与真实文本不一致 ⇒ 必须省略位置/宽度（否则 caret 表错位）：{:?} / {:?}",
            record.character_positions,
            record.character_widths
        );
        let update = app.a11y_update().expect("TreeUpdate").clone();
        let ak = update
            .nodes
            .iter()
            .find(|(id, _)| *id == accesskit::NodeId(record.id.get()))
            .map(|(_, node)| node)
            .expect("accesskit node");
        assert!(
            ak.character_positions().is_none()
                && ak.character_widths().is_none()
                && ak.text_direction().is_none(),
            "RE-02：口径不一致时不得写 character_positions/widths/text_direction"
        );
    }

    /// 造 `count` 个固定几何的行（`layout_override` ⇒ 边界可预测，根的直接子节点）。
    fn rows_fixture(app: &mut TestApp, count: usize) -> Vec<NodeId> {
        let window = app.window_id();
        let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
        let root = tree.root();
        (0..count)
            .map(|index| {
                NodeBuilder::boxed()
                    .layout_override(Rect::new(0.0, index as f32, 10.0, 1.0))
                    .build(tree, root)
                    .expect("row")
            })
            .collect()
    }

    /// E5：分批首树**不节流** —— 不推进时钟（模拟时间静止）也必须逐批交付到收敛。
    ///
    /// 修前行为：`A11Y_THROTTLE` 对**每一笔**生效（首笔之后每批要等 100 ms），40k 节点 ≈
    /// 20 × 100 ms ≈ 2 s。修后只有「单点高频增量」被 100 ms 合并；还有待发批次时立即继续。
    ///
    /// 变异守卫：删掉 `App::a11y_pending` 的 `a11y_backlog_pending()` 旁路 ⇒ 首笔之后
    /// `a11y_pending` 为假（时钟未推进），循环一轮都跑不了，本用例红。
    #[test]
    fn a_large_first_tree_is_not_throttled() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let window = app.window_id();
        let rows = rows_fixture(&mut app, 5000);
        let _ = app.drain_pump().expect("首帧");
        assert!(
            app.a11y_update().expect("首笔").tree.is_some(),
            "首笔必须全树"
        );
        app.assert_a11y_contract();

        let mut batches = 1usize;
        while app.app().a11y_pending(window) {
            let _ = app.drain_pump().expect("后续批次（不推进时钟）");
            let update = app.a11y_update().expect("有待发批次时每轮都必须交付");
            assert!(update.tree.is_none(), "只有首笔携带 TreeInfo");
            app.assert_a11y_contract();
            batches += 1;
            assert!(batches < 16, "分批必须收敛（实际 {batches} 笔）");
        }
        assert!(
            batches >= 3,
            "5000 节点必须分多批（实际 {batches} 笔；若只有 1 笔说明预算/语义被改动）"
        );

        // 增量仍被 100 ms 合并：修复没有把「单点数据更新」也变成连续投递。
        app.app_mut()
            .window_mut(window)
            .expect("window")
            .tree_mut()
            .mark_a11y(rows[0]);
        assert!(
            !app.app().a11y_pending(window),
            "单点增量仍在 A11Y_THROTTLE 窗口内（省电目标不变）"
        );
        app.advance(std::time::Duration::from_millis(100));
        assert!(app.app().a11y_pending(window), "节流窗口到期后增量恢复交付");
        let _ = app.drain_pump().expect("到期补发");
        app.assert_a11y_contract();
    }

    /// E5：DPI 变化（`set_scale`）后的整轮全量重建同样**不受节流**、无停顿地分批补齐，
    /// 且每批 bounds 已乘新 scale。
    #[test]
    fn a_dpi_change_redelivers_all_batches_without_stall() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let window = app.window_id();
        let rows = rows_fixture(&mut app, 5000);
        let _ = app.drain_pump().expect("首帧");
        while app.app().a11y_pending(window) {
            let _ = app.drain_pump().expect("首树剩余批次");
            app.assert_a11y_contract();
        }

        let scale = Scale::new(2.0).expect("scale");
        app.app_mut()
            .window_mut(window)
            .expect("window")
            .set_scale(scale);
        let _ = app.drain_pump().expect("DPI 首笔");
        assert!(
            app.a11y_update().expect("DPI 首笔").tree.is_some(),
            "DPI 变化后的第一笔必须是全树"
        );
        app.assert_a11y_contract();
        // 首笔（DFS 前 2048 节点）含 root + rows[0..2046]：bounds 必须是 逻辑 × 2。
        let first = app.a11y().expect("DPI 首笔 snapshot").clone();
        let record = first.node_for(rows[0]).expect("row 0 在首笔里");
        let logical = app.probe().bounds(rows[0]).expect("logical bounds");
        assert_eq!(
            record.bounds,
            logical.to_physical(scale),
            "DPI 变化后首笔的 bounds 必须已乘新 scale（不是旧值）"
        );

        let mut batches = 1usize;
        while app.app().a11y_pending(window) {
            let _ = app.drain_pump().expect("DPI 剩余批次（不推进时钟）");
            app.assert_a11y_contract();
            batches += 1;
            assert!(batches < 20, "DPI 全树分批必须收敛（实际 {batches} 笔）");
        }
        assert!(batches >= 3, "5000 节点的 DPI 全树必须分多批重发");
    }

    /// E5 / RE-04：三端泵接线所读的 **App 级信号** —— 每笔非最终批次交付后
    /// `a11y_backlog_pending()` 必须为真（平台泵据此 `dispatcher.wake()` 下一轮），收敛后为假；
    /// 单点高频增量不得触发（否则泵会被无谓连续唤醒）。
    ///
    /// 边界：三端泵本身（`event_loop`/`pump` 里三行 wake 接线）需要真实事件循环，单测不执行；
    /// 本用例钉住接线读的**唯一信号**，平台接线后的连续交付由真机/验收记录（报告 §8.3）。
    #[test]
    fn a11y_backlog_signals_a_pump_wake_after_every_non_final_batch() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let window = app.window_id();
        let rows = rows_fixture(&mut app, 5000);
        let _ = app.drain_pump().expect("首帧");
        app.assert_a11y_contract();
        assert!(
            app.app().a11y_backlog_pending(),
            "首笔之后仍有分批 ⇒ 平台必须收到 wake 信号"
        );
        let mut batches = 1usize;
        let mut wakes = 1usize;
        while app.app().a11y_pending(window) {
            let _ = app.drain_pump().expect("后续批次（不推进时钟）");
            app.assert_a11y_contract();
            batches += 1;
            if app.app().a11y_backlog_pending() {
                wakes += 1;
            }
            assert!(batches < 16, "分批必须收敛（实际 {batches} 笔）");
        }
        assert!(batches >= 3, "5000 节点必须分多批");
        assert_eq!(
            wakes,
            batches - 1,
            "除最后一笔外每笔交付后都必须请求 wake（实际 {wakes} 次 / {batches} 批）"
        );
        assert!(
            !app.app().a11y_backlog_pending(),
            "收敛后不得残留 wake 信号（否则泵会空转）"
        );
        // 单点增量不满足 backlog ⇒ 平台不需要额外唤醒（仍走 100ms 合并）。
        app.app_mut()
            .window_mut(window)
            .expect("window")
            .tree_mut()
            .mark_a11y(rows[0]);
        assert!(
            !app.app().a11y_backlog_pending(),
            "单点数据增量不得让泵连续唤醒"
        );
    }

    /// E5 / RE-04：三端接线共享的唯一函数 `App::wake_for_a11y_backlog` —— 有未交付批次时
    /// 必须在测试 dispatcher 上观察到真实 wake；收敛后不得再 wake（避免泵空转）。
    ///
    /// 变异：删掉 `wake_for_a11y_backlog` 里的 `dispatcher.wake()` ⇒ 第一个断言红。
    /// 三端泵各只保留一行调用（真实事件循环不在单测覆盖，见报告 §8.3）。
    #[test]
    fn the_backlog_wake_helper_wakes_the_pump_until_converged() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let window = app.window_id();
        let _rows = rows_fixture(&mut app, 5000);
        let dispatcher = app.app().dispatcher();
        let _ = app.drain_pump().expect("首帧");
        app.assert_a11y_contract();

        let _ = dispatcher.take_frame_wake();
        app.app().wake_for_a11y_backlog(dispatcher.as_ref());
        assert!(
            dispatcher.take_frame_wake(),
            "有未交付批次时，交付后必须唤醒平台泵（RE-04 接线核心）"
        );

        while app.app().a11y_pending(window) {
            let _ = app.drain_pump().expect("后续批次（不推进时钟）");
            app.assert_a11y_contract();
        }
        let _ = dispatcher.take_frame_wake();
        app.app().wake_for_a11y_backlog(dispatcher.as_ref());
        assert!(
            !dispatcher.take_frame_wake(),
            "收敛后不得再唤醒泵（否则空闲零唤醒退化）"
        );
    }
}

/// R13-12：`UserTask` 回投链的端到端用例（`AppHandle::send` → `Dispatcher::push` →
/// `drain_tasks` → `UserTask::run`）。
///
/// 这条链路在旧实现里**零覆盖**：`TestApp` 没有任务泵，a11y 用例全部 `dispatch_request` 直调，
/// 于是「任务被丢掉 / 在帧之前执行 / 重复执行 / 空闲轮不执行」四种 bug 都不可能被测出来。
#[cfg(test)]
mod dispatcher_tests {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    use super::TestApp;
    use crate::{
        math::Rect,
        node::builder::NodeBuilder,
        paint::{
            color::Color,
            style::{PaintSpec, StylePatch},
        },
    };

    /// 跨线程 `send` 的任务必须在下一次 `drain` 里执行，且只执行一次。
    #[test]
    fn a_task_posted_from_another_thread_runs_on_the_next_drain() {
        let mut app = TestApp::new();
        let handle = app.handle();
        let ran = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&ran);
        std::thread::spawn(move || {
            handle.send(move |_app| flag.store(true, Ordering::SeqCst));
        })
        .join()
        .expect("posting thread");

        assert!(
            !ran.load(Ordering::SeqCst),
            "回投任务不能在下一次 drain 之前执行"
        );
        let _ = app.drain().expect("frame");
        assert!(ran.load(Ordering::SeqCst), "drain 必须执行本轮回投的任务");
        assert_eq!(app.tasks_run(), 1, "任务只允许执行一次");
        let _ = app.drain().expect("next frame");
        assert_eq!(app.tasks_run(), 1, "同一条任务不得重复执行");
    }

    /// 任务在帧**之后**执行：它写下的状态必须等到下一轮才出帧。
    ///
    /// 这是生产泵 ④/⑤ 的时序（帧 → a11y → 任务）。若测试泵把任务放在帧之前（或丢了任务泵），
    /// 这条用例立刻变红 —— 正是「依赖 dispatcher 时序的 bug」的回归面。
    #[test]
    fn a_task_runs_after_the_frame_and_its_write_lands_on_the_next_frame() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let color = app.app_mut().signal(Color::from_rgba8([255, 0, 0, 255]));
        {
            let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
            let root = tree.root();
            NodeBuilder::boxed()
                .layout_override(Rect::new(0.0, 0.0, 20.0, 20.0))
                .paint(PaintSpec {
                    background: Color::from_rgba8([0, 0, 0, 255]),
                    ..PaintSpec::default()
                })
                .bind_style(move |cx| StylePatch {
                    background: Some(*cx.read(&color)),
                    ..StylePatch::default()
                })
                .build(tree, root)
                .expect("bound box");
        }
        let first = app.drain().expect("首帧").clone();
        assert!(first.rendered, "首帧必须提交");
        let settled = app.frame_record().frame_index;

        // 任务写状态：发生在**本轮帧之后** ⇒ 本轮不得因此出帧。
        let handle = app.handle();
        handle.send(move |app| {
            app.write(&color, Color::from_rgba8([0, 255, 0, 255]));
        });
        let same_round = app.drain().expect("任务轮").clone();
        assert_eq!(
            app.frame_record().frame_index,
            settled,
            "任务在帧之后执行：本轮的 FrameIndex 不得因这次写入推进"
        );
        assert!(
            !same_round.rendered,
            "任务写下的状态只能下一轮出帧（本轮帧已经跑完）"
        );
        assert_eq!(app.tasks_run(), 1);

        let next = app.drain().expect("下一轮").clone();
        assert!(
            next.rendered,
            "任务写下的状态必须在下一轮出帧（丢了任务泵 / 没有唤醒都会停在这里）"
        );
        assert_eq!(
            app.frame_record().frame_index.get(),
            settled.get() + 1,
            "任务写入只允许产生一帧"
        );
    }

    /// 空闲轮也要执行任务（生产泵的 ⑤ 与帧门禁无关：没有帧可出时任务照样回投）。
    #[test]
    fn tasks_run_even_when_the_frame_is_idle() {
        let mut app = TestApp::new();
        let _ = app.drain().expect("首帧");
        let handle = app.handle();
        let ran = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&ran);
        handle.send(move |_app| {
            counter.fetch_add(1, Ordering::SeqCst);
        });

        let record = app.drain().expect("空闲帧").clone();
        assert!(!record.rendered, "任务本身不产生像素 ⇒ 帧门禁必须 Idle");
        assert_eq!(
            ran.load(Ordering::SeqCst),
            1,
            "空闲帧也必须执行回投任务（生产泵的 ⑤ 与帧门禁无关）"
        );
    }

    /// 一次 drain 执行队列里的**全部**任务，且按 push 顺序（FIFO）。
    #[test]
    fn tasks_run_in_fifo_order_and_only_once() {
        let mut app = TestApp::new();
        let handle = app.handle();
        let order = Arc::new(Mutex::new(Vec::new()));
        for index in 0..3usize {
            let log = Arc::clone(&order);
            handle.send(move |_app| log.lock().expect("task log").push(index));
        }
        let _ = app.drain().expect("frame");
        assert_eq!(
            *order.lock().expect("task log"),
            vec![0, 1, 2],
            "回投任务必须按 push 顺序执行"
        );
        assert_eq!(app.tasks_run(), 3);
        let _ = app.drain().expect("next frame");
        assert_eq!(app.tasks_run(), 3, "队列取空后不得重复执行");
    }
}
