# 绘制与共享渲染审查

> 审查对象：`crates/ui_core/src/paint/`、`crates/ui_core/src/render/` 共享层与 `render/shaders/`（HEAD `a9a4220`）。只读审查，未修改源码。
> 基线：`cargo test -p ui_core --lib` 通过（751 passed）。本文只记录已从代码路径核实的问题，按对 Kytrix「高频行情、几十万图元、局部刷新、多窗口」目标的影响排序。

## 总体判断

数据契约的「单一种类、单一转换点」方向是对的：`Primitive`/`GpuPrimitive` 有静态尺寸断言，颜色统一到 linear premultiplied，`DamageAccumulator` 的合并不动点、`RetireQueue` 的帧序号分组、`LatestFrame` 的最新者胜都实现得比较克制、可测。但绘制层目前更像「正确性原型」，还不是能支撑目标负载的渲染架构：

- **局部刷新没有真正减少 GPU 工作**：damage 只变成 scissor；每个 dirty 矩形都会重新执行整帧全部 draw op，且每帧仍全量重建、复制、上传 CPU 实例缓冲。
- **自定义几何通道尚未闭环**：`Path.fill` 与 `GeometryDraw.transform` 在编码中丢失，macOS/Linux 遇到 geometry 直接返回错误，生产构建也没有 `GeometryStore` 可铸造 `GeometryId`。K 线/深度图的路径与静态网格方案当前不可用。
- **三平台存在直接可证的 Vulkan 实例绘制参数错误**：`cmd_draw` 把实例起点传成了 `firstVertex`，任何 `start > 0` 的实例段都会读错实例并生成错误 quad。
- 颜色、图集、裁剪还有若干会在长会话、多窗口、缩放场景下暴露的正确性问题；已有测试覆盖不足（无 10 万级图元基准、无跨窗口 DPI/图集换代回归）。

结论：**不建议按当前状态作为 GPUI 的替代品上线**；先修 P0 的 Vulkan 实例参数与 geometry 通道，再决定 damage 是走「逐批次裁剪 + 保留式缓冲」还是改为 tile/retained command list，否则高帧率目标无法达成。

## 发现（P0-P3）

### P0-1 Linux Vulkan 实例绘制参数错位，`start > 0` 的实例段全部画错

**证据**
- `crates/ui_core/src/render/linux/compositor.rs:1432-1434`：`cmd_draw(command, 4, len, start, 0)`。`vkCmdDraw` 的参数顺序是 `(vertexCount, instanceCount, firstVertex, firstInstance)`，这里把 `start` 传给了 `firstVertex`、把 `firstInstance` 固定为 0。
- `crates/ui_core/src/paint/instance.rs:266-274`：`FrameOp::Instances.start` 的定义是实例缓冲的 `run_start`，不是顶点起点。
- `crates/ui_core/src/render/shaders/quad.vert.glsl:11-14`：顶点用 `instances[gl_InstanceIndex]`，并直接用 `gl_VertexIndex` 构造 `corner`；`firstVertex = start` 会让 `gl_VertexIndex` 从 `start` 开始，`corner` 可能大于 1，quad 直接退化/错位。

**问题**
任何被 `FrameOp::Geometry` 打断的实例段（`flush_run` 后第二段 `start > 0`）都会：① 从 `instances[0..len]` 读实例，渲染成前面的图元；② 顶点 ID 不从 0 开始，4 个角点不再是 `(0,0)/(1,0)/(0,1)/(1,1)`。第一段 `start == 0` 时恰好正常，所以只在包含 Path/Geometry 的复杂场景暴露，容易被简单测试漏掉。

**影响**
Linux/Vulkan 上 K 线、带路径图标的界面会出现随机图元、错位、缺块；Windows/macOS 正常，形成严重跨平台不一致。对「三平台目标替代 GPUI」是 P0 视觉正确性阻塞。

**成熟实现对照**
D3D12 的 `DrawInstanced(4, len, 0, start)`、Metal 的 `drawPrimitives(...baseInstance: start)` 都使用 StartInstanceLocation/baseInstance；Vulkan 对应物就是 `firstInstance`，不是 `firstVertex`。详见 `contract.md` §5/§7 的实例下标约定。

**建议**
改为 `cmd_draw(command, 4, len, 0, start)`；补一条 Linux 帧记录/像素回归：场景含 `Quad → Geometry → Quad`，断言第二段实例实际取自 `start..start+len`。同时把 `FrameOp::Instances` 的 `start` 语义写进 `contract.md` 的一处权威定义，三平台共用。

**置信度**：高（参数顺序与数据语义均已在代码中逐项核对）。

---

### P0-2 Geometry/Path 通道丢失填充色与变换，生产构建无法构造，macOS/Linux 直接报错

**证据**
- `crates/ui_core/src/paint/instance.rs:401-413`：`Primitive::Path` 只把 `path.geometry` 和 `Transform::IDENTITY` 写入 `GeometryDraw`，`Path.fill` 完全没有读取。
- `crates/ui_core/src/paint/instance.rs:414-423`：`Primitive::Geometry` 把 `*draw` 放入 geometry 表，但 `FrameOp::Geometry`（同文件 `137-149`）只有 `index/clip_index/clip_rect`，不携带 `transform` 或颜色。
- `crates/ui_core/src/render/windows/compositor.rs:1773-1806`：`geometry_root` 把 `draw.color0` 硬编码为不透明白、`basis` 硬编码为单位矩阵、`translation` 为 0；`resolve` 只返回 `GeometryId`，拿不到 `GeometryDraw.transform`。
- `crates/ui_core/src/render/macos/compositor.rs:710-719`、`crates/ui_core/src/render/linux/compositor.rs:1437-1446`：任何 `FrameOp::Geometry` 都返回 `EngineError::GeometryRegister`，两个平台没有 geometry pipeline/缓冲。
- `crates/ui_core/src/paint/geometry.rs:34-41`：`GeometryId::for_test` 仅 `#[cfg(any(test, feature = "test-support"))]`；仓库没有 `GeometryStore`，生产应用无法取得 `GeometryId`，`Canvas::push_path`/`push_geometry` 实际不可用。

**问题**
`Path.fill` 是公开字段，但编码后被丢弃；`GeometryDraw.transform` 是公开契约，但 Windows 录制端完全不消费；macOS/Linux 把整帧打成错误；应用层没有合法方式创建 `GeometryId`。这不是「尚未优化」，而是 geometry 子系统的闭环缺失。

**影响**
K 线实体/面积图/深度图的路径填充只能得到白色（Windows）或整帧 `GeometryRegister` 错误（macOS/Linux）；静态几何无法着色或平移缩放。按目标业务（K 线、深度图）这属于功能阻塞，且会掩盖其他 layout/damage 问题。

**成熟实现对照**
Skia 的 `drawPath`/`drawVertices` 把 `SkPaint`（颜色）与当前矩阵一起提交；WebRender 的 display item 把 color/transform 放在同一绘制项中；GPUI 的 `Path` 绘制也携带 paint。主流实现不会把颜色留给一个「默认白」的常量。

**建议**
1. 把 `color0`（以及可选的 `GeometryDraw.transform`）加入 `FrameOp::Geometry` 或 geometry 表的可消费对象；`Path.fill` 在编码时 resolve 后写入。
2. 三个后端都实现 geometry pipeline（至少 Windows 已有的 Ui/Textured 两条），macOS/Linux 不要返回错误。
3. 增加 `GeometryStore`（或等价的公开铸出/释放 API），删除 `for_test` 口子；用代际校验 `GeometryRegistry::lookup`，避免槽位复用后旧 id 画新几何。
4. 将连续的 `FrameOp::Geometry` 合并为一次批量提交（同一 pipeline/state），否则 K 线每根蜡烛一个 draw call 仍不可行。

**置信度**：高（`Path.fill`/`transform` 在代码中均无读者，平台分支为显式 `Err`）。

---

### P1-1 Damage 只作为 scissor：每个 dirty 矩形重复执行全部 draw op

**证据**
- `crates/ui_core/src/render/windows/compositor.rs:1243-1262`：`Plan::Partial` 对 `damage.as_slice()` 每个矩形设置 scissor，然后调用一次 `draw_ops(payload.ops(), ...)`；`draw_ops` 内部对 `FrameOp::Instances` 执行 `DrawInstanced(4, len, 0, start)`，所以矩形数 N 就是 N 倍全部 draw op。
- `crates/ui_core/src/render/macos/compositor.rs:426-443`、`crates/ui_core/src/render/linux/compositor.rs:878-895` 同样逐 damage 矩形重放整份 op list。
- `crates/ui_core/src/render/stats.rs:3-10` 明确写着「局部 damage 会按 scissor 重复执行全部 draw op」；Windows 单测 `draw_calls_follow_full_and_partial_scissor_passes` 也断言 2 个 damage 矩形产生 2 次 draw。
- `crates/ui_core/src/render/compositor.rs:87-101`：最新帧槽合并被跳过帧的 damage 时只是把矩形**拼接**，既不 union 也不压缩，GPU 忙时会累积到 16 个矩形，下一帧就是 16 次全量 draw op。

**问题**
scissor 只减少片元着色，不减少顶点/实例处理；一个 16 脏矩形的帧会把整场景处理 16 遍。反过来，即使只有 1 个脏矩形，20 万实例仍全部进入顶点阶段，damage 在 GPU 上几乎不省顶点成本。

**影响**
高频行情下「一个 ticker 数字变化」可能伴随多个小矩形（文本、divider、tooltip），每帧 draw call/顶点工作量成倍增长；GPU 忙导致跳帧时又进一步累积 damage，形成越忙越慢的正反馈。对 200k 图元场景不可接受。

**成熟实现对照**
Chrome/cc 以 tile/picture layer 为单位重绘，只有与 damage 相交的 tile 重新 raster；Skia 的 `SkSurface`/picture 也按 clip/picture bounds 裁剪；WebRender 的 picture cache 把保留的 tile 作为 GPU 纹理，只重绘 dirty tile。没有成熟实现会把「整帧命令流」按每个 dirty rect 重放。

**建议**
- 短期：给每个 `FrameOp::Instances` 批次记录 AABB（min/max 合并所有实例），按 damage rect 做 CPU 端批次剔除；geometry 同理用 `GeometryDesc.bounds` + transform AABB。
- 中期：把 scene 切成 tile/retained command list，按 dirty tile 选择要重放的 op 子集；damage 矩形先在 UI 侧合并/去重，`Plan::merge` 复用 `DamageAccumulator` 的合并规则。
- 单测应覆盖「多 damage 矩形不导致 draw op 重放超过相交批次」而不是只断言调用次数。

**置信度**：高（三平台代码与 stats 注释、单测均证明当前行为）。

---

### P1-2 每帧全量重建/复制/上传实例缓冲，局部刷新只省了 lane 构造

**证据**
- `crates/ui_core/src/window.rs:1032`：只要本帧有任意 damage/节点重建，`paint_frame` 就调用 `self.encoder.encode(&self.scene, scale, &self.resolver)`，没有任何「只编码 dirty lane」的入口。
- `crates/ui_core/src/paint/instance.rs:236-263`：`encode` 先 `buffer.clear()`，再遍历 `scene.order()` 的全部 lane 和全部 primitive；`encode_scene` 见同文件 `351-366`。
- `crates/ui_core/src/render/frame.rs:72-88`：`FramePayload::from_buffer` 把整个实例缓冲 `extend_from_slice` 复制到跨线程 payload。
- `crates/ui_core/src/render/windows/compositor.rs:1420-1434`：每个 in-flight 槽把 `instances.len()` 全部字节再 `copy_nonoverlapping` 到 mapped upload buffer；`GpuPrimitive` 128 B（`paint/instance.rs:25-45`）。

**问题**
场景中一个节点变色，成本仍是 O(全部图元)：CPU 遍历 + 颜色 resolve + 128 B/实例写入 + 跨线程整块复制 + 上传整块复制。以 20 万 quads 粗算：实例缓冲 25.6 MB，每帧至少 2 次整块 memcpy（约 51 MB），再加上每个实例的 shader vertex fetch。lane 复用只避免了 `Primitive` 构建，没有避免实例编码和上传。

**影响**
静态图表每帧也在全量编码/上传；K 线/深度图几十万图元时 CPU 与内存带宽先于 GPU 成为瓶颈。这与「局部刷新」目标直接冲突，也会让 GPU 计时看起来正常但 UI 线程帧时间超标。

**成熟实现对照**
Skia/Graphite 的 GPU buffer 以 op/instance 的保留式缓冲或动态环形缓冲增量更新；WebRender 的 GPU cache 按 element 增量更新；Chrome 的 tile raster 只生成 dirty tile 的命令。目标替代物 GPUI 的批次也应尽量复用已编码 instance。

**建议**
- 让 `Scene`/`InstanceBuffer` 持有按 lane 的 stable offset；重建 lane 时只重写该 lane 的实例段，未变化段只更新 draw 范围。
- 为实例缓冲设计 GPU 持久/环形 buffer，`FramePayload` 只传 dirty byte ranges；跨线程别整块复制。
- 给 100k/200k 实例加基准（当前 benches 只有 512 node，无法证明目标负载）。

**置信度**：高（调用路径是唯一入口，未发现任何缓存分支）。

---

### P1-3 顶点 clip 夹紧破坏 clip 后 image/glyph 的 UV，局部裁剪会拉伸纹理

**证据**
- `crates/ui_core/src/render/shaders/quad.vert.glsl:19-34`：先用 `lo = max(lo, clip_rect.xy - 2)`、`hi = min(hi, clip_rect.xy + w + 2)` 夹紧 world quad，然后 `v_uv = lerp(p.uv.xy, p.uv.zw, corner)`；`corner` 仍是原始 unit quad 的 0/1，不随夹紧后的 world 位置变化。
- `quad.hlsl:19-35`、`quad.metal:24-40` 逐行等价，因此三平台都有同一缺陷。
- `crates/ui_core/src/render/shaders/contract.md:104-116` 把该夹紧称为「纯优化、不改变像素」，但只讨论了 SDF 覆盖率，未处理图集 UV。

**问题**
当 glyph/image 被部分裁剪（滚动视口、overflow 容器最常见）时，quad 的可见部分从 `clip.x` 开始，但 UV 仍从 `u0` 开始，纹理被压缩/拉伸到剩余可见区域。例：原 quad `[0,100]`、UV `[0,1]`，clip 左边界 `50`，夹紧后绘制 `[48,100]`、UV 仍 `[0,1]`，x=50 的片元采样 `u≈0.038` 而不是 `0.5`。完全裁掉的实例退化为零面积没问题，但部分裁剪会明显错图。

**影响**
滚动列表/裁剪容器边缘的文字和图标会横向缩放、错位，属于跨平台共有的视觉错误；K 线图表裁切时图例/图标也会被拉伸。

**成熟实现对照**
Skia/Chrome 的 clip 由 scissor、clip mask 或 `saveLayer` 完成，不会通过缩小几何而保持原 UV；若必须缩小几何，UV 要按 world 位置重新计算（例如 `uv = mix(p.uv.xy, p.uv.zw, (world_px - origin) / size)`）。

**建议**
- 保持 `v_local_px` 用真实 `bounds` 计算的前提下，把 `v_uv` 改为基于**未夹紧的原点/尺寸**按 `world_px` 求比例；或在图集/图元路径关闭顶点夹紧，仅依赖 `clip_mask` 与 scissor。
- 增加「部分裁剪的 image/glyph」像素回归：断言 clip 内外边缘采样没有缩放。
- 同一修正应用到 HLSL/Metal/GLSL 三份源文件，并更新 `contract.md` 的「纯优化」表述。

**置信度**：高（几何/UV 映射可直接推导，三份 shader 结构一致）。

---

### P1-4 每帧对每个图元重新 resolve 颜色，宽色域路径还包含非线性 gamut mapping

**证据**
- `crates/ui_core/src/paint/color.rs:262-288`：`ColorResolver::resolve` 每次都做 `normalized_dynamic` → `resolve_linear`/`gamut_mapped_linear`；`Oklab`/`DisplayP3` 还可能走 24 次 chroma 二分（`GAMUT_BISECTION_STEPS`）。
- `crates/ui_core/src/paint/instance.rs:531-552`：`emit_quad` 每个 Quad 都调用 `resolver.resolve(fill)` / `resolve(border)` / 两个渐变端点；`emit_glyph`、`emit_image`、`emit_shadow` 同样逐实例 resolve。
- 项目自己的风险记录（phase-06 R4/V-15）估算约 **0.3 ms / 5000 图元**；即 20 万图元约 12 ms/帧仅花在颜色 resolve 上（还与平台/颜色空间有关）。

**影响**
高频率、大量小图元（K 线每根蜡烛多个 quad）时，CPU 在编码阶段就被颜色解析占满；即使 GPU 空闲，UI 线程也可能超过 16.6 ms。宽色域主题色会放大成本。

**成熟实现对照**
Skia 的 `SkColor4f`/`SkPaint` 在绘制前已解析为工作色彩空间；WebRender/Servo 在 display list 构建时缓存颜色；Chrome 的 paint 也使用已解析的 `SkColor`。通常不会在每帧 GPU 编码循环里重复做色彩空间转换。

**建议**
- 在 `Primitive`/lane 中缓存 `LinearPremulRgba`（或按 `Color` 值做 resolver 缓存，注意 f32 位比较），编码只做平凡拷贝。
- 至少对同一 `PaintSpec` 的静态颜色做节点级缓存；先补 20 万实例的基准验证。
- 把 `ColorResolver::interpolate` 与 shader 渐变的空间语义在文档中分开（v1 渐变明确是 linear premultiplied，见 `DEVIATIONS` F-32）。

**置信度**：高（调用点确定，成本量级由项目自身基准估算支持）；具体数值需实测。

---

### P1-5 `Canvas` 是原子的 damage 单元：一个小改动可触发整块画布乃至全帧重绘

**证据**
- `crates/ui_core/src/paint/canvas.rs:34-38`：`Canvas` 只有一个 `bounds: Rect`，文档要求它必须是所有内部图元 bounds 的并集，damage/命中只用它。
- `crates/ui_core/src/paint/pass.rs:592-596`：`Content::Canvas` 展开后只 push `canvas.bounds()` 一次作为 damage；不展开内部图元的 bounds。
- `crates/ui_core/src/render/damage.rs:19-20,133-142`：damage 面积超过帧面积 50% 直接退化为 `Full`。

**问题**
K 线/深度图通常用一个覆盖整个图表的 `Canvas` 承载所有 candle/深度柱。任何一次数据 tick 或 crosshair 移动都让 `canvas.bounds` 覆盖整个图表；图表占窗口 >50% 时每帧直接 `Plan::Full`。即使小于 50%，也会把整个图表区域作为多个重绘 pass 的 scissor，而不是只画变化的最后一根 candle 或鼠标区域。

**影响**
高频行情/鼠标 hover 场景下局部刷新失效，和「大量图元 + 局部刷新」目标冲突；这是比通用 damage 算法更贴近业务的一处粒度问题。

**成熟实现对照**
Chrome/Skia 的 picture/tile 按内容 dirty rect 重绘；GPUI 的 element 有 layout bounds，对自绘元素通常支持显式 `invalidate` 区域；WebRender 的 picture cache 以 tile 为粒度。画布级 AABB 只适合作为保守上限，不适合作为唯一 damage 源。

**建议**
- 为 `Canvas` 增加显式 `dirty`/`damage` API（例如 `Canvas::push_dirty(Rect)` 或每类 `push_*` 自动并入 changed bounds），`PrimitiveEmitter` 用它而不是整个 `canvas.bounds()`。
- 或将大图表拆成 tile/分块 `Canvas`，按数据分区更新；把 crosshair 放到独立 overlay Canvas，避免主图表整块重绘。
- 在 damage 层面记录 canvas 内部图元 union，允许 `Canvas` 只报告变化子矩形。

**置信度**：高（damage 只用 `canvas.bounds()` 是代码事实；业务影响由目标场景推断）。

---

### P1-6 字形图集换代/扩容没有使所有窗口的文本 lane 失效，跨窗口 DPI 或容量逃逸会留下旧 UV

**证据**
- `crates/ui_core/src/render/atlas.rs:239-259`：`clear()` 使 `generation += 1`；`grow()` 先改 page 再 `clear()`，旧 tile 全部失效。
- `crates/ui_core/src/text/atlas.rs:138-159`：分配失败时循环 `grow()`，到 2048 仍失败则 `atlas.clear()` + `GlyphAtlas::clear()`，此后所有 glyph 重新光栅/重新装箱。
- `crates/ui_core/src/app.rs:2813-2816`：任一窗口 `scale_epoch` 变化就调用全局 `atlas.clear()` + `glyph_atlas.clear()`；图集是进程级共享，但该操作只发生在一个窗口的 `refresh_text` 里。
- `crates/ui_core/src/app.rs:2897-2947`：`layout_text` 以 `(width, generation)` 判断是否需要重排；generation 变化会重新 `set_text_line`。
- `crates/ui_core/src/node/tree.rs:1298-1315`：`set_text_line` 只更新 `TextSlot` 的 `line/laid_width_bits/laid_generation`，**不调用 `mark_paint`**。
- `crates/ui_core/src/paint/pass.rs:191-207`：paint 复用只依赖节点 `REUSABLE` + `clip_key`；文本节点若不脏就继续使用旧 lane 里的旧 `Primitive::GlyphRun`（旧 `AtlasTile` UV/generation）。

**问题**
图集 generation 是全局的，但文本 lane 的失效不是全局的。一个窗口 DPI 变化或某个窗口触发 glyph 容量 `clear` 后：其他窗口/其他文本节点在下一帧 `layout_text` 里会看到 generation 变化并重建 `ShapedLine`，但 `set_text_line` 不标 PAINT，paint pass 继续复用旧 lane，旧的 UV 指向已重排的图集页。结果可能是错字、花字或采样到其他字形。即使当前窗口，除触发重建的文本节点外也可能出现同样问题。

**影响**
多窗口 Kytrix（目标明确要求多窗口）在一个窗口改变 DPI/缩放、或长会话中文/emoji 触发图集扩容后，其他窗口出现文本错乱，且要等一次无关的 PAINT 事件才可能恢复。

**成熟实现对照**
Skia 的 glyph atlas/strike cache 在换代时会让所有使用旧 strike 的文本重新生成绘制数据；Chrome 的 font cache 按字体/尺度隔离，不会让一个窗口的 scale 变化静默复用旧图集 UV。多窗口渲染器通常使用每窗口 glyph atlas，或全局 generation 驱动所有窗口的文本重绘。

**建议**
- 在 atlas generation 变化时，遍历所有窗口，对所有文本节点 `mark_paint`（并清 `REUSABLE`）；或者把 atlas generation 写入 `TextSlot` 并在 paint 的复用判断里比较。
- 更彻底：按窗口/scale 维护独立 glyph atlas（共享字形缓存但各自 UV 页），避免一个窗口 DPI 变化影响其他窗口。
- 加回归：两个窗口不同 scale；窗口 A 触发 `grow/clear` 后窗口 B 的文本帧记录/像素仍正确。

**置信度**：中高（代码路径完整；实际是否触发取决于窗口是否进入 `layout_text`，但「set_text_line 不标 PAINT」是确定的）。

---

### P1-7 图集单页且无淘汰：长会话字形耗尽会全清重光栅；图片页被首个大图永久锁档

**证据**
- `crates/ui_core/src/render/atlas.rs:19-30`：每个 `AtlasKind` 单页，最大 2048²（Glyph R8 4 MiB、Image BGRA8 16 MiB）；无页列表/无 LRU。
- `crates/ui_core/src/text/atlas.rs:139-159`：glyph 分配失败时按 512→1024→2048 扩容；到顶后 `atlas.clear()` 再重试一次；`GlyphAtlas::clear()` 清空所有记录，之后所有文本重新光栅、装箱。
- `crates/ui_core/src/resources.rs:198-221`：图片页首次 `insert` 时按第一张图选档并**锁定**；后续更大图直接 `Capacity` 拒绝（`page_for_tile` 见 `render/atlas.rs:42-49`）。
- `crates/ui_core/src/render/atlas.rs:262-274` 的 `mark_written`/`DirtyRows` 只处理行区间，没有淘汰策略。

**问题**
Glyph 缓存只增不减（`GlyphAtlas::records`/`ids` 不主动淘汰）；中文字体/多字号/长会话可能超过 2048² 的 R8 容量，触发全页 `clear` + 全部重光栅，且如果工作集本身大于容量会周期性抖动。Image 页则可能因一张 2046×2046 图把整页锁在 2048²，之后小图无法装入，或者第一张 16×16 图锁定 512² 导致后续大图失败。

**影响**
长会话中文行情界面可能出现「打字/翻页时文本整体卡顿、闪烁」；多窗口下每个窗口都有一份 16 MiB 图片纹理 + 4 MiB 字形纹理，内存放大。对目标「复杂图表 + 多窗口」属于容量风险。

**成熟实现对照**
Skia 的 `SkStrikeCache`/`SkGlyph` 有多页/淘汰与 purge；WebRender 的 `TextureCache` 有 LRU 逐出和页分配；Qt 的 glyph cache 也按大小/使用频率管理。单页无淘汰只适合小图标集，不适合长期运行的中文 UI。

**建议**
- 实现多页 glyph atlas + LRU（或至少按使用频率清理不活跃字形），页满时先淘汰再扩容，避免全清。
- 图片图集按尺寸分桶（小图页、大图页）或允许多页；`ResourceImage` 携带 page 字段，`AtlasTile` 增加 page；更新 `GpuPrimitive.params1.y` 的 texture_index 语义（需三平台同步，属契约变更）。
- 给「大量 CJK 字形 + 多字号 + 多窗口」做容量基准；把 glyph record 的 `ids`/`records` 生命周期纳入诊断。

**置信度**：高（单页和 clear/retry 代码明确）；实际触发阈值依赖字体/文本规模，需以产品字体实测。

---

### P2-1 `StylePatch.opacity` 是逐图元 alpha，不是组透明度；Canvas/Geometry 内容完全不参与

**证据**
- `crates/ui_core/src/paint/style.rs:118-120`：`opacity` 文档写「整体不透明度（在预乘空间缩放）」。
- `crates/ui_core/src/paint/pass.rs:438-444`：先算出 `opacity`；`455-542` 分别乘到 shadow/background/border/gradient 的 `Color` 上。
- `crates/ui_core/src/paint/pass.rs:579-604`：`emit_content` 处理 `Canvas`/`Geometry` 时没有 `opacity` 参数，也没有对 `Rc<Canvas>` 内的图元做任何 alpha 缩放；`Primitive::Canvas` 自身也没有 opacity 字段。
- 同一节点背景 + 边框是两个 Quad 时，各自乘 opacity 再做 premultiplied over，边框区域会出现二次混合，视觉 alpha 高于 0.5。

**问题**
「整体 opacity」在实现上是每个 primitive 各自 alpha；重叠区域叠加错误，不能表达 group opacity。更严重的是自绘画布是 K 线/图表的主路径，`opacity: 0.5` 对它完全无效，淡入淡出动画在图表上会突然出现/消失。

**影响**
禁用态、hover 淡入、面板整体淡出等常见 UI 效果在含 Canvas/多图元节点上视觉错误；若设计/产品依赖 `StylePatch.opacity` 做动画，会出现闪烁或突变。

**成熟实现对照**
Chrome/Skia 用 `saveLayerAlpha`/compositing layer 实现组透明度；Qt Quick 的 `opacity` 作用在 item 子树（必要时离屏合成）；WebRender 的 stacking context opacity 也是先合成再乘 alpha。逐 primitive 乘 alpha 只在单图元且无重叠时等价。

**建议**
- 短期在文档/API 明确「逐图元 alpha，不保证组语义」并禁止 Canvas 使用；或为 `Canvas` 增加 `opacity` 字段，编码时对展开的每个颜色做预乘缩放（只对不重叠内容近似正确）。
- 中期增加离屏 group layer/opacity 合成通道；至少对含 Canvas/Geometry 的节点支持 `saveLayer` 等价物。
- 增加「两个半透明重叠子图元 + opacity 0.5」的像素回归。

**置信度**：高（代码路径明确）；是否作为 v1 可接受限制需产品决定（DEVIATIONS V-51 已登记类似限制，但当前文档仍称「整体」）。

---

### P2-2 任意 layout 变化会把祖先全部标脏并产生根节点全幅 damage

**证据**
- `crates/ui_core/src/node/tree.rs:1877-1886`：`touch` 对 `LAYOUT|PAINT` 清除 `REUSABLE` 并 `accumulate_damage()`（`prev_layout ∪ layout`）。
- `crates/ui_core/src/node/tree.rs:1894-1923`：`bubble_layout` 沿父链 `touch(parent, DirtyFlags::LAYOUT)`，直到父不依赖子尺寸；flex 根通常一路到 root。
- `crates/ui_core/src/paint/pass.rs:187-210`：`reusable` 要求 `REUSABLE`；祖先被清后全部重建 lane。`PaintPass` 对根背景 quad 的 `damage.push(self.bounds, ...)`（`pass.rs:528-540`）会把整个根布局盒并入 damage。
- `crates/ui_core/src/render/damage.rs:137-142`：总面积 >50% 直接 `Full`。

**问题**
任意一个子节点尺寸变化（文本宽度、价格位数、面板拖动）都会沿父链清 `REUSABLE`，根背景重新生成并产生整窗 damage；即使根背景没变、只有一个小文本盒移动，也会全帧重绘。高频价格文本若宽度变化，会每 tick 触发全帧。

**影响**
布局相关的更新（尤其数字宽度变化、状态标签宽度变化）会把局部刷新退化为全量；GPU/CPU 成本随窗口面积而非变化面积增长。

**成熟实现对照**
Chrome 的 paint invalidation 记录 old/new paint rect，父层背景只在受影响区域重绘；WebRender 的 picture cache tile 只重绘 dirty tile；Qt Quick 的 `update()` 也只标记 item 的 dirty region。

**建议**
- `bubble_layout` 只标 `LAYOUT`，不要在祖先上 `accumulate_damage` 整个布局盒；用子节点 old/new absolute bounds 的并集作为 damage。
- `paint pass` 对仅有 `LAYOUT` 的祖先，若自身 `PaintSpec` 和 bounds 未变，尝试复用其 lane（需要把「内容变化」与「仅布局传播」分开的脏位）。
- 给「文本宽度变化」和「分栏拖动」加 damage 面积回归。

**置信度**：高（dirty 传播与 damage 代码直接对应）；实际是否全帧取决于根是否有背景和面积阈值。

---

### P2-3 `Canvas` 的 bounds 完全由应用手工维护，错误即静默 under-damage

**证据**
- `crates/ui_core/src/paint/canvas.rs:34-38`：`bounds` 契约是「必须 ⊇ 所有内部图元 bounds 并集（damage/命中只用它）」。
- `crates/ui_core/src/paint/canvas.rs:53-98`：`push_quad/push_text/push_image/push_path/push_shadow/push_geometry` 都只 `lane.push(...)`，不更新 `self.bounds`，也没有 debug 断言包含关系。
- `crates/ui_core/src/paint/pass.rs:592-596`：`Canvas` 节点只按 `canvas.bounds()` 产生 damage，不展开内部图元 union。

**问题**
自定义图表/自绘控件常见的用法是先用布局盒或固定 `Rect` 建 Canvas，再 push 许多图元；一旦 bounds 少算（例如阴影外扩、抗锯齿边缘、渐变/边框），damage 就小于实际像素，旧内容会残留在画面上。`Canvas` 是公开写入口，这个错误完全静默。

**影响**
K 线/深度图等自绘图表可能出现鼠标移动残影、缩放后旧柱子残留，且很难定位；比单纯性能问题更难查。

**成熟实现对照**
Skia `SkPicture` 的 cull rect 通常由记录器计算/校验；GPUI 的 element bounds 来自布局树；WebRender 的 display list 有整体 bounds。公开绘制容器一般不会让应用手填一个 must-contain 的 AABB。

**建议**
- `Canvas` 在每次 `push_*` 时自动 union primitive bounds（至少 debug 构建校验并在 release 用 debug_assert 采样）。
- 为 `Canvas` 增加 `bounds_mut`/显式 `set_bounds` 或让 `paint pass` 展开内部图元 damage；提供「empty canvas + push」自动推导 bounds 的构造函数。
- 加「元素超出 Canvas bounds」的像素/帧记录测试。

**置信度**：高（API 与 damage 代码事实）；实际影响取决于应用是否正确维护 bounds，但契约本身是高风险 footgun。

## 做得好的地方

- **字节级契约意识强**：`GpuPrimitive` 的 128 B/16 B 布局、`GpuClipEntry` 48 B、`RootUniform` 112 B 都有 `offset_of!`/`size_of!` 静态断言；`contract.md` 与 Rust 常量、三份 shader 有 build.rs 锚点校验，三平台字段顺序基本一致。
- **颜色基础扎实**：`ColorResolver` 集中处理 sRGB/linear/P3/Oklab/Oklch、alpha clamp、预乘和 gamut mapping；shader 端不做 gamma/预乘，目标格式 sRGB 作为唯一编码点；Windows persist 的 UNORM + sRGB RTV、macOS/Linux 的 sRGB attachment 都是正确方向。
- **damage 算法本身实现干净**：物理像素域外扩、clip 求交、面积不增合并、确定性排序、16 个矩形/50% 面积退化为 Full，单测覆盖了边界、L 形、顺序无关；`DamageAccumulator` 容量复用。
- **生命周期与异步管线考虑周全**：`RetireQueue` 按帧序号延迟释放、`InFlightRing` 按 acquire 次数轮转、`LatestFrame` 合并被覆盖帧的 damage、三平台 GPU 线程有 sticky error/PanicGuard 设计，说明作者理解异步 GPU 生命周期。
- **lane/clip 增量模型有明确契约**：每节点一条 lane、clip 前缀自包含、`clip_key = (depth, covering)` 防 resize/重挂复用旧裁剪，`clear_lane` 保留容量，`REUSABLE` 复位点放在提交后，都是正确的方向。
- **测试覆盖广**：`cargo test -p ui_core --lib` 751 项通过；`paint/instance.rs` 覆盖 `FrameOp` 顺序、clip 去重、每个 kind 的字段映射；damage/atlas 都有独立单测。

## 存疑 / 需实测

1. **`FLAG_OPAQUE` 的语义**：`contract.md:57-65` 说 shader 不使用它、只供 P09 选无混合管线。但当前判据只要求 `color0.is_opaque()`，未排除圆角/AA 边缘；一旦真有 no-blend variant，圆角外 alpha=0 的像素会被写成黑色而不是保留 dst。建议 P09 实现前把判据收紧为「整数轴对齐矩形、无圆角、无 AA」或删除该 flag。
2. **`Plan::merge` 的 damage 膨胀**：`render/compositor.rs:87-101` 只拼接不合并，GPU 忙时多个跳过帧可堆到 16 个矩形；虽不改变正确性，但会放大 P1-1 的重复 draw op。建议按 `DamageAccumulator` 的合并规则处理。
3. **macOS/Linux 每帧整面 blit/copy**：`render/macos/compositor.rs:9`、`render/linux/compositor.rs:1` 设计上承认 present 目标跨帧内容无保证，damage 只省 draw 不省 copy；4K/高刷下带宽成本需实测（设计 V-91/R-7 已登记）。
4. **颜色 resolve 的具体成本**：phase-06 R4 估算 0.3 ms/5000 图元；当前没有 10 万/20 万实例的 `frame_full`/`frame_incremental` 基准（benches 只有 512 node），无法判断 P1-4 的真实严重度，建议先补基准再决定缓存策略。
5. **图集 UV 边约定与透明 padding**：`paint/atlas.rs:183-195` 和 `contract.md:185-193` 明确用「边」、padding 清零；`phase-02` 接受放大时最外 0.5 纹素有 ≤1 px 柔边。此点不是 bug，但若目标包含大幅背景图/图标缩放，建议验证视觉；如不接受，需在 UV 内缩与 1:1 采样之间做取舍。
6. **`StylePatch.background` 与静态 gradient 的优先级**：`paint/pass.rs:473-527` 中 gradient 存在时 `background` 被忽略（包括 patch 的 `background`）。若产品用 patch 做 hover/选中背景，gradient 组件上的动态态不会生效；需确认这是设计意图还是遗漏。
7. **`Clip::Bounds` 对节点自身内容的作用**：`paint/pass.rs:208-237` 在 `rebuild_lane` 之后才 push 自身 clip，只作用于子节点 lane；文本/Canvas 作为自身 `Content` 不会被自己的 `Clip::Bounds` 裁剪。若产品依赖「容器 clip 自己内部的 Canvas」，需要改为在 `emit_content` 外包一层 clip（或在文档明确限制）。
8. **`GeometryRegistry::lookup` 未校验 `GeometryId.generation`**：`render/windows/pipeline.rs:540-544` 只按 index 查表；等 P09 `GeometryStore` 接入槽位复用后，旧 `GeometryId` 可能取到新 geometry。属于生命周期待补项。
9. **`PaintPass` 的隐藏子树**：`paint/pass.rs:177-181,256-299` 对 `display:none` 子树每帧递归 `hide_subtree`，即使已隐藏、无 lane；在大隐藏面板 + 其他动画时会持续 O(隐藏子树) 遍历。建议增加「已隐藏已清」标记，只在首次清除时推 damage。
10. **`FramePayload` 的全局 buffer 池与多窗口**：`render/frame.rs:59-68` 用进程级 `Mutex<Vec<PayloadBuffers>>`，多窗口同时发布时可能争用；目前临界区小，需实测。
