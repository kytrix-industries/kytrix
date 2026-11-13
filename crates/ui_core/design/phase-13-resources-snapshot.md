# Phase 13 · 资源、像素 golden、bench、诊断 — 详细设计（v1）

> **P18 迁移**：demo 已并入 `..`，不再是独立 crate；本文中的
> `crates/ui_core_demo` 路径与命令是当时的历史记录，当前命令见 `design/phase-18-demo-example.md`。
>
> 编号说明：主设计文档与本仓 `design/phase-0*.md` 的排期把「资源」记为 **P15**、把「像素快照 / bench」记为 **P18**。
> 本阶段（实施排期第 13 个）**同时**收口这两项，外加 §9.1 的四个诊断导出。文中出现 `P15` / `P18` 时指的是
> **上游文档的阶段编号**，与本阶段的「P13」是同一次交付的两个切片。
> 另注：`DEVIATIONS` 里 F-127 / V-87 / V-93 提到的「P13」（窗口装饰 / 光标形状）是**旧排期的另一个阶段**，
> 与本阶段无关，本阶段不实现窗口装饰与光标形状。
>
> 上游依据：`custom-gui-engine-design.md` §5.6 / §6.4 / §6.5 / §6.7 / §8.5 / §9.1 / §9.3；
> `design/phase-02-paint-foundation.md` §10（`AtlasKind` / `AtlasTile` / UV / 预乘 / 容量逃逸）；
> `design/phase-05-text.md` §9–§10（分配器 / 字形 tile 缓存，本阶段照此模式建图片缓存）；
> `design/phase-08-d3d12.md` §7.7（readback / `SnapshotImage` 字节语义 / 容差提案）；
> `design/phase-12-macos-linux.md`（三平台 readback 现状，F-137）；
> `design/DEVIATIONS.md`（F-11 `usvg` 归 `base_ui`、F-24 `AtlasTile` 在 `paint::atlas`、M-09 target 形态、S-85 整页上传、V-22
> 文档债）。

---

## 0. 结论摘要

| #  | 结论                                                                                                                                                                                                                                       | 位置              |
|----|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|-------------------|
| 1  | `ResourceId` 是 8 B 带代际句柄（复用 `node::id::Arena`），由 `ResourceCache::insert` 铸出；`unload` 后**永不复活**                                                                                                                         | §5.1              |
| 2  | 解码只接受 **PNG / JPEG / WebP**（`image 0.25.10`，按 `guess_format` 判定）；16-bit 走 `to_rgba8()` 降位；不做 ICC / gAMA（`image` 无色彩管理）⇒ 资产契约「字节必须已是 sRGB」                                                             | §6                |
| 3  | 图集存 **线性空间预乘 → sRGB 重编码** 的 `BGRA8`（不是编码域预乘、不是直通 alpha）。probe 实测：编码域预乘与正确做法最大差 **60/255**；本方案 8 bit 往返最坏线性误差 **0.0097**                                                            | §6.2              |
| 4  | **v1 不做自动 LRU 逐出**。tile 一旦交给应用即恒有效（`Canvas` 是值语义快照，引擎无法在不重建应用画布的前提下失效 tile）；容量溢出是强类型 `DecodeCause::Capacity`，由应用 `unload` 后重试                                                  | §5.4 / §18 裁决 ① |
| 5  | **v1 不做引擎侧远程图片磁盘缓存**（主设计 §8.5 的字面要求降级为应用层，登记 F-139）                                                                                                                                                        | §5.7 / §18 裁决 ② |
| 6  | `AtlasSet` 删除：字形分配器直接是 `App::atlas: AtlasAllocator`，**图片分配器 + 页缓冲移入 `ResourceCache`**（图片不随 DPI 换代）                                                                                                           | §8.1              |
| 7  | 像素 golden：`SnapshotCase`（场景 / 尺寸 / scale / 主题）+ `Tolerance` + `SnapshotDiff` + `compare`，目录 `tests/golden/warp/<case>_<scale>_<theme>.png`，`UPDATE_GOLDEN=1` 生成；**v1 只跑 Windows WARP**，mac/Linux 显式 skip 并打印原因 | §9                |
| 8  | 容差**不冻结数值**（D19）：`Tolerance::default()` = `per_channel: 1` / `max_over_tolerance_pixels: 0`，标注「P08 §7.7 提案值，待 L3 实测校准」                                                                                             | §9.5              |
| 9  | 文字像素 golden **不进 v1**（宿主字体依赖）；字形像素由既有 sanity 用例 + L2 结构断言覆盖（F-144）                                                                                                                                         | §9.7              |
| 10 | bench：`benches/perf.rs`（criterion 0.8.2，`harness = false`）8 组基准，**非合并门禁**；门禁只要求 `--no-run` 可编译                                                                                                                       | §10               |
| 11 | 诊断：`FrameDump` / `DependencyGraphDump` / `A11yTreeDump` / `DamageOverlay`，全部挂**既有** `test-support` feature（不新增 feature），强类型 DTO + `Display` 文本，零平台句柄                                                             | §11               |
| 12 | `cargo test` 基线实测 **590**（默认）/ **622**（`test-support`）+ 集成 6；`Cargo.lock` 预期**零变化**                                                                                                                                      | §13 / §14         |

**本阶段只做设计 + probe，不改仓库代码、`Cargo.toml`、`Cargo.lock`、不 commit。**

---

## 1. 范围与非目标

### 1.1 交付

| #  | 文件                                                         | 内容                                                                                                                                                                          |
|----|--------------------------------------------------------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 1  | `src/resources.rs`（新建，**模块根**）                       | `ResourceId` / `ResourceImage` / `DecodeCause` / `ResourceCache`（唯一 impl）                                                                                                 |
| 2  | `src/resources/cache.rs`（新建，私有）                       | `Entry` / `Page`（页缓冲 + 写 tile + 清区）                                                                                                                                   |
| 3  | `src/resources/image.rs`（新建，私有）                       | `decode`：格式判定 → 限制 → 缩放 → `to_rgba8` → 线性预乘 → sRGB 重编码 → BGRA                                                                                                 |
| 4  | `src/error.rs`（改）                                         | `EngineError::ResourceDecode { cause: DecodeCause }`（F-138）                                                                                                                 |
| 5  | `src/lib.rs`（改）                                           | `pub mod resources;`                                                                                                                                                          |
| 6  | `src/app.rs`（改）                                           | `App::resources{,_mut}` / `upload_image_page` / `resource_page_pixels` / `dependency_graph`（cfg test-support）                                                               |
| 7  | `src/render/atlas.rs`（改）                                  | 删除 `AtlasSet`（F-148）；`IMAGE_PAGE` / `GLYPH_PAGE` 保持 `pub(crate)`                                                                                                       |
| 8  | `src/render.rs`（改）                                        | 出口去掉 `AtlasSet`                                                                                                                                                           |
| 9  | `src/test_support/snapshot.rs`（扩写）                       | `SnapshotCase` 扩字段、`SnapshotTheme` / `SnapshotScene` / `Tolerance` / `SnapshotDiff` / `compare` / golden 读写 / `capture` / `assert_golden` / `skip_reason` / `gpu_guard` |
| 10 | `src/test_support/diagnostics.rs`（新建）                    | 四个 dump 的 DTO + 文本 + `DamageOverlay`                                                                                                                                     |
| 11 | `src/test_support.rs`（改）                                  | `pub mod diagnostics;`                                                                                                                                                        |
| 12 | `src/test_support/app.rs`（改）                              | `snapshot` 同时上传图片页（仅当 `!resources.is_empty()`）                                                                                                                     |
| 13 | `src/paint/instance.rs`（改，test-support）                  | `InstanceBuffer` 的 `instances()` / `clips()` / `ops()` 只读访问器（cfg `test-support`）                                                                                      |
| 14 | `src/node/dependency.rs`（改，test-support）                 | `DependencyGraph::dependents()`（cfg `test-support`；DTO 转换在 `diagnostics`）                                                                                               |
| 15 | `tests/snapshot.rs`（新建）                                  | 6 个 golden 组（每组内顺序跑该组的 scale×theme）+ 2 个纯 CPU 用例，`required-features = ["test-support"]`                                                                     |
| 16 | `tests/golden/warp/*.png` + `tests/golden/README.md`（新建） | 13 个 golden + 说明（§9.8）                                                                                                                                                   |
| 17 | `benches/perf.rs`（新建）                                    | 8 组 criterion 基准                                                                                                                                                           |
| 18 | `Cargo.toml`（改）                                           | `image` 依赖、`criterion` dev-dep、`[[test]] snapshot`、`[[bench]] perf`                                                                                                      |
| 19 | `..`（新建）                                                 | 快速开始 / 架构 / 测试·golden·bench 命令 / 偏差入口                                                                                                                           |
| 20 | `crates/ui_core_demo/src/scene.rs`（改）                     | 内嵌 PNG + 两个 `Image` 节点（`ResourceCache` 的**生产**消费者）                                                                                                              |

### 1.2 非目标（本阶段一行不写）

- 图集**多页 / 页扩容 / `page` 字段**（契约变更，V-14）。
- 图集**增量行上传**的渲染器改造（`queue_upload` 的整页 `to_vec` 是 P09 遗留，V-98）。
- **`bind_canvas` / 画布重建**（自动淘汰的前提，契约变更）。
- 磁盘缓存 / 网络下载 / URL 语义（§5.7）。
- 运行时 SVG / `usvg`（F-11）。
- 三平台 L3 job 的 CI 落地（只给占位说明与命令，§9.9）。
- 诊断的 UI 窗口 / 平台 overlay（`DamageOverlay` 只是 `Canvas`）。
- 文字 / emoji 像素 golden（F-144）。
- 任何指标阈值（帧时间 / 内存 / draw call：D19）。

---

## 2. 事实基线与 probe 记录

### 2.1 已落地代码的既成事实（逐条附位置）

| #  | 事实                                                                                                                                                      | 位置                                                                                                            | 对本阶段的影响                                                                                       |
|----|-----------------------------------------------------------------------------------------------------------------------------------------------------------|-----------------------------------------------------------------------------------------------------------------|------------------------------------------------------------------------------------------------------|
| 1  | `AtlasTile` 已是公开只读数据（`atlas` / `u0..v1` / `generation`，24 B）                                                                                   | `paint/atlas.rs:28-55`                                                                                          | `ResourceCache::insert` 直接返回它，不新增图集类型                                                   |
| 2  | `AtlasKind::Image` 的纹理格式是 `BGRA8 sRGB`（`DXGI_FORMAT_B8G8R8A8_UNORM_SRGB` / `B8G8R8A8_SRGB` / `MTL` 同名），alpha 直通                              | `paint/atlas.rs:12-17`、`render/windows/atlas.rs:309`、`render/linux/atlas.rs:357`、`render/macos/atlas.rs:201` | 存 **sRGB 编码的预乘** 字节即与硬件解码一致（§6.2）                                                  |
| 3  | 图元公式 `out = texture(image_atlas, uv) * color0`，`FLAG_OPAQUE` 未参与片元分支                                                                          | `render/shaders/quad.frag.glsl:22-23`、`quad.hlsl:50`、`quad.metal:60`、`common.glsl:10`                        | **不需要改三套 shader**（这是选线性预乘方案的主要理由）                                              |
| 4  | `Image` 图元编码：`params1.y = TEXTURE_IMAGE(1)`、`color0 = resolve(modulation)`（预乘线性）                                                              | `paint/instance.rs:586-606`                                                                                     | 调制色与 tile 均为预乘，乘法闭合                                                                     |
| 5  | `Canvas::push_image` 要求 `tile.atlas == AtlasKind::Image`，但应用层**无合法 tile 来源**                                                                  | `paint/canvas.rs:74-83`                                                                                         | `ResourceCache` 是 `Image` tile 的**唯一**合法生产者（与 `GlyphRun` 同一纪律）                       |
| 5b | **`contract.md` §8 已冻结图片图集的字节语义**：「纹素是 *linear-premultiplied 后 sRGB 编码*」「上传方职责：P15 必须 linear 空间预乘 → sRGB 编码后写图集」 | `render/shaders/contract.md:190-193`                                                                            | 本阶段的 §6.2 就是实现这条既有契约；phase-02 §10.3 的「alpha 直通」措辞过时（以 `contract.md` 为准） |
| 6  | `Canvas` 是值语义快照（`#[derive(Clone)]`，48 B），挂载时写进 `NodeKind::Canvas`，无 `bind_canvas`                                                        | `paint/canvas.rs:33-38`、`node/builder.rs:138`                                                                  | 决定了「tile 不可失效」（§5.4）                                                                      |
| 7  | `AtlasAllocator` 单页 2048²，1 纹素 padding，`allocate` 返回 `content`/`padded`/`id`                                                                      | `render/atlas.rs:20-22,90-151`                                                                                  | 图片页容量 = 2048²（16 MiB）                                                                         |
| 8  | **`AtlasAllocator::deallocate` 目前是 `#[cfg(test)]`**（槽位表也只在测试构建存在）                                                                        | `render/atlas.rs:29-33,154-166`                                                                                 | `unload` 要求把它**提升为生产代码**（F-147）                                                         |
| 9  | `clear()` 才 `generation += 1`；`allocate`/`deallocate` **不**换代                                                                                        | `render/atlas.rs:169-178`、phase-02 §10.6                                                                       | 逐块释放不会让在用 tile 失效（但释放区会被复用 ⇒ 卸载契约）                                          |
| 10 | `AtlasSet { glyph, image }`，且 `clear_all()` 在 **DPI 换代**时把两页都清空                                                                               | `render/atlas.rs:203-246`、`app.rs:2281-2284`                                                                   | 图片页被 DPI 换代清空 = 悬垂 UV ⇒ 必须移出 `AtlasSet`（F-148）                                       |
| 11 | `GlyphAtlas` 的「key → tile + 数据 + origin」、`page_dirty`、`write_page`、`clear` 是图片缓存的现成范本                                                   | `text/atlas.rs:53-234`                                                                                          | `ResourceCache` 照此形态，但**不做** `clear+retry`                                                   |
| 12 | 字形页上传是**整页**：`upload_glyph_page` 在 `App::frame` 内、paint 之前调用                                                                              | `app.rs:404-430,2189`                                                                                           | 图片页同序接入（§8.2）                                                                               |
| 13 | `render/windows/atlas.rs::queue_upload` 收**整页** `pixels` 并 `to_vec()`，只按 `DirtyRows` 拷贝行                                                        | `render/windows/atlas.rs:89-104,156+`                                                                           | 首次上传必须整页（保证 padding 为 0），之后只报脏行（V-98）                                          |
| 14 | `TestApp::snapshot` 只上传**字形页**（`glyph_page_pixels`）                                                                                               | `test_support/app.rs:228-243,255-270`                                                                           | 图片在快照里会采样到全 0 页 ⇒ 必须补图片页上传                                                       |
| 15 | `SnapshotCase` 目前只有 `size` + `scale`；`SnapshotImage` 是 `{width,height,rgba8}`                                                                       | `test_support/snapshot.rs:11-36`                                                                                | 扩字段 + 新增 diff/golden                                                                            |
| 16 | `SnapshotImage` 字节 = **预乘 sRGB 编码的 RGBA8**（BGRA→RGBA 交换后）                                                                                     | `phase-08 §7.7`（主设计 §6.4）                                                                                  | golden 落盘原样存，不反预乘（F-143）                                                                 |
| 17 | WARP 设备并行会互相拖垮（本机实测 device removed），串行锁只在 `#[cfg(test)]` 且 crate 私有                                                               | `render/windows/device.rs:535-541`                                                                              | 集成测试需要**公开的**串行锁（S-131）                                                                |
| 18 | `EngineError` 的 `ResourceDecode` 尚未落地；冻结表 payload 是 `{ resource, detail }`                                                                      | `phase-01 §6.2.2 #16`、`error.rs:6-11`                                                                          | payload 改为 `{ cause }`（F-138，需裁决 ⑤）                                                          |
| 19 | `NodeId`/`Arena`/`ArenaKey` 是带代际句柄的现成实现                                                                                                        | `node/id.rs:14-98,106-193`                                                                                      | `ResourceId` 复用同一套（含 `next_generation` 的 0 非法 invariant）                                  |
| 20 | `App` 已持有 `AtlasSet` / `GlyphAtlas` / `TextContext`（进程级一份，单 GPU 设备）                                                                         | `app.rs:89-100,314`                                                                                             | `ResourceCache` 同层（`App` 字段），不进 `Window`                                                    |
| 21 | 测试基建只在 `test-support` feature 下编译（发布物零符号）                                                                                                | `lib.rs:18-21`、`Cargo.toml:8-16`                                                                               | 诊断与 golden 全部挂这个 feature（F-146）                                                            |

### 2.2 probe ①：`image 0.25.10` 的真实行为

probe crate：`%TEMP%\kg-p13-probe`（
`image = { version = "0.25.10", default-features = false, features = ["png","jpeg","webp"] }`，离线模式跑通）。
**实测输出（原样）**：

```text
png bytes = 136
guess_format = Ok(Png)
decoded = Rgba8 4x4
rgba8 exact = true
16-bit decoded = Rgba16 → rgba8 [0, 0, 0, 255, 62, 62, 62, 255, 125, 125, 125, 255, 187, 187, 187, 255]
limits classify = Err("too-large: Image size exceeds limit")
guess bogus = Err(Unsupported(UnsupportedError { format: Unknown, kind: Format(Unknown) }))
guess gif = Ok(Gif)
resize = 4x3 first=[6, 9, 0, 255]
resize 0 = false                       # 0 宽 resize 不 panic（返回空图），仍必须自己 guard
max naive-vs-correct error = 60
premultiplied round-trip worst linear error = 0.00968
png round-trip exact = true
alpha=0 stored rgb = 0
```

结论（全部写进实现）：

| 事实                                                                                                        | 设计后果                                                                                                         |
|-------------------------------------------------------------------------------------------------------------|------------------------------------------------------------------------------------------------------------------|
| `ImageError` **非** `#[non_exhaustive]`，穷尽 match 可编译（probe 的 `_` 臂被 `unreachable_patterns` 警告） | 错误映射写**穷尽 match**，`image` 升级新增变体 = 编译错误（符合期望）                                            |
| `image::Limits` 是 `#[non_exhaustive]`，**不能**用结构体字面量构造（`E0639`）                               | 必须 `let mut limits = Limits::default(); limits.max_image_width = …`                                            |
| `guess_format` 与 feature 无关：未开 `gif` feature 也返回 `Ok(Gif)`                                         | 「只接受 PNG/JPEG/WebP」必须是**策略判定**，不能依赖 feature 裁剪                                                |
| `guess_format(&[0u8;8])` → `Err(Unsupported{format: Unknown})`                                              | 空/垃圾字节 → `DecodeCause::UnsupportedFormat { format: "unknown" }`                                             |
| 16-bit PNG 解码为 `Rgba16`，`to_rgba8()` 静默降位（0/62/125/187）                                           | 16-bit 资产按 8 bit 处理，写进资产契约（无错误、有精度损失）                                                     |
| `Limits` 命中 → `ImageError::Limits`                                                                        | 映射 `DecodeCause::TooLarge`，payload 带实际 `width/height`（读 `DynamicImage` 之前要从 decoder 拿，见 §6.3 注） |
| PNG 存盘 → `image::open` 逐字节一致（RGBA8）                                                                | golden 可无损落盘/读回（`RgbaImage::save` / `open().to_rgba8()`）                                                |
| 编码域预乘 vs 线性预乘最大差 **60/255**                                                                     | 必须线性预乘（§6.2）                                                                                             |
| 线性预乘 → 8 bit sRGB → 反解最坏线性误差 **0.0097**（alpha ≥ 32）                                           | 可接受；alpha < 32 的误差更大，写进 V-99                                                                         |
| `imageops::resize(.., 0, h, ..)` 不 panic                                                                   | 目标尺寸必须自己 clamp 到 `>= 1`                                                                                 |

### 2.3 probe ②：`criterion 0.8.2` 的依赖形态

probe crate：`%TEMP%\kg-p13-bench-probe`（`[[bench]] harness = false`）。 **实测**：

```text
criterion = { version = "0.8.2", default-features = false, features = ["cargo_bench_support"] }
cargo bench --no-run  →  Finished `bench` profile，产出 benches\perf.rs 可执行文件
cargo tree            →  criterion v0.8.2（无 plotters / 无 rayon）
```

| 事实                                                                                                                           | 设计后果                                                                           |
|--------------------------------------------------------------------------------------------------------------------------------|------------------------------------------------------------------------------------|
| `default` features = `rayon + plotters + cargo_bench_support`；关掉后只剩 reporting 依赖（clap / ciborium / serde）            | dev-dep 用 `default-features = false, features = ["cargo_bench_support"]`（S-130） |
| `criterion::black_box` 已 `#[deprecated(note = "use std::hint::black_box()")]`                                                 | benches 用 `std::hint::black_box`                                                  |
| `criterion_group!` / `criterion_main!` 仍在 `macros.rs`，`harness = false` 必需                                                | `[[bench]] harness = false`                                                        |
| `criterion` / `plotters` / `ciborium` / `tinytemplate` 已在 `Cargo.lock`（`gpui-pre-bench-metrics` 引入，`:2101,:4164,:7794`） | **`Cargo.lock` 零变化**                                                            |

### 2.4 基线门禁（本机实测，2026-11-13）

```text
cargo test -p ui_core --lib -- --list                          → 590 tests
cargo test -p ui_core --features test-support --lib -- --list   → 622 tests
cargo test -p ui_core --features test-support --tests -- --list → 6 tests（tests/frame_record.rs）
Doc-tests ui_core                                              → 0 tests
```

---

## 3. 文件树与阶段归属

```text
crates/ui_core/
├── README.md                                    P13 # 引擎侧入口（本阶段新建）
├── Cargo.toml                                   P01 # P13 追加 image / criterion / [[test]] / [[bench]]
├── benches/
│   └── perf.rs                                  P13
├── src/
│   ├── lib.rs                                   P01 # + `pub mod resources;`
│   ├── resources.rs                             P13 # 模块根：ResourceId / ResourceImage / DecodeCause / ResourceCache
│   │   └── resources/{cache,image}.rs           P13 # 私有子模块
│   ├── test_support.rs                          P08 # + `pub mod diagnostics;`
│   │   └── test_support/{app,probe,record,snapshot,diagnostics}.rs  # diagnostics 本阶段新增
│   └── render/atlas.rs                          P05 # -AtlasSet；IMAGE_PAGE 保持 pub(crate)
└── tests/
    ├── frame_record.rs                          P07
    ├── snapshot.rs                              P13
    └── golden/                                  P13 # 数据目录（v1 只有 warp/）
        └── warp/*.png
```

**模块形态（硬约束，沿用 M-06 / M-07）**：

- 不用 `mod.rs`；`resources.rs` 是**自命名模块根**，可定义公开类型（`definition_in_module_root` 只管 `mod.rs`/`lib.rs`/
  `main.rs`）。
- 公共 API 禁止 `pub use`；`resources/{cache,image}.rs` 是**私有**子模块，其中类型一律 `pub(crate)`。
- **一个类型的全部 impl 在同一文件**（`multiple_inherent_impl = forbid`）：`impl ResourceCache` 只在 `resources.rs`，
  `impl Page` / `impl Entry` 只在 `cache.rs`，`impl TestApp` 只在 `test_support/app.rs`、`impl App` 只在 `app.rs`。
  因此诊断的**自由函数**写在 `diagnostics.rs`（`frame_dump(&TestApp)` / `a11y_dump(&A11yTreeSnapshot)` 等），
  不给 `TestApp` 加方法。
- `resources/svg_build.rs` **不创建**（F-11：构建期 SVG 归 `base_ui`，引擎不声明 `usvg`）。

---

## 4. 公共 / 私有边界

### 4.1 公开路径表增量（应用层迁移契约）

| 路径                                                                                                                                                                                                           | 类型                                 | 阶段 |
|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|--------------------------------------|------|
| `ui_core::resources::ResourceId`                                                                                                                                                                               | 句柄（`Copy`，8 B）                  | P13  |
| `ui_core::resources::ResourceImage`                                                                                                                                                                            | 解码结果（`id` + `tile` + 像素尺寸） | P13  |
| `ui_core::resources::DecodeCause`                                                                                                                                                                              | 解码失败分类                         | P13  |
| `ui_core::resources::ResourceCache`                                                                                                                                                                            | 字节 → 图集 tile                     | P13  |
| `ui_core::error::EngineError::ResourceDecode`                                                                                                                                                                  | `{ cause: DecodeCause }`             | P13  |
| `ui_core::app::App::{resources, resources_mut}`                                                                                                                                                                | 缓存访问                             | P13  |
| `ui_core::test_support::snapshot::{SnapshotCase, SnapshotTheme, SnapshotScene, Tolerance, SnapshotDiff, GoldenError}`                                                                                          | golden 契约                          | P13  |
| `ui_core::test_support::snapshot::{capture, assert_golden, write_golden, compare, skip_reason, gpu_guard}`                                                                                                     | golden 驱动                          | P13  |
| `ui_core::test_support::diagnostics::{FrameDump, InstanceDump, OpDump, ClipDump, DependencyGraphDump, DependencyOwnerDump, A11yTreeDump, A11yNodeDump, DamageOverlay, frame_dump, dependency_dump, a11y_dump}` | 诊断                                 | P13  |

`AtlasTile` 与 `AtlasKind` **不在** `resources` 下（F-24 已定案：`ui_core::paint::atlas::{AtlasKind, AtlasTile}`）。
P01 §3 文件树里 `resources.rs  # ResourceId / ResourceCache / AtlasTile / AtlasKind` 的注释**历史正文不改**，
本条即路径表的修订记录（照 F-24 的写法）。

### 4.2 §1.3 公开白名单的扩充（N7 / S-16 格式）

主设计 §1.3 的「资源」行原文是 `ResourceId, ResourceCache, AtlasTile（只读）`；本阶段补：

```text
资源：ResourceId, ResourceCache, ResourceImage, DecodeCause, AtlasTile（只读，paint::atlas）
测试：TestApp, TestWindow, Probe, FrameRecord, SnapshotCase, SnapshotTheme, SnapshotScene,
      Tolerance, SnapshotDiff, GoldenError, FrameDump, DependencyGraphDump, A11yTreeDump, DamageOverlay
```

诊断四件套属「测试」行（`test-support` 才存在），不进应用层运行路径。

### 4.3 每个 `pub(crate)` item 与它的**非 test 消费者**（dead_code 审计表）

| item                                    | 位置                 | 非测试消费者                                                                                                                               |
|-----------------------------------------|----------------------|--------------------------------------------------------------------------------------------------------------------------------------------|
| `ResourceCache::new`                    | `resources.rs`       | `App::new`                                                                                                                                 |
| `ResourceCache::page_pixels`            | `resources.rs`       | `App::upload_image_page`、`App::resource_page_pixels`                                                                                      |
| `ResourceCache::take_dirty_rows`        | `resources.rs`       | `App::upload_image_page`                                                                                                                   |
| `Page::{new,write,clear_rect,bytes}`    | `resources/cache.rs` | `ResourceCache::{insert,unload,page_pixels}`                                                                                               |
| `resources::image::decode`              | `resources/image.rs` | `ResourceCache::insert`                                                                                                                    |
| `AtlasAllocator::deallocate`            | `render/atlas.rs`    | `ResourceCache::unload`（**从 `cfg(test)` 提升**）                                                                                         |
| `InstanceBuffer::{instances,clips,ops}` | `paint/instance.rs`  | 仅 `test-support` 的 `diagnostics::frame_dump`（这三个访问器本身 `#[cfg(feature = "test-support")]`，生产构建零符号 ⇒ 不产生 `dead_code`） |
| `DependencyGraph::dependents`           | `node/dependency.rs` | 仅 `test-support` 的 `diagnostics::dependency_dump`（经 `App::dependency_graph`，同样 `cfg` 门控）；`node` 层不引用 `test_support` 的 DTO  |

公开项（`insert` / `unload` / `len` / `is_empty` / `capture` / `compare` …）在库 crate 里不受 `dead_code` 约束；
其中 `unload` 的**正确性**由 `resources.rs` 的单测（插入→卸载→复用区）与 `tests/snapshot.rs` 的 image case 覆盖。

---

## 5. `src/resources.rs`（公开契约）

### 5.1 `ResourceId`

```rust
/// 图片资源句柄（8 B，带代际）。字段私有：只能由 [`ResourceCache`] 铸出（S-20 的同类理由）。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResourceId {
    index: u32,
    generation: NonZeroU32,   // 0 永不是合法代际
}

impl ResourceId {
    /// 槽位下标。
    #[must_use]
    pub const fn index(self) -> u32;
    /// 代际，恒在 `1..=u32::MAX`。
    #[must_use]
    pub const fn generation(self) -> u32;
    /// `(generation << 32) | index`（结构化日志 / 帧记录的单值标识）。
    #[must_use]
    pub const fn to_bits(self) -> u64;
    /// 仅测试构造（不扩大公开面）。
    #[cfg(test)]
    pub(crate) const fn for_test(index: u32, generation: u32) -> Self;
}

impl fmt::Debug for ResourceId { /* "ResourceId(r3.1)" */ }
impl crate::node::id::ArenaKey for ResourceId { /* pub(crate) */ }
```

- **生命周期**：`insert` 成功 → 铸出 id；`unload(id)` → 释放槽位并把代际 `+1`（`next_generation`）；
  旧 id 永远 `unload` 返回 `false` / 不会命中新资源。
- id 由 `Arena<Entry, ResourceId>`（`node/id.rs:106-193`，crate 私有，直接复用，不新造对象池）持有。

### 5.2 `ResourceImage` / `DecodeCause`

```rust
/// 一次成功插入的结果（`Copy`；应用层自己保存，引擎不再回查）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResourceImage {
    /// 该资源的句柄（`unload` 的唯一入口；`insert` 铸出，随条目一起返回）。
    pub id: ResourceId,
    /// 可直接塞进 `Primitive::Image` / `Canvas::push_image` 的图集矩形。
    pub tile: AtlasTile,
    /// 图集内容区像素宽（缩放后的最终尺寸，`1..=2046`）。
    pub width: u32,
    /// 图集内容区像素高。
    pub height: u32,
}

/// 解码 / 装箱失败的业务分类（`EngineError::ResourceDecode` 的 payload）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeCause {
    /// 无法识别的格式，或不是 PNG / JPEG / WebP。
    UnsupportedFormat { format: SmolStr },        // "gif" / "unknown" / "bmp" …
    /// 字节无法产生有效像素（含格式自洽性失败、`image` 的 `Parameter` / `IoError`）。
    Corrupt { format: SmolStr, detail: SmolStr },
    /// 源图尺寸超出 `MAX_SOURCE_DIM`（解码限制），或缩放后仍为 0。
    TooLarge { width: u32, height: u32, limit: u32 },
    /// 图片页放不下该 tile（`unload` 其它资源后重试）。
    Capacity { width: u32, height: u32, page: (u32, u32) },
}
```

`Display` / `EngineError` 文案（英文诊断，沿用 F-07）：

```text
image resource decode failed: unsupported image format `gif` (PNG, JPEG and WebP only)
image resource decode failed: corrupt `png` bytes: <detail>
image resource decode failed: image is 9000x4000, limit is 8192 per side
image resource decode failed: a 900x700 tile does not fit the 2048x2048 image atlas; \
unload a resource and retry
```

`DecodeCause` 不出现在 `Display` 文案以外的地方；应用层用**穷尽 match** 决定策略（占位图 / 提示 / 卸载重试）。

### 5.3 `ResourceCache` 逐字签名

```rust
/// 图片资源缓存：字节 → 解码 → 图片图集 tile + 页缓冲。
///
/// **不持有 GPU 对象**：页缓冲由 `App::upload_image_page` 交给窗口的渲染器（与字形页同一路径）。
pub struct ResourceCache {
    /* entries / allocator / page（脏行记在 allocator 内） */
}

impl ResourceCache {
    /// 新建（页尺寸恒为 `render::atlas::IMAGE_PAGE` = 2048²，BGRA8 = 16 MiB）。
    pub(crate) fn new() -> Self;

    /// 仅测试：小页（2048² 的页放不下几十种图；`AtlasAllocator::for_test` 同由）。
    #[cfg(test)]
    pub(crate) fn with_page(page: (u32, u32)) -> Self;

    /// 解码 `bytes` 并装箱，返回 tile 与像素尺寸。
    ///
    /// # Errors
    /// `EngineError::ResourceDecode { cause }`（§5.2 四类）。
    pub fn insert(&mut self, bytes: &[u8]) -> Result<ResourceImage, EngineError>;

    /// 释放一个资源：tile 归还分配器、页内区域清零。
    ///
    /// 契约：调用方必须保证**不再有 `Image` 图元引用该 tile**（通常先卸载/重建对应节点）。
    /// 返回 `false` = 陈旧句柄（幂等）。
    pub fn unload(&mut self, id: ResourceId) -> bool;

    /// 存活资源数。
    #[must_use]
    pub fn len(&self) -> usize;
    /// 是否为空（`Clippy::len_without_is_empty` 的配套；也是快照路径的开关）。
    #[must_use]
    pub fn is_empty(&self) -> bool;

    /// 图片页的当前内容（`page.0 * page.1 * 4` 字节，BGRA8 预乘 sRGB）。
    pub(crate) fn page_pixels(&self) -> ((u32, u32), &[u8]);
    /// 取走自上次上传以来被写脏的**行**（首次调用返回整页，保证 GPU 页的 padding 为 0）。
    pub(crate) fn take_dirty_rows(&mut self) -> DirtyRows;
}
```

`Default` 不实现（与 `GlyphAtlas` 不同：构造有 16 MiB 分配，隐式默认值危险）。

### 5.4 容量与淘汰（**定死**）

| 项         | 决定                                                                              | 理由                                                                                                                                                                                                                                                                        |
|------------|-----------------------------------------------------------------------------------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 容量       | 单页面积：2048×2048 纹素；单块上限 `MAX_TILE_DIM = 2046`（2048 − 2 纹素 padding） | P02 §10.3 冻结；`AtlasAllocator` 已强制                                                                                                                                                                                                                                     |
| 溢出       | `DecodeCause::Capacity`，**不** `clear()`、**不** 换代                            | `clear()` 会让所有在用 tile 的 UV 失效，而 tile 已被应用持有（见下）                                                                                                                                                                                                        |
| 淘汰       | **无自动 LRU**；`unload(id)` 是唯一淘汰方式，策略在应用层                         | `Canvas` 是值语义快照（`paint/canvas.rs:33`），tile 在建树时就被捕获进 lane；引擎**无法**在不重跑应用建树代码的前提下让旧 tile 失效。自动淘汰会留下悬垂 UV：被释放的区被新资源复用后，旧 `Image` 会**画出别人的图**（静默错误像素）。v1 选「报错 + 让应用决定」而不是「猜」 |
| LRU 记账   | **不实现**（不存 `last_used`、不存访问序）                                        | 没有消费者 = 死代码（AGENTS 1.2）；`base_ui` 才是淘汰策略的持有者（D15）                                                                                                                                                                                                    |
| 卸载后区域 | `unload` 把 `padded` 区**清零**并标脏行                                           | 误用（仍有图元引用）时表现为**透明**而不是串图，代价是几 KB 写入                                                                                                                                                                                                            |

**为什么不与字形路径共用「`clear + retry`」**（`text/atlas.rs:137-149` 的写法）：字形 tile 由 `layout_text` 每次需要时
重新解析（`GlyphAtlas::ensure`），换代后自动重建；图片 tile 只在应用**建树**时解析一次，引擎没有重建入口。
两者语义不同，必须分开（F-140）。

**若要自动淘汰需要先补什么**（登记为后续阶段的前置条件，不在本阶段）：
① `bind_canvas`（或等价的「建树代码可重跑」契约）；② 图片页换代后「全部 PAINT + 应用重建画布」的协调；
③ 逐 tile 引用计数。三项都是契约变更，不能为了「看起来更完整」在本阶段偷做。

### 5.5 `insert` 算法（逐字）

```text
1. let decoded = resources::image::decode(bytes)?;                 // §6，返回 BGRA8 预乘 sRGB
2. let Some(allocation) = self.allocator.allocate((decoded.width, decoded.height)) else {
       return Err(EngineError::ResourceDecode { cause: DecodeCause::Capacity {
           width: decoded.width, height: decoded.height, page: self.page.dimensions() } });
   };
3. self.page.clear_rect(allocation.padded);                        // 1 纹素 padding 必须为 0
   self.allocator.mark_written(allocation.padded);                   // allocate 内部只标了 content 行
4. self.page.write(allocation.content, &decoded.bgra, decoded.width * 4);
5. let image = ResourceImage { tile: allocation.tile, width: decoded.width, height: decoded.height };
   let entry = Entry { image, alloc: allocation.id, content: allocation.content, padded: allocation.padded };
   let _id = self.entries.insert_with(|_| entry);                  // id 只给调用方，Entry 不自引用
6. Ok(image)
```

要点：

- **`step 2` 失败不 `clear()`**（F-140）；不重试。
- `step 3/4` 用 `padded`/`content` 两个矩形：`content` 是 UV 对应区，`padded` 是可能被 `etagere` 放大的整块
  （phase-02 §10.4 的实现要点 5–6）。padding 复用时可能残留上一块的内容 ⇒ 先清 `padded` 再写 `content`。
- **脏行不新增字段**：`AtlasAllocator` 已有 `mark_written` / `take_dirty_rows`（`render/atlas.rs:181-192`），
  `ResourceCache::take_dirty_rows()` 直接转发；`new()` 用 `mark_written(整页)` 保证首次上传是整页。
- 行宽 `page.0 * 4`；`decoded.width * 4` 是源行宽。
- `Allocation::tile.generation` 恒为图片页当前代际（v1 恒 1，因为不清页；`generation` 字段保留给未来多页/换代）。

### 5.6 `unload` 算法

```text
1. let Some(entry) = self.entries.remove(id) else { return false };
2. self.allocator.deallocate(entry.alloc);          // 幂等（F-147 的槽位表守卫）
3. self.page.clear_rect(entry.padded);
4. self.allocator.mark_written(entry.padded);       // 清零区必须上传，否则 GPU 侧仍是旧图
5. true
```

### 5.7 远程图片磁盘缓存：**v1 不做**（结论 + 理由，F-139）

| 项                       | 内容                                                                                                                                                                                |
|--------------------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 主设计原文               | §8.5「引擎提供 `ResourceCache`：字节 → 解码 → GPU 句柄 / `AtlasTile`，**含远程图片的磁盘缓存**；资源文件在 `base_ui`」                                                              |
| 结论                     | v1 **不实现**。`ResourceCache` 的输入只是 `&[u8]`；字节从哪来（网络 / 磁盘 / 内嵌）是应用层的事                                                                                     |
| 理由 ①                   | **没有 v1 消费者**：远程头像属于 `base_ui` / `download_provider`（M6/M8 才重写）。引擎在 P13 无法验证任何真实语义（TTL、离线、ETag、隐私目录、配额）                                |
| 理由 ②                   | **IO 纪律冲突**：引擎主线程禁止阻塞 IO，磁盘读写要么 `spawn_blocking`（`clippy.toml` 全仓禁用，M-08）要么经 `AppHandle` 排队；把这条链路塞进 `ResourceCache` 等于让缓存拥有线程模型 |
| 理由 ③                   | **缓存键未定义**：URL？ETag？请求尺寸？主题？任意拍一个都要在 M6 重写。且磁盘缓存写的是**字节**（压缩态），与图集 tile（解码态）是两层，混在一起会出现两份真值                      |
| 后续形态（记录，不实施） | 应用层 `base_ui::assets::disk`：`url → sha256 路径 + TTL`，命中后把**字节**交给 `ResourceCache::insert`。引擎 API 已是「字节进」，因此后续是**纯增量**，无重构                      |
| 触发条件                 | M6 落地首个远程头像消费者；或产品要求离线可用                                                                                                                                       |

### 5.8 SVG（明确边界）

- 引擎**不声明** `usvg`，不提供任何运行时/构建期 SVG 光栅（F-11）。
- 54 个 Phosphor 图标由 `base_ui` 的 `build.rs` 在构建期转成路径数据或预生成图集（P15/P19 范围），
  产物作为 `&[u8]` 交给 `ResourceCache`（PNG）或 `Geometry`（路径）。
- 验收含一条依赖门禁：`cargo tree -p ui_core | findstr usvg` 必须无输出（§13）。

---

## 6. `src/resources/image.rs`（解码管线）

### 6.1 逐字签名

```rust
/// 解码结果：**预乘 sRGB 编码**的 BGRA8 字节（行主序，行宽 `width * 4`）。
pub(crate) struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub bgra: Box<[u8]>,
}

/// 字节 → BGRA8 预乘 sRGB。错误一律走 `DecodeCause`（不返回 `image::ImageError`）。
pub(crate) fn decode(bytes: &[u8]) -> Result<DecodedImage, DecodeCause>;
```

### 6.2 颜色空间与预乘（**定死**）

**存储格式 = 线性空间预乘后 sRGB 编码的 BGRA8**（这就是 `render/shaders/contract.md:190-193` 已经冻结的
「上传方职责」，本阶段只是把它在 CPU 侧实现出来）：

```text
对每个源像素 (r,g,b,a)：
    lin = srgb_decode(r/255)                      # 0..1 线性
    premul = lin * (a/255)
    stored_rgb = srgb_encode(premul) * 255        # 8 bit
    stored_a = a
```

为什么不是另外两种：

| 方案                                             | 结果                                                                                                      | 结论                                                         |
|--------------------------------------------------|-----------------------------------------------------------------------------------------------------------|--------------------------------------------------------------|
| A. 直通 alpha（存原始 sRGB，shader 里乘 alpha）  | shader 是 `texel * color0`（`quad.frag.glsl:23`），不做 `rgb*a` ⇒ 半透明图在**预乘混合**下会出现亮边/过亮 | 要改三套 shader + `contract.md`，且 `FLAG_OPAQUE` 语义也要动 |
| B. **编码域预乘**（`stored = srgb_encoded * a`） | probe 实测与正确做法最大差 **60/255**（中灰 128 @ a=128：naive 64 vs 正确 93）                            | 低 alpha 严重偏暗                                            |
| **C. 线性预乘 + sRGB 重编码（采用）**            | 硬件 sRGB 解码后正好是线性预乘；shader 不动；8 bit 往返最坏线性误差 **0.0097**                            | ✅                                                           |

配套实现：**64 KiB LUT**。`OnceLock<Box<[u8; 1 << 16]>>`，键 `(color << 8) | alpha`，值
`srgb_encode(srgb_decode(color) * alpha)`；
首次使用时构建（65 536 次 `powf` ≈ 1 ms），之后每像素 3 次查表。mathematically 与逐像素计算逐位一致（同一函数）。

量化代价（写进 §15 V-99）：alpha < 32 时预乘后的线性值很小，sRGB 编码的 8 bit 量化相对误差变大；
这是**预乘存储的固有代价**（任何 8 bit 预乘纹理都一样），不是本方案的缺陷。

### 6.3 解码步骤（逐字）

```text
1. guess_format(bytes) → ImageFormat；Err → UnsupportedFormat { format: "unknown" }
2. 只接受 Png / Jpeg / Webp；其它 → UnsupportedFormat { format: <extensions_str()[0] 或 "unknown"> }
3. ImageReader::new(Cursor::new(bytes)).with_guessed_format()
     Err(io) → Corrupt { format: <已知>, detail: io.to_string() }
4. let mut limits = image::Limits::default();          // #[non_exhaustive]，禁止字面量构造
   limits.max_image_width  = Some(MAX_SOURCE_DIM);
   limits.max_image_height = Some(MAX_SOURCE_DIM);
   reader.limits(limits);
5. reader.decode()
     Ok(dynamic) → 6.
     Err(Limits)       → TooLarge { width: ?, height: ? }   // 见下注
     Err(Decoding)     → Corrupt { format, detail }
     Err(Unsupported)  → UnsupportedFormat { format }
     Err(Parameter|Encoding|IoError) → Corrupt { format, detail }
6. let (w, h) = (dynamic.width(), dynamic.height());
   if w > MAX_TILE_DIM || h > MAX_TILE_DIM:
       let scale = (MAX_TILE_DIM as f32 / w as f32).min(MAX_TILE_DIM as f32 / h as f32);
       let (tw, th) = ((w as f32 * scale).floor().max(1.0) as u32,
                       (h as f32 * scale).floor().max(1.0) as u32);
       dynamic = dynamic.resize_exact(tw, th, FilterType::Triangle);   // 编码域重采样
7. let rgba = dynamic.to_rgba8();     // 16-bit / 灰度 / 调色板 全部归一
8. 预乘 + BGRA（§6.2 的 LUT），返回 DecodedImage
```

注（`Err(Limits)` 的尺寸）：`ImageError::Limits` 不带实际尺寸。两条实现路线，本设计选 **(a)**： (a)
`TooLarge { width: 0, height: 0, limit: MAX_SOURCE_DIM }`，文案写「至少一边超过 limit」——不为一个诊断字段
去二次解析头部； (b) 用 `ImageReader::into_dimensions()` 再拿到尺寸（多一次读取，且对部分格式不可靠）。
→ 若实现期发现 `into_dimensions()` 在 PNG/JPEG/WebP 上都可靠，可改 (b) 并在 DEVIATIONS 补记；**默认 (a)**。

常量（`resources/image.rs` 内，`pub(crate)` 供单测引用）：

| 常量             | 值                     | 理由                                                                           |
|------------------|------------------------|--------------------------------------------------------------------------------|
| `MAX_TILE_DIM`   | `2046`                 | 图片页 2048 − 2 纹素 padding（P02 §10.3），等于单块上限                        |
| `MAX_SOURCE_DIM` | `8192`                 | 8192²×4 = 256 MiB < `Limits::default().max_alloc`(512 MiB)；再大属应用层预缩放 |
| 缩放滤波         | `FilterType::Triangle` | 无振铃、成本线性；`Lanczos3` 对 UI 缩略图过度                                  |

**编码域重采样的已知代价**（写进 V-101）：`image::imageops::resize` 在 sRGB 编码域做双线性，不还原线性光；
与浏览器/主流 UI 框架一致，v1 不做线性空间重采样（3× 成本，且 `image` 没有该滤波器）。
半透明边缘的「透明像素带色」fringe 未验证，登记 V-101。

### 6.4 格式策略（**定死**）

| 格式                                                                  | v1                     | 说明                                                                                                                                           |
|-----------------------------------------------------------------------|------------------------|------------------------------------------------------------------------------------------------------------------------------------------------|
| PNG                                                                   | ✅                     | 主用（品牌图 / 预览图 / 图标位图）                                                                                                             |
| JPEG                                                                  | ✅                     | 头像 / 照片                                                                                                                                    |
| WebP                                                                  | ✅                     | 头像（含 alpha）                                                                                                                               |
| GIF / BMP / ICO / TIFF / EXR / HDR / PNM / QOI / TGA / DDS / farbfeld | ❌ `UnsupportedFormat` | 虽然 workspace 的 `image` feature 开着了 `bmp`/`gif`/`ico`（`base_ui` 需要），引擎**按策略拒绝**；`guess_format` 与 feature 无关（probe §2.2） |
| 16-bit / 灰度 / 调色板                                                | ✅（降位）             | `to_rgba8()` 归一；精度损失写进契约                                                                                                            |

**ICC / gAMA 明确不处理**：`image 0.25.10` 把 ICC 当元数据透传、不做色彩管理（probe §2.2 的源码事实：
`src/codecs/png.rs:181` 只读 `icc_profile`）。资产契约：**交给引擎的字节必须已经按 sRGB 编码**；
非 sRGB 资产（Display P3 截图、AdobeRGB JPEG）由应用层预转换，否则颜色会偏。

---

## 7. `src/resources/cache.rs`（页缓冲）

### 7.1 类型

```rust
/// 一条存活资源。
pub(crate) struct Entry {
    /// 交给应用的结果（tile + 像素尺寸）。
    pub image: ResourceImage,
    /// `etagere` 的释放键（`Allocation::id`）。
    pub alloc: etagere::AllocId,
    /// 内容区（页面坐标，不含 padding）。
    pub content: PhysicalRect,
    /// 分配到的整块（含 padding；卸载时清零它）。
    pub padded: PhysicalRect,
}

/// 图片页的 CPU 缓冲（BGRA8，行主序）。
pub(crate) struct Page {
    width: u32,
    height: u32,
    bytes: Box<[u8]>,
}

impl Page {
    pub fn new(width: u32, height: u32) -> Self;              // 全 0
    pub fn dimensions(&self) -> (u32, u32);
    pub fn bytes(&self) -> &[u8];
    /// 把 `src`（行宽 `stride`，`rect.width*4` 有效）写进 `rect`；越界行/列裁剪。
    pub fn write(&mut self, rect: PhysicalRect, src: &[u8], stride: usize);
    /// 把 `rect` 清零（padding 与卸载）。
    pub fn clear_rect(&mut self, rect: PhysicalRect);
}
```

`Page` 的行宽恒为 `width * 4`；不做行对齐（CPU 侧无对齐要求；GPU 侧的对齐在 `render/windows/atlas.rs` 里）。

### 7.2 `ResourceCache` 的私有字段（`resources.rs`）

```rust
pub struct ResourceCache {
    entries: Arena<Entry, ResourceId>,
    allocator: AtlasAllocator,      // 图片页（paint::atlas::AtlasKind::Image），内部持有脏行
    page: Page,
}
```

字段顺序固定为「arena / allocator / page」，`insert`/`unload` 按 §5.5/§5.6 顺序操作
（先分配、后写页、最后入 arena；卸载反之），保证任何一步失败都不留下「arena 有、页里没有」的中间态。
`new()` 调 `allocator.mark_written(整页)`，让第一次 `take_dirty_rows()` 就是整页（保证 GPU 侧 padding 为 0）。

---

## 8. 引擎接线

### 8.1 `AtlasSet` 删除（F-148）

现状：`AtlasSet { glyph, image }`，`App::refresh_text` 在 DPI 换代时 `atlas.clear_all()` 把**两页**都清空
（`app.rs:2281-2284`）。图片 tile 由应用持有，清页 = 全部 `Image` 悬垂。

改法：

| 位置              | 变更                                                                                                                                                              |
|-------------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `render/atlas.rs` | 删除 `AtlasSet` 与 `image_page_for_test` / `image_generation_for_test` / `clear_all`；`GLYPH_PAGE` / `IMAGE_PAGE` 保持 `pub(crate) const`                         |
| `render.rs`       | `pub(crate) use atlas::{Allocation, AtlasAllocator, DirtyRows};`（去 `AtlasSet`）                                                                                 |
| `app.rs`          | `atlas: AtlasAllocator`（`AtlasAllocator::new(AtlasKind::Glyph)`）；`atlas.clear()` 取代 `clear_all()`；新增 `resources: ResourceCache`（`ResourceCache::new()`） |
| `resources.rs`    | `ResourceCache` 持有 `AtlasAllocator::new(AtlasKind::Image)` + `Page::new(IMAGE_PAGE…)`                                                                           |

语义收益（写进文档）：**图片不随 DPI / 字体换代失效**（图片不是按 scale 光栅化的，tile 与 scale 无关）。

### 8.2 `App` 的图集页上传（**广播**）

```rust
// app.rs（impl App 内，pub(crate)）
/// 把两张图集页广播给**所有窗口**的渲染器（每帧一次，paint 之前）。
pub(crate) fn upload_atlas_pages(&mut self) {
    let glyph_dirty = self.glyph_atlas.take_page_dirty();
    let image_rows = self.resources.take_dirty_rows();
    let image_dirty = !image_rows.spans().is_empty();

    // 字形页：有脏 ⇒ 每个窗口整页；没脏 ⇒ 只补给「从未收到过整页」的窗口（新建窗口/换渲染器）。
    // 图片页：有脏行 ⇒ 已收到的窗口拿增量、未收到的拿整页。
}
```

**为什么必须广播（评审阻塞 ②）**：图集页是**进程级**共享数据（`AtlasAllocator` / `GlyphAtlas` 一份），
而纹理是**每窗口一份**（`AtlasTextures`）；`frame_all` 逐窗提交，若第一次 `take` 就把脏行消费给当前窗口，
其余窗口的纹理永远收不到内容（旧实现就是这样：第一个窗口之后，第二个窗口的图片/字形页恒为 0）。
因此：①脏旗标/脏行由 `upload_atlas_pages` **统一消费一次**再广播；②每个窗口记 `glyph_page_received` /
`image_page_received`，**没收到过整页的窗口拿整页**；③`Window::set_renderer` / `enable_offscreen_renderer`
把两个标记清零（换渲染器 = 换一套空纹理，必须重传）。

调用点：`App::frame`（每帧，`paint_frame()` 之前；不再带 `window` 参数）。
`TestApp::snapshot` 走自己的临时渲染器，直接 `queue_atlas_upload` 整页（不经过这里）。

### 8.3 `TestApp::snapshot` 上传两页

`test_support/app.rs` 的两个 `#[cfg]` 分支都要在 `queue_atlas_upload(Glyph, …)` 后补：

```rust
// 已有：let ((page_width, page_height), glyph_pixels) = self.app.glyph_page_pixels();
if ! self .app.resources().is_empty() {
let ((image_width, image_height), image_pixels) = self.app.resource_page_pixels();
debug_assert_eq! ((image_width, image_height), (page_width, page_height), "两页页宽必须相同");
let mut rows = crate::render::DirtyRows::default();
rows.mark(0, u32::MAX);                     // 快照路径恒整页（临时渲染器没有历史内容）
renderer.queue_atlas_upload(AtlasKind::Image, image_pixels, rows);
}
```

`App::resource_page_pixels(&self) -> ((u32, u32), &[u8])`（crate 私有）直接返回 `resources.page_pixels()`； **不**复制 16
MiB（`queue_upload` 内部已经 `to_vec` 一次，见 V-98）。
两页页宽必须相同：渲染器的构造函数只有一个 `page` 参数（`new_offscreen_with_page` / `new_warp_offscreen_with_page`），
由 `debug_assert_eq!` 守护（实现期把断言写进 `TestApp::snapshot`）。

### 8.4 demo 的生产消费（防「Resources 死代码」）

`crates/ui_core_demo/src/scene.rs`：

- 新增 `const ICON_PNG: &[u8]`：**16×16 PNG**，四象限 alpha = 255/170/85/0，颜色为可辨认的三色
  （由 `image` 离线编码后以字节字面量入库，约 200 B；生成命令写进注释）。
- `scene::build_into` 里 `let icon = app.resources_mut().insert(ICON_PNG)?;`（demo 侧把错误 `?` 成 `Box<dyn Error>`），
  然后在一个 32×32 的位置用 `Canvas::new(...) + push_image(Image { bounds, tile: icon.tile, modulation: Color::WHITE })`
  挂两个节点：1:1 与 2× 放大各一。
- 这条路径同时验证：`insert` → 页写入 → `App::upload_image_page` → 三平台采样（Windows 上 `--snapshot` 可看像素）。

`unload` 的生产消费者：无（公开 API，由单测覆盖正确性）；这是**有意的**——淘汰策略属应用层。

---

## 9. 像素 golden（`test_support/snapshot.rs` + `tests/snapshot.rs`）

### 9.1 `SnapshotCase`（扩字段）

```rust
/// 一次快照请求（合成场景 + 分辨率 + scale + 主题）。
///
/// `scene` / `theme` **不参与渲染**：它们只决定 golden 的文件名与「挂哪个合成场景」。
/// 渲染仍然是「读最近一次提交的实例缓冲」（P08 §7.7 的既有语义），因此 `TestApp::snapshot`
/// 只消费 `size` / `scale`。场景由 `test_support::snapshot::mount(app, scene, theme)` 在 `drain()` 之前挂载。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SnapshotCase {
    pub scene: SnapshotScene,
    pub theme: SnapshotTheme,
    pub size: Size,
    pub scale: Scale,
}

impl SnapshotCase {
    /// `"shapes_1.25_dark"`（文件名主干）。
    #[must_use]
    pub fn golden_name(&self) -> SmolStr;
    /// `CARGO_MANIFEST_DIR/tests/golden/warp/<golden_name>.png`
    #[must_use]
    pub fn golden_path(&self) -> PathBuf;
}

/// 合成场景（引擎内建，不依赖 `base_ui`：集成测试不能反向依赖应用层）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotScene { Shapes, Shadow, Gradient, Colors, Image, Clip }
impl SnapshotScene {
    pub const ALL: [Self; 6];
    #[must_use]
    pub fn token(self) -> &'static str;         // "shapes" …
}

/// 明暗「主题」在引擎里只是**合成场景的配色标签**（D15：主题语义在应用层）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotTheme { Light, Dark }
impl SnapshotTheme {
    #[must_use]
    pub fn token(self) -> &'static str;
}
```

`scale` 的 token 规则（**定死**，避免 `1.0` vs `1` 的文件名漂移）：

```text
fn scale_token(scale: f32) -> SmolStr {
    let text = format_smolstr!("{scale:.2}");          // "1.00" / "1.25" / "2.00"
    SmolStr::new(text.trim_end_matches('0').trim_end_matches('.'))   // "1" / "1.25" / "2"
}
```

`golden_name()` 用 `format_smolstr!` 拼（AGENTS §2.2：**禁止** `format!` → `String` → `SmolStr::from/into`；
`SmolStr::new(&str)` 是允许的构造方式）。
`golden_path()` 用 `Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/warp").join(format!("{name}.png"))`
（路径不是短字符串，`String`/`PathBuf` 合规）。

### 9.2 差图统计与容差

```rust
/// 逐通道容差（D19：**本阶段不冻结数值**，见 §9.5）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tolerance {
    /// 单通道允许的最大绝对差（0..=255）。
    pub per_channel: u8,
    /// 允许「有任一通道超出 `per_channel`」的**像素数**上限。
    pub max_over_tolerance_pixels: u32,
}

impl Tolerance {
    /// 逐字节相等。
    pub const EXACT: Self = Self { per_channel: 0, max_over_tolerance_pixels: 0 };
}
impl Default for Tolerance {
    fn default() -> Self { Self { per_channel: 1, max_over_tolerance_pixels: 0 } }
}

/// 一次比较的统计。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SnapshotDiff {
    /// 全图最大单通道差（0..=255）。
    pub max_channel_diff: u8,
    /// 全图所有通道差的均值（0..=255 标度）。
    pub avg_channel_diff: f32,
    /// 有任一通道差 > `tolerance.per_channel` 的像素数。
    pub over_tolerance_pixels: u32,
    /// 参与比较的像素数（= width * height）。
    pub compared_pixels: u32,
    /// 第一个超容差像素的坐标（x, y）。
    pub first_over_tolerance: Option<(u32, u32)>,
}

/// 比较两张读回图（尺寸必须一致）。
///
/// # Errors
/// `GoldenError::Dimensions`（宽高不等）。
pub fn compare(actual: &SnapshotImage, golden: &SnapshotImage, tolerance: Tolerance)
               -> Result<SnapshotDiff, GoldenError>;
```

判定规则（**定死**）：`pass ⇔ over_tolerance_pixels <= tolerance.max_over_tolerance_pixels`。
`max_channel_diff` / `avg_channel_diff` 只进报告与 golden README，不参与判定（否则单像素离群会掩盖面积性偏差）。

### 9.3 golden 目录与命名

```text
crates/ui_core/tests/golden/
├── README.md                     # 字节语义 / 重新生成命令 / 设备分组 / 为什么没有 lavapipe、metal-real
└── warp/                         # v1 唯一存在的设备分组
    ├── shapes_1_light.png        shapes_1_dark.png
    ├── shapes_1.25_light.png     shapes_1.25_dark.png
    ├── shapes_2_light.png        shapes_2_dark.png
    ├── shadow_1_light.png
    ├── gradient_1_light.png
    ├── colors_1_light.png        colors_1_dark.png
    ├── image_1_light.png         image_2_light.png
    └── clip_1_light.png
```

- 文件名 = `<scene>_<scale>_<theme>.png`（主设计 §6.4 是 `<case>_<scale>_<theme>`，一致）。
- `lavapipe/` 与 `metal-real/` **v1 不创建**：空目录是噪音，等对应 CI job 真正产出 golden 时再建
  （登记进 F-143 的备注；主设计 §6.4 的三目录结构不变式不变）。
- **字节语义**：golden 就是 `SnapshotImage.rgba8` 原样（**预乘 sRGB 编码的 RGBA8**）。不做反预乘——反预乘要除 alpha，
  低 alpha 会放大误差，破坏「逐字节可复现」。因此 golden 在普通看图器里会比最终显示**偏暗**，README 写明。
- **不进发布物**：`Packager.toml` 无 `resources` 字段（本机确认），`tests/` 不在打包输入里；仓库 `.gitignore` 也不排除
  golden（必须入库）。

### 9.4 生成 / 更新 / 比对命令

```powershell
# 生成或更新（Windows / WARP）
$env:UPDATE_GOLDEN = '1'
cargo test -p ui_core --features test-support --test snapshot -- --nocapture
Remove-Item Env:UPDATE_GOLDEN

# 正常比对（CI 与本地同一条）
cargo test -p ui_core --features test-support --test snapshot -- --nocapture

# 单个 case（测试名 = golden_<scene>）
cargo test -p ui_core --features test-support --test snapshot -- golden_image
```

```bash
# bash 等价
UPDATE_GOLDEN=1 cargo test -p ui_core --features test-support --test snapshot
cargo test -p ui_core --features test-support --test snapshot
```

`UPDATE_GOLDEN` 存在即写（不看值），写完后**同一测试继续跑比较**（写 → 读 → 比较必须零差异，形成自检）。

### 9.5 容差默认值与「不冻结」（D19）

| 项                                               | 值      | 状态                                                   |
|--------------------------------------------------|---------|--------------------------------------------------------|
| `Tolerance::default().per_channel`               | `1`     | 沿用 P08 §7.7 的提案值；**待 L3（WARP 实机）实测校准** |
| `Tolerance::default().max_over_tolerance_pixels` | `0`     | 同上                                                   |
| `Tolerance::EXACT`                               | `0 / 0` | 参数类断言（非像素）用它                               |

写进 `tests/golden/README.md` 与本文档：**D19 明确本设计不预设像素容差数值**；`1` 只是「首次生成时用来发现
明显噪声」的起点，校准动作属 L3 / P7，校准时只改常量 + 更新 README，不改机制。
环境变量不做覆盖开关（预览值太多会变成第二套配置层）；需要实验时改常量重跑。

### 9.6 无设备 / 非 Windows 的 skip 策略（**定死**）

| 平台    | 行为                                                                                                                                                                       |
|---------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| Windows | 跑。`SnapshotError::NoRenderer` **不应出现**（WARP 是系统组件）；出现即**测试失败**并打印 detail。`SnapshotError::Gpu` 也是失败（设备创建/读回失败是真故障，I10 不许静默） |
| macOS   | `skip_reason()` 返回 `Some("pixel goldens are Warp-only in v1 …")`，每个 case 打印一行 `skip <name>: <reason>` 并 `return`。**不**比对 warp golden（跨设备比会永久红）     |
| Linux   | 同上；额外原因：`new_offscreen` 在 Linux 恒 `UnsupportedGpu`（F-137），根本没有读回路径                                                                                    |

「跳过」必须**可见**：`eprintln!` + 注释里指向本文档；禁止 `#[ignore]`（`cargo test` 默认不跑，等于无声消失）。

### 9.7 文字像素 golden：**v1 不做**（F-144）

| 理由         | 说明                                                                                                                                            |
|--------------|-------------------------------------------------------------------------------------------------------------------------------------------------|
| 宿主字体依赖 | 引擎没有随包字体（`FontStack` 走系统字体，P06 D17）。字形像素随宿主机字体文件/版本变化，golden 必然跨机失败                                     |
| 已有替代覆盖 | ①既有 `rendered_text_produces_visible_glyph_pixels`（覆盖率既非 0 也非全屏，宿主无关）；②L2 的 `GlyphRun` 实例/UV/位置断言；③P06 的塑形语料测试 |
| 启用前提     | 打包 4 个 TTF（拉丁/CJK/emoji）并让 `FontStack` 能指向资产字体（契约变更），与 L5 字体矩阵一起做                                                |

`SnapshotScene` 因此**没有** `Glyphs` 变体（不留空壳）。登记 V-96。

### 9.8 case 表（逐字）

| scene      | 场景内容（合成）                                                              | size（逻辑） | scales     | themes      | 文件数 |
|------------|-------------------------------------------------------------------------------|--------------|------------|-------------|--------|
| `Shapes`   | 直角矩形 / 四角不同圆角 / 1px 边框 / 圆角叠圆角                               | 240×160      | 1, 1.25, 2 | light, dark | 6      |
| `Shadow`   | 解析阴影 blur 4 与 16、offset (2,4)、半透明色                                 | 240×160      | 1          | light       | 1      |
| `Gradient` | 线性水平 / 线性垂直 / 径向                                                    | 240×160      | 1          | light       | 1      |
| `Colors`   | 纯色 / 半透明叠加（premultiplied）/ 高彩度 OKLCH（gamut mapping 边界）        | 160×120      | 1          | light, dark | 2      |
| `Image`    | 16×16 四象限 alpha(255/170/85/0) 的 tile：1:1 + 2× 放大 + 一档白色/半透明调制 | 160×120      | 1, 2       | light       | 2      |
| `Clip`     | 嵌套 clip（圆角 + 溢出子矩形）                                                | 240×160      | 1          | light       | 1      |
| **合计**   |                                                                               |              |            |             | **13** |

场景构建纪律：

- 只用公开 API（`NodeBuilder` / `Canvas` / `Color` / `Image` / `Rect` / `LayoutOverride`），**不用** taffy 直接构造
  （除了 `Image`/`Clip` 需要的 `Style.inner.size`，与 `tests/frame_record.rs` 的既有写法一致）。
- 每个场景的节点带稳定 `id`（`"shapes.rect0"` 之类），失败时能对上 L2 记录。
- `Colors` 的高彩度用例用 `Color::Oklch { l, c, h }`，覆盖 P02 §5.4 的 sRGB gamut mapping（`clip` 路径）。
- `Image` case 的 PNG **在测试内用 `image` 现编码**（`RgbaImage::from_raw(..).write_to(Png)`），不落外部资产：
  同一份字节既喂 `ResourceCache::insert`（走真实解码），又作为「场景内容」的唯一来源。

### 9.9 CI 三平台 job（**占位说明**，本阶段不落地）

现状：`.github/workflows/` 只有 `release.yml`（本机确认），没有任何测试 job。本阶段只写「将来怎么接」：

| job                      | runner                          | 设备                                          | 命令                                                            | 说明                                                                                                                                                                                     |
|--------------------------|---------------------------------|-----------------------------------------------|-----------------------------------------------------------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `pixel-snapshot-windows` | `windows-latest`                | 系统 WARP（`EnumWarpAdapter`）                | `cargo test -p ui_core --features test-support --test snapshot` | 打印 `adapter name / driver version / MaxSupportedFeatureLevel / HighestShaderModel`（主设计 §6.4 的能力集约束）；**启用 SM 6.6 变体前**先核对 WARP 来源（系统 d3d10warp vs NuGet WARP） |
| `pixel-snapshot-linux`   | `ubuntu-latest` + Mesa lavapipe | lavapipe（`VK_ICD_FILENAMES=lvp_icd.*.json`） | 同上（预期 skip 直到有 `golden/lavapipe`）                      | 离屏 image 渲染，不依赖 Wayland compositor；job 存在但当前**只能**产出 skip 日志                                                                                                         |
| `pixel-snapshot-macos`   | macOS 实机/GPU 直通 runner      | 真实 Metal                                    | 同上（预期 skip 直到有 `golden/metal-real`）                    | 主设计 §6.4：runner 不可用时**阻塞**而非静默跳过；v1 尚无 golden，故先只做「能编译 + 能 skip」的占位                                                                                     |

**合并门禁**（本阶段）：Windows 本地/CI 的 `tests/snapshot.rs` 必须全绿（13 个 case）。 **不**把 mac/Linux 的 skip 当作通过证据：它们的
L3 有效性等 `golden/{lavapipe,metal-real}` 真正入库后另行开启。

### 9.10 golden 驱动 API

```rust
/// 建 TestApp → 挂场景 → drain → snapshot。
///
/// # Errors
/// `GoldenError::Snapshot`（设备/渲染/读回失败）。
pub fn capture(case: SnapshotCase) -> Result<SnapshotImage, GoldenError>;

/// 冷启动一个 case：与 `capture` 相同，但**不**复用任何 App（每个 case 一个 TestApp）。
pub fn mount(app: &mut TestApp, scene: SnapshotScene, theme: SnapshotTheme);

/// 比较（`UPDATE_GOLDEN` 存在时先写 golden，再读回比较）。
///
/// # Errors
/// `GoldenError::{Snapshot, Io, Diff}`；`Diff` 含统计与 golden 路径。
pub fn assert_golden(case: SnapshotCase, tolerance: Tolerance) -> Result<SnapshotDiff, GoldenError>;

/// 覆盖写 golden（父目录不存在时创建）。
pub fn write_golden(case: SnapshotCase, image: &SnapshotImage) -> Result<(), GoldenError>;

/// 本平台是否跳过像素 golden（`Some(原因)` = 跳过）。
#[must_use]
pub fn skip_reason() -> Option<&'static str>;

/// 进程内串行化 GPU 快照（多个 `#[test]` 并行会各自建 WARP 设备，见 §2.1 #17）。
#[must_use]
pub fn gpu_guard() -> std::sync::MutexGuard<'static, ()>;
```

```rust
/// golden 层错误。**不** `derive(Eq)`：`Diff.diff` 含 `f32`。
#[derive(Clone, Debug, PartialEq)]
pub enum GoldenError {
    Snapshot(SnapshotError),
    /// 读回尺寸与 golden 不同。
    Dimensions { actual: (u32, u32), golden: (u32, u32) },
    /// 读/写 golden 失败（含「golden 不存在」）。
    Io { path: PathBuf, detail: SmolStr },
    /// golden 文件不是可解码的 PNG / 尺寸不自洽。
    GoldenDecode { path: PathBuf, detail: SmolStr },
    /// 输入图自相矛盾（`rgba8` 长度 ≠ `width × height × 4`）；显式错误而非 `debug_assert`
    /// ——release 下 `debug_assert` 会被编掉，`zip` 会**静默少比**。
    Malformed { detail: SmolStr },
    /// 超容差（含统计与热力图路径）。
    Diff { case: SmolStr, diff: SnapshotDiff, golden: PathBuf, heatmap: PathBuf },
}
```

失败时写**热力图**到 `CARGO_MANIFEST_DIR/../../target/golden-diff/<golden_name>.png`（不入库）：
超容差像素涂洋红 `(255,0,255)`，其余像素取 `actual` 的 1/2 亮度（便于肉眼定位）。

### 9.11 `tests/snapshot.rs` 的形态

```rust
//! L3：像素 golden（Windows WARP）。
//! macOS / Linux：`skip_reason()` 非空 ⇒ 打印原因并返回（见 design/phase-13 §9.6）。

use ui_core::{math::{Size, Scale}, test_support::snapshot::{...}};

fn run(case: SnapshotCase) {
    if let Some(reason) = skip_reason() {
        eprintln!("skip {}: {reason}", case.golden_name());
        return;
    }
    let _guard = gpu_guard();
    let diff = assert_golden(case, Tolerance::default()).expect("golden");
    eprintln!("{} max={} avg={:.3} over={}", case.golden_name(), diff.max_channel_diff, diff.avg_channel_diff, diff.over_tolerance_pixels);
}

#[test]
fn golden_shapes() { for scale in [1.0, 1.25, 2.0] { for theme in [Light, Dark] { run(case(Shapes, theme, scale)); } } }
#[test]
fn golden_shadow() { run(case(Shadow, Light, 1.0)); }
#[test]
fn golden_gradient() { run(case(Gradient, Light, 1.0)); }
#[test]
fn golden_colors() { for theme in [Light, Dark] { run(case(Colors, theme, 1.0)); } }
#[test]
fn golden_image() { for scale in [1.0, 2.0] { run(case(Image, Light, scale)); } }
#[test]
fn golden_clip() { run(case(Clip, Light, 1.0)); }

#[test]
fn compare_reports_stats_without_a_device() { /* 纯 CPU：两张手造 SnapshotImage */ }
#[test]
fn compare_rejects_dimension_mismatch() { /* 纯 CPU */ }
```

一个 `#[test]` 内顺序跑多个 case（同一个锁内），失败时把 case 名带进 panic 消息（`expect(&name)`）。

### 9.12 读回路径（逐平台，**定死**）

```text
capture(case):
 1. TestApp::with_options(WindowOptions { title, size: case.size, scale: case.scale })
 2. mount(&mut app, case.scene, case.theme)        # 公开 NodeBuilder / Canvas 建树
 3. app.drain()?                                   # styles → text → layout_text → paint → encode
                                                   # （实例缓冲已就绪；无 renderer，present 是 no-op）
 4. app.snapshot(case)                             # 只读最近提交的实例缓冲 + 图集页
      let surface = Rect::new(0,0,size.w,size.h).to_physical_covering(scale)   # 物理像素
      Windows: Renderer::new_warp_offscreen_with_page(surface, glyph_page_w)
      macOS  : Renderer::new_offscreen_with_page(surface, glyph_page_w)        # 真实 Metal 离屏
      Linux  : Renderer::new_offscreen_with_page(..) → Err(UnsupportedGpu)     # F-137
      queue_atlas_upload(Glyph, glyph_page_pixels, DirtyRows::full)
      if !resources.is_empty() { queue_atlas_upload(Image, resource_page_pixels, DirtyRows::full) }
 5. render_snapshot(instance_buffer)：整帧提交（Full plan）→ 等完成 → 读回
      D3D12: persist(RT) → COPY_SOURCE → CopyTextureRegion(READBACK heap, footprint=资源格式)
             → SetEventOnCompletion + WaitForSingleObject → Map → 去 RowPitch padding → BGRA→RGBA
      Metal: persist blit → MTLBuffer(shared) → waitUntilCompleted → getBytes
      Vulkan: host-visible buffer ← cmd_copy_image_of_buffer（P12 的 readback.rs；Linux 快照路径恒失败）
 6. SnapshotImage { width, height, rgba8 }         # sRGB 编码、预乘、RGBA 顺序
```

要点：

- 尺寸用 `to_physical_covering`（AA/图集按外扩，P01 §7.1）；`width/height` 是**物理像素**，因此同一 case 在
  scale=2 下的 golden 是 480×320。
- 读回的 sRGB/预乘语义由渲染目标格式决定（P08 §7.7）；本层**不做**任何色彩变换，只做 BGRA→RGBA 与去 padding
  （已由 `readback.rs` 完成），所以 `SnapshotImage` → PNG 是逐字节的。
- 快照用的临时渲染器**不复用** `App` 的渲染器（`TestApp` 恒 `renderer = None`）：图集页必须显式重传，
  否则字形/图片为全 0（P09 的 S-85 已确立该路径）。

---

## 10. bench（`benches/perf.rs`）

### 10.1 Cargo 形态

```toml
[[bench]]
name = "perf"
path = "benches/perf.rs"
harness = false

[dev-dependencies]
criterion = { workspace = true, default-features = false, features = ["cargo_bench_support"] }
```

- **无 `required-features`**：benches 只用公开 API（`App`/`Window`/`ColorResolver`/`NodeBuilder`），
  不依赖 `test-support`，因此 `cargo bench --no-run` 在默认 features 下就能编译（门禁才可执行）。
- `criterion::black_box` 已弃用 ⇒ 一律 `std::hint::black_box`（S-130）。

### 10.2 基准集（覆盖主设计 §6.7 的六项）

| bench id                | 测什么                                                                               | 入口（公开 API）                                             | 覆盖 §6.7 的           |
|-------------------------|--------------------------------------------------------------------------------------|--------------------------------------------------------------|------------------------|
| `color_resolve`         | 颜色转换：`ColorResolver::resolve`，256 个混合空间色（Srgb/Oklch/Oklab/P3 + 高彩度） | `paint::color::ColorResolver::resolve`                       | 颜色转换               |
| `tree_build_512`        | 512 个 box 的建树（`NodeBuilder::build`）+ 首帧样式求值                              | `App::create_window` / `NodeBuilder` / `App::refresh_styles` | 实例构建（CPU 侧前段） |
| `text_shape_32`         | 32 个文本节点的塑形（每轮换文本，避开缓存短路）                                      | `App::write` + `App::refresh_text`                           | 塑形                   |
| `text_layout_32`        | 塑形后重排                                                                           | `App::layout_text`                                           | 布局（文本部分）       |
| `layout_512`            | taffy 布局（512 节点，flex + grid）                                                  | `Window::compute_layout`                                     | 布局                   |
| `frame_full_512`        | 整帧：样式 → 文本 → 布局 → lane 重建 → 编码 → damage                                 | `Window::set_size`（交替尺寸强制全 damage）+ `App::frame`    | 实例构建 + damage 合并 |
| `frame_incremental_512` | 只改一个信号后的增量帧（lane 复用 + damage 合并）                                    | `App::write` + `App::frame`                                  | damage 合并            |
| `signal_write_64`       | 依赖失效：一个被 64 个 `bind_style` 读的信号，写入（交替值避开等值短路）             | `App::write`                                                 | 依赖图失效             |

**为什么没有独立的 `damage_merge` / `instance_encode` microbench**：`render::damage::DamageAccumulator` 与
`paint::instance::FrameEncoder` 都是 crate 私有，且**不应**进公开白名单（主设计 §1.3）。为 bench 开一个
`pub` 出口会扩大 API 面换取一个数字，不划算。两者由 `frame_full_512` / `frame_incremental_512` 覆盖，
文档在此写明「顺带测量」关系（避免误读为纯 merge 成本）。

### 10.3 可复现性纪律

| 项       | 约定                                                                                                                                                                                                                                                                         |
|----------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 环境     | headless（`renderer = None`）：**不**受窗口前台化 / `PrintWindow` / A/B 消融影响（主设计 §6.7 的那些陷阱只在 GPU 计时下成立；GPU 计时属 L5）                                                                                                                                 |
| 固定输入 | 树内容 / 颜色 / 文本全部字面量；不用随机数、不读系统字体以外的环境（文本用 `"Kytrix 0123"` 这类稳定串）                                                                                                                                                                      |
| runtime  | 每组用 `iter_batched_ref(BatchSize::SmallInput, setup, routine)`：setup 建 `current_thread` runtime + `App` + 窗口 + 树（**不计时**），routine 只测目标 pass。`App::new` 需要 `Handle`，`App` 只持 `Handle` 不持 runtime ⇒ runtime 必须活到本批结束（放进 setup 的返回元组） |
| 采样     | criterion 默认（100 samples / 3 s warmup / 5 s 测量）；`frame_*` 用 `sample_size(30)` 控制总时长                                                                                                                                                                             |
| 报告     | 首次跑出的基线写进 PR 描述，**不入库**（没有 baseline 文件；避免机器指纹入库）                                                                                                                                                                                               |
| 非门禁   | 门禁只要求 `cargo bench -p ui_core --bench perf --no-run` 通过；数值不进 CI 判定（D19）                                                                                                                                                                                      |

### 10.4 波动风险（R-4）

- 512 节点帧在 debug 下不可用 ⇒ bench 只跑 `--release`（criterion 默认 bench profile）。
- 文本 bench 受系统字体影响（塑形/光栅量随字体不同）⇒ `text_*` 的绝对值只在同机对比有意义；写进文件头注释。
- 机器负载：criterion 自带离群剔除；文档要求「固定机器 + 空载 + 三次取中位数」。

---

## 11. 诊断（`test_support/diagnostics.rs`）

### 11.1 启用方式与「不进发布物」

| 项          | 决定                                                                                                                                                                                                                                                          |
|-------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| feature     | **沿用 `test-support`**，不新增 `diagnostics` feature（F-146）。理由：诊断的输入（`FrameRecord` / `InstanceBuffer` / `A11yTreeSnapshot` / `FrameStats`）全部只在 `test-support` 下存在；再加一个 feature 只会产生「diagnostics 开了但 record 没开」的非法组合 |
| 编译门      | `src/test_support.rs` 里 `pub mod diagnostics;`，整个模块被 `lib.rs` 的 `#[cfg(feature = "test-support")]` 包住                                                                                                                                               |
| 发布物自查  | `cargo build -p ui_core_demo --release` 后 `findstr /M /C:"FrameDump" target\release\ui_core_demo.exe` **必须无输出**（§13）                                                                                                                                  |
| `cfg(test)` | 单测可直接用；集成测试/诊断构建走 `--features test-support`                                                                                                                                                                                                   |
| 平台句柄    | DTO 只含标量 / `enum` / `PhysicalRect` / `NodeId` / `WindowId` / `SmolStr`；**禁止**出现 `windows::*` / `ash` / `objc2*` 类型（本模块不 `use` 任何平台模块，code review 可一眼看出）                                                                          |

### 11.2 `FrameDump`

```rust
/// 一帧的**可 diff 结构化记录**（*不是* replay 引擎：v1 没有 `--replay` 工具，见 F-145）。
#[derive(Clone, Debug, PartialEq)]
pub struct FrameDump {
    pub frame_index: u32,
    pub viewport: PhysicalRect,
    pub scale: f32,
    pub damage: Vec<PhysicalRect>,
    pub instances: Vec<InstanceDump>,
    pub ops: Vec<OpDump>,
    pub clips: Vec<ClipDump>,
}

/// 一个 `GpuPrimitive` 的明文镜像（字段与 128 B 契约一一对应）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InstanceDump {
    pub kind: u32,           // params1.z
    pub texture: u32,        // params1.y
    pub bounds: [f32; 4],
    pub corner_radius: [f32; 4],
    pub color0: [f32; 4],
    pub color1: [f32; 4],
    pub params0: [f32; 4],
    pub uv: [f32; 4],
    pub clip_index: u32,
    pub clip_rect: [f32; 4],
}

/// `FrameOp` 的镜像。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpDump {
    pub instances: Option<(u32, u32)>,   // Some((start, len)) = FrameOp::Instances
    pub geometry: Option<u32>,           // Some(index) = FrameOp::Geometry
    pub clip_index: u32,
}

/// `ClipEntry` 的镜像（`meta[0]` = parent）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClipDump {
    pub rect: [f32; 4],
    pub corner_radius: [f32; 4],
    pub parent: Option<u32>,
}

/// 从 `TestApp` 最近一帧捕获（无帧时返回空的 `instances`，不 panic）。
#[must_use]
pub fn frame_dump(app: &test_support::app::TestApp) -> FrameDump;

impl fmt::Display for FrameDump { /* 文本，见下 */ }
```

文本格式（`Display`，**稳定、逐行、可 diff**；数字一律十进制、浮点 `{:.4}`）：

```text
frame 42 viewport 0,0,800,600 scale 1.2500
damage 2
  rect 12,34,120,56
  rect 0,0,800,600
instances 17
  inst 0 kind=1 tex=0 bounds=4.0000,6.0000,40.0000,16.0000 clip=0
  inst 1 kind=2 tex=1 bounds=8.0000,8.0000,32.0000,32.0000 clip=2 uv=0.0010,0.0020,0.0170,0.0180
ops 3
  op instances 0..17
clips 3
  clip 0 rect=0.0000,0.0000,240.0000,160.0000 r=0.0000,0.0000,0.0000,0.0000 parent=none
```

### 11.3 `DependencyGraphDump`

```rust
/// 依赖图（`SignalId → Owner`）的明文快照。
#[derive(Clone, Debug, PartialEq)]
pub struct DependencyGraphDump {
    /// 按 `signal.index()` 升序（HashMap 迭代序不可复现，必须排序）。
    pub signals: Vec<SignalDump>,
    pub edges: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SignalDump {
    pub signal: SignalId,
    pub owners: Vec<DependencyOwnerDump>,
}

/// `Owner` 的公开镜像（`Owner` 本身是 crate 私有）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DependencyOwnerDump {
    Node { window: WindowId, node: NodeId },
    Derived(DerivedId),
}

/// 依赖图的公开捕获入口（自由函数：`impl App` 只能在 `app.rs`，见 §3 的「一个类型一个 impl 文件」）。
#[must_use]
pub fn dependency_dump(app: &App) -> DependencyGraphDump;
```

访问路径（实现细节，避免隐私墙）：`App` 在 `app.rs` 里追加
`#[cfg(feature = "test-support")] pub(crate) fn dependency_graph(&self) -> &DependencyGraph`（与既有
`#[cfg(feature = "test-support")]` 的 `Window::record` 同型）；`DependencyGraph` 在 `node/dependency.rs`
内**只**追加一个
`#[cfg(feature = "test-support")] pub(crate) fn dependents(&self) -> &ahash::HashMap<SignalId, OwnerSet>`
（**不**在 `node` 层引用 `test_support` 的 DTO——否则依赖方向倒挂），DTO 转换与排序全在
`diagnostics::dependency_dump` 里做。

排序：`SignalDump.owners` 按 `(window.to_bits, node.index, node.generation)` / `DerivedId::index()` 排序；`signals` 按
`SignalId::index()` 升序——HashMap 迭代序不可复现，文本必须自己定序。

### 11.4 `A11yTreeDump`

```rust
/// 一笔 a11y 交付的明文快照（`full == false` 时只含变更节点 + 祖先链）。
#[derive(Clone, Debug, PartialEq)]
pub struct A11yTreeDump {
    pub full: bool,
    pub focus: A11yNodeId,
    pub diagnostics: usize,
    pub nodes: Vec<A11yNodeDump>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct A11yNodeDump {
    pub engine: NodeId,
    pub role: A11yRole,
    pub label: Option<SmolStr>,
    pub value: Option<SmolStr>,
    pub bounds: PhysicalRect,
    pub hidden: bool,
    pub disabled: bool,
    pub actions: Vec<A11yAction>,
    pub children: Vec<A11yNodeId>,
}

#[must_use]
pub fn a11y_dump(snapshot: &A11yTreeSnapshot) -> A11yTreeDump;
```

### 11.5 `DamageOverlay`

```rust
/// 把 damage 矩形画成可叠加的 `Canvas`（洋红 1px 描边 + 12.5% 填充），走**正常管线**：
/// 没有平台 overlay 窗口，因此能截进快照、能在三平台一致。
#[derive(Clone, Debug, PartialEq)]
pub struct DamageOverlay {
    pub rects: Vec<PhysicalRect>,
}

impl DamageOverlay {
    /// 从一帧记录取 damage。
    #[must_use]
    pub fn of(frame: &FrameRecord) -> Self;
    /// 生成叠在 `viewport`（物理像素）之上的 `Canvas`（1 px 描边，按 `scale` 反算逻辑坐标）。
    #[must_use]
    pub fn canvas(&self, scale: Scale) -> Canvas;
}
```

`canvas()` 的输出：每个 damage 矩形 4 个 1px 细 `Quad`（描边）+ 1 个 `Color::from_rgba8([255,0,255,32])` 填充，
`bounds` 取 `PhysicalRect` 反算的逻辑矩形（`scale.to_logical`）。不写 `Scene`/lane：它是**值**，由调用方挂到节点上。

### 11.6 验收断言（诊断）

| 断言                                                             | 用例                                  |
|------------------------------------------------------------------|---------------------------------------|
| `FrameDump` 的实例数与 `FrameRecord.stats.instances` 相等        | `diagnostics.rs` 单测                 |
| `FrameDump` 文本逐行稳定（同输入两次 `Display` 相等）            | 同上                                  |
| `DependencyGraphDump` 的 `edges` 等于所有 owners 数之和          | 同上                                  |
| 排序稳定（同一依赖图两次 dump 的 `signals` 顺序相同）            | 同上                                  |
| `A11yTreeDump` 的节点数 = 快照节点数；`focus` 透传               | 同上                                  |
| `DamageOverlay::canvas` 的 bounds 覆盖所有 damage（含 1px 描边） | 同上                                  |
| 发布构建无 `FrameDump` 字符串                                    | §13 的 `findstr` 自查（手动/CI 可选） |

---

## 12. 测试清单（本阶段必须提交的用例）

### 12.1 `src/resources.rs`（14）

| #  | 用例                                        | 断言                                                                                                    |
|----|---------------------------------------------|---------------------------------------------------------------------------------------------------------|
| 1  | `resource_id_is_8_bytes`                    | `size_of::<ResourceId>() == 8`                                                                          |
| 2  | `resource_id_to_bits_layout`                | `(gen << 32) \| index`，`Debug` 文案 `r{index}.{gen}`                                                   |
| 3  | `insert_returns_stable_tile`                | 同字节插两次 → 两个不同 tile（无按内容去重）                                                            |
| 4  | `insert_png_happy_path`                     | `width/height` 为源尺寸，`tile.atlas == Image`、`generation == 1`                                       |
| 5  | `insert_jpeg_and_webp`                      | 两种格式都能插（用 `image` 现编码的极小图）                                                             |
| 6  | `insert_rejects_gif`                        | `ResourceDecode { cause: UnsupportedFormat { format: "gif" } }`                                         |
| 7  | `insert_rejects_garbage`                    | `UnsupportedFormat { format: "unknown" }`                                                               |
| 8  | `insert_rejects_corrupt_png`                | PNG 头 + 垃圾 → `Corrupt`                                                                               |
| 9  | `insert_rejects_oversize_source`            | 用小页 + 大图触发 `Capacity`（`MAX_SOURCE_DIM` 路径用 8193×1 的 PNG 太慢，改单测 `decode` 层，见 12.2） |
| 10 | `unload_is_idempotent_and_frees_slot`       | 连调两次：`true, false`；再插复用槽位 → 旧 id `unload` 仍 `false`                                       |
| 11 | `unload_clears_the_page_region`             | 卸载后页对应区全 0；`take_dirty_rows` 含该行区间                                                        |
| 12 | `capacity_error_carries_page_and_tile_size` | 小页塞满后再插 → `Capacity { width, height, page }`                                                     |
| 13 | `capacity_does_not_bump_generation`         | 溢出后 `tile.generation` 与既有 tile 相同（不清页）                                                     |
| 14 | `failed_insert_leaves_no_entry`             | 失败后 `len()` 不变、页未变                                                                             |

### 12.2 `src/resources/image.rs`（17）

| #  | 用例                                                 | 断言                                                                         |
|----|------------------------------------------------------|------------------------------------------------------------------------------|
| 1  | `decode_png_rgba8_is_byte_exact_after_unpremultiply` | alpha=255 的图：`unpremul(stored) == src` 逐字节                             |
| 2  | `decode_premultiplies_in_linear_space`               | 已知像素 (128,128,128,128) → 存储值等于 probe 的线性预乘表值（93/93/93/128） |
| 3  | `decode_alpha_zero_zeroes_rgb`                       | a=0 的像素 rgb 全 0                                                          |
| 4  | `decode_swaps_red_and_blue`                          | BGRA 顺序（`bgra[0..4] == [b,g,r,a]`）                                       |
| 5  | `decode_16bit_downshifts`                            | `Rgba16` 输入 → 8 bit（0/62/125/187 那组）                                   |
| 6  | `decode_downsamples_without_upscaling`               | 4000×100 的图 → 宽 2046、高 51（`floor`），小图不变                          |
| 7  | `decode_never_returns_zero_dimension`                | 1×1 图与极端比例都 ≥ 1                                                       |
| 8  | `decode_rejects_unsupported_formats`                 | gif/bmp magic → `UnsupportedFormat`                                          |
| 9  | `decode_maps_limits_to_too_large`                    | 小 `MAX_SOURCE_DIM`（单测覆盖常量注入）→ `TooLarge { limit }`                |
| 10 | `decode_maps_corrupt_bytes`                          | 截断的 PNG → `Corrupt { format: "png" }`                                     |
| 11 | `premultiply_lut_matches_scalar_formula`             | LUT 与逐像素 `powf` 实现对全部 65 536 组合逐位相等                           |
| 12 | `decode_empty_input`                                 | `UnsupportedFormat { format: "unknown" }`                                    |

### 12.3 `src/resources/cache.rs`（8）

`page_new_is_zeroed` / `page_write_clips_out_of_bounds` / `page_write_respects_stride` / `clear_rect_zeroes_only_rect` /
`clear_rect_accepts_inverted_rect_noop` / `page_dimensions_roundtrip` / `write_then_read_back_row` /
`page_is_16_mib_for_default_page`。

### 12.4 `test_support/snapshot.rs`（13，全部纯 CPU，无需 GPU）

| #  | 用例                                                                                   |
|----|----------------------------------------------------------------------------------------|
| 1  | `golden_name_uses_trimmed_scale`（1.0 → `1`，1.25 → `1.25`，2.0 → `2`）                |
| 2  | `golden_path_is_under_tests_golden_warp`                                               |
| 3  | `golden_path_is_unique_per_case`（`SnapshotScene::ALL × 2 themes × 3 scales` 去重）    |
| 4  | `compare_identical_images_is_zero`                                                     |
| 5  | `compare_counts_over_tolerance_pixels`                                                 |
| 6  | `compare_reports_max_and_avg`                                                          |
| 7  | `compare_records_first_divergent_pixel`                                                |
| 8  | `compare_rejects_dimension_mismatch`                                                   |
| 9  | `tolerance_default_is_recorded_value`（锁住 `1 / 0`，改常量必须改测试 + README）       |
| 10 | `skip_reason_is_some_off_windows` / `skip_reason_is_none_on_windows`（`cfg` 两个用例） |
| 11 | `snapshot_case_is_copy_and_debug`（防止误加堆字段）                                    |

### 12.5 `test_support/diagnostics.rs`（10）

见 §11.6 的 6 条 + `frame_dump_handles_idle_frame`（无提交帧时 `instances` 为空、不 panic）+
`op_dump_distinguishes_instances_and_geometry`。

### 12.6 `src/app.rs`（新增 6：`resources_tests`）

| # | 用例                                                                                                              |
|---|-------------------------------------------------------------------------------------------------------------------|
| 1 | `resources_are_process_level`（`resources_mut` 插入后 `resources()` 可见；换窗口不变）                            |
| 2 | `image_page_upload_is_skipped_without_new_tiles`（headless：`take_dirty_rows` 首次整页、第二次空）                |
| 3 | `snapshot_uploads_the_image_page`（Windows：插入 1 张 16×16 后快照，图集区出现非零像素；非 Windows `NoRenderer`） |
| 4 | `insert_invalidates_nothing`（插入不动任何节点的 `DirtyFlags`；图片是应用主动权）                                 |
| 5 | `dependency_dump_lists_bind_style_owners`（test-support）                                                         |

### 12.7 `src/render/atlas.rs`（改：删 1 / 改 1 / 加 1）

- 删除 `atlas_set_keeps_kinds_independent_and_clears_together`（`AtlasSet` 已删，F-148）。
- 改 `default_page_sizes_match_paint_contract`：去掉 `AtlasSet` 部分，只断言 `GLYPH_PAGE`。
- 新增 `image_page_constant_is_2048`（`IMAGE_PAGE == (2048, 2048)`，因为 `ResourceCache` 依赖它）。
- 新增 `deallocate_works_outside_tests`（F-147 的回归：生产构建可用）。

### 12.8 `tests/snapshot.rs`（8：6 个 golden 组 + 2 个纯 CPU）

见 §9.11。

### 12.9 计数（实现期实测）

新增单测 **70** 项：`resources.rs` 14 + `resources/image.rs` 17 + `resources/cache.rs` 8 +
`test_support/snapshot.rs` 13 + `test_support/diagnostics.rs` 10 + `app.rs::resources_tests` 6 +
`error.rs` 2 + `render/atlas.rs` 1（新增）/ 1（改写）；集成新增 **8** 项（`tests/snapshot.rs`）。

| 构建                                      | 基线实测 | 实测                                    |
|-------------------------------------------|----------|-----------------------------------------|
| `cargo test -p ui_core --lib`             | 590      | **636**                                 |
| `--features test-support --lib`           | 622      | **691**                                 |
| `--release --features test-support --lib` | —        | **682**（debug_assert 门控的 9 项不跑） |
| 集成（`--tests`）                         | 6        | **14**                                  |

---

## 13. 验收门禁

```powershell
# 0) 前置（shader 工具链缺失时）
$env:ui_core_SKIP_SHADERS = '1'

# 1) L0/L1/L2 单测（含新增 resources / snapshot / diagnostics 单测）
cargo test -p ui_core
cargo test -p ui_core --features test-support
cargo test -p ui_core --features test-support --test frame_record
cargo test -p ui_core --features test-support --test snapshot        # Windows WARP；mac/Linux skip

# 2) release 下探针一致性（既有要求）
cargo test -p ui_core --release --features test-support

# 3) bench 只要求可编译（非门禁）
cargo bench -p ui_core --bench perf --no-run

# 4) lint / 格式 / 文档
cargo +nightly clippy -p ui_core -p ui_core_demo --all-targets --all-features -- -D warnings
cargo +nightly fmt -p ui_core -p ui_core_demo -- --check
cargo doc -p ui_core --no-deps

# 5) 发布物自查
cargo build -p ui_core_demo --release
findstr /M /C:"FrameDump" /C:"SnapshotDiff" /C:"DamageOverlay" target\release\ui_core_demo.exe   # 必须无输出

# 6) 依赖纪律
cargo tree -p ui_core | findstr /C:"usvg"      # 必须无输出（F-11）
cargo tree -p ui_core | findstr /C:"plotters"  # 必须无输出（S-130）
git diff Cargo.lock                               # 只允许 ui_core 依赖表里的 "criterion"/"image" 两行
```

| 门禁               | 期望                                                                                                                                                                     |
|--------------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 默认测试           | `590 → 636`（实测）；既有 590 项**逐项不变**（含 `test_support/app.rs` 的 2 个快照用例）                                                                                 |
| `test-support`     | `622 → 691`（实测）；集成 `6 → 14`（`frame_record` 6 + `snapshot` 8）                                                                                                    |
| release            | 682 项全绿（debug_assert 门控的 9 项在 release 下不跑，与既有口径一致）                                                                                                  |
| 快照               | `tests/snapshot.rs` 13 个 golden 全绿（Windows WARP，实测 0.4 s）；mac/Linux 只出 skip 日志且 **exit 0**                                                                 |
| bench              | `--no-run` 通过；不跑数值                                                                                                                                                |
| clippy / fmt / doc | 零 warning（`missing_docs` 未开启，V-22 债务不扩大：新增公开项**必须**有文档注释）                                                                                       |
| 发布物             | `test_support::*` 零符号；`usvg` / `plotters` 不在依赖树；`git diff Cargo.lock` 只有两行依赖边                                                                           |
| 交叉 target        | `aarch64-apple-darwin` / `x86_64-unknown-linux-gnu`（`SKIP_SHADERS=1` + stub `pkg-config`）`cargo check` exit 0；唯一的 warning 是既有的 V-94（`InFlightRing::reserve`） |

---

## 14. Cargo 变更（本阶段只给结论，不改文件）

### 14.1 `..`

```toml
[features]
# 不变：test-support = []（诊断与 golden 都挂它，F-146）

[[test]]
name = "frame_record"          # 既有
path = "tests/frame_record.rs"
required-features = ["test-support"]

[[test]]                       # 新增
name = "snapshot"
path = "tests/snapshot.rs"
required-features = ["test-support"]

[[bench]]                      # 新增
name = "perf"
path = "benches/perf.rs"
harness = false

[dependencies]
# P13 资源解码（workspace 已钉 0.25.10；六个 codec feature 已在 base_ui 的图内）
image = { workspace = true }

[dev-dependencies]             # 新增整节
criterion = { workspace = true }

# 根 Cargo.toml（workspace）—— criterion 的构建形态只能写在这里：
# criterion = { version = "0.8.2", default-features = false, features = ["cargo_bench_support"] }
```

### 14.2 为什么用 workspace 声明（AGENTS §6）

- `image` / `criterion` 都已在 `[workspace.dependencies]`（`Cargo.toml:102-109`、`:85`），子 crate **不写版本**。
- `image` 的 feature 集合由 workspace 单点控制（`bmp,gif,ico,jpeg,png,webp`）；引擎**不**另开 feature 列表
  （重复声明会制造「两个地方决定 codec」的漂移）。
- `criterion` 的 `default-features = false` 是**构建形态**而非版本，但 **Cargo 不允许子 crate 在
  `workspace = true` 的同时覆盖 `default-features`**（实测：`default-features = false cannot override
  workspace's default-features`），所以它必须落在根 `[workspace.dependencies]` 的那一行里。

### 14.3 `Cargo.lock` 的变化（只有两行依赖边）

| 包                | 锁文件位置        | 来源                                                                  |
|-------------------|-------------------|-----------------------------------------------------------------------|
| `image 0.25.10`   | `Cargo.lock:5147` | 已在（`base_ui` / `ui_core_demo`）                                    |
| `criterion 0.8.2` | `Cargo.lock:2101` | 已在（`gpui-pre-bench-metrics` 引入，M8 后由本 crate 持有）           |
| `plotters 0.8.2`  | `Cargo.lock:7794` | 已在；本 crate **不启用**该 feature（`cargo tree -p ui_core` 看不到） |

⇒ 门禁 `git diff Cargo.lock` **只允许出现 `ui_core` 依赖表里的两行边**（`"criterion"` / `"image"`）， **不允许**
出现新包或版本变化。若出现 `image` 的 patch 版本跳动（0.25.9/0.25.10 都在锁内），
实现期以 `cargo update -p image --precise 0.25.10` 保持，**不**升/降大版本。

---

## 15. 风险与缓解

| #    | 风险                                                     | 等级 | 对策                                                                                                                                                                             |
|------|----------------------------------------------------------|------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| R-1  | golden 跨设备差异（WARP vs 真机、WARP 版本、驱动小版本） | 高   | golden 按设备分组（v1 只有 `warp/`）；容差可配；CI job 打印 WARP 能力集；软件/真机差异由 L3/P7 量化（V-95）                                                                      |
| R-2  | **字体依赖**使文字像素不可作 golden                      | 高   | v1 排除文字 golden（F-144）；由 `rendered_text_produces_visible_glyph_pixels` + L2 结构断言覆盖；固定字体资产后再启用（V-96）                                                    |
| R-3  | `image` 的 codec feature 体积累积                        | 中   | 六个 codec 已在 `base_ui` 的依赖图内 ⇒ 引擎**不新增** codec 代码；体积实测登记 V-97；若 release 超预算，改动点在 workspace 的 `image` feature 列表（影响 `base_ui`，需单独立项） |
| R-4  | bench 波动导致误读                                       | 中   | 非门禁；固定机器/空载/中位数；`frame_*` 降 `sample_size`；文本 bench 标注字体依赖                                                                                                |
| R-5  | 每个 golden case 建两个 2048² WARP 纹理（4 + 16 MiB）    | 中   | 13 个 case 串行；已有 4 MiB 字形页先例；实测耗时登记 V-100；若过慢再引入「快照专用小页」（需 `App` 可注入页尺寸，非本阶段）                                                      |
| R-6  | 图片页整页上传（`queue_upload` 的 `to_vec` 16 MiB）      | 中   | 只在「有新 tile/卸载」的帧上传；脏行已按 `DirtyRows` 精确（首次整页保证 padding=0）；V-98 跟踪                                                                                   |
| R-7  | 预乘 sRGB 8 bit 的低 alpha 量化                          | 低   | probe 量化（0.0097 @ alpha≥32）；V-99；必要时改 16-bit 图集（契约变更）                                                                                                          |
| R-8  | `unload` 误用（仍有图元引用）                            | 中   | 契约写进 rustdoc；卸载区清零（误用表现为透明）；单测覆盖「卸载→复插」；不可能在编译期阻止（`Canvas` 值语义）                                                                     |
| R-9  | 编码域缩放的半透明 fringe                                | 低   | V-101；`Image` case 的 golden 会暴露明显问题                                                                                                                                     |
| R-10 | WARP 设备并行导致 device removed                         | 中   | `gpu_guard()`（S-131）；lib 单测继续用 `gpu_test_lock`                                                                                                                           |

---

## 16. 追加到 `design/DEVIATIONS.md` 的条目（§十八）

> 格式照既有条目：`F-xxx`（与上游文档不同的事实）/ `S-xxx`（实现纪律）/ `V-xx`（待复核）。
> 编号接在 P12 之后：`F-138…`、`S-128…`、`V-95…`。

### F-138 `EngineError::ResourceDecode` 的 payload 从字符串改为强类型分类

| 项         | 内容                                                                                                                                                                                                                     |
|------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 设计原文   | P01 §6.2.2 #16：`ResourceDecode { resource: ResourceId, detail: SmolStr }`                                                                                                                                               |
| 偏差       | 改为 `ResourceDecode { cause: DecodeCause }`；`DecodeCause` = `UnsupportedFormat { format }` / `Corrupt { format, detail }` / `TooLarge { width, height, limit }` / `Capacity { width, height, page }`                   |
| 原因       | ①`detail: SmolStr` 无法 match，AGENTS §3 要求「上层能按类型决定策略」——占位图 / 提示 / 卸载重试是三种不同处理；②解码发生在 `ResourceId` **铸出之前**，冻结表里的 `resource` 字段只能填一个刚被释放的陈旧句柄（语义谎言） |
| 修订后设计 | 见本文档 §5.2；`Display` 保持英文完整句；P01 冻结表的历史正文不改，本条为 payload 修订记录                                                                                                                               |

### F-139 远程图片磁盘缓存 v1 不做

| 项         | 内容                                                                                                                       |
|------------|----------------------------------------------------------------------------------------------------------------------------|
| 设计原文   | 主设计 §8.5：「引擎提供 `ResourceCache`：字节 → 解码 → GPU 句柄 / `AtlasTile`，**含远程图片的磁盘缓存**」                  |
| 偏差       | v1 只做**内存**缓存，输入是 `&[u8]`；磁盘缓存/网络取字节归应用层（`base_ui` / `download_provider`）                        |
| 原因       | 无 v1 消费者（M6 才重写头像）；引擎主线程禁阻塞 IO（`clippy.toml` 禁 `spawn_blocking`）；缓存键（URL/ETag/TTL/尺寸）未定义 |
| 修订后设计 | §5.7；后续形态是应用层 `base_ui::assets::disk`，命中后仍调 `ResourceCache::insert` ⇒ 纯增量、无重构                        |

### F-140 图片图集的容量逃逸是强类型错误，不是 `clear + retry`

| 项         | 内容                                                                                                                                                                                      |
|------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 设计原文   | P02 §10.6（P05 落地）：`allocate → None` ⇒ `atlas.clear()` + 重试 ⇒ 仍失败 `AtlasExhausted`                                                                                               |
| 偏差       | 图片页**不** clear、不换代；溢出直接 `DecodeCause::Capacity`                                                                                                                              |
| 原因       | 字形 tile 由 `layout_text` 按需重建（`GlyphAtlas::ensure`），换代自愈；图片 tile 只在应用建树时解析一次，`Canvas` 是值语义快照 ⇒ 清页会让应用手里的 UV 全部悬垂（且被复用后画出别人的图） |
| 修订后设计 | §5.4；`AtlasExhausted { kind }` 继续只服务字形路径                                                                                                                                        |

### F-141 图片图集存「线性预乘 → sRGB 重编码」的 BGRA8（与 `contract.md` §8 对齐，phase-02 §10.3 的措辞作废）

| 项         | 内容                                                                                                                                                                                                                                  |
|------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 设计原文   | P02 §10.3：「`BGRA8` **sRGB 编码**（硬件 sRGB 解码）+ **直通 alpha**」；shader `out = color0 * vec4(texel.rgb, texel.a)`                                                                                                              |
| 事实核对   | 已落地的 `render/shaders/contract.md:190-193` 冻结的是**另一套**（且更严格）：「纹素是 *linear-premultiplied 后 sRGB 编码*」「上传方职责：P15 必须 linear 空间预乘 → sRGB 编码后写图集；改成直通 alpha 时 shader 与本文件必须同时改」 |
| 偏差       | 以上传方职责为准：`srgb_decode(rgb) * alpha` → `srgb_encode` → 存 8 bit；alpha 直存。**phase-02 §10.3 的「直通 alpha」措辞视为过时**（P02 正文不改，以 `contract.md` 为准，本条即修订记录）                                           |
| 原因       | ①直通 alpha 在预乘混合下会亮边，且要改三套 shader + `contract.md`；②编码域预乘 probe 实测最大差 **60/255**；③线性预乘 + 硬件 sRGB 解码 = shader 零改动，8 bit 往返最坏线性误差 0.0097                                                 |
| 修订后设计 | §6.2；`contract.md` **不需要**改（既有条款已正确），只需 P15 按它实现；`SnapshotScene::Image` 的 golden 是这条契约的像素验收面                                                                                                        |

### F-142 只接受 PNG / JPEG / WebP；`image::Limits` 用 `default()` + 字段赋值

| 项         | 内容                                                                                                                                                                                                         |
|------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 设计原文   | §8.5「品牌 PNG / 预览 PNG 走 `image 0.25` + 图集」；未约束上限与其它格式                                                                                                                                     |
| 偏差       | ①按 `guess_format` **策略性**拒绝其它格式（`UnsupportedFormat`）；②`MAX_SOURCE_DIM = 8192`、`MAX_TILE_DIM = 2046`；③`Limits` 必须 `default()` + 赋值（`#[non_exhaustive]`，probe `E0639`）；④16-bit 静默降位 |
| 原因       | `guess_format` 与 feature 无关（未开 `gif` 也返回 `Ok(Gif)`，probe）；workspace 的 `image` 开了 6 种 codec，靠 feature 裁剪挡不住；尺寸上限是解压炸弹的强类型出口                                            |
| 修订后设计 | §6.3/§6.4                                                                                                                                                                                                    |

### F-143 golden 的字节语义是「预乘 sRGB RGBA8」原样落盘；v1 只建 `warp/`

| 项         | 内容                                                                                                                                                                                            |
|------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 设计原文   | 主设计 §6.4：`tests/golden/{warp,lavapipe,metal-real}/{case}_{scale}_{theme}.png`                                                                                                               |
| 偏差       | ①不改名、不反预乘（golden = 读回字节原样，PNG 无损往返，probe 验证逐字节一致）；②v1 只有 `tests/golden/warp/`，另两个目录在对应 job 产出 golden 时创建；③golden 在普通看图器里偏暗，README 写明 |
| 原因       | 反预乘要除 alpha（低 alpha 放大误差），会破坏「逐字节可复现」；空目录是噪音                                                                                                                     |
| 修订后设计 | §9.3                                                                                                                                                                                            |

### F-144 文字/emoji 像素 golden 不进 v1

| 项         | 内容                                                                                                                                                     |
|------------|----------------------------------------------------------------------------------------------------------------------------------------------------------|
| 设计原文   | 主设计 §6.5：图集维度必须至少 1 个 case「字形（拉丁 / CJK / emoji）」                                                                                    |
| 偏差       | v1 的 golden 集合**不含**文字；字形像素由 `rendered_text_produces_visible_glyph_pixels`（覆盖率 sanity）+ L2 的 `GlyphRun` 字段断言 + P06 的塑形语料覆盖 |
| 原因       | 引擎不随包字体（D17/Fontique 系统字体），字形像素随宿主机字体文件/版本变化 ⇒ golden 跨机必红；固定字体资产属契约变更（`FontStack` 指向资产字体）         |
| 修订后设计 | §9.7；V-96 跟踪；`SnapshotScene` 不留空 `Glyphs` 变体                                                                                                    |

### F-145 `FrameDump` 是「可 diff 的结构化记录」，不是已实现的 replay

| 项         | 内容                                                                                                                     |
|------------|--------------------------------------------------------------------------------------------------------------------------|
| 设计原文   | §9.1：「`FrameDump`（导出 `Scene` 为可重放记录）」                                                                       |
| 偏差       | v1 导出实例/op/clip/damage 的强类型 DTO + 稳定文本；**没有** `--replay` 工具，也没有「从 dump + atlas 重放」的引擎入口   |
| 原因       | 真正的 replay 需要「atlas 页内容 + shader 产物 + 平台 renderer」三件套的封装，属独立工具；本阶段的价值在 bug 报告与 diff |
| 修订后设计 | §11.2                                                                                                                    |

### F-146 诊断挂既有 `test-support` feature，不新增 `diagnostics` feature

| 项         | 内容                                                                                                                         |
|------------|------------------------------------------------------------------------------------------------------------------------------|
| 设计原文   | §9.1：「诊断开关（仅 debug / test）」                                                                                        |
| 偏差       | 四个 dump 与 `DamageOverlay` 全部在 `#[cfg(feature = "test-support")]` 下；不新增 feature                                    |
| 原因       | 诊断的输入（`FrameRecord` / `InstanceBuffer` / `A11yTreeSnapshot`）本身只在 `test-support` 存在；两个 feature 会产生非法组合 |
| 修订后设计 | §11.1                                                                                                                        |

### F-147 `AtlasAllocator::deallocate` 生产化；`ResourceCache` 不做自动 LRU

| 项         | 内容                                                                                                                                         |
|------------|----------------------------------------------------------------------------------------------------------------------------------------------|
| 设计原文   | §9.3：「图集 `etagere` + **LRU** + `generation`」                                                                                            |
| 偏差       | ①`deallocate` 从 `#[cfg(test)]` 提升为生产代码（幂等槽位表一起生产化）；②**不**实现自动 LRU 逐出，淘汰 = 应用显式 `unload`，策略在 `base_ui` |
| 原因       | 见 §5.4：`Canvas` 值语义 ⇒ 自动淘汰 = 悬垂 UV = 静默错图；且 v1 没有 `bind_canvas` 重建入口。LRU 记账若只存不用即死代码（AGENTS 1.2）        |
| 修订后设计 | §5.4/§5.6；自动淘汰的前置条件（`bind_canvas` + 全量 PAINT 协调 + 逐 tile 引用计数）登记于此，供后续阶段评估                                  |

### F-148 `AtlasSet` 删除；图片分配器与页缓冲移入 `ResourceCache`

| 项         | 内容                                                                                                                                                                                               |
|------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 设计原文   | §1.2 文件树 + P02 §5.6「字形与图片分图集」；P05 的 `AtlasSet { glyph, image }`                                                                                                                     |
| 偏差       | 删除 `AtlasSet`，`App::atlas` 直接是字形 `AtlasAllocator`；图片 `AtlasAllocator` + `Page` 由 `ResourceCache` 持有                                                                                  |
| 原因       | ①`AtlasSet::clear_all()` 在 **DPI 换代**时把图片页也清空（`app.rs:2281`）= 悬垂 UV，必须拆开；②图片 tile 与 scale 无关，本就不该随 DPI 换代；③只有一个分配器时 `AtlasSet` 是空壳包装（AGENTS 1.2） |
| 修订后设计 | §8.1                                                                                                                                                                                               |

### S-128 `tests/` 与 `benches/` 的 target 形态（M-09 的落地细节）

- `tests/snapshot.rs`：`required-features = ["test-support"]`；`tests/golden/` 只是数据目录（Cargo 不把子目录当 target）。
- `benches/perf.rs`：`[[bench]] harness = false`，**无** `required-features`（只用公开 API）。
- 不建 `tests/perf.rs`（同一文件不能既是 test target 又是 bench harness，M-09）。

### S-129 `SnapshotCase` 的 `scene` / `theme` 不参与渲染

- `TestApp::snapshot` 语义保持 P08 §7.7：「读最近一次提交的实例缓冲」，只消费 `size` / `scale`。
- `scene` / `theme` 的消费者是 `snapshot::mount` 与 `golden_name` / `golden_path`（两个真实消费者，不是死字段）。

### S-130 `criterion` 关闭默认 features（**写在 workspace 根**）；`black_box` 用 `std::hint::black_box`

- **根 `Cargo.toml`**：`criterion = { version = "0.8.2", default-features = false, features = ["cargo_bench_support"] }`
  （probe：无 plotters/rayon）。
- **子 crate**：`criterion = { workspace = true }`。Cargo **不允许**在继承 workspace 依赖时覆盖 `default-features`
  （`error: default-features = false cannot override workspace's default-features`），因此这条配置只能在根声明。
- `criterion::black_box` 在 0.8 已 `#[deprecated]`；`real_blackbox` feature 也不再需要（std 版本等价）。

### S-131 集成测试的 GPU 串行锁走 `test_support::snapshot::gpu_guard()`

- lib 单测继续用 `render/windows/device.rs::gpu_test_lock()`（`#[cfg(test)]` 保持）；
- 集成测试（`tests/snapshot.rs`）用 `test_support::snapshot::gpu_guard()`（`test-support` 构建），两者都是
  「进程级 `Mutex` + poison 恢复」，不共享同一个静态（跨二进制本来就不共享地址空间）。

### V-95 golden 跨设备 / 软件 vs 真机差异未量化

| 触发                                                       | 动作                                                                                           |
|------------------------------------------------------------|------------------------------------------------------------------------------------------------|
| L3（WARP 实机 / lavapipe / metal-real）任一处开始出 golden | 用同一 case 跑 WARP 与真实 GPU，量化 max/avg/超容差像素，回填 `Tolerance::default()` 与 README |

### V-96 文字 / emoji 像素无 golden（依赖宿主字体）

| 触发                                       | 动作                                                           |
|--------------------------------------------|----------------------------------------------------------------|
| 打包固定字体资产、`FontStack` 支持资产字体 | 新增 `SnapshotScene::Glyphs` 变体，同时覆盖 §6.5 的 emoji 维度 |

### V-97 `image` 解码 feature 的二进制体积增量未实测

| 触发                          | 动作                                                                                                               |
|-------------------------------|--------------------------------------------------------------------------------------------------------------------|
| 发布体积预算复核（V-47 同批） | 用 `cargo bloat`/安装包差分测 codec 增量；超预算则改 workspace 的 `image` feature 列表（影响 `base_ui`，单独立项） |

### V-98 图片页上传的整页 `to_vec`（P09 遗留）

| 触发                       | 动作                                                                                         |
|----------------------------|----------------------------------------------------------------------------------------------|
| 多张图/大图时 CPU 时间明显 | 把 `AtlasTextures::queue_upload` 改成「行菱形缓冲 + 行跨度」（与 V-64 的字形增量上传一起改） |

### V-99 低 alpha 的预乘 8 bit 量化误差

| 触发                                                 | 动作                                                               |
|------------------------------------------------------|--------------------------------------------------------------------|
| golden / 真机出现低 alpha 图（阴影贴图、淡出）的色阶 | 图集改 `BGRA8` 为 `RGBA16F`（契约变更）或对低 alpha 图走非图片路径 |

### V-100 每个 golden case 建两个 2048² WARP 纹理的耗时/内存

| 触发                                               | 动作                                                                |
|----------------------------------------------------|---------------------------------------------------------------------|
| `tests/snapshot.rs` 单次运行 > 60 s 或 CI 内存吃紧 | 给 `App` 注入页尺寸（`SnapshotCase` 加 `page` 字段）或复用 renderer |

### V-101 编码域缩放的半透明 fringe 未验证

| 触发                             | 动作                                                              |
|----------------------------------|-------------------------------------------------------------------|
| `Image` golden 出现可见暗边/亮边 | 改为「先预乘再缩放」（在预乘域重采样）或线性空间重采样（成本 ×3） |

---

## 17. 实施顺序（实现代理的直接输入）

| 步 | 内容                                                                                                                            | 自检                                                                     |
|----|---------------------------------------------------------------------------------------------------------------------------------|--------------------------------------------------------------------------|
| 1  | `resources/image.rs`（含 LUT）+ 单测 12 条                                                                                      | `cargo test -p ui_core resources::image`                                 |
| 2  | `resources/cache.rs`（`Page` / `Entry`）+ 单测 8 条                                                                             | 同上                                                                     |
| 3  | `render/atlas.rs`：`deallocate` 生产化、删 `AtlasSet`                                                                           | `cargo test -p ui_core render::atlas`                                    |
| 4  | `resources.rs`（`ResourceId` / `ResourceImage` / `DecodeCause` / `ResourceCache`）+ 单测 14 条                                  | `cargo test -p ui_core resources`                                        |
| 5  | `error.rs`：`ResourceDecode { cause }` + Display 用例                                                                           | `cargo test -p ui_core error`                                            |
| 6  | `app.rs`：字段接线 + `upload_image_page` + `resource_page_pixels` + `resources{,_mut}` + `dependency_graph`（cfg test-support） | `cargo test -p ui_core app`                                              |
| 7  | `test_support/app.rs`：`snapshot` 上传图片页 + 用例 3                                                                           | `cargo test -p ui_core --features test-support test_support::app`        |
| 8  | demo：内嵌 PNG + 两个 `Image` 节点                                                                                              | `cargo run -p ui_core_demo`、`--features snapshot -- --snapshot out.png` |
| 9  | `test_support/snapshot.rs`：类型 + compare + golden IO + `capture`/`assert_golden`/`skip_reason`/`gpu_guard` + 单测 11 条       | `cargo test -p ui_core --features test-support test_support::snapshot`   |
| 10 | `tests/snapshot.rs` + `UPDATE_GOLDEN=1` 生成 13 个 golden + README                                                              | 去掉 `UPDATE_GOLDEN` 后全绿                                              |
| 11 | `test_support/diagnostics.rs` 四个 dump + 单测 8 条                                                                             | `cargo test -p ui_core --features test-support diagnostics`              |
| 12 | `benches/perf.rs` 8 组                                                                                                          | `cargo bench -p ui_core --bench perf --no-run`                           |
| 13 | `..`                                                                                                                            | 命令逐条跑通                                                             |
| 14 | 门禁全量（§13）                                                                                                                 | 全绿 + `Cargo.lock` 零 diff                                              |

**实现纪律**：不加 `#[allow]` / `#[expect]`；不留 `#[cfg(test)]` 之外的死代码；`unsafe` 为零（本阶段不碰平台层）；
公开项必须有 rustdoc（`missing_docs` 未开启，但 V-22 的债务**不许扩大**）。

---

## 18. 需裁决项（5，含建议）

| # | 议题                                                                | 建议                                                            | 备选与代价                                                                                                            |
|---|---------------------------------------------------------------------|-----------------------------------------------------------------|-----------------------------------------------------------------------------------------------------------------------|
| ① | **自动 LRU 淘汰**（主设计 §9.3 提到「图集 + LRU」）                 | **v1 不做**：`unload` 显式淘汰 + `Capacity` 强类型错误（F-147） | 做自动淘汰 ⇒ 必须先加 `bind_canvas` 或等价「画布可重建」契约 + 全量 PAINT 协调 + 逐 tile 引用计数；否则会出现静默错图 |
| ② | **远程图片磁盘缓存**（主设计 §8.5 的字面要求）                      | **v1 不做**，降级为应用层（F-139）                              | 做 ⇒ 引擎要引入 IO 线程模型 / 缓存键 / TTL / 隐私目录，且 v1 无消费者可验证                                           |
| ③ | **图片预乘方案**（影响三套 shader 是否要改）                        | **线性预乘 + sRGB 重编码**（F-141），shader 零改动              | 直通 alpha ⇒ 改 14 个 shader 源文件 + `contract.md` + 三平台像素 golden 重测；编码域预乘 ⇒ 实测偏色 60/255            |
| ④ | **文字像素 golden 排除 v1**（与主设计 §6.5 的「字形图集」维度冲突） | **排除**，用 sanity + L2 覆盖（F-144）                          | 纳入 ⇒ golden 绑定宿主字体，跨机必红，需要先打包字体资产（契约变更）                                                  |
| ⑤ | **`ResourceDecode` payload 改为强类型**（改 P01 冻结表 #16）        | **改**为 `{ cause: DecodeCause }`（F-138）                      | 保持 `{ resource, detail }` ⇒ 调用方无法分类；且 `resource` 在解码失败时只能是陈旧句柄                                |

---

## 附录 A · 与上游文档的交叉引用与**文档一致性修订**

| 文档                                  | 位置                                                                           | 修订                                                                                                                                                           |
|---------------------------------------|--------------------------------------------------------------------------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `custom-gui-engine-design.md`         | §6.9 命令清单 `cargo bench --bench engine`                                     | 实际 bench 名是 `perf`（M-09）：以 `benches/perf.rs` 为准，主设计正文不改，本条即说明                                                                          |
| `custom-gui-engine-design.md`         | §8.5 远程磁盘缓存                                                              | F-139 降级（v1）                                                                                                                                               |
| `custom-gui-engine-design.md`         | §9.3 「图集 + LRU」                                                            | F-147（v1 不做自动淘汰，策略归应用层）                                                                                                                         |
| `custom-gui-engine-design.md`         | §6.5 「字形（拉丁/CJK/emoji）」golden                                          | F-144（v1 排除，V-96 跟踪）                                                                                                                                    |
| `design/phase-01-foundation.md`       | §3 文件树 `resources.rs  # ResourceId / ResourceCache / AtlasTile / AtlasKind` | `AtlasTile`/`AtlasKind` 已在 `paint::atlas`（F-24）；`resources` 只留 `ResourceId`/`ResourceCache`/`ResourceImage`/`DecodeCause`                               |
| `design/phase-01-foundation.md`       | §3 文件树 `test_support/{probe,frame_record,snapshot,oracle}.rs`               | `oracle.rs` **取消**（oracle 在 `TestApp::oracle_diff/assert_oracle`，`test_support/app.rs:167-207`）；新增 `diagnostics.rs`                                   |
| `design/phase-01-foundation.md`       | §6.2.2 #16 `ResourceDecode` payload                                            | F-138                                                                                                                                                          |
| `design/phase-02-paint-foundation.md` | §10.3 图片图集「直通 alpha」的措辞                                             | **作废**：`render/shaders/contract.md:190-193` 已冻结「linear-premultiplied 后 sRGB 编码 + 上传方职责」，F-141 以 `contract.md` 为准；`contract.md` 本身不需改 |
| `design/phase-02-paint-foundation.md` | §10.6「v1 不做 LRU 逐出」                                                      | 与 F-147 一致（本阶段把「不做」的范围从分配器扩到 `ResourceCache`，并给出前置条件）                                                                            |
| `design/phase-05-text.md`             | §9.2 `AtlasAllocator` 的 `deallocate` 幂等守卫                                 | 实现从 `#[cfg(test)]` 提升为生产（F-147），行为不变                                                                                                            |
| `design/phase-08-d3d12.md`            | §7.7 `SnapshotDiff` / `SnapshotTolerance` 提案                                 | 本阶段落地为 `SnapshotDiff` / `Tolerance`（字段名按 §9.2）；**数值仍不冻结**（D19）                                                                            |
| `design/phase-12-macos-linux.md`      | F-137 Linux `new_offscreen` 返回 `UnsupportedGpu`                              | 是 `skip_reason()` 的 Linux 依据（§9.6）                                                                                                                       |
| `design/DEVIATIONS.md`                | M-09 / S-85 / V-22 / F-11 / F-24                                               | 本阶段的 target 形态、整页上传、文档债、`usvg` 归属、`AtlasTile` 路径都沿用，不重开                                                                            |

## 附录 B · 关键签名速查

```rust
// resources.rs
pub struct ResourceId;                                    // Copy, 8 B
pub struct ResourceImage {
    pub id: ResourceId,
    pub tile: AtlasTile,
    pub width: u32,
    pub height: u32
}
pub enum DecodeCause {
    UnsupportedFormat { format },
    Corrupt { format, detail },
    TooLarge { width, height, limit },
    Capacity { width, height, page }
}
pub struct ResourceCache;
impl ResourceCache {
    pub fn insert(&mut self, bytes: &[u8]) -> Result<ResourceImage, EngineError>;
    pub fn unload(&mut self, id: ResourceId) -> bool;
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
}

// error.rs
EngineError::ResourceDecode { cause: DecodeCause }

// app.rs
impl App {
    pub fn resources(&self) -> &ResourceCache;
    pub fn resources_mut(&mut self) -> &mut ResourceCache;
}

// test_support/snapshot.rs
pub struct SnapshotCase {
    pub scene: SnapshotScene,
    pub theme: SnapshotTheme,
    pub size: Size,
    pub scale: Scale
}
pub struct Tolerance {
    pub per_channel: u8,
    pub max_over_tolerance_pixels: u32
}
pub struct SnapshotDiff {
    pub max_channel_diff: u8,
    pub avg_channel_diff: f32,
    pub over_tolerance_pixels: u32,
    pub compared_pixels: u32,
    pub first_over_tolerance: Option<(u32, u32)>
}
pub fn compare(&SnapshotImage, &SnapshotImage, Tolerance) -> Result<SnapshotDiff, GoldenError>;
pub fn capture(SnapshotCase) -> Result<SnapshotImage, GoldenError>;
pub fn assert_golden(SnapshotCase, Tolerance) -> Result<SnapshotDiff, GoldenError>;
pub fn write_golden(SnapshotCase, &SnapshotImage) -> Result<(), GoldenError>;
pub fn skip_reason() -> Option<&'static str>;
pub fn gpu_guard() -> std::sync::MutexGuard<'static, ()>;

// test_support/diagnostics.rs
pub fn frame_dump(&TestApp) -> FrameDump;
pub fn dependency_dump(&App) -> DependencyGraphDump;
pub fn a11y_dump(&A11yTreeSnapshot) -> A11yTreeDump;
pub struct DamageOverlay {
    pub rects: Vec<PhysicalRect>
}
```
