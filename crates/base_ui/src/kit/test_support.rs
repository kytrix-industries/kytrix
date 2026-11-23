//! 测试断言辅助（`cfg(test)`；基于 `ui_core::test_support` 的 `TestApp` / `Probe` / `FrameRecord`）。
//!
//! 目的：把「无 GPU 引擎怎么开、帧怎么推、图元怎么读」的样板收在一处，供本批与后续 P2 批次复用
//! （设计 §6.6 / D-06 的测试折算口径：GUI 测试用 `TestApp` 的帧记录 + 布局探针，不用像素比较）。
//!
//! # 两个入口
//!
//! - [`test_app`] / [`test_app_with_scale`]：**同步**引擎（`TestApp`，current-thread runtime）。
//!   适合布局 / 图元 / 颜色 / 尺寸类断言：图标和品牌图都走同步 `ResourceCache::insert*`。
//! - [`DecodeApp`]：**异步解码**宿主。`TestApp` 的 current-thread runtime 在构造后不会被驱动，
//!   `insert_async*` 的后台任务不会跑；`DecodeApp` 用单 worker 的 multi-thread runtime + 公开的
//!   `App` API（`App::new` / `create_window` / `frame` / `take_resource_results`）复刻平台泵，
//!   让异步解码结果真的经引擎轮询交付。
//!
//! # 读图元的两种视图
//!
//! - [`Image`] 列表（[`image_primitives`]）：从 `Window::scene()` 读**本帧实际编码的图元**，
//!   含 `modulation` 等绘制参数（`FrameRecord` 的 `PrimitiveRecord` 只有种类 / 盒，不够）。
//! - [`frame`]：一次 `drain` 的 `FrameRecord`（调度 / 统计 / 增量口径）。
//!
//! 本模块**只**服务测试：版本间不保证 API 稳定（`cfg(test)` 编译，不进发布物）。

use std::{thread, time::Duration};

use ui_core::{
    a11y::{A11yNode, A11yNodeId, A11yTreeSnapshot},
    app::App,
    math::{Rect, Scale},
    node::id::NodeId,
    paint::primitive::{Image, Primitive},
    test_support::{
        a11y::{ShadowTree, tree_update_snapshot},
        app::TestApp,
        record::FrameRecord,
    },
    window::{WindowId, WindowOptions},
};

/// 默认测试引擎（800×600、scale 1.0、空树）。
#[must_use]
pub fn test_app() -> TestApp {
    TestApp::new()
}

/// 指定 DPI 的测试引擎（图标档位 / 物理目标尺寸类断言用）。
///
/// # Panics
/// `scale` 非有限或 `<= 0`（`Scale::new` 的契约；测试里传错是测试 bug）。
#[must_use]
pub fn test_app_with_scale(scale: f32) -> TestApp {
    let scale = Scale::new(scale).expect("测试 DPI 必须是有限正数");
    TestApp::with_options(WindowOptions {
        scale,
        ..WindowOptions::default()
    })
}

/// 当前窗口的根节点。
///
/// # Panics
/// 窗口不存在（`TestApp` 的窗口在构造时创建，测试生命周期内不会关）。
#[must_use]
pub fn root(app: &TestApp) -> NodeId {
    app.app()
        .window(app.window_id())
        .expect("测试窗口必须存在")
        .tree()
        .root()
}

/// 推进一帧并返回调度记录（帧记录）。
///
/// # Panics
/// 帧失败（`EngineError`：测到的真实失败，测试应当在这里就停）。
pub fn frame(app: &mut TestApp) -> FrameRecord {
    app.drain().expect("测试帧").clone()
}

/// 本帧场景里的全部图片图元（绘制序；`modulation` 即运行时调制色）。
#[must_use]
pub fn image_primitives(app: &TestApp) -> Vec<Image> {
    app.app()
        .window(app.window_id())
        .expect("测试窗口必须存在")
        .scene()
        .iter()
        .filter_map(|primitive| match primitive {
            Primitive::Image(image) => Some(*image),
            _ => None,
        })
        .collect()
}

/// 节点的逻辑像素布局盒（`Probe::bounds` 的简写）。
#[must_use]
pub fn bounds(app: &TestApp, node: NodeId) -> Option<Rect> {
    app.probe().bounds(node)
}

/// 浮层的**引擎包装节点**（E12 之后组件 a11y 角色的落点）：从面板节点沿父链走到 root 的直接子。
///
/// 浮层内部结构是 E13 的「包装 → 组根 → 面板」，但组根层数属于 `kit::overlay` 的实现细节；
/// 这里以「root 的直接子」为准，不依赖中间层数。
///
/// # Panics
/// `panel` 不在该窗口树里 / 面板不在 root 子树下（调用点是测试，说明组件没打开）。
#[must_use]
pub fn overlay_wrapper(app: &App, window: WindowId, panel: NodeId) -> NodeId {
    let tree = app.window(window).expect("测试窗口必须存在").tree();
    let root = tree.root();
    let mut cursor = panel;
    while let Some(parent) = tree.get(cursor).ok().and_then(|node| node.parent()) {
        if parent == root {
            return cursor;
        }
        cursor = parent;
    }
    panic!("浮层面板不在 root 子树下：{panel:?}");
}

/// 断言本帧恰好一张图片图元，并返回它。
///
/// # Panics
/// 图片数量不是 1（断言失败，带全部图片的调试信息）。
pub fn assert_single_image(app: &TestApp) -> Image {
    let images = image_primitives(app);
    assert_eq!(images.len(), 1, "期望恰好一张图片，实际：{images:?}");
    images[0]
}

/// 异步解码宿主：单 worker 的 multi-thread runtime + 一个 headless `App` + 一个窗口。
///
/// 用法：
/// ```ignore
/// let mut host = DecodeApp::new();
/// let mut images = AssetImages::new();
/// images.request(host.app_mut(), path, Size::new(24.0, 24.0))?;
/// let loaded = host.poll_until(600, |app| {
///     let loaded = images.poll(app);
///     (!loaded.is_empty()).then_some(loaded)
/// });
/// ```
pub struct DecodeApp {
    /// 保活：`App` 只持 `Handle`，runtime 被 drop 后后台解码会停（本字段没有读点，是 RAII 守卫）。
    _runtime: tokio::runtime::Runtime,
    app: App,
    window: WindowId,
}

impl DecodeApp {
    /// 新建宿主（窗口 800×600、scale 1.0）。
    ///
    /// # Panics
    /// runtime 建不起来（测试环境的基础设施故障）。
    #[must_use]
    pub fn new() -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("异步解码测试的 runtime");
        let mut app = App::new(runtime.handle().clone());
        let window = app.create_window(WindowOptions::default());
        Self {
            _runtime: runtime,
            app,
            window,
        }
    }

    /// 可变 `App`（投递异步资源 / 建树用）。
    pub fn app_mut(&mut self) -> &mut App {
        &mut self.app
    }

    /// 只读 `App`。
    #[must_use]
    pub fn app(&self) -> &App {
        &self.app
    }

    /// 窗口句柄。
    #[must_use]
    pub fn window_id(&self) -> WindowId {
        self.window
    }

    /// 反复「出帧 + 探测」直到 `probe` 返回 `Some` 或次数用尽；返回最后一次探测结果。
    ///
    /// 每次迭代先 `App::frame`（帧入口会轮询引擎的异步解码并填充交付缓冲），再调用 `probe`；
    /// 两次之间睡 2 ms 让后台解码线程有机会完成（与平台泵的「空闲等待」等价）。
    ///
    /// # Panics
    /// 出帧失败（`EngineError`）。
    pub fn poll_until<T>(
        &mut self,
        attempts: usize,
        mut probe: impl FnMut(&mut App) -> Option<T>,
    ) -> Option<T> {
        for _ in 0..attempts {
            let _ = self.app.frame(self.window).expect("解码宿主的帧");
            if let Some(value) = probe(&mut self.app) {
                return Some(value);
            }
            thread::sleep(Duration::from_millis(2));
        }
        probe(&mut self.app)
    }
}

impl Default for DecodeApp {
    fn default() -> Self {
        Self::new()
    }
}

// ── a11y 断言辅助（P2-2e：列表 / 表格 / 树采纳 E9 集合语义） ────────────────────────────

/// 逐笔校验 `TreeUpdate` 的影子树。
///
/// 与 [`TestApp::assert_a11y_contract`] 同源（都用 `ui_core::test_support::a11y::ShadowTree`，
/// 含 E9b 的**父唯一性**契约），区别是这里由测试自己**逐笔**应用：可以断言「每一笔都合法」，
/// 而不是只看 `TestApp` 记录的最近一笔。用法：
///
/// ```ignore
/// let mut updates = A11yUpdates::new();
/// // 每个 drain 之后：
/// updates.apply(&app);
/// // 场景结束：
/// assert!(updates.applied() >= 2, "多笔交付都必须过契约");
/// ```
#[derive(Debug, Default)]
pub struct A11yUpdates {
    shadow: ShadowTree,
    applied: usize,
}

impl A11yUpdates {
    /// 空影子。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 应用最近一帧的 `TreeUpdate`（没有交付时返回 `false`）。
    ///
    /// # Panics
    /// `TreeUpdate` 违反 AccessKit 契约（父唯一 / 无孤儿 / 删除可达 / 焦点存在）。
    pub fn apply(&mut self, app: &TestApp) -> bool {
        let Some(update) = app.a11y_update() else {
            return false;
        };
        if let Err(error) = self.shadow.apply(update) {
            panic!(
                "TreeUpdate 违反 AccessKit 契约：{error}\n--- update ---\n{}",
                tree_update_snapshot(update)
            );
        }
        self.applied += 1;
        true
    }

    /// 已应用的笔数。
    #[must_use]
    pub fn applied(&self) -> usize {
        self.applied
    }

    /// 影子树里的节点数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.shadow.len()
    }

    /// 影子树是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.shadow.is_empty()
    }
}

/// 强制一笔全树 a11y 交付并返回快照（同时校验「增量树 = 全树」oracle 与最近一笔契约）。
///
/// 需要先 [`TestApp::enable_a11y_capture`]。
///
/// # Panics
/// a11y 增量与全量分叉，或最近一笔违反契约。
#[must_use]
pub fn full_a11y(app: &mut TestApp) -> A11yTreeSnapshot {
    if let Some(diff) = app.a11y_oracle_diff() {
        panic!("a11y 增量与全量分叉：{diff}");
    }
    app.assert_a11y_contract();
    app.a11y().expect("强制全树交付必须产出 a11y 快照").clone()
}

/// 全树 a11y 快照，但**跳过**「增量 = 全树」oracle 断言（只校验契约）。
///
/// 用途：组合控件（虚拟列表面板 + 浮层焦点陷阱）下，引擎的 a11y 增量在「节点变为可聚焦后
/// 的 `Focus` 动作」上会与全树分叉（P2-4c 复现：`List` 容器增量缺 `Focus` 动作、全树有）。
/// 这是引擎 a11y 脏位缺口（登记在 `.docs/gui-migration/spec-deltas/kit-menu.md` 残余），
/// 组件侧无法修；本入口让「角色 / 标签 / 状态」类断言不被它阻塞，oracle 本身仍有
/// [`full_a11y`] 覆盖。
///
/// # Panics
/// 最近一笔 `TreeUpdate` 违反 AccessKit 契约。
#[must_use]
pub fn a11y_snapshot_without_oracle(app: &mut TestApp) -> A11yTreeSnapshot {
    let _ = app.a11y_oracle_diff();
    app.assert_a11y_contract();
    app.a11y().expect("强制全树交付必须产出 a11y 快照").clone()
}

/// 推进一帧并断言本帧的 a11y `TreeUpdate` 合法（没有交付的帧是 no-op）。
///
/// # Panics
/// 帧失败，或本帧 `TreeUpdate` 违反契约。
pub fn frame_a11y(app: &mut TestApp) {
    let _ = app.drain().expect("a11y 帧");
    app.assert_a11y_contract();
}

/// 快照里某个引擎句柄的 a11y 记录（全树快照上使用）。
///
/// # Panics
/// 快照里没有该节点（多半是拿增量快照当全树用了）。
#[must_use]
pub fn a11y_of(snapshot: &A11yTreeSnapshot, node: NodeId) -> &A11yNode {
    snapshot
        .node_for(node)
        .unwrap_or_else(|| panic!("a11y 快照里没有引擎节点 {node:?}（需要全树快照）"))
}

/// 快照里某个引擎句柄的 a11y 身份。
///
/// # Panics
/// 快照里没有该节点。
#[must_use]
pub fn a11y_id(snapshot: &A11yTreeSnapshot, node: NodeId) -> A11yNodeId {
    a11y_of(snapshot, node).id
}

/// a11y 树的硬不变量：每个节点**只有一个父**，且父的 `children` 必须列出它（E9 父唯一性 /
/// 无孤儿）。与 `ui_core::a11y::tree` 测试里的同名 helper 同口径，断言面是引擎快照。
///
/// # Panics
/// 双父 / 孤儿 / children 指向快照里不存在的节点。
pub fn assert_single_parent_per_node(snapshot: &A11yTreeSnapshot) {
    let mut by_id: ahash::HashMap<A11yNodeId, &A11yNode> = ahash::HashMap::default();
    for node in snapshot.nodes() {
        by_id.insert(node.id, node);
    }
    let mut parent_of: ahash::HashMap<A11yNodeId, A11yNodeId> = ahash::HashMap::default();
    for node in snapshot.nodes() {
        for child in &node.children {
            let Some(child_node) = by_id.get(child) else {
                panic!(
                    "节点 {:?} 的 children 里有快照外的子节点 {child:?}（不可达 / 孤儿）",
                    node.engine
                );
            };
            if let Some(previous) = parent_of.insert(*child, node.id) {
                assert_eq!(
                    previous, node.id,
                    "节点 {:?} 同时挂在 {previous:?} 与 {:?} 两个父下",
                    child_node.engine, node.id
                );
            }
        }
    }
    for node in snapshot.nodes() {
        let Some(parent) = node.parent else {
            continue;
        };
        let Some(parent_node) = by_id.get(&parent) else {
            // 窗口根的 a11y 父不在快照范围内；其余父必须在。
            continue;
        };
        assert!(
            parent_node.children.contains(&node.id),
            "节点 {:?} 的父 {parent:?} 没有把它列进 children（孤儿）",
            node.engine
        );
    }
}

#[cfg(test)]
mod tests {
    use ui_core::{math::Rect, node::builder::NodeBuilder, resources::DecodeState};

    use super::{
        DecodeApp, assert_single_image, bounds, frame, image_primitives, root, test_app,
        test_app_with_scale,
    };

    /// 默认引擎：窗口存在、根节点稳定、空树没有图元。
    #[test]
    fn the_default_harness_is_a_live_window_with_an_empty_tree() {
        let mut app = test_app();
        let window_root = root(&app);
        let record = frame(&mut app);
        assert!(record.frame_index.get() >= 1, "首帧必须有提交序号");
        assert_eq!(root(&app), window_root, "帧不重建根节点");
        assert!(image_primitives(&app).is_empty(), "空树没有图片图元");
        assert!(bounds(&app, window_root).is_some(), "根节点必须有布局盒");
    }

    /// 布局探针：`layout_override` 的盒从 `Probe` 读得回来（后续批次断言几何的统一入口）。
    #[test]
    fn the_probe_reads_layout_boxes() {
        let mut app = test_app();
        let window = app.window_id();
        let rect = Rect::new(4.0, 6.0, 20.0, 12.0);
        let window_root = root(&app);
        let node = NodeBuilder::boxed()
            .layout_override(rect)
            .build(
                app.app_mut().window_mut(window).expect("窗口").tree_mut(),
                window_root,
            )
            .expect("挂载");
        let _ = app.drain().expect("首帧");
        assert_eq!(bounds(&app, node), Some(rect));
    }

    /// `assert_single_image`：恰好一张时返回它，多于 / 少于一张时断言失败。
    #[test]
    fn assert_single_image_counts() {
        let mut app = test_app();
        let window = app.window_id();
        let window_root = root(&app);
        for _ in 0..2 {
            let _ = crate::kit::icon::IconSpec::new(crate::kit::icon::IconKind::Check)
                .build(app.app_mut(), window, window_root)
                .expect("图标");
        }
        let _ = app.drain().expect("首帧");
        assert_eq!(image_primitives(&app).len(), 2);
        let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert_single_image(&app);
        }));
        assert!(failure.is_err(), "两张图时 assert_single_image 必须失败");
    }

    /// DPI 工厂：窗口 scale 真的被设置（图标档位断言依赖它）。
    #[test]
    fn the_scale_factory_sets_the_window_scale() {
        let hdpi = test_app_with_scale(2.0);
        assert_eq!(
            hdpi.app()
                .window(hdpi.window_id())
                .expect("窗口")
                .scale()
                .factor(),
            2.0
        );
        let normal = test_app();
        assert_eq!(
            normal
                .app()
                .window(normal.window_id())
                .expect("窗口")
                .scale()
                .factor(),
            1.0
        );
    }

    /// 异步宿主：`insert_async` 的结果真的经引擎帧轮询交付（这正是 `TestApp` 拿不到的路径）。
    #[test]
    fn the_decode_host_delivers_async_results() {
        let mut host = DecodeApp::new();
        let bytes = crate::kit::icon::raster(
            crate::kit::icon::IconKind::User,
            crate::kit::icon::RasterTier::Small48,
        );
        let pending = host
            .app_mut()
            .resources_mut()
            .insert_async(bytes::Bytes::from_static(bytes));
        let delivered = host.poll_until(600, |app| {
            let results = app.take_resource_results();
            results
                .into_iter()
                .find(|(handle, _)| *handle == pending)
                .map(|(_, state)| state)
        });
        let Some(state) = delivered else {
            panic!("异步解码必须在 ≈1.2 s 内交付");
        };
        match state {
            DecodeState::Ready(image) => {
                assert_eq!((image.width, image.height), (48, 48));
            }
            other => panic!("图标 PNG 应解码成功，实际 {other:?}"),
        }
    }
}
