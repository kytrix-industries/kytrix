# 嵌套滚动路由 · 实施补充设计（Addendum）

| 项 | 值 |
|---|---|
| 状态 | DRAFT v2（2026-11-26 第二轮 AD1–AD4 裁决已并入；待用户终审；实施前不修改生产代码） |
| 日期 | 2026-11-26 |
| 父文档 | [`crates/ui_core/design/scroll-routing-design.md`](scroll-routing-design.md)（下称“主计划书”，行号均指该文件） |
| 来源 | 用户裁决 A1/A2/A3 + AD1–AD4；Chromium `main` 源码快照（`E:\kytrix-scratch\tmp-verify\input_handler.cc` 等，抓取日期见 `E:\kytrix-scratch\research\chrome.md:3`）；当前 HEAD 源码走查 |
| 作用域 | 只改 `crates/ui_core/design/` 下 Markdown；不写生产代码 |
| 结论 | ① 45° 规则按 Chromium 精确复刻（Line 单位按候选/目标各自的 `line_height` 换算，无全局换算）；② Table 横向滚动全局默认生效，内容宽 = Σ Fixed 宽 + Σ Flex 最小宽，`Column::flex` 三参数显式最小宽；③ 表头采用 **Table 内部 sticky header**（无镜像滚动槽），水平偏移唯一真源 = 表体 `List.offset.x`；④ 水平条/垂直条用 Table 局部 rail 留 4 px 尾隙，不重叠，不扩展 `ScrollbarOptions` |

> 本文件只写“与主计划书不同的增量与裁定”。凡主计划书未被本文件覆盖的条款（P0–P4 既有任务、E/K/G 既有用例、删除清单、门禁）继续有效，编号不变。

---

## 0. 覆盖关系（主计划书 → 本补充书）

| 主计划书位置 | 原默认 | 本补充书处置 |
|---|---|---|
| §3.5 不采纳第 3 条（`:205`“Chrome 45° 投影本批简化”） | 简化 | **被 A1 覆盖**：精确复刻 45° + 向量投影 |
| §3.1 对角 bullet（`:155`） | 只描述、不采纳 | **被 A1 覆盖**：采纳 |
| §4.5 消费算法（`:398-432`） | 单目标 + 自身启用轴整包夹紧 | **被 A1 覆盖**：在 `apply_to_target` 内插入 Chrome 的 `applied_delta` 计算 + 45°/投影 + 消费记账 |
| §4.3 “行单位：`drives_px = delta_lines * line_height`”（`:359`） | 按容器 `line_height` | **被 A1/本文 §2.5 明确**：目标选择时逐候选用各自 `line_height`；选定后用目标 `line_height` 消费 |
| §4.6“组件级横向滚动不在本批”（`:454`） | 不做 | **被 A2 覆盖**：Table 横向纳入本批（AD1 全局默认） |
| §7.1 E12（`:697`） | “单赢家、不拆轴、无投影” | **被 A1 扩写**：E12 保留行为断言，新增 E34–E38 覆盖投影数学与消费记账 |
| §9.2 O1（`:830`） | 简化、可不复刻 | **已裁决：精确复刻**（A1） |
| §9.2 O3（`:832`） | Table 横向不在本批 | **已裁决：纳入本批**（A2 + AD1） |
| §9.1 R13（`:824`） | Table 横向不在本批 | **被 A2 覆盖**：风险改由本文 §6 承接 |
| §2.2 缺陷 G（`:105`） | 双轴容器 | 范围扩大：Table 表体也是双轴容器（A2） |
| 附录 D（`:971-975`） | 实施偏差模板 | 不变；本补充书新增偏差也记入附录 D |

---

## 1. 用户裁决记录

### 1.1 第一轮（2026-11-26）

| 编号 | 裁决点 | 用户裁决 | 与主计划书默认的差异 |
|---|---|---|---|
| A1 | O1 对角规则 | **精确复刻 Chrome 的 45° 规则与向量投影**，不做“简化首个可消费” | 主计划书 §3.5/§4.5 的简化作废 |
| A2 | O3 Table 横向 | **Table 横向滚动纳入本批**；Table 内容宽度 = **所有 Fixed 列宽总和 + 所有 Flex 列最小宽度总和**；Flex 最小宽必须由构造点给出（A2 原文“默认 80 + 链式覆盖”已被 AD4 修订） | 主计划书 §4.6/§9.2-O3/R13 的“不在本批”作废 |
| A3 | gallery 演示 | **新增宽表演示**（窄视口出现水平条、宽视口不出现，表头与表体对齐） | 主计划书只有 ⑦ 双轴演示 |
| A4 | 其余裁决 | 主计划书 D1–D6、O2、O4–O10 不变（**D3 后由裁决 C 追加修订**：滚动动画能力删除，见下） | 无 |
| C | 滚动动画能力（D3 追加，用户选 C） | **删除 `scroll_to_animated` / `AnimationTarget::Scroll`（滚动动画能力整体删除）**；行滚轮与程序化滚动统一立即 `scroll_to` | 主计划 D3 原文“`scroll_to_animated` 仅保留给程序化/ensure-visible”作废 |

### 1.2 第二轮（2026-11-26，AD1–AD4，覆盖本文 v1 的对应草稿）

| 编号 | 裁决点 | 用户裁决 | 对本文 v1 的修订 |
|---|---|---|---|
| AD1 | Table 横向默认范围 | **对所有 Table 全局默认生效**；现有 10+ widget 行情表**在调用点显式给小的 min_width** 保持旧密度；**不新增 `TableOptions` 开关** | §3.2/§3.10 明确“无开关、逐调用点显式取值” |
| AD2 | 滚动条角落 | **不接受 4 px overlay 重叠**。改为 **Table 局部几何**：垂直条底部留 4 px 尾隙、水平条右侧留 4 px 尾隙，二者不重叠；**不扩展 `ScrollbarOptions` 公共 API** | 删除 v1 §3.8“接受重叠”与 §6.3-AD2；改为 §3.4/§3.8 的 rail 方案 |
| AD3 | 表头共享水平偏移 | 采用 **Table 内部 sticky header**（不改引擎级 sticky 布局）；表头垂直固定，水平偏移唯一真源 = 表体 `List.offset.x`，`Table::sync` 写入 header；header 仍做纵向代理 | **删除 v1 的“镜像 ScrollSlot + `a11y: false`”方案**；改写 §3.4/§3.6/§3.9/§3.11/§6.3；删除“备选 sticky 不在本批”表述 |
| AD4 | `Column::flex` 签名 | **`Column::flex(header, weight, min_width: LogicalPx)` 三参数**；**取消默认 80 与链式 `.min_width()`**；全部调用点显式给最小宽；`ColumnWidth::Flex { weight, min_width }` 保留 | §3.3/§3.10/§4/§7 全部改三参数；gallery 备注显式 80、分类显式 96 |

> 本补充书**不重新打开** D1–D6（主计划书）与 A4。A1 的“整段吞掉/投影”只作用于**已闩锁目标**的消费记账；不改变 D1“单目标闩锁、余量丢弃、不向祖先结转”。

> **编号说明**：第二轮裁决在本补充书内记作 **AD1–AD4**（避免与主计划书 D1–D6 冲突）；主设计书引用本补充书时同样使用 AD1–AD4，主计划书 D1–D6 原义保持不变。

---

## 2. O1：Chrome 45° 规则与向量投影的精确复刻（A1）

### 2.1 源码事实与调用链（Chromium `main` 快照）

```text
InputHandlerProxy::HandleGestureScrollBegin        input_handler_proxy.cc:1211
  └─ InputHandler::ScrollBegin                     input_handler.cc:136
       ├─ HitTestScrollNode                        input_handler.cc:1835
       ├─ FindNodeToLatch（用 delta hint 选目标）   input_handler.cc:2341 / 264
       │    └─ CanConsumeDelta                     input_handler.cc:2462
       └─ DidLatchToScroller                       input_handler.cc:2439
GSU（每个 tick）
InputHandlerProxy::HandleGestureScrollUpdate        input_handler_proxy.cc:1292
  └─ InputHandler::ScrollUpdate                    input_handler.cc:348
       ├─ ResolveScrollGranularityToPixels         input_handler.cc:1805
       ├─ 用像素覆盖 scroll_state.delta            input_handler.cc:402-404
       └─ ScrollLatchedScroller                    input_handler.cc:2195
            ├─ applied_delta = 实际偏移变化         input_handler.cc:2257-2279
            ├─ 45° 规则 + 向量投影                 input_handler.cc:2295-2309
            └─ scroll_state.ConsumeDelta           input_handler.cc:2314
```

关键前置事实：

- `ScrollBegin` **DCHECK 进入时 `delta_x == 0 && delta_y == 0`**（`input_handler.cc:136-139`）；方向信息只在 `delta_*_hint` 里。
- `ScrollStateData` 注释：“Delta hints are equal to deltas of the first gesture scroll update event in a scroll sequence and are used for hittesting.”（`cc/input/scroll_state_data.h:31-35`）。
- **符号约定**：`scroll_state.delta = -event.delta`（`input_handler_proxy.cc:114-137`；TRACE 也用 `-delta`，`:1296-1301`）⇒ Chrome 的 delta 与 offset 增加方向同向，`applied = new − old` 与 delta 同号（`:2257-2279`）；本引擎的 `InputEvent.delta` 是平台方向（与 offset 反向），路由内部需取负归一化（§2.4）。
- 鼠标滚轮在浏览器进程被翻译成 GSU；GSB 由首个**非零** GSU 合成（`components/input/mouse_wheel_event_queue.cc:238-264`、`:345-355`）；零 delta 不发 GSB（同文件 `:244-255`）。
- `FindNodeToLatch` 只在 `ScrollBegin` 调用一次（`input_handler.cc:264`），后续 GSU 直接进 `ScrollLatchedScroller`（`:421`）。
- `FindNodeToLatch` 的消费性判定按 `is_beginning()` 选 hint 或当前 delta（`input_handler.cc:2396-2399`）；`CanConsumeDelta` 内部同样（`:2465-2471`），且 **零向量直接返回 true**（`:2473-2474`）。
- 候选跳过规则：节点 **两轴都 `user_scrollable_* == false` 且允许链式传递**时 `continue`（`:2377-2385`）；`user_scrollable_*` 是 CSS/合成属性（`cc/trees/scroll_node.h:58-59`），不是“当前是否还有滚动范围”。
- 不可链式（`overscroll-behavior != auto`）时**仍闩锁该节点**并 `set_is_scroll_chain_cut`（`:2401-2410`）。本引擎不建模 `overscroll-behavior`（D5 未选），映射为“全部节点等价 `kAuto`”。

### 2.2 `ScrollLatchedScroller` 的精确顺序（`input_handler.cc:2195-2334`）

动画路径（`:2208-2253`）本引擎不采纳（D3 立即到位；**裁决 C 进一步删除滚动动画能力**）。非动画路径逐行：

```text
1. delta := (scroll_state.delta_x, scroll_state.delta_y)                     // :2201
   // ScrollUpdate 已把它换成“像素”（:386-404），并已做 rail（轴向锁定）。
2. applied_delta := 目标节点的实际偏移变化
   - 视口节点：Viewport::ScrollBy(...).consumed_delta                        // :2257-2274
   - 普通节点：ScrollSingleNode(scroll_node, delta, ...)                     // :2276-2278
     ScrollSingleNode 内部先 UserScrollableDelta（未启用轴清零，:2148→2704-2714）
     再按 limits 夹紧后返回 new_offset - old_offset                           // :2118-2140
3. overscroll_delta_for_main_thread_ += delta - applied_delta                 // :2281（45° 调整之前记账）
4. 若 |applied_delta.x| <= 0.1 且 |applied_delta.y| <= 0.1：return            // :2284-2293（kScrollEpsilon=0.1，:99）
   —— 直接返回，不进入 45° 分支；普通节点此时不 ConsumeDelta。
5. 仅当目标不是视口节点时：                                                   // :2295
   angle := SmallestAngleBetweenVectors(applied_delta, delta)                 // :2300
   if angle < 45.0:
       applied_delta := delta                                                // :2302（整段吞掉，含未滚动轴）
   else:
       applied_delta := ProjectVector(delta, applied_delta)                  // :2306（把请求 delta 投影到“已滚方向”）
   delta_applied_to_content := applied_delta                                 // :2308
6. scroll_state.set_caused_scroll(|delta_applied_to_content.x| > 0.1,
                                  |delta_applied_to_content.y| > 0.1)         // :2311-2313
7. scroll_state.ConsumeDelta(applied_delta.x, applied_delta.y)               // :2314
```

`ConsumeDelta` 语义（`cc/input/scroll_state.cc:19-25`）：`delta_x -= x; delta_y -= y; if (x || y) delta_consumed_for_scroll_sequence = true;`（无 epsilon）。

数学定义（`cc/base/math_util.cc`）：

- `SmallestAngleBetweenVectors(v1, v2)`（`:794-800`）：
  `dot = DotProduct(v1,v2) / |v1| / |v2|` → clamp `[-1,1]` → `RadToDeg(acos(dot))`，返回 `[0°,180°]`。
- `ProjectVector(source, destination)`（`:802-808`）：
  `k = DotProduct(source,destination) / |destination|²`；返回 `k * destination`。

单位换算（`input_handler.cc:1805-1833`）：

- `kScrollByPage`：`kMinFractionToStepWhenPaging = 0.875` + `ResolveScrollPercentageToPixels`（`cc/input/scroll_utils.h:22`）。
- `kScrollByLine`：两轴都乘 `LineStep()`（`:1828-1830`）。
- `LineStep() = kPixelsPerLineStep(40) * painted_device_scale_factor`（`:1769-1771`、`cc/input/scroll_utils.h:20`）。
- **事实差异**：鼠标滚轮进入合成器前，浏览器已把 delta 单位限制为 `{page, precise-pixel, pixel}`（`mouse_wheel_event_queue.cc:180-188`），所以 Chrome 鼠标滚轮不经过 `kScrollByLine`；本引擎 Windows 滚轮是 `ScrollUnit::Line`（主计划书 `:68`），行→像素换算由本引擎定义（§2.5）。

### 2.3 边界与分支矩阵（源码推导）

| 情形 | Chromium 行为 | 依据 | 本引擎映射 |
|---|---|---|---|
| `delta == (0,0)` | 走不到 45°：`applied` 必为 0 → `:2284` 早退；且 wheel 零 delta 不发 GSB | `:2284-2293`、`mouse_wheel_event_queue.cc:244-255` | 零 delta 不建立/不更换会话；只刷新既有会话时间戳（§4.4） |
| `applied == (0,0)` | `:2284` 早退，不做角度计算，普通节点不 `ConsumeDelta` | `:2284-2293` | 不滚、不消费、不出帧；会话保持原目标（D1/I5） |
| 单轴容器 + 对角 delta | `applied` 只有一轴；`angle < 45°` 时 `consumed = 整个请求 delta`；`>=45°` 时投影到已滚轴 | `:2300-2307` | 同左。**不得先把请求 delta 的未启用轴清零再算角度**（清零只发生在 `applied` 的计算里，`:2148`） |
| `angle == 45°` 精确边界 | 条件为严格 `< 45`（`:2300`），等于 45 走 `ProjectVector` | `:2299-2307` | 严格 `< 45.0`，等于 45 投影 |
| 双轴容器，两轴都能动 | `applied ≈ clamp(delta)`，角度≈0 → `<45` 整段吞掉 | `:2300-2302` | 同左 |
| 双轴容器，一轴到边界 | `applied` 只剩另一轴；角度大则投影，只消费可动分量 | `:2300-2307` | 同左 |
| 未启用轴 | `UserScrollableDelta` 清零后算 `applied`；`delta` 本身不清零，参与角度比较 | `:2148`、`:2704-2714`、`:2300` | 容器 `ScrollAxes` 不含该轴 ⇒ `applied` 该轴恒 0；请求 delta 保留原值参与角度 |
| delta 符号方向 | Chrome `delta` 与 offset 增加同向（`scroll_state.delta = -event.delta`，`input_handler_proxy.cc:114-137`），`applied = new - old` 同号 | `:2257-2279`、`input_handler_proxy.cc:114-137` | 本引擎 raw 与 offset 反向（`app/input.rs:652-660`）⇒ 路由内 `requested = -raw_converted`；`goal = offset + requested`；`applied/consumed` 用同一归一化空间 |
| 视口节点 | 45° 分支整体跳过（`:2295`） | `:2295` | 本引擎无视口/浏览器控件二分：所有 `ScrollSlot` 走普通节点分支。**映射决定，不是未验证** |
| `applied` 非零但 `delta` 为零 | 不可达（`applied` 由 `delta` 派生） | `:2257-2279` | 输入校验后不可达；纯函数仍定义 `angle=0 → 吞掉` |
| 零向量数学 | `SmallestAngleBetweenVectors` 无零向量 DCHECK（`:794-800`），得 NaN；`ProjectVector` 分母无保护（`:802-808`）。但两处都在 `:2286` 早退之后 | `:794-808`、`:2284-2293` | 纯函数显式定义：任一向量为零向量时 `angle = 0`、`project = source`（仅定义早退后的可达集） |
| 45° 是否改变偏移 | **不改变**。`applied_delta = delta` 只影响 `caused_scroll_*`/`ConsumeDelta`/overscroll 记账；偏移在 `:2257-2279` 已写完 | `:2257-2314` | 偏移只应用 `applied`；`consumed` 单独记账（§2.7-F） |
| 余量是否结转 | 不结转：`ConsumeDelta` 后的残余只用于 overscroll/指标，不回到祖先 | `:2281`、`:2314`；`chrome.md §2.3` | 与 D1/I4 一致：余量丢弃 |

> **澄清（源码事实）**：Chrome 的 45° 不决定“对角分量分给哪个祖先”（祖先拿不到分量，`chrome.md:52-55`），它决定**已闩锁节点“认领多少请求量”**（`ConsumeDelta`、caused-scroll、overscroll 记账）。本引擎 D1/I4 下偏移恒等于 `applied`，因此 45° 的唯一可观测产物是消费记账；本批仍按裁决精确实现并测试。

### 2.4 本引擎算法与 Line 单位换算（无全局换算，含符号归一化）

数据模型事实：`ScrollSlot.line_height` 是**每个槽自己的**字段（`interaction.rs:204-210`），`ScrollConfig.line_height` 默认 48（`interaction.rs:165-172`）；主计划书 §4.3 的 `can_consume` 也明确对每个候选用自己的 `line_height` 换算（`:359`）。因此**不存在“目标选择前的一次全局换算”**。

**符号契约（A1 复审修正）**：

- Chrome：`ScrollState.delta = -event.delta`（`input_handler_proxy.cc:114-137`；TRACE 打印也用 `-delta`，`:1296-1301`），因此 **delta 与 offset 增加方向同向**，`applied_delta = new_offset - old_offset` 与 `delta` 同号（`input_handler.cc:2257-2279`）。45° 角度比较的就是这两个同号向量。
- 本引擎：`InputEvent::Scroll.delta` 是平台滚轮方向（Windows 下滚为负），现状 `on_scroll` 用 `after = before - step*drives`（`app/input.rs:652-660`）⇒ **raw delta 与 offset 增加方向反向**。
- 因此路由内部必须归一化：`requested = -raw_converted`（Line 先乘该候选/目标的 `line_height`，再取负）；`goal = slot.offset + requested`；`applied = new_offset - slot.offset`；`consumed` 也在同一归一化符号空间里计算。
- **不改 `InputEvent.delta` 语义**：平台入口、`hit`、零 delta 判定、`Shift` 换轴都在 raw 空间完成；归一化只发生在路由内部（`request_px` 一步），不向公共 API 泄露负号约定。
- 该归一化与 Chrome 的 `-event.delta` 是同一动作（本引擎的 raw 即 Chrome 的 event delta），因此角度语义与 Chrome 一致。

纯函数（`pub(crate)`，与目标/代理/会话同模块，不新建结构体层）：

```text
const EPS: f32 = 0.1;                                    // = Chrome kScrollEpsilon（input_handler.cc:99）

fn request_px(slot, raw, unit) -> LogicalPoint
    // 行→像素：两轴同乘 slot.line_height（O2 默认）；Pixel 原样；再取负归一化到 offset 增加方向
    lifted = match unit { Line => (raw.x * slot.line_height, raw.y * slot.line_height), Pixel => raw }
    return (0 - lifted.x, 0 - lifted.y)

fn applied_delta(slot, viewport, requested) -> LogicalPoint
    // 每启用轴: new = clamp(slot.offset + requested, 0, max_offset)；applied = new - slot.offset
    // 未启用轴: applied 恒 0（等价 Chrome UserScrollableDelta，input_handler.cc:2704-2714）
    // 等价 Chrome ComputeScrollDelta/ScrollSingleNode 的偏移变化（input_handler.cc:1996-2021、:2118-2140）

fn smallest_angle_between(v1, v2) -> f32                  // 公式同 math_util.cc:794-800；任一零向量 ⇒ 0
fn project_vector(source, destination) -> LogicalPoint    // 公式同 math_util.cc:802-808；destination 零向量 ⇒ source

struct ScrollApplication { applied: LogicalPoint, consumed: LogicalPoint, moved: bool }

fn apply_scroll(slot, viewport, requested) -> ScrollApplication
    applied := applied_delta(slot, viewport, requested)
    if !(|applied.x| > EPS || |applied.y| > EPS):
        return { applied, consumed: ZERO, moved: false }  // 对应 :2284-2293
    angle := smallest_angle_between(applied, requested)
    consumed := if angle < 45.0 { requested } else { project_vector(requested, applied) }
    return { applied, consumed, moved: true }
```

符号自检：raw 下滚（负）⇒ `requested` 为正 ⇒ `goal.offset` 增加 ⇒ `applied` 为正 ⇒ 与 Chrome 的“delta 正、applied 正”一致。E34 全部示例均按此契约（requested 为归一化后、offset 增加方向的 px）。

调用规则：

1. **Shift 换轴先于一切换算**（在原始单位上交换 `x/y`，与现状 `app/input.rs:597-601` 一致）。
2. **目标选择时逐候选换算+归一化**：对每个候选 `c` 调 `request_px(slot(c), raw_delta, unit)`，再用该值算 `applied` 做 `can_consume`；命中候选的换算结果即该次选择的 hint 版本。没有“先算出唯一 px delta”的步骤。
3. **选定目标后**：用 `request_px(target_slot, raw_delta, unit)` 得到本次请求（归一化 px），再 `apply_scroll` 得到 `applied/consumed`（§2.4）；写偏移用 `goal = offset + requested`。
4. 零 delta 判定用原始 delta（零乘任何 `line_height`、取负仍为零），不需要先知道目标。
5. `Line` 与 `Pixel` 只在 `request_px` 内部分支；45° 数学始终在同一目标的 px 空间里比较（requested 与 applied 同单位、同符号方向），角度不受 `line_height` 影响（同轴同比例缩放）。
6. 双轴共用同一个 `line_height`（主计划书 O2 默认，未裁决变更；见 §6.2）。

**与 Chrome 的差异（已核实，不是未验证）**：Chrome 的 GSU delta 已经是共享的 viewport 像素，因此所有候选看到同一个 delta；本引擎 `line_height` 挂在槽上，只能对每个候选用其自己的行高换算。直接后果：同一 `Line` 事件在两个行高不同的候选上会得到不同的 px 大小，选择结果可能与“先换算一次再选”不同；本引擎按“逐候选”语义执行（与主计划书 §4.3 的逐容器 `line_height` 完全一致），E34m/E34n 固化该语义。

### 2.5 Line 单位换算规则（写死）

1. `Line → px` 的唯一入口是 `request_px(slot, …)`；**不存在全局/路由级换算常量**。
2. `request_px` 同时完成 **行→像素** 与 **符号归一化**（`requested = -raw × line_height`，即“offset 增加方向”）；调用方不得再自行取负或相加。
3. 目标选择阶段：每个候选使用**自己的** `slot.line_height`（逐候选）。
4. 消费阶段：使用**闩锁目标自己的** `slot.line_height`（同一目标整段序列不变，D1）。
5. `Shift+wheel` 先换轴，再按目标/候选的 `line_height` 换算并归一化。
6. 两个轴共用同一 `line_height`（O2 默认）；因此 Line 输入在 45° 中不需要额外处理。
7. Chrome 参考事实：`kPixelsPerLineStep=40`（`scroll_utils.h:20`）、`LineStep()`（`:1769-1771`）只用于行单位 GSU；鼠标滚轮在浏览器层已转像素（`mouse_wheel_event_queue.cc:180-188`）。本引擎的每槽 `line_height` 是既有语义（`app/input.rs:652-660`、主计划书 §4.3），A1 不改它，只规定换算时机、归属与符号。
8. 用例约定：E34 的 requested 均为“归一化后、offset 增加方向”的目标 px；E34a 的 “3 行 × 48” 表示**目标 `line_height = 48` 且向下滚（raw 为负）**；涉及选择阶段的用例（E34m/E34n/E38）显式给各候选的 `line_height` 与 raw 方向。

### 2.6 目标选择：`CanConsumeDelta` 与“初始 delta/hint”语义

Chrome 语义：只在**序列开始**判定（`input_handler.cc:264`）；用**首个非零 GSU 的 delta 作 hint**（`scroll_state_data.h:31-35`、`input_handler_proxy.cc:78-87`）；“可消费”= 夹紧后偏移会变（`ComputeScrollDelta != 0`，`:2490-2495`，内部含 `LimitDeltaToScrollerSize :1996-2021`）；零 delta 的 `CanConsumeDelta` 返回 true（`:2473-2474`），但 wheel 零 delta 不产生 GSB，故不可达。

本引擎映射（候选链主计划书 §4.3 不变）：

```text
select_target(candidates, raw_delta, unit):
    对候选链自近到远：
        若 !tree.is_effectively_visible(c)（自身与全部祖先可见，含隐藏祖先）
           || !tree.is_effectively_interactive(c): continue      // 主计划书 §4.3-7；跳过并继续向上
        slot = scroll_slot(c)
        若 slot.axes == 无启用轴: continue                       // 对应 :2381-2385（两轴皆不可滚则跳过）
        requested = request_px(slot, raw_delta, unit)            // 用该候选自己的 line_height + 归一化符号
        applied = applied_delta(slot, viewport(c), requested)    // 不写 offset；applied = clamp(offset+requested)-offset
        若 |applied.x| > EPS || |applied.y| > EPS: return c        // 对应 CanConsumeDelta==true
    返回 None（滚轮丢弃；本引擎没有“回退到视口”的隐式目标）
```

- **候选链的连续代理**：`scroll_candidates` 支持 `A→B→C` 连续代理链（沿代理一直跟随到无代理/失效/环/hop 超限；中间节点不产出），见主计划书 §4.3 规则 2/4。
- **可见性**：命中路径已排除不可见节点；代理目标可能跨子树，故必须用 `is_effectively_visible`（含隐藏祖先）而不是 `is_visible_locally`；不满足的候选跳过并继续链，不终止。

- **hint = 序列首事件的原始 delta + unit**，由各候选按自己的 `line_height` 解释；本引擎没有独立 GSB，序列首事件同时充当 hint 与请求量（Chrome 的 GSB(hint)+首 GSU(delta) 合并为一次，因为两者 delta 相同）。
- 目标选定后，本事件再用 §2.4 的 `apply_scroll` 应用同一原始 delta（换算用目标行高）。
- 后续事件不重新选目标（D1）；即使 `applied` 变为 0 也只是本事件不滚（Chrome `:2284-2293`）。
- **不得**用 `can_consume` 的“整段请求量”去预清零未启用轴：清零只发生在 `applied` 内（等价 `UserScrollableDelta`，`:2148`）。

### 2.7 与主计划书 §4.5 的合并写法

```text
route_wheel(window, position, delta, unit, phase, modifiers, timestamp):
    hit = hit_at(position)（模态作用域过滤保持现状，app/input.rs:589-596）
    if hit is None:
        # 无命中：只做相位清理，绝不滚动闩锁目标
        按相位清理会话（Cancel/End(Discrete) 清会话；End(Continuous) 只收盘；
                        Begin 清旧会话；Update/Momentum 保持）
        return
    raw = shift_swap(delta, modifiers)            // Shift 先换轴（原始单位；不改 InputEvent.delta 语义）
    if raw == 0: 只做相位清理（含零 delta Begin 清会话、End 收盘），return
    session = session_for(window, phase, timestamp)                 // 主计划书 §4.4
    if session 的 target 已失效:
        if phase == End: 清会话; return                              # End 遇失效 target：只清会话、不消费
        session = None                                                # Discrete/Update/Momentum：当前事件重新仲裁并继续消费
    if phase == End && session is None:
        # 无会话 End：不建立新会话、不滚动（仅按收盘语义结束）
        return
    target = match session:
        Some(s) if s.target 存活 => s.target                        // D1/I5：不重选
        _ => select_target(scroll_candidates(hit.node), raw, unit)  // §2.6：逐候选换算+归一化
    if target is None: return
    slot = scroll_slot(target)
    requested = request_px(slot, raw, unit)                          // 目标行高 + 归一化（offset 增加方向）
    application = apply_scroll(slot, viewport(target), requested)    // §2.4
    if application.moved:
        goal = slot.offset + application.applied                     // 等价的 offset 加法；不重置另一轴
        scroll_to(handle(target), goal)                              // 一次写两轴
        refresh_hover_under_pointer(window)
    // consumed 只记账（见下），不参与 offset/帧/hover 决策
    if session newly created: store(session{target, ...})
```

**目标失效语义**：`Discrete`/`Update`/`Momentum` 检测到会话 target 失效时，**当前事件重新仲裁并处理本次 delta**（不丢事件）；`End` 遇失效 target **只清会话、不消费**。与主设计 §4.4/§4.10 一致；会话中的节点卸载不 panic、不访问悬空节点。

**`consumed` 的用途（本批）**：`ScrollApplication.consumed` **只供单测/未来 overscroll 记账**；本批**没有生产消费者**——不参与 `scroll_to` 的 goal、不影响 `requested_frame`/hover/a11y/damage，也不传给其它容器（D1/I4）。`requested - applied` 与 `consumed - applied` 同样不传给其它容器；不在 `InputOutcome`/公共 API 新增字段。

### 2.8 45° 与行高判定用例（纯函数级，确定性）

> 约定：下表的 **requested 均为 `request_px` 之后的归一化 px**（正 = offset 增加方向；raw 下滚为负，因此 E34a 的 raw 是 −3 行）。`EPS = 0.1` 与 Chrome 的 `kScrollEpsilon` 一致（`input_handler.cc:99`）；`|applied| <= 0.1` 视为未移动；判定用未取整 f32。

| ID | 输入 | 期望 |
|---|---|---|
| E34a | target `line_height = 48`，raw 向下 3 行 ⇒ requested (0,144)；offset y=0/max=500 | applied = clamp(0+144)−0 = (0,144)、angle 0°、consumed (0,144) |
| E34b | requested (12,3)；单轴纵、y max=500、offset y=0 | applied (0,3)、angle ≈75.96°、consumed = 投影 (0,3) |
| E34c | requested (2,3)；同上 | applied (0,3)、angle ≈33.69°、consumed (2,3) |
| E34d | requested (3,3)；同上 | angle 45.00°（严格边界）⇒ consumed = 投影 (0,3) |
| E34e | requested (0,0) | consumed (0,0)；无副作用 |
| E34f | requested (30,0)；单轴纵、max=500 | applied (0,0) ⇒ 早退、不投影、consumed (0,0) |
| E34g | requested (10,100)；双轴 x 在右边界（offset.x=max_x）、y 可动 | applied (0,100)、angle ≈5.71° ⇒ consumed (10,100) 整段吞掉 |
| E34h | requested (80,100)；同上 | angle ≈38.66° ⇒ consumed (80,100) |
| E34i | requested (120,100)；同上 | angle ≈50.19° ⇒ consumed = 投影 (0,100) |
| E34j | requested (0,100)；双轴都可动 | applied (0,100)、angle 0° ⇒ consumed (0,100) |
| E34k | requested (-100,10)；x 轴禁用、y 可动（y max=500） | applied (0,10)、angle ≈84.29°（>45）⇒ consumed = 投影 (0,10) |
| E34l | requested (5,0)；单轴纵、max=0 | applied (0,0) ⇒ 不进入 45°、consumed (0,0) |
| E34m | 候选 A（`line_height=20`，y max=500）、B（`line_height=48`，y max=500）；raw dy=−3 行（向下滚） | A 的 requested=(0,60) ⇒ 可消费 ⇒ 选 A；**不得**用全局 48 换算，也不得漏掉取负 |
| E34n | 候选 A（`line_height=20`，y max=0）、B（`line_height=48`，y max=500）；raw dy=−3 | A 的 (0,60) 不可消费 ⇒ 继续；B 用 48 换算 (0,144) 可消费 ⇒ 选 B |

> `EPS = 0.1` 与 Chrome 的 `kScrollEpsilon` 一致（`input_handler.cc:99`）；`|applied| <= 0.1` 视为未移动；判定用未取整 f32。

### 2.9 未验证/不可核实项（不作为事实）

| 编号 | 事项 | 状态 | 最小可行规则 |
|---|---|---|---|
| U1 | 本引擎是否需要 overscroll 记账（Chrome `:2281`） | 本引擎无 overscroll（D5 未选） | 不实现；只保留 `ScrollApplication` 的 `applied/consumed` |
| U2 | Chrome `LimitDeltaToScrollerSize`（`:1773-1799`）对 wheel 是否默认启用 | 快照内未核实特性默认值 | 不实现；只在大 delta 时影响 `CanConsumeDelta`，本批用例 delta 远小于容器尺寸 |
| U3 | `ConsumeDelta` 的 `x || y` 浮点真值判断（`scroll_state.cc:23`）是否完全照搬 | 事实已核实，映射选择 | 照搬：`x != 0 || y != 0` 即 `consumed_in_sequence = true` |
| U4 | 触控板 `rails_mode` 各平台取值路径（`chrome.md:113` 标未验证） | 未验证 | 本引擎只做 `Shift` 换轴，路由层不做轴锁定；O2 未裁决 |
| U5 | 逐候选 `line_height` 与 Chrome“共享 viewport px delta”的差异 | **本文映射决定（非未验证）** | E34m/E34n 固化；若未来要求与 Chrome 完全同构，需把 Line 换算移到入口并引入全局步长（与 O2 一起裁决） |

---

## 3. O3：Table 横向滚动（A2 + AD1–AD4）

### 3.1 现状与承载能力结论（源码证据）

- `Table` 表头与表体是兄弟子树：`outer → header(header_row…)` + `outer → body → List`（`ui_kit/src/kit/table.rs:463-497`、`:625-637`）；表体 `List` 的滚动槽在 `{key}:body:viewport`（`list.rs:583-599`、`app/virtual_list.rs:32-48`）。
- `List` 复用一个 `VirtualList` + 一个 `ScrollSlot`；虚拟列表在三处把 `scroll.content` 覆盖成 `(viewport.width, content_height)`（`app/virtual_list.rs:97-110`、`:247-249`、`:316-318`），行节点由 `set_virtual_row` 设为 `position:absolute; left:0; right:0; width:auto; height:N`（`node/tree/layout.rs:78-99`）。
- `ScrollBar` 是绝对定位的兄弟装饰节点（`scroll_bar.rs:199-221`）；`ScrollbarOptions` 只有一个四边同值的 `inset`（`:69-77`、`:106-110`），**无法表达“只留一侧尾隙”**。
- List 的自有滚动条是可选的：`build` 仅在 `options.scrollbar` 时构建（`list.rs:600-602`），`attach` 同（`:637-639`）。
- 引擎已有可用的运行时定位公共 API：`NodeTree::set_style`（`node/tree.rs:528-530`，实现 `node/tree/layout.rs:46-60`）、`Node::style()`（`node/tree.rs:206`）、`Style: Clone`（`paint/style.rs:19-20`）与 `Style::position/inset_left/inset_top`（`:56-59`、`:296-314`）。
- 绝对定位的包含块是**最近 positioned 祖先**（`layout/taffy_tree.rs:587-615` 的 `absolute_child_relative_to_positioned_ancestor`；`:618-649` 的包含块语义）。
- 结论：**现有 List 单轴实现无法直接承载 Table 横向**（行宽、content.width、ScrollAxes、水平偏移共享都缺），需要 §3.5 的最小重构；表头共享偏移采用 AD3 的 Table 内部 sticky 方案（§3.4），不新增引擎 sticky 原语。

### 3.2 宽度模型（A2 + AD1 + AD4）

```text
column_contribution(c) = match c.width {
    Fixed(w)                            => w,
    Flex { min_width, .. }              => min_width,
}
table_content_width = Σ column_contribution(c) for c in columns
```

- `Column::flex(header, weight, min_width: LogicalPx)`：**最小宽由构造点显式给出**；无默认值、无链式覆盖（AD4）。
- 视口宽 ≥ 内容宽 ⇒ `max_x = 0`，不出现水平条；Flex 列按权重吃满剩余宽度。
- 视口宽 < 内容宽 ⇒ `max_x = content - viewport`，出现水平条；Flex 列停在各自 `min_width`。
- **零宽回退（兼容旧纵向布局）**：`content_width == 0`（全部 Flex 且 `min_width` 均为 0）时，Table **不启用横向**——内层 List `content_width = None`、`axes = Vertical`、行宽 = viewport、`max_x = 0`、不建/不显示 hbar；新增零宽回退用例覆盖（`content_width > 0` 才传 `Some`/Both，见 §3.6/§3.7）。
- span 列：跨列单元格宽 = 被跨列 `column_contribution` 之和，该跨列的权重参与剩余空间分配（结构同现状 `span_cell_style` 的 “fixed 和 + flex 权重和”，`table.rs:142-186`）。
- **AD1**：横向对所有 Table 默认生效，不新增 `TableOptions` 字段。既有 widget 行情表在各自调用点显式给小的 `min_width`（如 `px(0.0)` 保持旧“可压缩到 0”密度），逐点记录在 §3.10。

### 3.3 数据模型与 `Column` API 变更（AD4）

```rust
pub enum ColumnWidth {
    Fixed(f32),
    Flex { weight: f32, min_width: LogicalPx },   // AD4：结构保留
}

pub struct Column {
    pub header: SmolStr,
    pub header_binding: Option<TextValue>,
    pub width: ColumnWidth,
    pub align: ColumnAlign,
    pub sortable: bool,
    pub column_span: usize,
}
```

- `Column::fixed(header, width)`：不变（`Fixed(width.max(1.0))`），**没有 min_width 概念**。
- `Column::flex(header, weight, min_width: LogicalPx)`：`Flex { weight: weight.max(0.0), min_width: min_width.max(LogicalPx::ZERO) }`。**无默认 80、无 `.min_width()` 链式方法**（AD4；原 A2 草稿作废）。
- `ColumnWidth::weight()` 保留（Flex 权重，Fixed 为 0）；新增 `ColumnWidth::contribution() -> f32`（Fixed→宽，Flex→`min_width.value()`）。两个概念不混用一个方法。
- span 计算（重写 `span_cell_style`，`table.rs:142-186`）：`contribution = Σ contribution(covered)`、`flex_weight = Σ weight(covered)`；`flex_weight > 0` ⇒ `flex(flex_weight, 1.0, Dimension::length(contribution))` + `min_width(contribution)`；否则 `.width(px(contribution)).flex_none()`。`clamp_span` 不变（`:188-190`）。
- `TableInner` 增加 `content_width: LogicalPx`（构造时一次求和；当前无 `set_columns`/动态列宽 API，无需在 sync 重算）。
- `TableOptions`：**不新增字段**（AD1）。现有 `scrollbar: bool`（`table.rs:192-222`）同时控制水平/垂直两条 bar 是否构建。

### 3.4 节点结构与 sticky header（AD3）

```text
{key}:table                  outer（flex_col, size_full, min_*_0, overflow_hidden, relative）★
├── {key}:header             header（flex_none, width_full, height=header_height+1,
│                                    overflow_hidden(Clip::Bounds), relative）★
│   └── {key}:header:content header_content（absolute; top=0; left=-list.offset.x〔Table::sync 写入〕;
│                                           width=max(content_width, viewport.width)〔Table::sync 写入〕;
│                                           height=100%; flex_col）
│       ├── {key}:header:row header_row（flex_row, width_full, height=header_height）
│       │   └── 各表头单元格（span_cell_style，与表体同构）
│       └── divider（width_full, height=1）
├── {key}:body               body（flex_1, relative, width_full, min_width_0, min_height_0）★
│   ├── List body（viewport = ScrollSlot **Both**，Clip::Bounds；ListOptions.scrollbar = false）
│   └── {key}:vbar-rail      vbar_rail（absolute; top=0; right=0; bottom=0; width=4）★
│       └── {key}:body:scrollbar     垂直 ScrollBar（track 填满 rail）
└── {key}:hbar-rail          hbar_rail（absolute; left=0; right=4px; bottom=0; height=4）★
    └── {key}:scrollbar-h             水平 ScrollBar（track 填满 rail）
```

- **底部 4 px 带**：`options.scrollbar = true` 时 `outer` 设 `padding_bottom = 4 px`（水平条厚度，构建期已知）。body 是 `flex_1`，其底边 = 表底 − 4 px ⇒ 垂直条（`vbar_rail` bottom=0）在表底上方 4 px 结束；水平条位于 outer 的底部 padding 区（绝对定位相对父 border box，`layout/taffy_tree.rs:618-649`），不覆盖任何行内容。`options.scrollbar = false` 时 padding 为 0、两条 bar/rail 都不构建。

★ 为本次新增/修改的样式（`relative` 用于给绝对定位装饰提供包含块，依据 `layout/taffy_tree.rs:587-615`；List 的 outer 也是这么做的，`list.rs:570-582`）。

- **sticky 语义**：`header` 是 `body` 的兄弟，位于布局上方 ⇒ 垂直位置天然固定（不随 List 纵向滚动，K15 断言纵向滚动后表头 y 不变）；`header_content` 的水平位置由 `Table::sync` 写成 `left = -List.offset.x`，宽度写成 `max(content_width, viewport.width)`，被 `header` 的 `Clip::Bounds` 裁剪 ⇒ 表头在视觉上横向跟随表体、纵向固定、宽视口下与表体同宽（列表行宽同为 `max(视口, content_width)`，否则宽视口列会错位）。
- **水平偏移唯一真源 = 表体 `List` 的 `ScrollSlot.offset.x`**。表头**没有 `ScrollSlot`**、没有镜像滚动槽、没有代理目标写入；`Table::sync` 做两件事：① 读 x（超界时先 `scroll_to` 夹回 `[0, max_x]`）→ 写 `header_content.style.inset_left(-x)`；② 读 `state.viewport.width` → 写 `header_content.style.width = max(content_width, viewport.width)`（viewport 宽变化时也写）。AD3 所说的“`Table::sync` 写入 header”落地为写入 `header` 的内容子节点（`header` 容器本身不动，负责背景与裁剪），语义上即“表头横向偏移与宽度由 Table 按 List offset/viewport 写入”。
- **写入机制（不新增引擎 API）**：`Table::sync` 在 `&mut NodeTree` 上 `let mut style = tree.get(header_content)?.style().clone();` → `style.position(Position::Absolute); style.inset_top(px(0)); style.inset_left(px(-x)); style.width_length(width);` → `tree.set_style(header_content, style)?;`。`set_style` 只用既有公共 API（`node/tree.rs:528-530`、`paint/style.rs:56-59`、`:296-314`）；x 与 width 都变化时才写（`TableInner.sticky_x`/`header_width: Cell<LogicalPx>` 双短路），静止时不触发布局（`node/tree/layout.rs:54-56` 的相等短路）。
- **同步时机**：Windows 事件泵先 `app.run_frame_hook()`（gallery `tick` → `Table::sync`）再 `frame_all`（`platform/windows/event_loop/pump.rs:646-654`）。滚动在输入阶段写入 List offset，故**同一帧**绘制前 header_content 已更新。`Table::sync` 在写入后调用 `app.request_frame()`（幂等；正常路径 `scroll_to`/`request_repaint` 已请求）。
- **resize 行为（P3 收口实测）**：虚拟列表 viewport 未纳入引擎 `clamp_scroll_offsets` 的 `scroll_containers`（`make_virtual_list` 只记 `virtual_containers`），因此**引擎不会在 resize 后夹紧虚拟列表的 x**；kit 侧在 `Table::sync` 中检测 `offset.x > max_x` 时用 `scroll_to` 夹回，并同步写 header 的 x/width。宽→窄 resize 的同一帧内可能有一帧错位，下一帧/夹回后收敛；K15 断言 resize 稳态列对齐（`P3/README.txt` 偏差 2，R1）。
- **为什么不是“把 header 放进 List viewport”**（v1 镜像槽之外的另一种 sticky 实现，被否决）：① 行由虚拟列表在 attach 后惰性挂载，树顺序上晚于 header ⇒ 行会盖住 header，引擎没有 z-index；② 需要每帧写 `top = offset.y`（纵向 pin），且 header 仍要 `Clip::Bounds`，收益不抵成本。
- **为什么删除镜像 `ScrollSlot`（AD3）**：镜像槽会让表头成为第二个滚动容器（目标解析、a11y、拖拽都要特判）；sticky 方案下表头不是滚动容器，`List.offset.x` 是唯一状态。
- **header 仍做纵向代理**：`App::set_scroll_proxy(header, Some(list_viewport))`。表头单元格上的滚轮候选链：cell → header_content → header（代理）→ list viewport（Both），因此纵向滚轮滚表体（K4 保持）、横向滚轮也滚表体；header 无自身 `ScrollSlot`，不会与代理冲突。`set_scroll_proxy` 校验“target 不是 node 后代”：list viewport 在 body 子树，header 与 body 是兄弟 ⇒ 合法。
- **裁剪/背景**：背景色仍在 `header` 节点上（不随内容移动）；`header_content` 与单元格透明；`header` 的 `Clip::Bounds` 负责横向裁剪。divider/header_row 放在 `header_content` 内、`width_full`，随 `header_content` 宽 = `max(content_width, viewport.width)`；窄视口下等于 content_width，宽视口下等于视口宽，横向滚动后仍覆盖视口（`width - x ≥ viewport`，x ≤ `max_x`）。

### 3.5 List 的最小重构（引擎 + kit）

| 文件 | 变更 | 理由 |
|---|---|---|
| `app/virtual_list.rs:14-49` | `VirtualListConfig` 增加 `content_width: Option<LogicalPx>`；`Some` 时 `ScrollSlot.axes = Both`、`slot.content.width = content_width`，否则保持 `Vertical` + viewport 宽 | 双轴虚拟列表的最小入口；默认行为不变 |
| `app/virtual_list.rs:97-110,247-249,316-318` | 三处 `scroll.content = (viewport.width, content)` 改为 `(slot.content_width.unwrap_or(viewport.width), content)`；`VirtualSlot` 存 `content_width` | 避免 rewindow/extents 覆盖横向内容宽 |
| `ui_kit/kit/list.rs:62-74` | `ListOptions` 增加 `content_width: Option<LogicalPx>`（默认 `None`） | Table 传入内容宽；普通 List 不变 |
| `ui_kit/kit/list.rs:1076-1137` | `row_builder`：`Some(w)` 时 `.width_full().min_width(w)`，否则维持 `.width_full().min_width_0()`；行保持 `overflow_hidden` | 行绝对定位 `left:0/right:0/width:auto` 由包含块拉伸，`min_width` 使其在窄视口溢出（`layout.rs:78-99`） |
| `ui_kit/kit/list.rs:609-646` | `attach` 把 `options.content_width` 传给 `make_virtual_list` | 建立双轴槽 |
| `ui_kit/kit/list.rs:600-602,637-639` | **不改**：`scrollbar=false` 时已跳过 bar 构建/attach；Table 传 `false`，自行构建两条 bar | 复用现有开关，避免 List 感知 Table 几何 |
| `ui_kit/kit/list.rs` | 其余不动：垂直条、虚拟化、选择、变高行全部复用 | 控制改动面 |

> **引擎不新增任何 API**。AD2/AD3 复核结论：v1 的 `ScrollSlot.a11y` 字段**无使用点**——表头不再有镜像 `ScrollSlot`（sticky 用布局写），两条 bar 是纯装饰节点（自身不建滚动槽），其余滚动槽都是真实语义容器 ⇒ **删除该字段，不引入**（最小 API；若未来出现“非语义滚动槽”再评估）。sticky 只用既有 `NodeTree::set_style`/`Style` 公共 API。

### 3.6 Table 的构建/挂载/同步（kit 侧）

- `TableInner` 新增：`content_width: LogicalPx`、`sticky_x: Cell<LogicalPx>`（初值 0）；`TableGeometry` 新增 `header: Option<NodeId>`、`header_content: Option<NodeId>`、`list_handle: Option<ScrollHandle>`、`vbar/hbar`（两个 `ScrollBar`，随 `TableInner` 持有）。
- `Table::new`（`table.rs:256-395`）：
  - 计算 `content_width`（§3.2）并存入 `TableInner`；**条件语义**：仅当 `content_width > 0` 时内层 `ListOptions { content_width: Some(content_width), scrollbar: false, .. }`（双轴）；**`content_width == 0`（全部 Flex 且 `min_width` 均为 0）时传 `None`、List 保持 `axes = Vertical`、行宽 = viewport、不建/不显示 hbar**（旧纯纵向布局兼容，避免零宽内容生成无意义双轴槽）；
  - 创建两个 `ScrollBar`：`ScrollbarAxis::Vertical`（key `{key}:body:scrollbar`，与原 List bar 同名沿用）与 `ScrollbarAxis::Horizontal`（key `{key}:scrollbar-h`），均 `ScrollbarOptions::default()`（thickness 4）。
- `Table::build`（`table.rs:463-678`）：
  - `outer` 补 `.relative()`；`options.scrollbar` 时 `padding_bottom = hbar thickness (4 px)`（AD2 的底部带，§3.4）；
  - `header` 显式 `height(header_height + px(1.0))`、`overflow_hidden()`、`relative()`；
  - `header_content`：`.absolute().left(px(0)).top(px(0)).width(px(content_width)).height_full().flex_col()`（初始宽 = content_width；宽视口由 `Table::sync` 改写为 `max(content_width, viewport.width)`），先建 `header_row`（`flex_row`, `width_full`, `height(header_height)`）与 divider（`width_full`）；表头单元格仍用 `span_cell_style`（窄视口内容宽固定为 `content_width`，不再需要 `min_width`）；
  - `body` 保持 `flex_1/relative`；`list.build(tree, body)`（List 不再建内部 bar）；`vbar_rail` 建在 `body`，`vbar.build(tree, vbar_rail)`；
  - `hbar_rail` 建在 `outer`（absolute left 0/right 4px/bottom 0/height 4），`hbar.build(tree, hbar_rail)`；**`content_width == 0` 时 hbar 不显示**（`scrollable=false`，不宣告横向；旧纵向布局无水平条）。
  - 记录 `geometry.{outer, header, header_content, body, header_row, empty}`。
- `Table::attach`（`table.rs:680-688`）：
  1. `list.attach(app, window)?`；2. `let viewport = list.viewport_node().ok_or(...)`；
  3. `vbar.attach(app, window, viewport)?`、`hbar.attach(app, window, viewport)?`（P3-1：track 代理 + `ScrollHandle`）；
  4. `app.set_scroll_proxy(window, header, Some(viewport))?`；
  5. 记录 `geometry.{window, list_handle}`，注册 sort handler（现状）。
- `Table::sync`（`table.rs:706-708`）：
  1. `list.sync(app)?`；
  2. sticky 写：读 `scroll_state`；`max_x = max(0, content_width - viewport.width)`；若 `offset.x > max_x` 先 `app.scroll_to(..., max_x)` 夹回（虚拟列表不在引擎 `clamp_scroll_offsets` 的范围内，kit 侧兜底）；再令 `x = offset.x`、`width = max(content_width, viewport.width)`；若 `x != sticky_x` **或** `width != header_width` ⇒ 改 `header_content` 的 `Style`（`inset_left(-x)`、`width_length(width)`，`position`/`inset_top` 保持）→ `tree.set_style(...)` → 两个 Cell 更新 → `request_repaint`；
  3. `vbar.sync(app)?`、`hbar.sync(app)?`；
  4. 聚合 changed 返回。
- `Table::release`（`table.rs:407-410`）：`set_scroll_proxy(header, None)`（忽略 `StaleNode`）→ `vbar.release` + `hbar.release` + `list.release` + `off_action`/`release_scope`（现状）。信号回收数从“列表三条 + 一条 bar 两条 + 排序一”变为“列表三条 + 两条 bar 各两条 + 排序一”，`releasing_the_table_scope_reclaims_its_signal_slots`（`:1623-1633`）的 `>= 6` 重算（预计 8）。
- 列对齐不变量：header/body 使用同一 `content_width`、同一 `span_cell_style`、同一个行内 padding；body 行宽 = `max(视口宽, content_width)`，**header_content 宽 = `max(视口宽, content_width)`**（由 `Table::sync` 写入）且 x 平移 = `-List.offset.x`；body 行的横向偏移也由同一个 `List.offset.x` 决定 ⇒ 窄/宽视口下列屏幕 x 恒等（K15）。

### 3.7 `scroll_state` / 内容宽度 / 滚动范围

- 表体 `ScrollState.content = (content_width > 0 ? content_width : viewport.width, virtual_content_height)`；**`content_width > 0` 时 `axes = Both`、`max_x = max(0, content_width - viewport.width)`**；**`content_width == 0`（全 Flex 零 min）时 `axes = Vertical`、内容宽 = viewport、`max_x = 0`，不出现水平条**（旧纵向布局兼容）。
- 水平 max 不随虚拟行窗口变化：`virtual_list.rs` 的 content 更新保留 `content_width`（§3.5）。
- 水平偏移不参与虚拟行锚点（锚点只看 y：`virtual_list.rs:170-180`、`:291-325`），无需改动。
- `clamp_scroll_offsets`（`app/scroll.rs:153-178`）每帧夹紧两轴；表头 x 由 `Table::sync` 跟随（resize 的一帧延迟见 §3.4/R1）。
- 表头不是滚动容器：`scroll_state`/`max_offset`/`a11y` 都不会看到它。

### 3.8 水平/垂直 `ScrollBar` 的布局、命中与代理（AD2）

| 项 | 设计 | 依据 |
|---|---|---|
| 归属 | Table 自建两条 `ScrollBar`，分别挂在 `vbar_rail`（body 内）与 `hbar_rail`（outer 内）；状态均来自 List 双轴 viewport | `scroll_bar.rs:164-197`、`:277-293` |
| 垂直条尾隙 | body 底边因 outer `padding_bottom = 4 px` 而在表底上方 4 px；`vbar_rail`: absolute top 0 / right 0 / **bottom 0** / width 4；ScrollBar track（`top/right/bottom=inset=0`）填满 rail ⇒ 垂直条在表底上方 4 px 结束 | `scroll_bar.rs:203-215`；`scroll_bar.rs:69-110` 的 `inset` 不扩展；outer 底部带见 §3.4 |
| 水平条尾隙 | `hbar_rail`: absolute left 0 / **right 4px** / bottom 0 / height 4；track 填满 rail ⇒ 水平条在垂直条左缘前 4 px 结束 | 同上 |
| 不重叠 | 垂直条占 x∈[W-4,W]、y∈[body_top, H-4]；水平条占 x∈[0,W-4]、y∈[H-4,H]；右下 4×4 为空白，**无重叠** | D2 用户裁决 |
| 布局占位 | 见 §3.4：`options.scrollbar = true` 时 outer 底部预留 4 px（静态、构建期已知）；`false` 时不预留、不建任何 bar | 避免水平条覆盖最后一行；宽视口下该带无绘制（bar 不出现） |
| 可见性 | thumb 的 `visible = scrollable = max > 0.5`（`scroll_bar.rs:386-408`）；宽视口 thumb 不绘制；track 无 paint、点击 `max=0` 时 no-op（`:569-580`） | 满足 A3“宽视口不出现” |
| 命中/拖拽 | 拖 thumb → `point(current, offset)` 只改本轴；点 track → 跳转 | `scroll_bar.rs:465-519`；A-H2 修正 |
| 滚轮 | track 代理 → List viewport（Both）；垂直滚轮在水平条上滚表体纵向（Chrome：滚动条区域算所属 scroller） | 主计划书 §4.7/P3-1；`chrome.md:75-76` |
| `scrollbar: false` | 不建 bar/rail/带；双轴滚轮仍可用 | `table.rs:299`、`list.rs:600-602` |
| 公共 API | **不扩展 `ScrollbarOptions`**；rail 是 Table 内部普通布局节点 | AD2 用户裁决 |

### 3.9 a11y 语义

- Table 节点继续接管表体 List 的滚动语义（lifted scroll，`a11y/tree.rs:771-812`）；双轴后 `A11yScroll` 为 `{x, y, x_max>0, y_max>0, orientation: None}`（P1 形状；Both 不自动设置 orientation，主计划书 O9/A-H3）。
- Table 自动获得 `ScrollUp/Down/Left/Right/ScrollIntoView` 动作（`a11y/mapping.rs:286-301`）；P1/P4 修正步长后，`ScrollLeft/Right` 用 viewport 宽度、`ScrollByItem` 用 `line_height`（主计划书 §4.6、`a11y/action.rs:450-464`）。
- 表头**不是**滚动容器 ⇒ a11y 不会多出一个滚动节点；表头单元格的滚动目标在 P4 迁移到候选链 + 代理后落到 List viewport（`a11y/tree.rs:804-812` 当前仍用结构 `scroll_ancestor`，表头单元格与 HEAD 一样无滚动目标；Table 自身的滚动语义来自 lifted body window）。
- 列/行语义不变：`column_count` 仍为逻辑列数（`table.rs:477-481`、测试 `:1450-1505`）；表头行/单元格的 a11y 父仍应上提到 Table（现有断言 `table.rs:1389-1393`，K20 守卫）。
- 新断言：窄表 `x_max > 0`、`orientation == None`、含 ScrollLeft/Right；宽表 `x_max == 0`；**无额外滚动容器**；水平滚动后表头单元格的 a11y 位置与表体同列单元格一致（用 a11y bounds 比对，见 K20）。

### 3.10 迁移影响（调用点清单，编译器穷举）

| 调用点 | 现状 | 影响 | 处置 |
|---|---|---|---|
| `Column::flex(` | `grep -n 'Column::flex('` 命中 **17 处**（`Column::{fixed,flex}(` 合计 31 处行，其余 14 处为 Fixed，不受签名影响） | **必须补第三参数 `min_width: LogicalPx`** | 逐点显式给值；测试与内建列取语义值（见下） |
| `crates/widgets/force_order/src/view.rs:235` | `Column::flex(header, weight)` | 同上 | 按旧密度取小值（建议 `px(0.0)` 保持可压缩；迁移时按实际布局确认） |
| `crates/widgets/financial_index/src/view.rs:1413` | 同上 | 同上 | 同上 |
| `crates/widgets/financial_calendar/src/view.rs:1149` | 同上 | 同上 | 同上 |
| `crates/widgets/hyperliquid_wallet/src/detail.rs:865` | 同上 | 同上 | 同上 |
| `crates/widgets/market_trade/src/columns.rs:108` | 同上 | 同上 | 同上 |
| `crates/widgets/watch_list/src/table.rs:604` | 同上 | 同上 | 同上 |
| `crates/settings_ui/src/pages/proxy/table.rs:553` | 同上 | 同上 | 同上 |
| `crates/ui_kit/examples/gallery.rs:1122`（⑧） | `Column::flex("描述", 2.0)` | 同上 | 显式给 80（与旧观感一致） |
| `crates/ui_kit/src/kit/table.rs`（测试/内建，`841/868/869/926/1183/1246/1248/1457/1459`，共 9 处） | 同上 | 同上 | 测试按用例显式给值（列宽断言同步重算） |
| `ColumnWidth::Flex(f32)` 构造 | 仅 `table.rs:834-845` 测试与内部 | → `Flex { weight, min_width }` | 测试改；内部 `contribution/weight` |
| `table.rs` 宽度断言（`:1027-1054`、`:1178-1237`、`:1239-1295`、`:1297-1325`） | 依赖“flex 可缩到 0/具体像素” | 公式变化后期望值变 | 改为“列对齐 + 相对宽 + 内容宽公式”断言；像素按新公式重算 |
| `table.rs:1327-1448`（a11y） | `scroll.vertical == true` | P1 后 `A11yScroll` 改形状；Table 双轴 | P1 迁移 + K20 新增双轴/无额外滚动容器断言 |
| `Table` 滚动条归属 | 内层 List 自带垂直条 | 改为 Table 自建两条 bar + rail；ListOptions.scrollbar=false | §3.6；`releasing…signal_slots` 期望值重算 |
| `ListOptions` 字面量 | Table 一处 + List 自身 + 测试/示例 | 新增 `content_width: Option<LogicalPx>` | 全部补 `content_width: None`；**Table 处按 `content_width` 条件 Some/None：`> 0` → `Some(content_width)` + List `axes=Both`；`== 0` → `None` + List `axes=Vertical`** |
| `VirtualListConfig` 字面量 | `List::attach` 一处 | 新增 `content_width: Option<LogicalPx>` | 同上 |
| `ScrollBar::attach` 调用点（List/Table/gallery/应用层） | 返回 `()` | 改 `Result`（P3-1） | 主计划书 §4.7 处理 |

### 3.11 测试矩阵（新增，编号承接主计划书）

| ID | 用例 | 断言 | 级别 |
|---|---|---|---|
| E34 | `smallest_angle_and_projection_match_chromium` | §2.8 E34a–E34l 全部通过（含 45° 严格边界、零向量、单轴/双轴、未启用轴、`EPS=0.1`） | P2 纯函数 |
| E34m | `target_selection_converts_lines_per_candidate` | §2.8 E34m；A（20px 行高）被选中，证明无全局换算 | P2 |
| E34n | `target_selection_continues_when_small_line_height_cannot_consume` | §2.8 E34n；A 不可消费 ⇒ B（48px）被选中 | P2 |
| E35 | `apply_scroll_consumes_whole_delta_within_45_degrees` | requested (2,3) 单轴纵 ⇒ applied (0,3)、consumed (2,3) | P2 |
| E36 | `apply_scroll_projects_beyond_45_degrees` | requested (12,3) ⇒ consumed (0,3) | P2 |
| E37 | `zero_or_blocked_delta_consumes_nothing_and_requests_no_frame` | applied (0,0) ⇒ 不 scroll/不消费/不 request_frame | P2 |
| E38 | `hint_based_target_selection_uses_first_event_delta` | 新序列首事件 `Line` delta 逐候选换算后选目标；后续事件不重选（D1） | P2 |
| K11 | `table_content_width_is_fixed_sum_plus_flex_min_sum` | 内容宽断言（含被 span 覆盖的列仍计入） | P1/P3 |
| K12 | `flex_requires_explicit_min_width_and_override_is_removed` | 三参数构造；`.min_width()` 不存在（编译期）；`ColumnWidth::Flex { weight, min_width }` 存值正确；Fixed 无 min 概念 | P3 |
| K13 | `wide_viewport_shows_no_horizontal_bar` | `hbar.scrollable == false`、thumb 不可见、`max_x == 0` | P3 |
| K14 | `narrow_viewport_shows_horizontal_bar` | `hbar.scrollable == true`、thumb 出现、`max_x == content - viewport` | P3 |
| K15 | `sticky_header_follows_the_list_offset_and_columns_align` | 拖 hbar/滚轮后：`header_content.inset_left == -list.offset.x`、`width == max(content_width, viewport.width)`；表头单元格与表体同列单元格屏幕 x 相同（窄/宽视口两种）；**纵向滚动后表头 y 固定**；resize 宽→窄后夹回 `[0,max_x]` 且稳态对齐 | P3 |
| K16 | `wheel_over_header_scrolls_body_and_not_the_page` | 表头单元格上水平滚轮：list.x 变、y 不变；外层页面不动；`header.offset` 无独立状态（表头无 ScrollSlot） | P3 |
| K17 | `two_axis_drags_do_not_reset_each_other` | 先横拖再竖拖：x 保持；先竖拖再横拖：y 保持 | P3 |
| K18 | `table_release_clears_header_proxy_and_is_idempotent` | 释放后 header proxy 清除、两条 bar 释放、重复 release 幂等、无 panic；信号回收数 ≥ 8 | P3 |
| K19 | `horizontal_scroll_state_keeps_both_axes_after_rewindowing` | 虚拟 rewindow/extents 后 `content.width` 不被覆盖；`ScrollState.axes = Both` | P3 |
| K20 | `table_horizontal_a11y_exposes_both_axes` | 窄表 x_max>0、orientation None、含 ScrollLeft/Right；无额外滚动容器；表头单元格 a11y bounds 与表体同列对齐 | P3 |
| K21 | `scrollbar_tail_gaps_do_not_overlap` | 垂直条 track 底 = 表底 − 4、水平条 track 右 = 表右 − 4；两矩形不相交；`ScrollbarOptions` 无新字段 | P3 |
| G7 | `--wheel-check`：宽表窄视口 | 内容/表头/**水平条 track 滚轮**/**thumb 拖动**；表头与表体同帧同偏移；表头 y 固定 | P3 |
| G8 | `--wheel-check`：宽表宽视口 | 不出现水平条；垂直滚轮仍只滚表体；页面不动 | P3 |
| G9 | `--wheel-check`：表头/表体列对齐 | 滚动后 `cell_node(0,k)` 与 `header_node(k)` 的屏幕 x 差为 0 | P3 |

> **P3 收口状态（2026-11-26，最终）**：K1/K2 已去 `#[ignore]` 并默认通过；K3–K21 默认 `cargo test -p ui_core -p ui_kit` 全绿（`ui_core` lib **2744 passed / 1 ignored**、`ui_kit` lib **796 passed / 0 ignored**，含 `App::clear_scroll` 清理用例）；header y 固定、G7 水平条 track 滚轮 + thumb 拖动、resize 宽→窄（kit 侧夹回、稳态对齐）、应用层 smoke 均随 P3 收口通过；`gallery --wheel-check` PASS（G1–G9 **37 断言**）、`--idle-check` idle frames 0。**P3 门禁改动集 37 文件；收口后工作区 40 文件**（含 `clear_scroll` 测试）。证据：`E:\kytrix-scratch\scroll-impl\P3\`（README 由 fixer 记录）。

#### 3.11.1 sticky 与列对齐的可测法

- 引擎 `interaction/scroll.rs::to_screen` 只累加祖先滚动偏移（`:85-89`）；sticky 表头的平移是**布局**（`header_content` 的 absolute left），不是滚动槽，因此：
  - 断言 `header_content` 的布局 `bounds.origin.x == -list.offset.x`、`bounds.size.width == max(content_width, viewport.width)`（`app.probe().bounds(header_content)`）；
  - 断言 `header_node(k).bounds.origin.x - header_content.origin.x == cell_node(0,k).bounds.origin.x - body_list.offset.x`（两边的“内容坐标”相同；窄/宽视口各一次）；
  - 纵向滚动一屏后断言 `header_content.bounds.origin.y` 与 `header_node(k).bounds.origin.y` 不变（header y 固定）；
  - 用 `Table` 内部 `geometry` 读 List `ScrollHandle` 的 offset，避免依赖未偏移布局当屏幕坐标。

---

## 4. A3：gallery 宽表演示

### 4.1 演示位置

- 新增 **⑩ 宽表（水平滚动 + sticky 表头）** 小节，追加在现有 ⑨ 之后；旧 **⑩–⑮ 顺延为 ⑪–⑯**（gallery 共 **16 sections**，`gallery.rs` 的 `section()` 编号已按此排列）。
- 两个 `Table`，同一列配置：
  - **窄容器**：`sized_box(..., 420.0, 240.0)` ⇒ 出现水平条（G7）；
  - **宽容器**：`sized_box(..., 1100.0, 240.0)` ⇒ 不出现水平条（G8）。
- 列配置（**AD4 三参数显式最小宽**；内容宽可手算）：

```rust
vec![
    Column::fixed("代码", 140.0).sortable(),
    Column::fixed("名称", 160.0).sortable(),
    Column::flex("备注", 2.0, px(80.0)),          // 显式 80（AD4：无默认）
    Column::flex("分类", 1.0, px(96.0)),          // 显式 96
    Column::fixed("价格", 100.0).align(ColumnAlign::End),
    Column::fixed("涨跌", 100.0).align(ColumnAlign::End),
    Column::fixed("成交额", 120.0).align(ColumnAlign::End),
]
// content_width = 140+160+80+96+100+100+120 = 796
```

- 窄容器 420 ⇒ `max_x = 376`（断言）；宽容器 1100 ⇒ `max_x = 0`、flex 列吃满剩余（备注/分类增长）。

### 4.2 验收断言（gallery `--wheel-check`）

| 断言 | 期望 |
|---|---|
| 窄表 hbar | `scrollable == true`；thumb 出现；`max_x == 796 - 表宽` |
| 宽表 hbar | `scrollable == false`；thumb 不可见；`max_x == 0` |
| sticky 表头 | 窄表水平滚动 1 格后：`header_content.left == -list.offset.x`、`width == max(796, 表宽)`；表头/表体首列屏幕 x 相同；纵向滚动时表头 y 不变 |
| G7 水平条交互（P3 收口） | 水平条 **track 滚轮** 与 **thumb 拖动**均只改 x、y 保持；表头与表体同帧同偏移；`gallery --wheel-check` PASS（G1–G9 **37 断言**） |
| resize 宽→窄（P3 收口） | kit 侧夹回 `x ≤ max_x`，header 写新 `width=max(content, viewport)`；稳态列对齐（同帧可能错位 1 帧后收敛，`P3/README.txt` 偏差 2） |
| 应用层 smoke（P3 收口） | `settings_ui` / `updater_ui` / `widget_gallery_ui` / `candlestick` 接线后编译通过、各自 smoke/手测通过（`cargo check --workspace` PASS） |
| 两轴不互斥 | 窄表先横拖到 x=100，再纵向滚轮：x 保持；再纵拖到底，x 保持 |
| 边界 | 水平滚到 max 后同序列继续：x 不再变、页面不动；停顿后再次水平滚：外层按新序列（Chrome 式） |
| 垂直能力不退化 | 窄表表体垂直滚轮只滚表体；表头滚轮滚表体（K4 保持） |
| 水平条尾隙 | 垂直条底与水平条右端在角部不相交（K21） |
| idle | `--wheel-check` 后分进程跑 `--idle-check`：无额外帧（I7；sticky 仅在 x/width 变化时写 style） |

---

## 5. 对 P0–P4 的增量任务与验收（不重写主计划书原文）

### P0（追加 0.5 人日 → 与既有 0.5 合并）

| 任务 | 产出 | 验收 |
|---|---|---|
| P0-4 基线 | HEAD 上记录：Table 无水平条、flex 列在窄视口可缩到 0、`Table` a11y 只有 vertical、Table 内部由 List 持有垂直条；**不写引用未落地 API 的测试** | 基线日志；供 P1/P3 对照 |

### P1（追加 1 人日）

| 任务 | 产出 | 验收 |
|---|---|---|
| P1-5 双轴 List 承载 | `VirtualListConfig.content_width`、`VirtualSlot.content_width`、三处 content 写入保留宽度、`ListOptions.content_width`、行 `min_width`；默认 `None` 行为与 HEAD 完全一致 | 既有 list/virtual 用例全绿；`content_width=None` 时 `axes=Vertical`、行宽行为不变 |
| P1-6 | **取消**（AD3 sticky 方案不再需要 `ScrollSlot.a11y` 标记；引擎无新增滚动槽） | — |

### P2（追加 0.5 人日）

| 任务 | 产出 | 验收 |
|---|---|---|
| P2-6 45° 内核 | §2.4 的 `request_px`/`applied_delta`/`smallest_angle_between`/`project_vector`/`apply_scroll`；`route_wheel` 逐候选换算 hint（§2.6） | E34–E38 绿；E12 按“偏移不变、consumed 记账”改写；E4/E5 保持 |
| P2-7 零 delta/相位 | 零 delta 不建立会话；`End/Cancel` 先处理相位再判零 delta | E8、E28 扩断言 |

> **裁决 C（滚动动画能力删除）与本轮修复的增量**：删除 E31 `scroll_animation_is_cancelled_by_new_wheel_sequence`、程序化动画取消改写（P2 清单 34→32），另删除 1 条 P2 前既有 `removal_tests` 滚动动画用例；`scroll_to_animated`/`AnimationTarget::Scroll`/`cancel_scroll_animation`/`lerp_point` 不保留。修复轮新增 11 条（原 9 条 + 收口轮 2 条）⇒ 最终 43 项新增/转绿；类别按 README 逐项标注 **existing / unignored / rewrite / new**。**最终实测**：`ui_core` lib **2736 passed / 1 ignored**、`ui_kit` lib **769 / 2 ignored**（P1 基线 2691/5 ⇒ +45/−4；相对修复前 P2 2728/1 ⇒ +11/−3）；`--all-features` 回归通过（日志为准）。证据：`scroll-impl/P2/README.md` + `research/review-p2-final.md`。

### P3（追加 1.5 人日）

| 任务 | 产出 | 验收 |
|---|---|---|
| P3-6 `Column` API（AD4） | `ColumnWidth::Flex { weight, min_width }`、`Column::flex(header, weight, min_width)`（删默认/链式）、`contribution()`、`span_cell_style` 重写、`TableInner.content_width` | K11/K12 绿；17 处 `Column::flex` 调用点全部显式给值；table 宽度断言重算 |
| P3-7 Table sticky + 双 bar（AD3/AD2） | outer/header/header_content 结构与 `relative`、header 显式高度、sticky 写（`sticky_x` + `header_width` 双 Cell + `set_style`，宽度写 `max(content_width, viewport.width)`）、List `scrollbar:false`、vbar/hbar 与 rail 尾隙、`Table::sync` 聚合 | K13–K17、K19、K21 绿；K15 覆盖宽/窄视口列对齐与 header y 固定；K9（几何守卫）不回归 |
| P3-8 代理扩展（对 P3-1/P3-2 的增量） | 两条 bar 的 track 代理到 list viewport；header 代理下横向滚轮也生效；header 无自身滚动槽 | K15/K16/K18、K4 保持 |
| P3-9 gallery | **⑩ 宽表**（三参数列配置；旧 ⑩–⑮ 顺延为 ⑪–⑯，共 16 sections）+ `--wheel-check` G7–G9（G7 含水平条 track 滚轮与 thumb 拖动） | 三端至少 Windows 真机 PASS；`P3/wheel.log` 记录 G1–G9 **37 断言** PASS |
| P3-10 迁移（AD1） | §3.10 调用点：17 处 flex 显式 min_width、widget 表按旧密度取值并逐点记录、List/VirtualListConfig 字面量、Table 条归属 | 编译零 warning；各 widget 最小用例/手测；`TableOptions` 不加字段 |

> **P3 错误语义（收口）**：`Input::attach` 失败**向上传播**（不吞错）；`ScrollBar::attach` 先校验后一次性写入，**失败无半写**、`release` 幂等；`Table::attach` 任一步失败**回滚已建部分**（清 header proxy、release 已 attach 的 bar）后返回错误，后续 `release` 幂等。新增最小公共 API **`App::clear_scroll(&ScrollHandle) -> Result<(), EngineError>`**：清除节点滚动槽，供 `Input::attach` 失败路径清理（**不请求帧**、不 damage、不更新 a11y）。`pump.rs` 的 **3 行 close/gate fix** 随 P3 实际 diff（门禁改动集 **37 文件**；收口后工作区 **40 文件**，含 `clear_scroll` 测试）。验收：K8/K18 + 各自 attach/clear_scroll 用例；主设计 §5.2/§5.4/§6 P3 注同步。

### P4（追加 0.5 人日）

| 任务 | 产出 | 验收 |
|---|---|---|
| P4-6 a11y 轴过滤 | `a11y_scroll_target(tree, node, axis: Option<ScrollAxes>)`：集合根提升优先，随后 `scroll_candidates` 第一个包含 axis 的容器；`None`=任意轴（IntoView/SetOffset/action_allowed）；**self/lifted 分支也按 axis 匹配；候选需 `is_effectively_visible && is_effectively_interactive`；动作能力表按 `ScrollAxes` + 该轴 `max_offset > 0` 过滤（单轴容器只宣告本轴动作；宽视口 Table/Both `x_max == 0` 不宣告 Left/Right）**；Table 双轴/代理断言 | **完成**：E18/E32/E32b 绿；K20（Table 双轴 a11y）不回归；E32 改名并扩横向 `Item` 步长 |
| P4-7 变异自检 | 45° 严格边界、投影参数、代理优先、键盘轴过滤、a11y 轴过滤、a11y 动作能力过滤、target 轴/可见性 | **完成**：M1–M7 逐项 RED→SHA256 还原→GREEN（见下）；P3-7 其余变异项（sticky 写/min_width/rail 尾隙/bar 代理）已在 P3 门禁完成，P4 未重复 |
| P4-8 文档 | 主计划书附录 E + 冲突标注；本文件回写 `custom-gui-engine-design.md` E22/§8.4；`kit-gallery.md` | **完成**：主设计 §4.8/§7.1/§6 P4/附录 D；E22 状态/API/§8.4；kit-gallery 16 分区 + G1–G9；新增 `E22.md`；行号已按 P4 源码更新 |

> **P4 收口实测（2026-11-26，最终）**：`ui_core` lib **2757 passed / 1 ignored**（P3 2744/1 ⇒ **+13**；本复审轮新增 10 条，含 a11y 动作轴/溢出过滤、lifted/self 轴匹配、键盘边界/hidden、零宽回退）、`ui_kit` lib **798 passed / 0 ignored**；`--ignored` 仅既有 dark-mode + `gui_smoke`；`gallery --wheel-check` PASS（G1–G9 **37 断言**）、`--idle-check` 0 帧、`--probe` exit 0；`cargo test -p ui_core -p ui_kit`、clippy `--workspace`、`fmt --all --check`、`check --workspace` 全 PASS；**按用户限制未跑 workspace 全量测试**。证据：`E:\kytrix-scratch\scroll-impl\P4\README.txt`。
>
> **P4-7 七条变异结果**（脚本 `P4/mutate.ps1`；每项记录 HEAD/CMD/SHA256 BEFORE→RESTORED/MATCH/RED→GREEN）：
> | # | 变异 | 红证据 | 还原 |
> |---|---|---|---|
> | M1 | `angle < 45.0` → `angle <= 45.0` | E34d 精确 45° 用例红（走投影而非整段吞掉） | SHA256 MATCH，GREEN |
> | M2 | `project_vector(requested, applied)` → `(applied, requested)` | E34i >45° 投影断言红 | SHA256 MATCH，GREEN |
> | M3 | `scroll_candidates` 代理优先移除 | 11 条红（候选链/代理回落/键盘+滚轮代理） | SHA256 MATCH，GREEN |
> | M4 | `default_key_scroll` 回退旧“最近滚动祖先” | E17/E17b/E33 三条键盘用例红 | SHA256 MATCH，GREEN |
> | M5 | `a11y_scroll_target` 轴过滤短路 | E32b 红 | SHA256 MATCH，GREEN |
> | M6 | a11y 动作能力过滤移除（不按 `ScrollAxes`/`max_offset` 过滤） | E18 组红（单轴/宽视口宣告错误） | SHA256 MATCH，GREEN |
> | M7 | target 轴/可见性过滤移除（self/lifted 不判 axis，候选不判 visible/interactive） | E18/E32b 组红（隐藏/无轴目标被选中） | SHA256 MATCH，GREEN |

---

## 6. 风险、未验证与已裁决项

### 6.1 风险

| ID | 风险 | 影响 | 缓解 |
|---|---|---|---|
| R1 | resize 宽→窄：虚拟列表 x 不在引擎 `clamp_scroll_offsets` 范围（`scroll_containers` 不含 virtual container），kit 在 `Table::sync` 用 `scroll_to` 夹回；夹回前同一帧可能错位 1 帧 | resize 时视觉 | 已实现 kit 侧夹回 + header x/width 同帧写；K15 断言稳态对齐；P3 README 偏差 2 记录（不改引擎）；真机确认 |
| R2 | AD1 全局默认 + AD4 强制显式最小宽：17 处调用点必须逐个决策，widget 行情表取值不当会改变密度 | 交易类表格观感 | 编译期清单 + P3-10 逐点记录取值；测试覆盖 1–2 个代表性 widget |
| R3 | sticky 写通过 `NodeTree::set_style` 触发 `header_content` 子树布局失效（`layout.rs:46-60`） | 每帧滚动一次小范围布局 | 只在 x 或 width 变化时写（`sticky_x`/`header_width` 双短路）；header 子树小；I7/idle 守卫（`--idle-check` idle frames 0） |
| R4 | 表头/表体列对齐依赖 header_content 宽与 List rows 同宽、同一 cell 样式 | 宽视口/跨视口 resize 时列错位 | **`Table::sync` 写 `width=max(content_width, viewport.width)`**（窄视口=content_width）；K11/K15/G9 守卫；P3 收口后 K15 覆盖窄/宽两种视口与 resize 稳态 |
| R5 | 虚拟列表三处 `content` 写入漏改 | rewindow/extents 后横向范围清零、hbar 消失 | K19 覆盖 rewindow + extents；变异自检 |
| R6 | `options.scrollbar=true` 时 `outer` 底部固定预留 4px 带；宽视口下无 bar 但有 4px 空白 | 密集表少 4px 高 | 已按 D2 选择“静态预留”最小实现；若要求宽视口零占位需动态带（额外状态/布局），列入 §6.2 待确认 |
| R7 | 45° 在本引擎下只影响消费记账，不改变偏移 | 若期望“斜向滚动有可见差异”会失望 | §2.3 源码澄清；E34 固化；overscroll/结转未来落地时才可见 |
| R8 | 水平条 rail 的 4px 尾隙与 `ScrollbarOptions.inset` 无关；未来若有人改 rail 几何可能破坏“不重叠” | 视觉/命中 | K21 守卫；rail 尺寸常量集中定义 |
| R9 | 表头 a11y bounds 来自布局平移而非滚动槽；与表体（滚动槽）的 a11y 偏移路径不同 | a11y 位置不一致 | K20 用 a11y bounds 比对；必要时在 P4 让 adapter 对 sticky 节点套同一偏移 |
| R10 | 符号归一化遗漏或写错方向（`request_px` 忘记取负 / 消费与选择用了不同符号） | 滚轮方向反、边界判定全错 | E34a（下滚 raw 为负 → applied 正）、E34m/E34n、K15 守卫；P4-7 变异自检含“去掉取负” |

### 6.2 未验证 / 实现取舍（不作为事实）

| 编号 | 事项 | 处理 |
|---|---|---|
| U1 | Chrome `kLimitScrollDeltaToScrollerSize` 默认特性状态（`input_handler.cc:1785-1786`） | 不实现；只在大 delta 时影响 `CanConsumeDelta` |
| U2 | 各平台 wheel→GSU 的 `rails_mode` 取值 | 只依赖 `Shift` 换轴（现状）；O2 未裁决 |
| U3 | 本引擎未来是否需要 overscroll 记账入口 | 本批不实现；纯函数返回值已就绪 |
| U5 | 逐候选 `line_height` 与 Chrome 共享 px delta 的差异 | 本文映射决定（非未验证）；E34m/E34n 固化；若要求同构需与 O2 一起裁决 |
| I1 | 宽视口下 `outer` 底部是否允许保留 4px 空带（AD2 的静态预留） | 本文取“允许（无绘制、不可命中内容）”；若要求零占位，需在 resize/阈值变化时改 padding（额外状态，未选） |
| I2 | `Table::sync` 用 `NodeTree::set_style` 写 sticky 偏移与宽度 | 已核实为既有公共 API（`node/tree.rs:528-530`、`paint/style.rs:56-59/296-314`），无需引擎改动；若后续引入“运行时平移/尺寸”公共 API 可替换 |
| I3 | resize 宽→窄的一帧错位（P3 README 偏差 2） | 虚拟列表 x 不在引擎 `clamp_scroll_offsets` 范围；kit 在 `Table::sync` 用 `scroll_to` 夹回并同帧写 x/width，稳态无错位；接受一帧瞬态（R1），未改引擎 |

### 6.3 已裁决项（AD1–AD4，第二轮，已关闭）

| 编号 | 裁决 | 本文落地 | 状态 |
|---|---|---|---|
| AD1 | Table 横向全局默认；不新增 `TableOptions` 开关；widget 调用点显式小 min_width | §3.2/§3.10/P3-10 | **已裁决** |
| AD2 | 不接受 4px overlay 重叠；Table 局部 rail 尾隙（垂直条底 4px、水平条右 4px）；不扩展 `ScrollbarOptions` | §3.4/§3.8/K21 | **已裁决** |
| AD3 | Table 内部 sticky header；水平偏移唯一真源 = List.offset.x；`Table::sync` 写入；header 仍做纵向代理；不改引擎 sticky | §3.4/§3.6/§3.9/§3.11/K15/K16/G7/G9 | **已裁决** |
| AD4 | `Column::flex(header, weight, min_width: LogicalPx)` 三参数；取消默认 80 与链式 `.min_width()`；全部调用点显式；`ColumnWidth::Flex { weight, min_width }` 保留 | §3.3/§3.10/§4/K12 | **已裁决** |

> 无新增待裁决项。§6.2-I1/I2 是实现取舍登记（本文已按最小改动选择），不阻塞实施；若用户要求调整，仅影响 P3-7 的局部几何/写入方式。

---

## 7. P0–P2 级设计问题自检（任务验收项）

| 检查 | 结论 |
|---|---|
| 45° 每个分支有 Chromium 行号依据 | 是（§2.2/§2.3/§2.4；`:2284-2314`、`math_util.cc:794-808`） |
| 是否把“未验证”当事实 | 否：U1–U3、I1–I2 单列；`ScrollStateData` hint、`kScrollEpsilon`、`LineStep`、零 delta 不发 GSB 均为快照源码直读 |
| 目标选择是否用初始 delta/hint | 是（§2.6；hint = 序列首事件原始 delta/unit，**逐候选用各自 line_height 换算**；不重选，D1） |
| Line 换算是否避免“全局换算” | 是（§2.4/§2.5：无全局换算；选择阶段逐候选、消费阶段用目标；E34m/E34n 固化；Shift 先换轴） |
| 符号契约是否明确且与 Chrome 同构 | 是（§2.4：Chrome `delta=-event.delta` 与 offset 同向；本引擎 raw 反向 ⇒ `requested=-raw×line_height`、`goal=offset+requested`、`applied=new−offset`；不改 `InputEvent.delta`；E34a/E34m/E34n + R10 守卫） |
| 未启用轴清零是否与 Chrome 同序 | 是（先算 applied 时清零，请求 delta 保留参与角度；§2.3/§2.4） |
| 45° 是否被误述为改变偏移 | 否（§2.3 末、R7 明确澄清） |
| Table 公式是否与用户裁决一致 | 是（§3.2；Fixed 和 + Flex 最小宽和；AD1 全局默认） |
| `Column` API 是否为 AD4 形态 | 是（§3.3；三参数、无默认、无链式；`ColumnWidth::Flex { weight, min_width }` 保留） |
| 表头/表体共享偏移是否为 AD3 sticky 形态 | 是（§3.4/§3.6；无镜像滚动槽、无 a11y 标记；List.offset.x 唯一真源；`Table::sync` 同帧写 `left=-x` 与 `width=max(content,viewport)`；header y 固定；resize 稳态由 K15 覆盖，R1/I3 记录一帧瞬态） |
| 滚动条几何是否为 AD2 形态 | 是（§3.8；rail 尾隙、不重叠、无 `ScrollbarOptions` 扩展；K21 守卫） |
| 水平条是否可命中/代理 | 是（§3.8；P3-1 代理 + A-H2 point 修正） |
| 与 List 纵向槽关系是否明确 | 是（表体 List 单槽双轴；表头非滚动容器；无第二套链式） |
| 现有 List 是否无法承载 | 是，给出最小重构（§3.5）；默认 `content_width=None` 时行为不变 |
| 迁移影响是否穷举 | 是（§3.10；17 处 flex 显式参数、14 处 Fixed 不受影响、List/VirtualList 字面量、table 宽度/a11y 断言、bar 归属） |
| 测试是否含两轴互不重置/窄宽视口/header-body 同步/sticky/resize | 是（K13–K17、K19–K21、G7–G9；K15 覆盖窄/宽视口列对齐、header y 固定与 resize 稳态；resize 一帧瞬态见 R1/I3） |
| 是否留有“静默失败” | 否（AD4 取消链式，无 no-op builder；17 处编译期缺参数） |
| 是否需要引擎新增 API | 否（sticky 用 `NodeTree::set_style`/`Style` 既有公共 API；a11y 标记方案已删） |
| 滚动动画能力是否删除（裁决 C） | 是：`scroll_to_animated`/`AnimationTarget::Scroll`/`scroll_animation_target` 全删，程序化与滚轮统一立即 `scroll_to`；E31 与程序化动画取消用例删除（§7 自检、主设计 §4.9/§5.2/§5.3/§8.1） |

---

## 8. 源码引用索引（本文件新增引用）

**Chromium（本地快照 `E:\kytrix-scratch\tmp-verify\` / `E:\kytrix-scratch\_tmp\`，`main` 2026-11）**

- `cc/input/input_handler.cc`：`ScrollBegin` 136-325；`ScrollUpdate` 348-560；`kScrollEpsilon` 99；`LineStep` 1769-1771；`LimitDeltaToScrollerSize` 1773-1800；`ResolveScrollGranularityToPixels` 1805-1833；`HitTestScrollNode` 1835-1905；`ComputeScrollDelta` 1996-2021；`ScrollSingleNode` 2144-2166；`ScrollLatchedScroller` 2195-2334；`CanPropagate` 2336-2339；`FindNodeToLatch` 2341-2422；`DidLatchToScroller` 2439-2460；`CanConsumeDelta` 2462-2496；`ShouldAnimateScroll` 2498-2506；`UserScrollableDelta` 2704-2714。
- `cc/base/math_util.cc`：`SmallestAngleBetweenVectors` 794-800；`ProjectVector` 802-808。
- `cc/input/scroll_state.cc`：`ConsumeDelta` 19-25；`DeltaOrHint` 27-32。
- `cc/input/scroll_state_data.h`：delta/hint 注释 23-35；`is_beginning` 40。
- `cc/input/scroll_utils.h`：`kPixelsPerLineStep` 20；`kMinFractionToStepWhenPaging` 22。
- `cc/trees/scroll_node.h`：`container_bounds/bounds` 44-52；`user_scrollable_*` 58-59。
- `components/input/mouse_wheel_event_queue.cc`：wheel→GSU 与单位 140-216；零 delta 不发 GSB 238-264；GSE 320-343；GSB 合成 345-355。
- `third_party/blink/renderer/platform/widget/input/input_handler_proxy.cc`：`CreateScrollStateDataForGesture` 61-149（hint 78-87、**符号取负 114-137**、rail 114-137）；`HandleGestureScrollBegin` 1211-1289；`HandleGestureScrollUpdate` 1292-1301（TRACE 用 `-delta`）。
- 研究备忘：`E:\kytrix-scratch\research\chrome.md` §2.2-2.4（`:41-55`）、§3.3（`:67`）、§4（`:73-78`）、§8 未验证（`:122-128`）。

**本仓库**

- `crates/ui_core/src/interaction.rs`：`ScrollAxis` 153-157；`ScrollConfig` 159-172；`ScrollState` 196-202；`ScrollSlot` 204-210；`PointerState` 118-129；`NodeInteraction` 593-601。
- `crates/ui_core/src/interaction/scroll.rs`：`clamp` 7-13；`max_offset` 15-23；`clamp_offset` 25-32；`viewport` 34-40；`scroll_ancestors` 42-56；`scroll_ancestor` 58-61；`scroll_ancestor_on` 63-70；`ancestor_offset` 72-83；`to_screen` 85-89。
- `crates/ui_core/src/app/input.rs`：`dispatch_input_tracked` 120-132、Scroll 分发 167-182；`on_scroll` 574-683（Shift 597-601、零 delta 602-604、chain 609-627、逐祖先消费 630-682）。
- `crates/ui_core/src/app/scroll.rs`：`make_scroll` 22-49；`set_scroll_content` 51-72；`scroll_to` 99-145（no-op 133-135、damage/a11y/request_frame 136-143）；`clamp_scroll_offsets` 153-178；`scroll_state` 194-205；**`scroll_to_animated` 207-239 已删除（裁决 C）**。
- `crates/ui_core/src/app/virtual_list.rs`：`make_virtual_list` 14-49；content 覆盖 97-110、247-249、316-318；`rewindow_virtual` 166-257；`apply_virtual_extents` 285-329。
- `crates/ui_core/src/node/tree.rs`：`Node::style()` 206；`set_style` 528-530；`set_layout_override` 531-537；`set_virtual_row` 538-545。
- `crates/ui_core/src/node/tree/layout.rs`：`set_style` 46-60；`set_absolute_inset` 101-115；`set_virtual_row` 78-99。
- `crates/ui_core/src/paint/style.rs`：`Style` 19-20；`position` 56-59；`inset_top` 296-299；`inset_left` 311-314。
- `crates/ui_core/src/layout/taffy_tree.rs`：绝对定位包含块 `absolute_child_relative_to_positioned_ancestor` 587-615；`out_of_flow_hoisted_to_ancestor_uses_containing_block` 618-649。
- `crates/ui_core/src/event.rs`：`ScrollUnit`/`ScrollPhase`/`is_continuous` 299-320；`InputEvent::Scroll` 414-421。
- `crates/ui_core/src/a11y/tree.rs`：`scroll_state` 506-517；`lifted_scroll_root/window` 771-802；`a11y_scroll_target` 804-812。
- `crates/ui_core/src/a11y/mapping.rs`：滚动动作提升 286-301。
- `crates/ui_core/src/a11y/action.rs`：滚动动作分发 151-155；步长 450-464。
- `crates/ui_core/src/platform/windows/event_loop/pump.rs`：`run_frame_hook` → `frame_all` 646-654；P3 **3 行 close/gate fix**（P3 门禁改动集 37 文件之一，不改变滚动路由语义）。
- `crates/ui_core/src/app/frame.rs`：帧内 `rewindow_virtuals`/`clamp_scroll_offsets` 151-155。
- `crates/ui_kit/src/kit/table.rs`：`ColumnWidth` 37-51；`Column` 82-140；`span_cell_style` 142-186；`clamp_span` 188-190；`TableOptions` 192-222；`TableInner` 224-239；`TableGeometry` 241-248；`new` 256-395；`build` 463-678；`attach` 680-688；`sync` 706-708；`release` 407-410；`header_node` 721-734；`cell_node` 737-755；outer/body 样式 466-482 / 625-637。
- `crates/ui_kit/src/kit/list.rs`：`ListOptions` 62-74；`ListInner` 480-497；`build` 570-607（bar 开关 600-602）；`attach` 609-646（bar 开关 637-639）；`sync` 861-928；`row_builder` 1076-1137。
- `crates/ui_kit/src/kit/scroll_bar.rs`：`ScrollbarAxis` 33-60；`ScrollbarOptions` 68-123；`build` 199-275；`attach` 277-293；`release` 295-300；`sync` 340-426；`on_track_click` 428-463；`on_thumb_drag` 465-519。
- `crates/ui_kit/examples/gallery.rs`：⑧ 表格 1106-1175；⑦ 双轴演示 1084-1104；`--idle-check` 136/1981；`tick`（`run_frame_hook` 驱动 sync）2056-2115。
- 主计划书：§3.1 `:148-157`；§3.5 `:189-207`；§4.3 `:298-361`；§4.4 `:363-396`；§4.5 `:398-432`；§4.6 `:434-463`；§4.7 `:465-491`；§5.5 `:576-600`；§7.1 `:680-717`；§9.1 R13 `:824`；§9.2 O1 `:830`、O3 `:832`；附录 D `:971-975`。
