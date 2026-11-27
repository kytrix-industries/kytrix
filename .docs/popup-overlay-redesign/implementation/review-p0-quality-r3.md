# P0 对抗性审查 第 3 轮（收口确认）

- 审查对象：worktree `dsh-1d175414-1a41`，HEAD `71a5a5cc` + 未提交 P0 改动（58 文件 + 未跟踪 `paint/extent.rs`，`git diff --stat` = 6331 insertions / 586 deletions；较第 2 轮 6038/585 增加约 293 行）
- 本轮范围：①复核 Q2-01 与第 2 轮全部结论；②复查 `canvas_content_extent` clip/range 重写与 gallery coverage floor；③复跑门禁
- **结论：Q2-01 已关闭（4 项数字逐一实测吻合）；代码/门禁本轮无 finding；仅新增 1 条 P3 文档归因口径（Q3-01，总 108 不变、一处命名差 1）。修掉该一行即全级别 0，可提交。**

---

## 1. Q2-01 复核（数字逐项实测）

| 项 | 文档值 | 本轮实测 | 判定 |
|---|---|---|---|
| `cargo test -p ui_core -p ui_kit` 合计 | 3554 passed / 2 ignored | **3554 passed / 2 ignored / 0 failed**（ui_core lib **2720**+1 ignored、frame_record 8、layout 4、snapshot 14、demo 39、ui_kit lib 769；口径与文档一致：不含 `cargo test` 的那次同 lib 单测过滤运行） | ✅ 完全一致 |
| `Canvas::new(` 构造点 / 文件数 | 132 / 41 | 机械计数（`(?<![A-Za-z_])Canvas::new\(`，`crates/**/*.rs`）**132 / 41** | ✅ 完全一致 |
| `pass.rs` 新增测试 | 19 | diff 内 `#[test]` 新增逐一枚举 = **19**（13 首轮 + 3 R-09/extent + 3 R2-01 clip/range） | ✅ |
| `removal_tests.rs` 新增测试 | 11 | 枚举 = **11** | ✅ |
| `test_support/pixels.rs` 新增测试 | 7 | 枚举 = **7**（5 + 滚动删除像素级 + Path 增量像素级） | ✅ |
| 文档落点 | — | 设计 §5.3:682、§10:884、§11.2:905/908/910、§11.3-7:923、§11.5:954，`p0-summary.md:10/21/26/36`，`kit-gallery.md:705` 均已同步新数字 | ✅ |

第 2 轮其余结论（Q-01…Q-16 复核表、性能/测试可靠性/残余风险各节）本轮无变化，未发现回退；第 2 轮唯一未关闭项即 Q2-01，现关闭。

---

## 2. 本轮新代码复查（针对 lead 指定的问题点）

### 2.1 `canvas_content_extent` clip/range 重写（`pass.rs:1444-1540`）

| 检查项 | 结论 | 证据 |
|---|---|---|
| 复杂度 | **单遍 O(画布图元总数)**（嵌套 canvas 递归时每个图元只访问一次，无重复遍历/无指数展开）；每个 primitve 最多一次 `intersect`（无 clip 时零成本） | `pass.rs:1452-1474`；`clipped_extent` 仅在 `clip.is_some()` 时求交 |
| `SmallVec` 容量 | `SmallVec<[Option<LogicalRect>; 8]>`（`pass.rs:1451`）内联容量 **8 = `scene::MAX_CLIP_DEPTH`**（`paint/scene.rs:7`）→ 合法场景零堆分配；非法更深链会溢出到堆，不 panic | `scene.rs:7` `MAX_CLIP_DEPTH: u8 = 8` |
| `ClipPop` 不平衡 | 多余 Pop：`saved.pop().flatten()` → `None`（clip 复位为无界），无 panic；方向上只会**放大** extent（保守），不会漏 damage。缺失 Pop：encoder 在 encode 期 `debug_assert_eq!(depth, entry_depth)` 报错，不会静默画错 | `pass.rs:1461-1463`；`instance.rs` canvas 递归 `debug_assert_eq!` |
| 非有限/零尺寸 clip | `LogicalRect::intersect` 对 non-convertible 返回 default(空) → extent 为空；与 encoder 行为一致（encoder 把非有限/零尺寸 clip 变成 `[x,y,0,0]`，即裁剪掉全部）→ 不会出现「画了但 extent 没算」 | `pass.rs:1454-1460`；`instance.rs::push_clip` 的 `rect=[x,y,0.0,0.0]` 分支 |
| 空 range / 越界 range | `geometry_drawn_bounds` 逐 chunk 用 `chunk_draw_range`（内部把 range 先 clamp 到 `index_count`，空交集返回 `None`）→ 空 range ≈ 空 extent、越界 range 等价于 clamp 后的实际绘制范围；与 encoder 的 `push_geometry_op`/`push_part` 同一函数、同构 | `pass.rs:1516-1540`；`paint/geometry.rs::chunk_draw_range`（clamp + TriangleList 对齐 + 块交集）；`instance.rs:1791 push_part` |
| 嵌套 canvas | 内层 extent 先按内层 clip 求交 → 平移 `inner.offset` → 再按外层 clip 求交；与 encoder 的「canvas 内 clip 在局部空间、offset 累加」顺序一致 | `pass.rs:1464-1468`；`instance.rs` 递归 `canvas_offset` |
| 是否可能**少算**（漏 damage） | 三条路径逐一排除：忽略的只有 `Text`（emit 前必被 `resolved_canvas` 解析/丢弃，`pass.rs:1815-1830` 先解析再求 extent）与 `Clip`（不产生像素）；圆角 clip 只取 rect（偏大）；Geometry 取整块 `meta.bounds`（偏大）；未命中 chunk 不计入 → 与实绘一致。**结论：只可能偏大（过度 damage），不会偏小** | `pass.rs:1815-1833`（先 resolve 后 extent）、`primitive_extent` 的兜底 |
| 是否引入每帧成本 | 仍只在 **Canvas lane 重建**时执行（lane 复用跳过）；相对实例编码同阶、常数 ≤1×；geometry 部分由 O(1) 变 O(chunks)（chunk 数 = ⌈index/4096⌉，量级个位数） | `pass.rs:1827`；实测无回归（§4） |
| 测试 | 3 条新增且非平凡：`a_canvas_clip_tightens_a_huge_geometry_extent_to_the_canvas_box`（10k×5k geometry + clip → extent 恰等于节点盒、`!damage_full`）、`a_geometry_draw_range_narrows_the_canvas_content_extent`（`ranged extent == chunk0`、`full extent == all`，精确等值）、`a_zoomed_map_style_canvas_redraw_stays_local`（20k×10k + scale(2) + clip → 首绘不整帧） | `pass.rs` 19 条中的 3 条 |

### 2.2 gallery coverage floor + 负向自检（`gallery.rs:2929-3070`、`:2785`）

| 检查项 | 结论 | 证据 |
|---|---|---|
| 会不会**假绿** | 不会。floor 是「额外更严」的断言（`compared ≥ fixture×30‰`）；残影判据仍是 `mismatched == 0`（逐像素、容差 span ≤ 2）；且同一阶段还要求注入故障后必须检测到 `mismatched ≥ 2`（负向自检）→ 检测器失效时门禁会红 | `gallery.rs:2960-2986` |
| 会不会**假红** | 仅当「旧浮层区域 ∪ 排除区」比较集缩小到 < 3% fixture 时才假红。当前实测 `compared 26104 / fixture 480000`（800×600@DPR1，`WindowOptions::default`）≈ **5.44%**，余量 **1.81×**；文档已记录（§11.3-9、§11.5、`kit-gallery.md:706`）。`first == last` 时自检直接返回 false（宁可红不可绿） | 文档值与我按几何复算同量级（≈2.7 万像素）；`pixels.rs`/`gallery.rs` 的 `first/last` 逻辑 |
| 是否依赖 fixture 尺寸 | **是**（`required = fixture 设备像素 × 30‰`，而 `compared` 由 popup 区域几何决定，不随 window 面积线性增长）→ 若把 gallery 窗口从 800×600 放大到 ≥ ~1000×750（DPR1）会假红；缩小或改 DPR（DPR 同比缩放两边）安全。**这是「失败方向为红」的标定耦合，非缺陷**，且已文档化；建议未来改 fixture 时同步重算 30‰ | `gallery.rs:2958-2959` + `WindowOptions::default` = 800×600 |
| 负向自检强度 | 用 `compare_region(baseline, baseline, …, tol=0)` 取比较集首/尾两点（确保分散），注入品红后要求 `mismatched ≥ 2`；`sampled_frames=45` 与 floor 保证比较集远大于 2 点 | `gallery.rs:3050-3070` |
| 108 条计数口径 | `checks` 只在 `record()` 内自增（`gallery.rs:2801`），无手工增量；18 条具名不变量与代码中无条件记录点一一对应（逐一核对） | 见 §3 Q3-01（唯一一处归因差 1） |

### 2.3 文档复查

- 新增口径（§11.3-13 的「Canvas 内部 clip 栈求交」、§11.3-17 的「clip + range 两道收紧」、§11.3-7 的统计口径说明、§11.5 的 coverage 数字）与代码一致；`push_part`（`instance.rs:1791`）引用真实存在；`PaintedExtent` 仍不计 lane 级 clip 的表述正确。
- **唯一发现的文档错误见 Q3-01**（108 分解的一处归因，总数仍正确）。

---

## 3. Findings

**P0 = 0，P1 = 0，P2 = 0，P3 = 1。**

### Q3-01（P3）108 分解里「45 changed-outside-damage」应为「44 changed-outside-damage + 1 full-replay」

- **位置（5 处同错）**：`design.md:925`（§11.3-9）、`design.md:958`（§11.5 门禁表）、`p0-summary.md:13`、`p0-summary.md:26`、`kit-gallery.md:705`；`design.md:911`（§11.2 P0-6 行）写作「45 采样帧逐帧」，不点名具体断言，属可接受表述。
- **证据（可推导，非猜测）**：
  1. `gallery.rs` 的 `observe`：`PixelFrame::Idle => return`（不计入），`PixelFrame::Full` 分支记录 `full-replay`，`PixelFrame::Incremental` 分支才记录 `changed-outside-damage`；`sampled_frames` 在函数末尾对**所有非 Idle 帧**自增。
  2. 实测输出 `sampled_frames=45` 且 `damage_full_frames=0`、门禁 PASS → 只有首帧是 `Full`（若后续出现 Full，`record(sampled_frames == 0, …)` 会记失败）。
  3. 因此 `changed-outside-damage = 45 − 1 = 44`，`full-replay = 1`；总数 44+1+20+20+3+2+18 = 108 与实测一致，仅仅是「45」被整体挂到了 `changed-outside-damage` 名下且漏列 `full-replay`。
- **影响**：文档计数口径精度（不影响门禁、代码或结论）。
- **修法**：把五处「45 changed-outside-damage」改为「44 changed-outside-damage + 1 full-replay（首帧整帧）」，或统一改为「45 条逐帧采样断言（44 changed-outside-damage + 1 full-replay）」。
- **是否阻塞提交**：不阻塞；改完即全级别 0。

---

## 4. 门禁与性能实测（本轮）

| 项 | 结果 |
|---|---|
| `cargo +nightly fmt --all -- --check` | EXIT=0 |
| `cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings` | EXIT=0，无 warning |
| `cargo test -p ui_core -p ui_kit` | EXIT=0，**3554 passed / 2 ignored / 0 failed** |
| `cargo run -p ui_kit --example gallery -- --pixel-check` | EXIT=0，`checks=108 sampled_frames=45 damage_full_frames=0`，`first_visible == stable == (496,91,300×375)`，`moved=(496,4,300×375)` |
| ui_core lib 测试二进制直接运行（排除 cargo/并发负载） | **7.13 s wall / 31.1 s CPU / 2720 用例**（第 2 轮 7.92 s / 34.5 s / 2717）→ 无回归 |
| `gallery.exe --pixel-check` 直接运行 | **2.91 s wall / 4.06 s CPU**（第 2 轮 3.05 s / 3.81 s）→ 无回归 |
| 新增代码规则扫描（全部新增行） | 注释 0、`#[allow]/#[expect]` 0、`#[path]` 0、`unsafe` 0、新依赖 0；`format!`/`to_string` 仅测试 1 处；生产路径无新增 `unwrap/expect/panic`（新函数只用 `unwrap_or*`/`Option` 组合） |

---

## 5. 残余风险（非缺陷，登记即可）

- **R1 coverage floor 与 fixture 耦合**：`required = fixture×30‰`，当前余量 1.81×，改 gallery 窗口尺寸/移动目标时需重算（假红方向，安全）。
- **R2 `primitive_extent` 忽略 `Text`**：当前调用点一定先 `resolved_canvas`（`pass.rs:1815`），所以成立；若未来新增「未解析画布求 extent」的调用点会静默少算 → 建议 P1 引用时加 `debug_assert` 或在函数契约里写明「只接受已解析画布」。
- **R3 其余 P0 残余**（真实窗口像素、跨滚动上下文 reparent、`bind_text` 首帧、独立 Geometry 升格 full、workspace 全量 `cargo test` 未跑）与第 2 轮 §6.2 一致，均已在设计 §11.3/§11.6 登记。

---

## 6. 结论

- **Q2-01：已关闭**（3554/2720、132/41、pass 19/removal 11/pixels 7 四项与实测逐项一致，文档 6 处落点已同步）。
- **本轮代码无 finding**：clip/range-aware `canvas_content_extent` 与 gallery coverage floor + 双点负向自检经代码级复核与门禁实测均成立；门禁四项全绿且无性能回归。
- **唯一 open 项是 1 条 P3 文档归因（Q3-01）**：108 分解里「45 changed-outside-damage」应写成「44 + 1 full-replay」。改掉这一行即 **全级别 0**。
- **是否可清零提交：可以。**（除上述一行文档措辞外，代码/门禁/规范/性能/测试可靠性各维度本轮均无 finding。）
