# Kytrix `ui_core` 审查报告（架构 / 性能 / 正确性）

- **审查对象**：`crates/ui_core` 生产代码（不含 `examples/`），基线 commit `a9a4220`，工作区干净。
- **规模**：`src/` 127 个 Rust 文件、约 5.86 万行（含内联单测）；另有三套手写 shader 与构建期契约校验。
- **方法**：9 个并行子审查（核心状态、布局交互、绘制共享层、Win32 平台、D3D12、macOS/Metal、Linux 平台、Vulkan、文本资源无障碍）逐模块核对调用链，再由主审交叉验证关键结论；未修改任何生产代码。
- **基准**：Windows 本机 release（criterion quick）；macOS 交叉 `cargo check` 通过；Linux 交叉 check 因宿主缺 `pkg-config` sysroot 未能完成（环境问题，非代码问题），Linux 结论为静态审查。
- **质量门禁实测**：`cargo test -p ui_core --features test-support`（807 单测 + 6 + 4 + 8 + 36 集成/场景测试全绿）、`clippy -D warnings`、`fmt --check`、`cargo doc` 全部通过。

---

## 0. 结论摘要

### 0.1 一句话结论

`ui_core` 的**工程基础**（三平台原生后端、契约校验、错误模型、测试密度）明显高于一般自研 GUI 原型，但**渲染数据通路仍是"全量重建 + 全量上传 + 按脏矩形重放"的原型形态**，且**复杂图表所需的动态几何通道尚未闭环**。以当前状态直接替换 GPUI，会先在图表场景的帧率和正确性上失败。

### 0.2 分维度评价

| 维度 | 评价 | 依据 |
|---|---|---|
| 模块分层与依赖方向 | **好** | 编译期选定单后端，无 wgpu/winit 抽象层；节点/场景/渲染职责清晰 |
| 三平台后端完整度 | **中上** | D3D12+DirectComposition、Metal+CAMetalDisplayLink、Vulkan+Wayland 均为真实原生实现 |
| 目标场景性能 | **不足** | 增量帧慢于全量帧（实测）；文本 32 标签 ≈ 6.8 ms；图元成本随总量线性 |
| 渲染正确性 | **有隐患** | Windows/Linux 各有一处实例基址错位（latent P0）；几何通道填充色/变换丢失 |
| 平台集成正确性 | **中** | Windows 剪贴板越界读；Linux IME 双通路、无键盘重复、滚轮量纲错；macOS 节奏源在主线程 |
| API 成熟度 | **中下** | 存在公开但不可用（`Geometry`）、不可更新（`Canvas`）、语义不一致（`visible`）的 API |
| 工程质量 | **好** | 强类型错误、契约静态断言、无 `allow`/`expect` 掩盖、注释解释"为什么" |

### 0.3 必须优先解决的 6 件事

1. **渲染通路重构**：局部刷新目前不减少 CPU 编码/上传，也不减少 GPU 顶点处理（§2.1）。
2. **图表几何通道闭环**：`Geometry`/`Path` 在生产路径不可用，且填充色/变换被丢弃（§2.2）。
3. **实例基址错位**：Windows D3D12 与 Linux Vulkan 各有一处，接上几何后首帧即错（§1.1）。
4. **虚拟列表布局时序**：布局后挂载的行永远是 0×0（§2.3）。
5. **文本高频路径**：每帧全树 clone+哈希，shape cache 仅 256 条（§2.4）。
6. **窗口生命周期**：运行时建窗/关窗不生效，Linux 只支持单窗口（§2.5）。

---

## 1. P0 —— 正确性阻塞（当前可能潜伏，接通几何后必现）

> 判定说明：两处 P0 都是"代码确定错误、但需要场景中出现 `Path`/`Geometry` 才会触发"。当前 `GeometryId` 在生产构建无法构造，所以尚未在产品路径暴露；一旦图表开始用几何通道，就是首帧即错。

### 1.1 Windows D3D12 与 Linux Vulkan 的实例基址错位

**证据**
- Windows：`render/windows/compositor.rs:1522` `DrawInstanced(4, len, 0, start)`；`render/shaders/quad.hlsl:11-12,35` 用 `SV_InstanceID` 直接索引 `instances[instance_id]`。
- Linux：`render/linux/compositor.rs:1433` `cmd_draw(command, 4, len, start, 0)`；`render/shaders/quad.vert.glsl:11-14` 用 `gl_InstanceIndex` 索引，并用 `gl_VertexIndex` 构造 quad 角点。
- macOS 对照：`render/macos/compositor.rs:700-706` 正确使用 `baseInstance = start`。
- 分段来源：`paint/instance.rs:266-275`（`flush_run`）与 `:401-423`（`Path`/`Geometry` 前先 flush）。

**问题**
- D3D12 的 `StartInstanceLocation` **不会**加到 `SV_InstanceID` 上（这正是 HLSL SM 6.8 新增 `SV_StartInstanceLocation` 的原因，见 [HLSL 提案 0015](https://microsoft.github.io/hlsl-specs/proposals/0015-extended-command-info/)）。因此每个 `Instances` 段都读 `instances[0..len]`，`start` 被忽略。
- Vulkan 把 `start` 传给 `firstVertex`：`gl_VertexIndex` 从 `start` 起算 → 角点 `(v&1, v>>1)` 不再是 `(0,0)/(1,0)/(0,1)/(1,1)`；同时 `firstInstance = 0` → 又读错实例。Linux 是**双重错误**。

**影响**：任何"quad → Path/Geometry → quad"的帧，第二段及之后的图元会重复第一段、角点退化、颜色/位置错乱。静默错误像素，不报 API 错误，像素 golden 只覆盖单一场景所以测不出来。

**成熟实现对照**：Metal 用 `baseInstance`；Vulkan 对应物是 `firstInstance`；D3D12 需要显式把 base instance 通过 root constant 或 SRV 偏移传进 shader。

**方向**
- Windows：加一个 `instance_base` root constant，shader 索引改为 `instances[instance_id + instance_base]`；或每段绑定不同 SRV 起始偏移。
- Linux：`cmd_draw(command, 4, len, 0, start)`。
- 补一条跨平台像素回归：`Quad → Geometry → Quad`，断言第二段取自 `start..start+len`。把 `FrameOp::Instances.start` 的语义写进 `shader/contract.md` 唯一权威处。

**置信度**：高（参数语义与 shader 索引均已逐项核对）。

### 1.2 `Geometry` / `Path` 通道未闭环

**证据**
- `paint/geometry.rs:34-41`：`GeometryId` 只有 `#[cfg(any(test, feature="test-support"))]` 的 `for_test`；全仓无 `GeometryStore`、无公开注册/释放 API。
- `node/builder.rs:159-166` 的 `NodeBuilder::geometry`、`paint/canvas.rs:86-98` 的 `push_path`/`push_geometry` 都是公开 API，但应用无法合法取得 `GeometryId`。
- `paint/instance.rs:401-413`：`Primitive::Path` 只写 `geometry` + `Transform::IDENTITY`，**`Path.fill` 从未读取**。
- `paint/instance.rs:137-149`：`FrameOp::Geometry` 不携带 `transform`/颜色。
- `render/windows/compositor.rs:1770-1806`：`geometry_root` 把 `color0` 硬编码为不透明白、basis 硬编码单位矩阵，`GeometryDraw.transform` 完全不消费。
- `render/macos/compositor.rs:710-719`、`render/linux/compositor.rs:1437-1446`：任何几何 op 直接返回 `EngineError::GeometryRegister`。

**影响**：K 线实体、面积图、深度图、折线这些"复杂图表"的主路径不可用。公开 API 给出的是"能编译、能挂载、运行时报错或画成白色"的行为，对迁移团队是严重的排错陷阱。

**成熟实现对照**：Skia 的 `drawVertices`/`drawPath` 把颜色与当前矩阵一起提交；WebRender 的 display item 把 color/transform 放在同一绘制项；Qt `QSGGeometryNode` 支持静态几何 + `markDirty(DirtyGeometry)` 局部更新。

**方向**
- 二选一，不要含糊：**(a)** 完整实现 `GeometryStore`（公开注册/释放、代际校验、三平台 GPU 缓冲、`fill`/`transform` 通过 uniform 或逐 draw 常量传入、动态 vertex buffer 更新接口）；**(b)** 在实现前把 `NodeBuilder::geometry`/`Canvas::push_path`/`Primitive::Path` 从公开面移除。
- 连续 `FrameOp::Geometry` 应合并为一次提交（同 pipeline/state），否则"每根蜡烛一个 draw call"仍不可行。
- `GeometryRegistry::lookup`（`render/windows/pipeline.rs:540-544`）目前只按 index 查表，未校验 generation，槽位复用后会取到新几何。

**置信度**：高（`fill`/`transform` 在代码中确实无读者，平台分支是显式 `Err`）。

---

## 2. P1 —— 目标场景的严重性能/正确性风险（迁移前必须解决）

### 2.1 局部刷新不产生局部成本（三平台共性，核心问题）

**证据**
- 每个 damage 矩形重放整份 draw list：`render/windows/compositor.rs:1243-1262`、`render/macos/compositor.rs:432-449`、`render/linux/compositor.rs:877-897`；单测 `render/windows/compositor.rs:2186-2189` 明确断言"2 个 damage 矩形 = 2 次 draw op"。
- 每帧全场景重新编码：`window.rs:1031-1032` → `paint/instance.rs:236-264`（`clear` + 遍历全部 lane/primitive）。
- 每帧整块复制：`render/frame.rs:72-88`；每帧整块上传：`render/windows/compositor.rs:1138-1145`、`macos:368-375`、`linux:629-637`。
- `render/damage.rs:17`：最多 16 个矩形，超过退化为全帧。
- 实测：`frame_full_512` ≈ 222 µs，`frame_incremental_512`（只改 1 个节点）≈ 288 µs，**增量反而更慢**。

**影响**：图表常见的"多标签/多指标同时刷新"会生成 4–16 个 damage 矩形，GPU 顶点处理被放大 4–16 倍；即使只有 1 个矩形，全部实例仍进入顶点阶段。CPU 侧成本与"场景总图元数"成正比而非"变化量"。这是与"高性能复杂图表"目标最直接的冲突。

**成熟实现对照**：Chrome `cc` 以 tile/picture layer 为单位重绘，只光栅失效 tile；Qt Quick 用 scene graph 节点 `markDirty` 合并批次；WebRender picture cache 只重绘 dirty tile；Skia 保留 display list/GPU buffer。没有成熟实现会把整帧命令流按每个 dirty rect 重放。

**方向（分三步，不要一步到位）**
1. **止血**：`Partial` 多矩形先合并为一个包围盒执行一次绘制（present 仍可用多个 dirty rect），把最坏情况从 N× 降到 1×。
2. **减编码**：`Scene`/lane 持有稳定 GPU 区间与脏区间，只重编码脏 lane；`FramePayload` 从"整帧快照"改为"基线 + 补丁"；`ColorResolver` 结果缓存在 `Primitive`/lane 上（当前每图元都重新做色彩空间转换，宽色域还含 24 次 chroma 二分）。
3. **真局部**：为 `FrameOp::Instances` 记录实例 AABB，按 damage 做 CPU 侧批次剔除；或引入 tile/retained command list。

**验收标准**：`frame_incremental_*` 必须显著快于 `frame_full_*`（≥2×）；10 万图元场景单帧增量更新 ≤ 8 ms；单 lane 变化时上传字节 ≈ 变化 lane 的实例字节。

### 2.2 虚拟列表：布局后挂载的行永远是 0×0

**证据**：`app.rs:2655`（`layout_if_dirty`）→ `app.rs:2679`（`rewindow_virtuals` 在布局与 `layout_text` **之后**）；`app.rs:1754-1783` 在布局后 `build` 新行；`node/tree.rs:1131-1139` 的 `settle_frame` 无条件清掉全树 `LAYOUT|PAINT` 并置 `REUSABLE`；`node/tree.rs:1592-1607` 的 `set_virtual_row` 只标 `LAYOUT`。

**问题**：首次建列表时视口为 0，只挂 1 行；布局后 `rewindow_virtuals` 补挂其余行，但这些节点本帧从未参与 taffy 布局。帧尾 `settle_frame` 清掉它们的 `LAYOUT`，下一帧 `needs_frame` 与 `layout_if_dirty` 都看不到脏位 → 行永久 0×0、不产生图元、不可见，直到某次无关的布局失效。

**影响**：行情表/成交明细/自选列表是核心场景；首屏只显示 1 行。现有测试 `virtual_list_mounts_rows_on_the_first_frame_and_grows_on_resize`（`app.rs:5316-5381`）只断言挂载区间（state.first/last），没断言行高，所以没暴露。机制上可确定，建议先用 `Probe` 断言行高 `bounds.height == extent` 复现一次再修。

**成熟实现对照**：Flutter sliver 在 layout 阶段按 viewport 惰性构建并当场布局；Qt `QAbstractItemView` 在 `updateGeometries` 中先算可见区再布局新项；Chrome LayoutNG 保证新挂载的 LayoutObject 在同一布局代内参与布局。

**方向**：`settle_frame` 只清"本帧实际参与过布局"的节点的 `LAYOUT`（加一个 layout epoch 标记）；或把 `rewindow_virtuals` 拆两阶段（布局前按上帧视口预挂载，布局后若 changed 则同帧再跑一次 `compute_layout`）。补"首帧后行高 == extent"的断言。

### 2.3 文本 pass 每帧全树 clone + 哈希；shape cache 容量不足

**证据**：`app.rs:2831-2867` 每帧遍历 `tree.order()`，对每个文本节点 `text_content(node).cloned()` 再 `shape_cached`；`text/context.rs:89-95` 每次构造 `ShapeKey` 都 `write(content.text.as_bytes())`（全文哈希）；`text/cache.rs:65-68` `ENTRY_LIMIT = 256`、`BYTE_BUDGET = 8 MiB`；`text/cache.rs:81-88` 命中还要 `order.iter().position` 线性扫描。

**问题**：窗口内不同文本超过 256 条时，LRU 在两次全量扫描之间必然逐出，`Rc::ptr_eq` 永不命中 → `set_text_shaped` 清空 `line/broken` → `layout_text` 全量重排 + 重光栅。实测 32 个文本同时变化：shape ≈ 3.23 ms、layout/raster ≈ 3.59 ms，合计 ≈ 6.8 ms/帧，已接近 60 Hz 预算的一半，且尚未计入绘制与上传。

**另两个相关问题**
- `text/shape.rs:154-160` 的 `approx_bytes` 用 `line.items().count() * 32` 估算，`items()` 是 run 数不是字形数 → 长单 run 文本的预算失真 10–100 倍，8 MiB 上限形同虚设。
- `text.rs:92-113` 的 `TextStyle` 没有 `font_features`/`font_variations`，无法请求 `tnum`（等宽数字）→ 价格/成交量列宽随内容抖动，表格无法对齐。这是高频数字表格的硬需求。

**成熟实现对照**：Chrome LayoutNG 把 shaping 缓存在 `ShapeResult` 并跟随 LayoutObject 生命周期；Skia `ParagraphCache` 按工作集配置容量；Flutter `TextStyle.fontFeatures`；CSS `font-variant-numeric: tabular-nums`。

**方向**：给 `NodeTree` 增加 text 脏集合，`refresh_text` 只处理集合内节点；把 node 级 `TextSlot.key` 检查前移到全局缓存查询之前；shape cache 改为弱引用或按工作集自适应；`TextStyle` 增加 `font_features`（含 `tnum` 便捷常量）并纳入缓存键。数字标签建议走"等宽数字 + 预计算 advance"快路径。

### 2.4 `FrameScheduler` 无 RAII abort，panic 后窗口永久冻结

**证据**：`window.rs:187-194` `FrameTicket(())` 无 `Drop`；`window.rs:215-239` `begin` 置 `Draining`，只有显式 `commit`/`abort` 复位；`app.rs:2633-2635` 把 `begin_frame` 的 `None` 当 `Idle` 返回。

**问题**：`refresh_styles`/`refresh_text`/`layout_if_dirty`/`paint_frame` 都可能执行用户闭包并 panic。panic 展开时 `FrameTicket` 只是被 drop，状态永远停在 `Draining`；之后 `request_frame()` 返回 false → `begin_frame` 返回 `None` → `App::frame` 返回 `Ok(Idle)`，窗口再也不出帧，且没有 API 能复位。

**成熟实现对照**：wgpu 的 surface texture / frame guard、GPUI 的 frame guard 都在 `Drop` 里回滚；Chromium `BeginFrame` 失败显式 `AbortFrame`；Qt `QPainter::begin/end` 也是 RAII。

**方向**：`FrameTicket` 改为带 `Drop` 的 guard（默认 `abort`，`commit` 用 `ManuallyDrop` 消费）；`App::frame` 外层 `catch_unwind` 转成 `EngineError`；补"panic 后再次 `App::frame` 仍能提交"的回归。

### 2.5 节点卸载不解除依赖边，死 owner 无界累积

**证据**：`node/dependency.rs:35-38` 注释明确"已卸载的 owner 允许残留，由 `App::write` 的 retain 惰性剔除"；`app.rs:1745-1749`（虚拟列表回收行直接 `tree_mut().remove`）、`app.rs:1890-1898`（`remove_node`）都不碰依赖图；`app.rs:3043` 每次写还要把整个 `OwnerSet` 快照成 `SmallVec`。

**问题**：只有 `close_window` 会对**仍存活**的节点 detach。虚拟列表反复滚动、浮层反复开关，每回收一个节点就留一条死边；`write` 是最高频热路径，成本从 O(活跃读者) 退化为 O(历史挂载过的所有读者)，并伴随每次写的堆分配。

**成熟实现对照**：GPUI 的 `SubscriberSet` 在 entity drop 时同步移除订阅；Qt `QObject` 析构自动断开 signal-slot；Chrome 的 paint invalidation 集合随 LayoutObject 销毁移除。

**方向**：`NodeTree::remove` 返回被删子树的 `(NodeId, deps)`，由 `App` 在删除前批量 `graph.detach`；或让依赖图按 `(WindowId, slot)` 索引，节点槽释放时按槽清理。

### 2.6 字形图集换代不清其他窗口的文本 lane（跨窗口 stale UV）

**证据**：`app.rs:2813-2816` 任一窗口 `scale_epoch` 变化就 `atlas.clear()` + `glyph_atlas.clear()`（两者都是**进程级**）；`node/tree.rs:1298-1315` 的 `set_text_line` **不调用 `mark_paint`**；`paint/pass.rs:187-190` 的 lane 复用只依赖 `REUSABLE` + clip key。

**问题**：触发窗口自身的文本节点因为 `scale_epoch` 不匹配会重新 shape 并 `mark_layout`/`mark_paint`，没问题。但**其他窗口**：`ShapeKey` 不含 atlas generation，shape cache 未被清（`text.cache` 与 `glyph_atlas` 是两套缓存），`Rc::ptr_eq` 命中 → `refresh_text` 跳过；随后 `layout_text` 发现 generation 变了会重排并 `set_text_line`，但**不标 PAINT** → paint 复用旧 lane 里的旧 `AtlasTile` UV，指向已重排的图集页。

**影响**：多窗口下某个窗口切 DPI/缩放、或长会话触发字形图集扩容后，其他窗口文本错乱（错字/花字/采样到别的字形），且要等一次无关 PAINT 才可能恢复。

**方向**：atlas generation 变化时遍历所有窗口对所有文本节点 `mark_paint`；或把 generation 写进 `TextSlot` 并纳入 paint 复用判据；更彻底是按窗口/scale 分图集（共享字形缓存、各自 UV 页）。

### 2.7 图片图集页由第一张图锁定

**证据**：`resources.rs:169-173`、`:198-207`、`:213-230`；`render/atlas.rs:42-49`。

**问题**：首次 `insert` 按该图尺寸选 512/1024/2048 并**永久锁定**。先加载 16×16 图标 → 页 = 512² → 之后任何 >510 px 的图必然 `DecodeCause::Capacity`。错误文案提示"unload 后重试"，但页尺寸永不变，重试无效。加载顺序决定整个进程的图片上限。

**成熟实现对照**：Skia/Chrome 图像缓存多页或独立纹理；Qt `QPixmapCache` 按大小淘汰；Flutter/Impeller `TextureAtlas` 支持多页。

**方向**：小图进共享 atlas、大图走独立纹理（最简可靠）；或多页图片 atlas，`AtlasTile` 增加 page 字段（需三平台同步改 `texture_index` 语义，属契约变更，要慎重）。

### 2.8 窗口生命周期是启动静态的；Linux 仅单窗口

**证据**：`app.rs:2142-2149` `create_window` 只插 `App.windows`；`platform/windows/event_loop.rs:606-613`、`platform/macos/event_loop.rs:71-103` 只在 `run` 启动时枚举一次；`platform/linux/event_loop.rs:37-115` 只取 `app.windows().next()`；`platform/windows/event_loop.rs:923-942` 只有用户点 X 才销毁原生窗口。

**问题**：运行时新建的设置/图表窗口不会创建原生窗口（`App::frame` 对着无 renderer 的 `Window` 空转）；应用调 `App::close_window` 只删 App 状态，原生窗口可能残留（幽灵窗口、事件投递到已不存在的状态）；Linux 多窗口完全不工作。

**成熟实现对照**：Chrome/Qt/GPUI 的窗口创建/销毁都是平台层生命周期的一部分，由应用层命令驱动。

**方向**：增加平台窗口生命周期命令（Create/Destroy/Show/Hide/Resize）与 App↔平台窗口注册表；若 v1 明确只支持启动期建窗 + 用户关闭，就把 `create_window`/`close_window` 的契约写进 API 文档并在 debug 断言，避免静默失效。Linux 若要多窗口必须先支持多 surface/多 renderer。

### 2.9 多窗口 = 每窗口独立 GPU 设备 + 独立 GPU 线程

**证据**：`render/windows/renderer.rs:1-31`（每窗口一条 `kytrix-d3d12` 线程）；`render/windows/compositor.rs:240`（每窗口 `Device::new()`）、`:274-301`（每窗口 Queue/Pipeline/AtlasTextures/RTV 堆）；`render/windows/swapchain.rs:65-67`（每次建链都新建 DXGI factory）；`render/macos/renderer.rs:120-141`；`render/linux/compositor.rs:151-162`。

**问题**：N 个窗口 = N 个 device + N 条队列 + N 套 PSO/根签名 + N 份图集纹理（glyph 4 MiB + image 16 MiB + 常驻 staging ~4.5 MiB/窗口）。图集更新时 UI 线程还要逐窗口 `to_vec()` 同一份像素。窗口创建在 UI 线程同步做设备创建、PSO 编译、两条交换链 prime + fence 等待，开窗卡顿。

**成熟实现对照**：Chrome 单 GPU 进程/单设备服务多窗口；Qt RHI 一个 device 对应多个 swapchain；D3D12 官方推荐同一设备多交换链。

**方向**：把 `Device`/`Queue`/`Pipeline`/`AtlasTextures` 提升到进程级（单设备多 swapchain/多 DComp visual），每窗口只留 surface/swapchain/persist/fence；窗口创建时 UI 线程只建 HWND + DComp target，GPU 侧异步挂接。

### 2.10 Windows 剪贴板读取无边界（越界读）

**证据**：`platform/windows/clipboard.rs:105-117`：`GlobalLock` 后按 `u16` 逐字扫描到 NUL，没有 `GlobalSize` 限制，也没有最大长度。`set_text` 在 `SetClipboardData` 失败时也没有 `GlobalFree`（`:56-75`）。

**问题**：`CF_UNICODETEXT` 由其它进程写入，Windows 只保证"约定上以 NUL 结尾"。若块内无 NUL，会越过块尾继续读 → 未定义行为/访问违例。用户从外部程序复制异常文本后，Kytrix 一读剪贴板就可能崩。

**成熟实现对照**：Win32 剪贴板读取必须先用 `GlobalSize` 限定可读字节；Chrome/Qt 都以格式数据长度或 `GlobalSize` 为边界，把"无 NUL"当无效数据。

**方向**：`GlobalSize` 取上限，`size/2` 限制扫描；奇数长度/空块/无 NUL 返回明确错误或空串（二选一并写进文档）；失败分支补 `GlobalFree`。

### 2.11 Linux 输入协议三处硬伤

**证据与问题**
1. **IME 双通路重复插入**：`platform/linux/surface.rs:673-694` 的 `translate_key` 在每次 Key Down 无条件 `key_get_utf8` 并 push `InputEvent::Text`；同窗口 text-input-v3 的 `CommitString`（`surface.rs:1214-1237`）也转成 `ImeEvent::Commit`；`platform/linux/ime.rs` 的 `composing` 状态从未参与判断。macOS（`view.rs:108-119`）和 Windows（`ime.rs:57-63`）都有抑制，Linux 是唯一缺的 → 输入"ni"可能得到"ni你"。
2. **完全没实现键盘重复**：`surface.rs:1003-1077` 的 `Dispatch<wl_keyboard>` 只处理 Keymap/Modifiers/Key/Enter/Leave，`_ => {}` 忽略 `RepeatInfo`。Wayland 规定重复由客户端按 `repeat_info` 自行合成 → 长按 Backspace/方向键只生效一次。
3. **滚轮量纲错**：`surface.rs:1138-1153` 把 `wl_pointer.axis.value`（surface-local 连续值，一格通常 10–15）当成"1/120 行"除以 120 → 0.08–0.12 行/格，滚动慢约一个数量级；真正的离散步长在 `axis_value120`（v8+，本实现绑定 v9）与 `axis_discrete`，两者都被 `_ => {}` 丢弃；也没有按 `wl_pointer.frame` 批处理。

**另**：`surface.rs:1033-1041` 丢弃 `wl_keyboard.modifiers.group`，`update_mask(..., 0, 0, 0)` 恒 group 0 → 切换键盘布局后字符仍按第一布局解析。

**方向**：IME 组合期间抑制 `wl_keyboard` 文本（或统一交给 text-input-v3）；保存 `(rate, delay)` 并用 calloop timer 合成重复键；按 `frame` 批处理，`axis_value120/120` 走行滚动、连续 `axis` 走像素滚动；`mods` 结构加 `group` 并传入 `update_mask`。

### 2.12 macOS：节奏源在主线程 + 帧率范围下限 30 Hz

**证据**：`render/macos/renderer.rs:425-427` 把 `CAMetalDisplayLink` 挂到 `NSRunLoop::mainRunLoop()`；`platform/macos/event_loop.rs:212-240,290-298` 又在同一主线程做布局/文本/paint/`app.frame`；`render/macos/renderer.rs:414-420` 用 `CAFrameRateRange::new(30.0, max, max)`。

**问题**
- 主线程被 CPU 帧工作占住时，display link delegate 晚到 → GPU 线程只能在更晚的 vsync 拿 drawable → 成串 skipped vsync。GPU 线程隔离只解决了一半，节奏源仍在 UI 上。
- `minimum = 30` 是告诉系统"30 fps 也可接受"，ProMotion 上会在 GPU 跟得上时降到 30 产生 judder。

**成熟实现对照**：Apple GPTK 明确要求 engine work 放专用 render thread、`CAFrameRateRangeMake(max,max,max)` 锁定刷新率；Chrome/Qt 同样把 drawable 获取与合成节奏与 UI 线程解耦。

**方向**：display link 挂到 GPU 线程自有 run loop；`min = preferred = max = NSScreen.maximumFramesPerSecond`；连续帧 timer（当前写死 `1.0/60.0`，`platform/macos/window.rs:316-317`）改按屏幕刷新率。

**另需确认**：`render/macos/queue.rs:221-232` 注释声称"Metal 完成回调可能乱序"并用 `CompletionOrder` 处理 retire，但三槽上传缓冲只用全局 3-permit semaphore 保护、`FrameSlot.wait_value` 从未读取。两条路必须选一条（per-slot `MTLSharedEvent`，或确认同队列完成有序后删掉 `CompletionOrder`），否则存在"后完成帧的 permit 让新帧覆盖仍被旧帧读取的上传缓冲"的风险。

### 2.13 Windows 全链路 `INFINITE` 等待，device lost 可能永久卡死

**证据**：`render/windows/queue.rs:289-306`（`WaitForSingleObject(INFINITE)`）、`render/windows/renderer.rs:1066-1068,1087-1117`（`WaitForMultipleObjects(INFINITE)`、`MsgWaitForMultipleObjects(INFINITE)`）、`render/windows/compositor.rs:1067-1073`（`Drop` 里 `wait_idle`）。

**问题**：所有 fence/事件等待无超时，等待期间不查 `GetDeviceRemovedReason`。device 被移除或 GPU 挂起时 fence 可能不再 signal → GPU 线程卡死 → UI 线程 join 无限等待 → 窗口关不掉、设备无法重建。TDR、驱动更新、显卡切换、远程桌面都是现实事件。

**方向**：所有 fence/交换链等待加有限超时（1–2 s），超时后查 `GetDeviceRemovedReason` 并走 device-lost 退出/重建；`Drop` 不应在 device 已丢失时继续无限 `wait_idle`。

---

## 3. P2 —— 重要但可排期

### 3.1 平台集成

| 问题 | 证据 | 影响 | 方向 |
|---|---|---|---|
| `WM_SETCURSOR` 未处理，`DefWindowProc` 用类光标覆盖应用光标 | 无 `WM_SETCURSOR` 分支；`platform/windows/window.rs:437-439` 类光标=箭头；`event_loop.rs:722-727` 泵里 `SetCursor` | 手型/I-beam/resize 光标在鼠标移动后被重置为箭头 | 在 `WndProc` 保存期望光标并处理 `WM_SETCURSOR` 返回 TRUE |
| `WM_GETMINMAXINFO` 把"最小客户区"当"最小窗口尺寸" | `event_loop.rs:349-358`、`window.rs:231-238` | 实际客户区可比目标小约 16×39 px，图表最小布局被压坏 | 用 `AdjustWindowRectExForDpi` 把客户区约束换算成外框 |
| `WM_DPICHANGED` 先按 `(100,100)` 重排再套建议矩形 | `event_loop.rs:887-901` + `window.rs:367-377` | 跨屏/缩放切换两次 `SetWindowPos`、两轮布局、闪烁 | 只应用 `lParam` 建议矩形 |
| 多窗口指针捕获/光标/帧定时器硬编码 `windows.first_mut()` | `event_loop.rs:722-751`；`app.rs:134-137` 的 `capture_request`/`cursor_pending` 不带 WindowId | 第二窗口拖拽捕获到第一窗口；首窗关闭后定时器不迁移，剩余窗口停帧 | 改为 `(WindowId, …)` 并存到每个 `NativeWindow` |
| `WM_CAPTURECHANGED` 只在自绘边框缩放分支处理 | `event_loop.rs:407-413` | 普通拖拽中切系统菜单后 App 的 `DragSession` 残留，后续点击全被吞 | 提升为通用分支：投递 PointerCancel / 清 drag+press；同时处理 `WM_CANCELMODE` |
| 修饰键按消息顺序维护，失焦不复位 | `platform/event.rs:126-135,213-222`；`WM_KILLFOCUS` 不清 modifiers | Alt+Tab 后 Shift/Ctrl 粘住 → 误拖拽/误选择 | 用 `GetKeyState` 读实时状态，或失焦时清空并补发 KeyUp |
| IME 始终开启，无 `ImmAssociateContext(NULL)` | `platform/windows/ime.rs` 无该调用 | 非文本控件焦点下 IME 截获按键，快捷键失效/弹候选窗 | 按"当前是否可输入"enable/disable IME |
| `WM_CLOSE` 无条件关窗，无 veto；`WM_QUERYENDSESSION` 未处理 | `event_loop.rs:518-529,923-942` | 应用无法弹"确认退出"，注销/关机无保存钩子 | `CloseRequested` 由 App 决定；补会话结束钩子 |
| 标题栏拖动进 `DefWindowProc` 模态循环，泵阻塞 | `event_loop.rs:377-386` 只接管边框，不含 `HTCAPTION` | 拖动期间动画/HUD 停帧，鼠标事件积压后一次性回放 | 接管 `HTCAPTION` 做自定义移动循环，或合并指针事件 |
| Linux `xdg_toplevel.close` 未转 `CloseRequested` | `surface.rs:977-979`；`event_loop.rs:168-172` 直接 `close_window` | 应用无法拦截关闭，三平台语义不一致 | 只 push `CloseRequested`，由应用决定 |
| Linux 忽略 decoration `configure` 模式 | `surface.rs:285-297,1240-1251` | KWin 强制 ServerSide 时双标题栏 | 保存实际模式并切换 CSD/SSD |
| Linux 忽略 `xdg_toplevel.suspended` | `surface.rs:957-981` 只解析 Maximized | 被遮挡/锁屏/最小化仍 60 Hz 出帧，耗电 | suspended 时暂停帧与 200ms 兜底 |
| Linux 无 `wl_touch`、无 `wl_pointer.set_cursor` | `surface.rs:123-130,1283-1300`；`event_loop.rs:164-165` 丢弃光标请求 | 触摸屏失效；所有控件显示箭头 | 绑定 `wl_touch`；用 `wayland-cursor`/`wp_cursor_shape_v1` |
| Linux 剪贴板复用已 cancelled 的 `wl_data_source`，从不 destroy `wl_data_offer` | `surface.rs:237-241`；`clipboard.rs:104-113,178-180` | 第二次复制可能失效；对象泄漏 | 每次发布新建 source，cancelled 即销毁；替换 selection 时销毁旧 offer |
| macOS 无 `NSMenu`/mainMenu | 全仓无 `NSMenu` 匹配；`event_loop.rs:62-65` | Cmd+Q/C/V/X/A、全屏等标准快捷键不可用 | 在 `finishLaunching` 前构建 App/Edit/Window 菜单 |
| macOS 失焦不清修饰键/IME/hover | `window.rs:244-249`、`view.rs:440-446` | Cmd-Tab 后快捷键粘滞、marked text 残留 | 失焦时清 modifiers、unmark、补 KeyUp |
| macOS IME 未 activate/deactivate，忽略 `replacementRange` | `view.rs:257-369` | 重转换/中英切换丢字重复字 | 持有 `NSTextInputContext` 并管理生命周期，处理 replacementRange |
| macOS `CAMetalLayer.colorspace` 未设置 | `render/macos/swapchain.rs:54-67` | P3/sRGB 双屏颜色不一致 | 显式 `kCGColorSpaceSRGB` |
| macOS display link 参数只在建窗时读一次 | `window.rs:191-198`、`event_loop.rs:80-85` | 拖到 120 Hz 屏仍按旧刷新率 | 监听 `NSWindowDidChangeScreen` 等通知并重建 |
| Linux 设备选择取第一个图形队列设备，未排除 CPU/软件适配器 | `render/linux/device.rs:159-195` | 可能静默选中 lavapipe 或 iGPU；首块不达标直接报错不回退 | 全量打分：跳过 CPU、优先 DISCRETE，选前先校验能力 |
| Linux swapchain 未用 min/maxImageExtent、supportedUsageFlags、supportedCompositeAlpha | `render/linux/swapchain.rs:110-141,170-196` | 大窗口 resize 或特殊合成器上建链失败 | 夹紧 extent、校验 usage、按 supported 选 composite alpha |
| Linux `SURFACE_LOST_KHR` 未走恢复路径 | `swapchain.rs:310-315,337-347` vs acquire 路径 `:270-279` | 合成器重启/热插拔直接粘性退出 | present 侧同样映射为可恢复错误 |

### 3.2 渲染与绘制

| 问题 | 证据 | 影响 | 方向 |
|---|---|---|---|
| 顶点 clip 夹紧破坏 image/glyph 的 UV | `quad.vert.glsl:19-34`、`quad.hlsl:19-34`、`quad.metal:24-40`；`contract.md:104-116` 称"纯优化" | 被部分裁剪的文本/图标被拉伸压缩（滚动列表边缘常见） | UV 按未夹紧的原始 world 位置求比例，或对图集图元不做顶点夹紧 |
| `StylePatch.opacity` 是逐图元 alpha，非组透明度；Canvas/Geometry 完全不参与 | `paint/pass.rs:438-444,455-542,579-604` | 重叠子图元二次混合；图表淡入淡出完全无效 | 文档明确为逐图元 alpha；或为 Canvas 加 opacity；中期做离屏 group layer |
| 任意 layout 变化清祖先 `REUSABLE` 并产生根级全幅 damage | `node/tree.rs:1877-1886,1894-1923`；`paint/pass.rs:187-210,528-540` | 价格文本宽度变化 → 每 tick 全帧重绘 | 布局冒泡只标 LAYOUT 不累积祖先 damage；用子节点 old/new bounds 并集 |
| `Canvas.bounds` 由应用手工维护，错了静默 under-damage | `paint/canvas.rs:34-38,53-98`；`paint/pass.rs:592-596` | 图表残影/旧柱子残留，难排查 | `push_*` 自动 union bounds；或 paint 展开内部图元 damage |
| 字形图集单页无淘汰，满了全清重光栅 | `render/atlas.rs:19-30`；`text/atlas.rs:138-159` | 长会话 CJK + 多字号可能周期性抖动 | 多页 glyph atlas + LRU tile 淘汰 |
| 图集页变化时 `wait_idle` + 重置全部 in-flight | `render/windows/atlas.rs:209-242`；`render/linux/atlas.rs:118-131` | 字体/DPI/首张大图时明显掉帧 | 新纹理 + 新 SRV + retire 旧纹理，不阻塞队列 |
| 全量帧仍 persist→back 整面 copy + 4 次 barrier | `render/windows/compositor.rs:1590-1639` | 4K/60 约 2 GB/s 额外带宽 | 常规帧直接渲染到 back buffer；persist 只用于 resize 过渡 |
| 未处理 `DXGI_STATUS_OCCLUDED` | `render/windows/swapchain.rs:155-167` | 被遮挡窗口仍按 vsync 渲染 | 区分 occluded 并暂停 GPU 循环 |
| macOS 每帧整面 blit persist→drawable | `render/macos/compositor.rs:454-461,725-750` | 5K/120Hz 可超 4 GiB/s | 全帧直接渲染到 drawable；partial 需实测后决定 |
| Linux 每帧整面 `cmd_copy_image` | `render/linux/compositor.rs:908-1011` | 同上 | 评估每 image 内容/age 记账，或明确 partial 只省 raster |
| Linux 上传缓冲每帧 map/unmap | `render/linux/compositor.rs:1328-1352` | 每帧 2 次驱动调用 | 持久映射（与 Windows F-210 一致） |
| 离屏 `submit_snapshot` 从不清 `fresh` | `render/linux/compositor.rs:482-484` | 离屏/基准每帧全量，掩盖 partial 问题 | 成功后调 `frame.presented()` |
| `Plan::merge` 只拼接不合并 damage | `render/compositor.rs:87-101` | GPU 忙时累积到 16 个矩形，放大重放成本 | 复用 `DamageAccumulator` 的合并规则 |
| `FLAG_OPAQUE` 判据未排除圆角/AA | `paint/instance.rs:554-556`；`contract.md:57-65` | 一旦启用 no-blend 管线，圆角外会写成黑色 | 收紧为"轴对齐矩形、无圆角、无 AA"或删除该 flag |
| `Clip::Bounds` 不裁剪节点自身内容（文本/Canvas） | `paint/pass.rs:208-237` | 容器 clip 不住自己的 Canvas | `emit_content` 外包一层 clip，或文档明确限制 |
| 背景色变更整条 1×1 swapchain 重建 | `render/windows/compositor.rs:660-689`；`swapchain.rs:65-96` | 主题动画时 COM/内核对象 churn | 保留一条链只 clear+present；factory 提升进程级 |
| 适配器选择只取第一个非软件适配器 | `render/windows/device.rs:352-375` | 混合显卡可能选到 iGPU | `EnumAdapterByGpuPreference(HIGH_PERFORMANCE)` + 配置覆盖 |

### 3.3 布局与交互

| 问题 | 证据 | 影响 | 方向 |
|---|---|---|---|
| 焦点遍历不排除 `display:none`/`visible:false` | `interaction/focus.rs:21-26` vs `hit_test.rs:109-115` | Tab 落到不可见控件，Enter 触发隐藏 action | 抽出与 hit_test 共享的可见性谓词 |
| `StylePatch.visible=false` 只隐藏自身，hit_test 却排除整棵子树 | `paint/pass.rs:177-182,438-451` vs `hit_test.rs:113-115` | "看得见但点不动"的死像素 | 统一语义（自身 or 子树） |
| `Position::Fixed` 布局在视口，paint/hit 却按树祖先滚动偏移平移 | `layout/taffy_tree.rs:124-126`；`hit_test.rs:130-135`；`paint/pass.rs:239-246` | 滚动容器内的固定工具栏/图例漂移且命中错位 | 维护 `scroll_exempt`，或禁止滚动子树内使用 Fixed |
| 内容缩小/视口变大后滚动偏移不重新夹紧 | `interaction/scroll.rs:22-40` 只被 `app.rs:1521` 调用；`app.rs:1483-1497,1718-1787` | 列表缩短后空白/过卷 | 布局与重开窗后统一 `normalize_scroll_offsets` |
| 命中测试每次指针事件全树递归，只对叶子做盒外剪枝 | `hit_test.rs:129-158`；`app.rs:1018,1074,1173` | 复杂图表 hover 是主要 CPU 热点 | 维护 `subtree_bounds` 剪枝或空间索引 |
| `focus_next` 每次 Tab 全树扫描 + `Vec` 分配；焦点不自动滚入视口 | `interaction/focus.rs:31-68`；`app.rs:1423-1446` | 大表格 Tab 卡顿；焦点在视口外不可见 | 维护焦点链缓存；焦点变化调用现有 `scroll_into_view` |
| 滚轮只选最近滚动祖先，不做滚动链；非 Shift 时丢弃水平 delta | `scroll.rs:53-62`；`app.rs:1175-1193` | 嵌套滚动边界吞事件；触控板横向滚不动 | 逐级向上传递；按轴选 delta |
| `on_drag` 按下即开会话，零位移也抑制 click；阈值不参与 | `app.rs:976-1008,1076-1102`；`drag.rs:9-12` | 可拖拽 tab/图表平移无法兼具点击 | 超阈值才开会话；抬起先更新 current |
| 浮层 outside-click 越过顶层 `Explicit`；Above/Left 未计自身尺寸 | `overlay.rs:28-47`；`interaction.rs:681-684`；`app.rs:243-257` | 嵌套 dialog 关闭顺序错；上方/左侧菜单位置错 | 遇到第一个 overlay 就停；按实际尺寸定位 + viewport flip |
| `MAX_CLIP_DEPTH=8`：paint 整帧失败，hit 静默截断 | `paint/pass.rs:228-236`；`hit_test.rs:53-59` | 合法复杂界面因 clip 深度失败；两系统行为不一致 | 轴对齐矩形 clip 做累计交集，从根上消除上限 |
| `order_insert`/`subtree_len`/`order_pos` 使建删为二次复杂度 | `node/tree.rs:1967-2031`；`app.rs:1762-1782` | 建几万节点秒级卡顿；虚拟列表滚动周期性掉帧 | 缓存 subtree_len、`Vec::splice` 整块插入，或帧初一次 DFS 重建 order |
| `Key::Other(u16)` 用平台私有键码 | `event.rs:103,283-289`；`platform/event.rs:246`、`macos/view.rs:535-568`、`linux/surface.rs:1306-1340` | 同一快捷键 binding 无法三平台通用，失败静默 | 引入逻辑键枚举，平台层做 physical→logical 映射 |
| 滚动动画每帧 `scroll_to` → `request_frame` → `dispatcher.wake` | `app.rs:2102-2106,1506-1536,744-747`；`macos/event_loop.rs:205-208` 注释已警告 830fps 忙循环 | 滚动动画自唤醒绕过平台节流，单核打满 | 拆出 `WakePolicy`，帧内只改 offset + damage |

### 3.4 文本 / 资源 / 无障碍

| 问题 | 证据 | 影响 | 方向 |
|---|---|---|---|
| 可变字体实例与合成样式未进光栅，`GlyphKey` 不含 variation | `text/shape.rs:242-262`；`text/raster.rs:85-97`；`text/atlas.rs:13-19`；默认字体栈首项是 `Segoe UI Variable Text`（`text.rs:336`） | 粗体/斜体按默认轴渲染，与 Parley advance 不一致 | 传 `normalized_coords`/`synthesis`，纳入 `GlyphKey` |
| 整 ppem + `hint(true)` + 无亚像素定位 | `text/shape.rs:244,267-268`；`text/raster.rs:94` | 125%/150% 缩放下字距跳动/个别字形重叠 | 按字号选 hinting，高 DPI 用 fractional ppem 或子像素桶 |
| 空文本节点的 inline box 被丢弃 | `text/shape.rs:191-193,221-232` | 纯图标/徽标行内内容消失，行高塌陷 | 特例改为 `text.is_empty() && inline_boxes.is_empty()` |
| 形状缓存按 run 数计费 | `text/shape.rs:154-160` | 长文本实际占用可达预算 10–100 倍 | 按 `positioned_glyphs().count()` 累加 |
| `TextPassReport` 的 raster/tofu/atlas 统计从未填充 | `text.rs:498-515`；`app.rs:2927-2939` 忽略 `_stats` | 缓存命中率/图集重建无法诊断 | 在 `layout_text` 聚合 `GlyphAtlasStats` |
| 图片无 ICC 色彩管理、EXIF 方向未处理 | `resources/image.rs:1-12` | Display P3 截图偏色、相机 JPEG 旋转 | 明确契约或接 `moxcms`（已在依赖图内） |
| 图片解码在主线程同步 | `resources.rs:213-215` | 大图/批量图标阻塞 UI | 异步解码或构建期预处理 |
| a11y 实时数据无 live region 与节流 | `a11y/adapter.rs:57-62,101-110,118-127`；`a11y.rs:236-269` 无 `live` | 屏读器被价格变化刷屏，用户无法听其它信息 | `A11yNode` 加 `live`（默认 Off），加最小间隔/帧合并 |
| a11y 表格/虚拟列表缺行列语义 | `a11y.rs:51-78` 无 Row/Cell；`a11y/tree.rs:125-140` 无 pos_in_set | 盘口/成交明细无法按"第 N 行共 M 行"导航 | 加 Row/Cell/ColumnHeader + pos_in_set/size_of_set |
| a11y Slider/CheckBox/Switch 默认动作与数值通道缺失 | `a11y/mapping.rs:97-105,121-139`；`a11y/adapter.rs:266-280` | 标准 RangeValue/Toggle 模式不可用 | 补 Increment/Decrement、numeric_value/min/max |
| a11y 文本选区索引是 grapheme 而非 UTF-8 字节 | `a11y/action.rs:104-111`；`text/inline_flow.rs:69-89` | CJK/emoji 选区错位 | 建 TextRun + character_lengths，或明确不支持 |
| macOS a11y `raise()` 期间取走 adapter | `macos/view.rs:426-438` | VoiceOver 更新瞬间读到空树/焦点跳容器 | raise 期间保持可查询的稳定只读上下文 |

---

## 4. P3 —— 改进项（择要）

- Linux 初始化失败路径大量裸 Vulkan 句柄无 RAII（`instance.rs:77-90`、`device.rs:273-325`、`swapchain.rs:200-224` 等），初始化重试/嵌入场景会泄漏。
- Linux 无显存预算/内存压力处理（`device.rs:277-281`），4K 多窗口 + 2048² 图集可能 OOM。
- Linux 关闭时 `queue_wait_idle` 后直接销毁 swapchain，未证明 presentation engine 已释放（`compositor.rs:1101-1113` 与 `swapchain.rs:15-17` 自身注释矛盾）。
- macOS `WindowRenderer::drop` 在主线程 join GPU 线程，驱动挂死时窗口关闭最长卡 2 s（`renderer.rs:79-84,543-563`）。
- Windows 启动失败/panic 展开路径销毁顺序与正常路径相反（`event_loop.rs:598-641`），可能先销毁 HWND 再等 GPU 线程退出。
- Windows IME 提交去重按 Unicode 标量计数而非 UTF-16 码元（`ime.rs:41-45`），非 BMP 字符会多吞/漏吞 `WM_CHAR`。
- 颜色动画 `lerp_color` 自实现非预乘 sRGB 插值，透明端点发黑，且与 `ColorResolver::interpolate` 重复（`interaction/animation.rs:41-69`）。
- `TextContext.family_list` 字段只当布尔用，每次 shape miss 仍重建 Vec（`text/context.rs:103-106`）。
- `TextContext::set_font_stack` 同内容也无条件 bump 版本并清缓存（`text/context.rs:67-74`）。
- `ShapedLine.first_baseline` 取自 max-content 快照而非最终 layout（`text/shape.rs:319-322`）。
- `cargo check --target x86_64-unknown-linux-gnu` 在宿主缺 pkg-config sysroot 时失败，CI 需保证 sysroot 或改用容器。

---

## 5. Kytrix 高频图表场景专项

### 5.1 现状：一个 1 万根 K 线的图表会怎么走

1. 建一个覆盖整个图表的 `Canvas`，`push_quad` 约 2–3 万次（实体 + 影线）。
2. 每次数据 tick 重建整个 Canvas 节点（`Canvas` 不可变，无更新 API），或为每根 K 线建独立节点 + `bind_style`。
3. 每帧：全树扫描 5–6 次 → 全场景编码 2–3 万实例（约 2.5–3.8 MB）→ 复制进 `FramePayload` → 上传 GPU → 按每个 damage 矩形重放全部实例。
4. Canvas 的 bounds 覆盖整图 → damage 面积 >50% → 直接退化为 `Plan::Full`。

结论：成本与"图表总图元数"成正比，与"本次变化量"无关。基准已证明增量帧没有优势。

### 5.2 要达到 60/120 Hz 的目标预算

| 指标 | 目标 | 现状风险 |
|---|---:|---|
| @60Hz UI 线程单帧预算 | ≤ 16.7 ms | 全量编码 + 6 次树扫描可能单帧超 |
| @120Hz | ≤ 8.3 ms | 32 个文本标签已 ≈ 6.8 ms |
| 单帧实例上传 | 只传变化 lane/区间 | 当前整帧上传 |
| damage 绘制成本 | 与 dirty 面积成正比 | 当前 N×全场景 |
| 图表平移/缩放 | 只改 transform/uniform | 当前需重建 Canvas/实例 |

### 5.3 建议的图表渲染架构

1. **静态几何常驻 GPU**：网格、坐标轴、低频几何用 `GeometryStore` + 动态 vertex buffer；平移/缩放只改 uniform。
2. **动态数据用可增量更新的显示列表**：每根 K 线/每个数据段对应稳定 GPU 区间，tick 只更新变化区间。
3. **图层分离**：背景网格 / 数据层 / 交叉线+HUD / 文本层各自独立 lane，更新互不影响。
4. **文本层专用缓存**：等宽数字 + 预计算 digit advance；数值变化只更新 glyph run。
5. **vsync 驱动**：按显示器刷新率调度，GPU 侧用 present wait/backpressure。

---

## 6. 与成熟实现对照（速查）

| 主题 | `ui_core` 现状 | Chrome / Qt / Skia |
|---|---|---|
| 局部刷新 | damage 只作 scissor，仍重放全场景 | tile-based raster + invalidation set |
| GPU 数据更新 | 每帧全量实例 buffer | 持久 buffer + 部分更新（GrMesh/QSGGeometry） |
| 图表几何 | 通道未闭环、颜色/变换丢失 | 静态/动态 geometry buffer + transform uniform |
| 文本缓存 | 256 条 shape LRU、单页 glyph atlas | text blob + 多页 glyph atlas |
| 多窗口 | 启动静态、每窗口独立设备、Linux 单窗口 | 共享 GPU 进程/设备 + 平台窗口注册表 |
| 帧调度 | 固定 16 ms / 1 ms timer | BeginFrame / vsync / display link / frame callback |
| 图片色彩 | 仅 sRGB、无 ICC | 全链路 color management |
| 无障碍 | 全树扫描增量、无 live region | 帧内合并 + live region + 表格语义 |
| API 分层 | 低层 API 直接暴露且部分不可用 | retained / retained+incremental 两套明确接口 |

---

## 7. 整改路线图

### 阶段 0：止血（1–2 周）

1. 修两处实例基址错位（Windows root constant / Linux `firstInstance`），补跨平台像素回归。
2. `Partial` 多矩形合并为一个包围盒执行一次绘制。
3. 隐藏或标注不可用 API：`NodeBuilder::geometry`、`Canvas::push_path`、`Primitive::Path`。
4. Windows 剪贴板加 `GlobalSize` 边界。
5. 图片图集：大图走独立纹理，避免首图锁档。
6. 文档化窗口生命周期与 Linux 单窗口限制。

### 阶段 1：核心重构（4–8 周，决定能否替换 GPUI）

1. **渲染通路**：lane 级稳定 GPU 区间 + 脏区间；`FramePayload` 改基线+补丁；`ColorResolver` 结果缓存。
2. **Canvas 可更新**：`set_canvas` / `CanvasHandle::update_range(dirty)`。
3. **GeometryStore**：公开注册/释放 + 三平台 GPU 缓冲 + fill/transform + 连续几何合并提交。
4. **脏集合**：`NodeTree` 维护 layout/paint/text/a11y 脏集，消除每帧 5–6 次全树扫描；`needs_frame` 改 O(1)。
5. **虚拟列表布局时序**：`settle_frame` 按 layout epoch 清理；补行高断言。
6. **依赖生命周期**：节点删除同步 detach；`FrameTicket` 改 RAII。
7. **单设备多窗口**：进程级 Device/Queue/Pipeline/Atlas，每窗口只留 surface/swapchain。

### 阶段 2：平台与体验（4–6 周）

1. 文本：数字快路径、`font_features`（tnum）、多页 glyph atlas、`approx_bytes` 修正。
2. 平台 vsync/刷新率调度（Windows DXGI waitable、macOS display link 挂 GPU 线程 + `max=max=max`、Linux frame callback）。
3. Linux 输入协议补齐（IME 抑制、键盘重复、axis_value120、modifier group）、`wl_touch`、cursor theme。
4. Windows 平台细节（`WM_SETCURSOR`、`WM_GETMINMAXINFO`、DPI 单次重排、捕获 per-window、IME enable/disable、关闭 veto）。
5. macOS 平台细节（NSMenu、失焦清理、IME context 生命周期、colorspace、换屏通知）。
6. a11y live region + 节流 + 表格语义 + 数值控件。
7. 运行时窗口生命周期 + Linux 多窗口。

### 阶段 3：门禁（与阶段 1 并行）

把性能基准变成 CI 硬门禁：

- 10k 图元图表增量更新 ≤ 2 ms CPU；100k 图元 ≤ 8 ms 或稳定 60 Hz。
- `frame_incremental_*` ≥ 2× 快于 `frame_full_*`。
- 单 lane 变化时上传字节 ≈ 变化 lane 字节 + 固定开销。
- 4 窗口：GPU 设备数 = 1。
- 32 个实时标签更新 ≤ 2 ms CPU。
- 补多窗口不同 DPI、图集换代、虚拟列表首屏、部分裁剪 UV 的像素/帧记录回归。

---

## 8. 审查限制

1. macOS/Linux 为**静态审查**；Metal/Vulkan 真实驱动行为、Wayland compositor 差异、120 Hz/混合 DPI/HDR 需实机复测。
2. 基准为 `criterion --quick` 本机数据，仅用于方向判断；正式数据应在目标硬件跑完整基准。
3. 未做 GPU 捕获（PIX/RenderDoc/NSight）逐帧分析；D3D12 屏障/描述符堆/带宽、Vulkan 同步验证仍需 GPU 工具确认。
4. 未审查 `examples/`（按要求排除）；`test_support` 仅作语义参考。
5. 设计文档已在 `a9a4220` 删除，本报告以当前代码行为 + 平台最佳实践为基准，不引用历史设计。

---

## 附录 A：证据索引（按问题）

| 问题 | 关键位置 |
|---|---|
| D3D12 实例基址 | `render/windows/compositor.rs:1522`、`shaders/quad.hlsl:11-12,35` |
| Vulkan 实例基址 | `render/linux/compositor.rs:1433`、`shaders/quad.vert.glsl:11-14` |
| Metal 正确对照 | `render/macos/compositor.rs:700-706` |
| 几何通道未闭环 | `paint/geometry.rs:34-41`、`paint/instance.rs:401-423`、`render/windows/compositor.rs:1770-1806`、`macos:710-719`、`linux:1437-1446` |
| 每 damage 重放 | `windows:1243-1262`、`macos:432-449`、`linux:877-897`、测试 `windows:2186-2189` |
| 全量编码/复制/上传 | `window.rs:1031-1032`、`paint/instance.rs:236-264`、`render/frame.rs:72-88`、`windows:1138-1145` |
| 虚拟列表时序 | `app.rs:2655,2679,1754-1783`、`node/tree.rs:1131-1139,1592-1607` |
| 文本全树 clone+哈希 | `app.rs:2831-2867`、`text/context.rs:89-95`、`text/cache.rs:65-88` |
| FrameTicket 无 RAII | `window.rs:187-244`、`app.rs:2633-2635` |
| 依赖边不清理 | `node/dependency.rs:35-38`、`app.rs:1745-1749,1890-1898,3043` |
| 图集换代 stale UV | `app.rs:2813-2816`、`node/tree.rs:1298-1315`、`paint/pass.rs:187-190` |
| 图片页锁定 | `resources.rs:169-173,198-230`、`render/atlas.rs:42-49` |
| 窗口生命周期 | `app.rs:2142-2149`、`windows/event_loop.rs:606-613,923-942`、`macos/event_loop.rs:71-103`、`linux/event_loop.rs:37-115` |
| 每窗口设备 | `windows/compositor.rs:240,274-301`、`windows/renderer.rs:1-31`、`macos/renderer.rs:120-141`、`linux/compositor.rs:151-162` |
| 剪贴板越界读 | `platform/windows/clipboard.rs:105-117` |
| Linux IME 双通路 | `platform/linux/surface.rs:673-694,1214-1237`、`linux/ime.rs` |
| Linux 无键盘重复 | `platform/linux/surface.rs:1003-1077` |
| Linux 滚轮量纲 | `platform/linux/surface.rs:1138-1153` |
| macOS 节奏源/帧率 | `render/macos/renderer.rs:414-420,425-427`、`platform/macos/window.rs:316-317` |
| Windows 全 INFINITE | `windows/queue.rs:289-306`、`windows/renderer.rs:1066-1117` |
| 裁剪 UV 拉伸 | `shaders/quad.vert.glsl:19-34`、`quad.hlsl:19-34`、`quad.metal:24-40` |

---

## 附录 B：分领域审查原始文件

| 文件 | 内容 |
|---|---|
| `review/findings-core-state.md` | 核心状态与帧调度（12 条） |
| `review/findings-layout-interaction.md` | 布局与交互（12 条） |
| `review/findings-paint-shared.md` | 绘制与共享渲染（含 shader 一致性，17 条） |
| `review/findings-windows-render.md` | Windows D3D12（12 条） |
| `review/findings-windows-platform.md` | Windows 平台集成（12 条） |
| `review/findings-macos.md` | macOS 与 Metal（12 条） |
| `review/findings-linux-render.md` | Linux Vulkan（12 条） |
| `review/findings-linux-platform.md` | Linux 平台集成（12 条） |
| `review/findings-text-res-a11y.md` | 文本 / 资源 / 无障碍（12 条） |

*报告生成：2026-11-16；审查基线 `a9a4220`。*
