# Markdown 组件重构设计（pulldown-cmark 内核 / GFM 3|5 覆盖 / 渲染性能优先）

- 状态：**v4 已实施**——实现提交序列 `9d3b04ce`（设计定稿 v3.4）→ `9a4d3e8d`（R1 ui_core 双装饰线 + run 背景）→ `9447be99`（R2 markdown 单遍管线）→ `0973bda5`（R3 夹具 + 七条门禁 + 90 条语料 + 基准），**本文档与实现一致，由最终审查确认**；§14 是实施结果回写（收口提交在本节回写之后产生）。v2 的 17 项决策 + v3 新增的 4 项决策全部由用户裁决且全部落地；**无待用户裁决项、无未决项**。
  - 「待拍板」判别口径：本文档给出的每个行为都可直接实现，不需要实施者再做二选一决策。唯一的 **R2 首件验证项**（代码块 1 节点是否可行）已在 R2 采**备选 A**：代码块 = 1 个 `MARKDOWN_CODE` text 节点（`background` + `padding(8px)` + `corner_radius(4px)` + mono），三条判据全部断言通过，结论与证据见 §14.3。
- 范围：`crates/ui_core/src/text*`、`crates/ui_core/src/paint/pass.rs`、`crates/ui_core/src/app/canvas_text.rs`（measure_text 语义引用）、`crates/ui_kit/src/kit/markdown*`（改为 `markdown.rs` + `markdown/` 子模块）、`crates/ui_kit/Cargo.toml`、root `Cargo.toml`、7 个外部调用点
- 配套文档：**`crates/ui_kit/design/markdown-test-inventory.md`**（173 条测试逐条处置台账，R2 提交说明按它勾选）
- 本轮约束：代码零注释（AGENTS §1.6）；背景、决策与偏差只写在本文件与提交说明，不写进代码
- 证据基准：`file:line` 为 2026-11-26 工作树（设计定稿时 HEAD `71a5a5cc`）逐条核对结果。`crates/ui_core` 的 `text.rs`/`shape.rs`/`context.rs`/`pass.rs` 行号是 **R1 之前**的基线，R1 落地后这些文件已变化（R1 提交 `9a4d3e8d`），因此**R4 起不再维护具体行号**：一律以 §5.11 的符号级描述、§14.2 的公开面清单和 ui_core 文档的节名引用为准（§7-R4）；`ui_kit`/`ui_core` 之外的证据未受 R1 影响。
- 相关文档：`crates/ui_core/design/custom-gui-engine-design.md`（引擎设计先例）、`.docs/text-input-redesign-design.md`（组件重构设计先例）
- 文档位置：`crates/ui_kit/design/markdown-redesign-design.md`（D10）。`.gitignore` 的 `.*` 规则会让 `.docs/` 下的**新**文件默认被忽略（现有 `.docs/text-input-redesign-design.md` 靠已被跟踪而留在库里），`crates/*/design/` 不受该规则影响——先例 `crates/ui_core/design/custom-gui-engine-design.md` 等 3 份文档均已被 git 跟踪
- 示例代码说明：**全文所有 `rust`/`text` 片段里的 `//` 注释只为说明语义**；实施时按 AGENTS §1.6 **不写任何注释**（代码意图靠命名/类型/结构表达）。

---

## 0. 决策记录（全部「用户已裁决」）

| # | 决策点 | 结论 | 状态 | 落点 |
|---|---|---|---|---|
| D1 | 解析内核 | **pulldown-cmark 0.13.6**（`default-features = false`），删除 comrak | 用户已裁决 | §6.6、§13 |
| D2 | GFM 扩展集 | 只开 `ENABLE_TABLES \| ENABLE_STRIKETHROUGH \| ENABLE_TASKLISTS`；`ENABLE_GFM` **关闭**；**不实现 GFM 扩展自动链接**。GFM 覆盖据此为 **3/5**：表格 / 任务清单 / 删除线覆盖，扩展自动链接不支持（登记偏差 §10.1-5），tagfilter 不适用（§4.3）。与当前 comrak 配置（`parse.rs:126-133` 只开 table + strikethrough，**未启用 autolink**）**行为等价**；但与最初「完整 GFM」目标相差 1 个扩展，差距如实登记。94 条 comrak 语料归档不入库（§12.3） | 用户已裁决 | §3.1.4、§4 |
| D3 | 数据层 | **单遍：事件流 → 节点树，不保留持久 IR**；只持有 `Arc<str>` 源文本；主题切换重新解析重建 | 用户已裁决 | §5.1、§5.2、§5.7 |
| D4 | 空行语义 | **保留源码空行占位节点**（视觉与现状一致）。实现口径：每空行 **1 个 spacer 节点**（现状 2 个：wrapper + `MARKDOWN_BLANK` 盒子），高度 = **正文字行高**（`body_size × 1.4`），**只在当前解析层的根容器生成**（顶层；引用/列表/表格内部不生成，HTML 块天然不含空行）。节点预算门禁随之上调，推导与端点规范化见 §5.3/§5.4，数字以 R3 实测落档为准 | 用户已裁决 | §5.3、§5.4、§9.3 |
| D5 | 软换行 | 渲染为**换行**（保持现状；pulldown 的 HTML 输出同样是换行，浏览器折叠为空格） | 用户已裁决 | §4.2、§5.4 |
| D6 | 保真度缺口 | **①②③④ 全部实现**，其中 ①② 需要改引擎：① 同一 run 同时画下划线与删除线；② per-run 背景（水平内边距 2px + 圆角 2px，**不改变推进宽度**，覆盖行盒高度）；③ 表格列宽改为**延迟建表**：`End(Table)` 时对每个 `CellBuf` 调 `App::measure_text(None)` 取**无约束自然宽** + 固定 basis（允许 flex 收缩，**不引入 grid**，另见 D18）；④ 非 `file:` 图片回退为 alt 文本 | 用户已裁决 | §4.4、§5.5、§5.11 |
| D7 | HTML 语义 | **语义映射子集**（§5.6，四类块模型）：pulldown 只给块/行内边界，**块内标签的配对与属性解析由本组件 tokenizer 负责**（迁移 `parse.rs:26-74` + `:381-588`，合计约 230 行）。顶层 HTML 块分四类：**自包含对**（1 次嵌套解析，逐对产出）/ **只有开标签**（**乐观开窗**：push `align_windows: Vec<Align>`，不做任何源文本前视）/ **只有闭标签**（关闭窗口）/ **其他**（丢标签、文字 1 次嵌套解析）。**对齐只在建节点时写一次**：段落的 `alignment` 由 `current_align()`（窗口栈顶，仅 depth 0 主解析层）在 `Start(Paragraph)` 时确定并写入 `TextStyle`；`</p>` 作为**顶层 HTML 块**时立即出栈，作为**段落内 `InlineHtml`** 时置 `pending_window_close`、等该段落 `End(Paragraph)` flush 建完节点后再出栈；**EOF 仍开启的窗口不撤销已施加的 align**（§10.1-22）。块级白名单 `<p>/<div>/<center>/<section>` → 对齐/居中的作用域（**不建节点**）、`<h1>`-`<h6>` → 标题、`<hr>` → 分隔线；行内白名单 `<b>/<strong>/<i>/<em>/<del>/<s>/<code>/<kbd>/<a>/<img>/<span>/<br>` → 对应 marks / 图片 / 换行；未知标签丢标签保文字；深度 > 0 的 HTML 块不解析 markdown（去标签纯文本段落）。与 GitHub 原始 HTML 渲染的差异登记 §10.1-2/§10.1-3/§10.1-4 | 用户已裁决 | §5.6、§8.1 |
| D8 | 改动边界 | **允许改 ui_core**：本轮实际用到的是「多装饰集合」与「run 背景」，**不引入 grid** | 用户已裁决 | §5.11、§13 |
| D9 | 交互 | 仅渲染；删除 `link_at`/`MarkdownView`/`MarkdownHandles`/`rich_text`/`Flow*`/`ImagePaintSource` 等未接线公开面 | 用户已裁决 | §6.4、§13 |
| D10 | 文档位置 | 本文件放 `crates/ui_kit/design/markdown-redesign-design.md`（配套 `markdown-test-inventory.md` 同目录）；旧位置 `.docs/markdown-redesign-design.md` 删除 | 用户已裁决 | 头部 |
| D11 | 性能门禁 | 新增 `crates/ui_kit/benches/markdown.rs`（criterion，`harness = false`）+ 结构门禁测试（§8.4 共 7 条） | 用户已裁决 | §7-R3、§8.4、§9.2 |
| D12 | 依赖声明 | root `Cargo.toml`：`pulldown-cmark = { version = "0.13.6", default-features = false }`，删除 comrak（AGENTS §5：依赖只在 workspace 根声明） | 用户已裁决 | §6.6 |
| D13 | 测试策略 | 旧断言重写为「节点树轮廓 + 渲染图元断言」+ GFM 语料 + **测试专用 HTML 发射器**做规范一致性门禁；173 条处置落成仓库内台账 | 用户已裁决 | §8.1–§8.3、§8.6 |
| D14 | 主题切换 | **全量重建节点树**（不引入新的 `bind_style` 依赖边；根节点保留现状的外观依赖边） | 用户已裁决 | §5.7 |
| D15 | 资源上限 | 块深 64 / 行内 marks 64 / 表格 200 行 × 20 列 / 溢出段落 100 行 / HTML 嵌套解析 16 次 × 64 KB；超限降级但**保留文字** | 用户已裁决 | §5.8 |
| D16 | 文档结构 | 沿用 text-input 先例（含 §13 迁移与删除清单、§14 实施结果回写预留） | 用户已裁决 | 全文 |
| D17 | 阶段 | **方案 B：合并原 P0+P1**，不写过渡 IR 适配层，直接进入新架构（中途不保证测试全绿，但每个提交都可编译） | 用户已裁决 | §7 |
| D18 | 表格列宽实现路径 | **延迟建表**：`Start(Table)`…`End(Table)` 期间只累积 `TableBuf`/`CellBuf` 不建节点；`End(Table)` 时对每个 `CellBuf` 调 `App::measure_text(content, None)` 取**无约束自然宽**（max-content，**不是**限宽换行尺寸）→ 每列取最大值 + 2×CELL_PADDING 作为 border-box `width`；主路径**不调用 `flex_none()`/`flex_1()`**（保持 taffy 默认 `grow=0/shrink=1/basis=auto`，总宽超可用宽时按比例收缩），`min_width_0` + `overflow_hidden` + `TextWrap::BreakWord`；`measure_text` 不可用时**明确豁免**回退 `flex_1()` 等分。可用宽度建树前不可得 → **不做总宽归一化**。**不引入 grid**（`StyleBuilder` 只暴露 flex，`paint/style_builder.rs:28-49`；`Style::default` 的 `display` 恒为 `Display::Flex`，`paint/style.rs:453-463`） | 用户已裁决 | §5.10、§5.5 |
| D19 | run 背景形态 | 水平内边距 2px + 圆角 2px、**不改变推进宽度**、覆盖**行盒高度**（`line_top..line_top+line_height`） | 用户已裁决 | §5.11 |
| D20 | bench 与 xtask | bench **不纳入** `cargo xtask gates --step bench`（该命令硬编码 `-p ui_core --bench perf`，`xtask/src/gates.rs:599,701,703-704,776`），仅在提交说明与本文档声明 | 用户已裁决 | §7-R3、§11 |
| D21 | 测试范围 | 只跑 `ui_core` 与 `ui_kit` 的测试；仓库其它 crate 只要求 `cargo check` 可编译（该让步写进提交说明；产品侧受影响 crate 的测试由 R2/R4 **手工补跑**并记录结果） | 用户已裁决 | §7、§11 |

---

## 1. 摘要

现状：`comrak 0.59`（`Cargo.toml:83`）→ 自研 IR → 引擎节点树的三段式管线，**6091 行生产+测试代码 / 173 条测试**，只开 `table` + `strikethrough`，每块建 2 个节点（wrapper + 内容），每个源码空行再建 2 个节点，每个 `<p align>` 包裹块二次解析，主题切换整树删除重建。

目标：换成 **pulldown-cmark 0.13.6 的单遍管线**，删除 IR，节点数降到每内容块 1 个 + 每空行 1 个，补齐任务清单/删除线/表格的完整渲染能力与 HTML 语义子集，并建立可执行性能门禁。

**两项范围扩大（相对 v2，均来自用户裁决）**：

1. **ui_core 引擎改造**（D6-①②/D8/§5.11）：`TextStyleExtras.decoration: Option<TextDecoration>` 换成「装饰集合 + run 背景」；装饰几何支持同一 run 两条线；新增背景矩形与 `ShapedTextLine.line_top`；`paint/pass.rs` 在字形前先画背景 Quad。这是 markdown 之外的真实改动面，风险与测试清单见 §5.11。
2. **HTML 语义子集**（D7/§5.6）：**自写 tokenizer（迁移约 230 行：`parse.rs:26-74` + `:381-588`）+ 四类块模型（自包含对 / 只有开标签→乐观开窗 / 只有闭标签→关窗出栈 / 其他）+ 白名单语义映射 + 容器内容一次嵌套解析**。迁移的关键事实：**pulldown 的块级 HTML 内容一律不解析 markdown**（CommonMark §4.6 raw passthrough，四组 probe 逐事件核实），生产文案的 `<kbd>`/`![]()`/`**` 全部落在 `<p align>` 这个 raw HTML block 里，因此容器内容必须二次解析才能恢复现状语义。

| 指标 | 现状（仓库 comrak 0.59 管线） | 目标 | 依据 |
|---|---|---|---|
| 生产代码 | **2,176 行**（= 6091 − 2194 `parse_tests.rs` − 1,721 其余 4 个模块的测试代码；按「测试模块起始行」口径，±2 行） | 约 1,300–1,700 行（按模块守恒估算：保留 tokenizer ≈230 + inline ≈250 + assets ≈170 + plain ≈80；新增 build/lines/table/html ≈450–700；删除 comrak→IR ≈530 与 view 的 wrapper/IR 映射 ≈400）。**不设硬指标**，R4 记录 `wc -l` 快照 | §7 |
| ui_core 增量 | 0 | 约 250–400 行（多装饰 + run 背景 + 测试） | §5.11 |
| 每内容块节点数 | 2（release notes 实测 139 = 35×2 + 34×2 + 根 1） | 1（同一文档 70 = 35×1 + 34×1 + 根 1） | §5.3、§9.3 |
| 每空行节点数 | 2 | **1**（spacer，高度 = 正文字行高，D4） | §5.3 |
| 事件遍历次数（单条 i18n notice） | 2（comrak 主解析 + `<p align>` 内容二次解析） | **1 次主遍历 + 1 次嵌套解析**（notice 无表格 → 无 `End(Table)` 测量）；release notes 同为 1 次主遍历 + 0 次嵌套 | §5.6.5、§9.1 |
| 含表格文档 | 1 次解析（列宽等分） | **1 次事件遍历**（`End(Table)` 延迟建表 + 逐单元格 `measure_text(None)`） | §5.10 |
| 解析器分配（7 KB / 1 MB） | 184 / 22,211（comrak 1.2 量级，仓库 0.59 未测） | 4 / 4（与文档大小无关） | §3.1 |
| 表格列宽 | `flex_1` 等分 | 自然宽 basis + flex 收缩（D18） | §5.10 |
| run 装饰 | 1 条/run（链接、删除线、键帽三选一） | 2 条/run（下划线 + 删除线可同时） | §5.11 |
| run 背景 | 无 | 有（行内代码 / 键帽底色） | §5.11、D19 |
| GFM 扩展覆盖 | 2/5（table + strikethrough，`parse.rs:127-130`） | **3/5**（+ tasklists；autolink 不实施、tagfilter 不适用） | §3.1.4、§4.3 |
| 性能门禁 | 无 | criterion 基准 + 7 条结构门禁 | §8.4、§9 |

**「GFM 覆盖」的准确表述**（避免夸大）：本轮覆盖 5 个扩展中的 **3 个**（表格、任务清单、删除线，全部由 pulldown 提供）；**扩展自动链接按 D2 不实施**（与当前 comrak 配置行为等价，但距「完整 GFM」差 1 个扩展，登记 §10.1-5）；**tagfilter 不适用**（不产出 HTML，§4.3）；原始 HTML 走语义映射子集，容器内容按 markdown 解析，与 GitHub 的 raw passthrough 不等价（§10.1-2/§10.1-3）。

---

## 2. 现状调查（证据链）

### 2.1 规模与结构

| 文件 | 行数 | 测试 | 职责 |
|---|---|---|---|
| `parse.rs` | 768 | 0 | comrak → IR；`<p align>`/`<kbd>` HTML tokenizer 与跨块配对；`SourceLines`；嵌套上限 |
| `parse_tests.rs` | 2194 | 113 | IR 级断言（含 3 个穷举 sweep） |
| `document.rs` | 401 | 5 | IR 类型与 `plain_text()` |
| `inline_flow.rs` | 655 | 16 | `MarkdownPiece` → `Flow` |
| `source.rs` | 580 | 20 | `file:` 图片 URI、`MarkdownAssets`、内禀尺寸 |
| `view.rs` | 1485 | 19 | IR → 节点树、样式、`MarkdownNode` 生命周期 |
| `mod.rs` | 8 | 0 | 模块声明 |
| 合计 | **6091** | **173** | |

外部耦合面（逐条核对，见 §6.5）：`parse_document` / `MarkdownAssets` / `MarkdownNode::{mount,sync,root,release,handles}` / `MarkdownDocument::plain_text`，共 **7 个调用点**（6 个生产 crate + `ui_kit` 的 gallery example），**没有第 8 个**；直接依赖 `ui_kit` 的 package **27 个**（全部是普通 `[dependencies]`）；既有的 markdown 相关断言 **23 条**（16 条渲染 + 3 条 `plain_text` 成品 + 4 条谓词/源码级），另 3 条间接相关（§8.5）。

### 2.2 现状管线

```text
source ──comrak 0.59 (arena AST)──► MarkdownDocument{blocks, links}
                                     │（每层容器 Arc<[..]> 一次堆分配；空行由 sourcepos 差值补充：
                                     │  每个空行 → BlankLine 块 → 渲染成 wrapper + MARKDOWN_BLANK 两个节点）
                                     ├─► view::build_* ──► NodeTree
                                     └─► plain_text()  ──► SmolStr
```

### 2.3 现有自定义语义与生产语料（口径修正）

1. `<p align="left|right|center">…</p>` 整段包裹。现实现是**自写 HTML tokenizer + 跨块配对 + 内容二次 comrak 解析**：type 1–5 块整体忽略（`parse.rs:154-157`，常量 `FIRST_TAG_BLOCK_TYPE = 6` 在 `:21`）；`open_tags` 收集跨顶层块的候选开标签（`:394-427`，`OpenTag{tag, align, block_end_line}` `:381-385`）；`find_close_tag` 在**原始源文本**上向前找精确 `</p>`（`:466-489`，常量 `ALIGN_TAG_CLOSE = "</p>"` `:22`）；`pair_blocks` 对内容再解析一次（`:560-588`）。
2. `<kbd>…</kbd>`：**精确匹配** `"<kbd>"` / `"</kbd>"`（`parse.rs:609-613`），内部文本标为键帽（等宽 + 下划线 + 背景），生产里 `<kbd>` 内是行内图片（按钮图标）。
3. 源码空行 → `MARKDOWN_BLANK` 占位节点（`view.rs:417-431`），块间 `BLOCK_GAP = 8px` / `HEADING_SPACE_ABOVE = 8px`（`view.rs:27-28`）；空行计数走 `SourceLines`：`starts` 行首表 + `blank` 行标记（`parse.rs:26-74`）、`blank_between(previous_end, next_start)`（`:69-73`，口径是**行索引**而非字节片段）、`push_blank_lines`（`:366-379`，首个块 `previous_end == None` 直接返回）。
4. `file:` 图片 URI + `?size=&type=`（`source.rs:34-104`），不可加载资产降级为 16×16 占位；`file:` 图片最终是 `InlineBoxPaint::Quad { color: image_placeholder, corner_radius: [2px;4] }`（`inline_flow.rs:96-108`）。
5. 嵌套上限 `MAX_BLOCK_NESTING = 64`（`parse.rs:20`）/ `MAX_INLINE_NESTING = 64`（`parse.rs:18`）；**容器超限降级为扁平文本，行内超限直接丢弃该子树文字**（`parse.rs:599-601`，被 `parse_tests.rs:315-341` 钉死）。
6. HTML 契约由测试钉死：`parse_tests.rs:880-1429` 的 34 条 align/HTML 块测试 + 12 条 kbd 相关测试，覆盖块内跨空行、一块内多对、pair 内 heading/list/code fence、注释/引号/未闭合/CRLF 容错、精确形态、行中 `<p>` 不生效（逐条见 `markdown-test-inventory.md`）。

**i18n 富文本语料（按 `is_markdown` 谓词逐条清点，两份 locale 各 1298 个叶子）**：

| 项 | 事实（可复算） |
|---|---|
| 富文本条目 | `en.yaml` **36 条**、`zh-CN.yaml` **39 条**（合计 75 条；去重 key **39** = 共有 36 + 仅 zh-CN 3 条） |
| `<p align>` | en **35** / zh-CN **35**（合计 **70** 个开标签 + 70 个闭标签），**全部 `align="center"`**，0 处 `left`/`right` |
| `<kbd>` | en **2** / zh-CN **2**（合计 4 个），只在 `common.market_not_loaded` 与 `widget.empty.hint` |
| 行内图片 `![` | en **3** / zh-CN **3**（3 个 key × 2 语言；2 个在 `<kbd>` 内，1 个带 `?size=16`） |
| `](` | en 3 / zh-CN 3，**全部来自图片**，真正的 markdown 链接 **0 条** |
| `**` | en 1 / zh-CN 1（`widget.watch_list.market_metadata_not_loaded`） |
| 反引号行内代码 | en **0** / zh-CN **3 条**（4 个行内代码段、8 个反引号字符：`error.financial_index.unknown_category` 一条里有两段），zh 专有 |
| `<br>` / 列表 / 表格 / 围栏 / `*em*` / 实体 / 其他标签 | 全部 **0** |
| 无 `<p>` 包裹的富文本 | **en 仅 `widget.empty.hint`**（`<kbd>` + 行内图片）；**zh-CN 有 4 条**：`widget.empty.hint` + `error.financial_index.{duplicate_category,duplicate_index,unknown_category}`（后三条只走行内路径） |
| 块内真换行 | 仅 `common.market_not_loaded`（en/zh 各 1），落在 `<p align>` 块内的 3 个换行（4 行块） |

> **口径修正**：v2 §2.3 的「35 处 `<p>`、2 处 `<kbd>`」是单文件口径；两语言合计 **70 个 `<p>`、4 个 `<kbd>`**；`align` 只用 `center`，`left`/`right` 必须自造用例（§8.1「段落对齐」）。

### 2.4 现状的性能浪费点

| # | 浪费 | 证据 | 消除方式 |
|---|---|---|---|
| W1 | 每块一个 wrapper 节点只为挂 margin（文本块 2 节点/块） | `view.rs:402-457` | §5.3 节点形状 |
| W2 | 每源码空行 2 个节点（wrapper + `MARKDOWN_BLANK`） | `view.rs:416-431` | D4（降为 1 个 spacer） |
| W3 | `<p align>` 内容二次解析（新 `Arena` + 新 `comrak::parse_document`） | `parse.rs:560-569` | **保留（语义要求，不是浪费）**：D7 要求容器内容按 markdown 解析，v3 用 pulldown 嵌套解析（§5.6.3），生产 notice 第二次输入 ≤ 322 B、< 1 µs。本行保留在此以纠正 v2「解析次数 2 → 1」的错误指标 |
| W4 | 每层容器 `Arc<[T]>` 一次堆分配；每样式切换一个 `SmolStr` | `document.rs:56-176` | 删除 IR |
| W5 | 每块 `format_smolstr!("MARKDOWN_BLOCK_{n}")` | `view.rs:416-419` | 删除编号 id（只保留根 `MARKDOWN` + 静态可读 id） |
| W6 | 主题切换整树删除重建 | `view.rs:304-337` | D14（保留重建，但节点数 139 → 70） |
| W7 | `plain_text()` 先解析整树再拍平 | `document.rs:178-232` | §6.3 事件旁路 |
| W8 | 每段至少 1 个 `TextRun`，即使整段同样式 | `inline_flow.rs:199-210` | §5.5 run 合并（基础样式段 0 run） |

> W8 的准确代价：run 数进入 `ShapeKey`（`text/cache.rs:31-62`），光栅期造成 `O(items × runs)` 取色扫描；带 run 的段落在主题切换时必然重新 shaping（run 颜色入键，`cache.rs:41`），零 run 段落命中缓存。
>
> W3 的边界：只有**块级 HTML 容器**内的内容需要嵌套解析；release notes、纯 markdown 文档、`widget.empty.hint` 都不触发。

### 2.5 引擎能力边界（现状事实 + 本轮处置）

| 能力 | 现状事实 | 证据 | v3 处置 |
|---|---|---|---|
| 每个 text 节点整段 shaping 一次 | `TextContext::shape_cached` | `ui_core/src/text/context.rs:136-209` | 不变 |
| `ShapeKey` = 文本哈希 + 长度 + 基础样式哈希（**不含基础 color/alignment**）+ runs（区间/样式/**run color**）+ `normalized_runs` + inline box 几何 + `font_stack_version` | 主题换色：零 run 段落命中缓存；任何带 run 段落必然重 shaping | `text/cache.rs:31-62`、`text.rs`（`hash_into`） | 补「装饰集合 + 背景」进 `hash_into`（§5.11） |
| 文本测量 `App::measure_text(&mut self, &TextContent, Option<LogicalPx>)` | **`None` = 无约束（`AvailableSpace::MaxContent`），返回未换行的自然尺寸**；`Some(w)` = 限宽测量（返回在 w 下断行后的尺寸） | `app.rs:1892-1899`、`app/canvas_text.rs:207-233`、测试 `measure_text_returns_the_unwrapped_size`（`:409`）/`measure_text_wraps_at_the_requested_width`（`:422`） | **表格建表测量用 `None`**（D18/§5.10） |
| 每个 run 只能一个 decoration | `TextStyleExtras.decoration: Option<TextDecoration>`（R1 前） | `text.rs:190-199,320-322`（R1 前） | **R1 改为集合**（`TextDecorations`） |
| 无 per-run 背景/边框/基线偏移 | `TextStyleExtras` 无背景字段（R1 前） | `text.rs:190-199`（R1 前） | **R1 新增 `background: Option<TextBackground>`** |
| inline box 可按字节索引插入、参与断行、可画四边形或图片 | `InlineBoxSpec{id,index,size,baseline,align,paint}`；`InlineBoxPaint::{Image{resource,modulation}, Quad{color,corner_radius:[LogicalPx;4]}}` | `text.rs:750-770`（R1 前） | 不变；**`file:` 图片继续用 `Quad`**（§6.4） |
| 节点可读样式与布局盒 | `Node::style() -> &Style`、`Style::inner() -> &taffy::Style`、`Node::layout() -> LayoutBox`（border-box） | `node/tree.rs:206,257`、`paint/style.rs:28` | 门禁 7 用它读 padding 还原内容盒 |
| 布局 = taffy flex + 绝对定位；**节点样式层无 grid、无 `flex_basis`/`flex_grow`/`flex_shrink` 单独 setter** | `StyleBuilder` 只有 `flex_row/flex_col/flex_1/flex_none`（`flex_1` = grow1/shrink1/basis0；`flex_none` = grow0/shrink0/basis auto）；`Style::default()` 即 taffy 默认（grow0/shrink1/basis auto） | `paint/style_builder.rs:28-50`、`paint/style.rs:453-463` | **表格列宽靠「`width()` + taffy 默认 shrink=1」实现**，禁止调用 `flex_none()`（§5.10） |
| 文本 wrap 宽度 = 内容盒宽度（padding 已扣除） | `node/tree/layout.rs:160-176` | 已核实 | 不变 |
| 布局/文本 pass 按脏标记执行；稳定帧零重排零 shape | `TextPassReport`/`TextLayoutReport` | `text.rs`（R1 前 `:841-863`） | 门禁保持（§8.4） |
| 稳定帧的 `FrameStats` 是 `default`（提交帧才填充） | 门禁必须断言 `FrameOutcome::Idle` / `!frame_record().rendered` | `app/frame.rs:68,80,86,113,230-265`、`window/record.rs:16-23` | 门禁保持 |
| markdown 子树禁止挂 `bind_*` 依赖边；**根节点保留外观依赖边** | debug 断言 + 回归测试（子树 remove 返回空依赖边、根仍持有外观边） | `view.rs:275-284`（根 `bind_style`）、`view.rs:326-329`、`view.rs:1426` | 门禁 6 只对**子树**断言（§8.4） |
| `NodeKind::Text(Box<TextContent>)` 公开；`TextContent.{text,style,inline_boxes}` 公开、`runs()` 公开 | 测试可自建 outline | `node/tree.rs:54-58`、`text.rs:536-580`（R1 前） | 测试用它做 outline 断言 |
| `Window::size()` 可得窗口逻辑尺寸 | 但**表格挂载时的可用宽度在布局前不可得**（父节点可能是任意对话框内容列） | `window.rs:715` | `measure_text(None)` 自然宽 + flex 收缩，不需要可用宽度（D18） |

> 表中 `ui_core` 的 `text.rs`/`shape.rs`/`context.rs`/`pass.rs` 行号均为 **R1 之前**的基线；R1 落地后结构会变，以 §5.11（符号级描述）与 §14 回写为准。

### 2.6 生产消费路径与门禁覆盖边界

按「谁把 `<p align>` 富文本送到哪里」分三条路，`is_markdown` 只在第一条路上生效：

| 路径 | 生产可达量（每语言） | 入口 | 本轮是否在管线内 |
|---|---|---|---|
| A. `Notice` → `Placeholder::notice` → markdown 组件 | **21 条**（19 条 `Notice::message("…")` 富文本 + 1 条 `Notice::MarketNotLoaded`→`common.market_not_loaded` + 1 条常量间接的 `widget.empty.hint`） | `crates/widget/src/status.rs:68`（谓词）、`crates/widget/src/ui/placeholder.rs:86-98` | **在**（保真门禁主对象，§8.5） |
| B. 文案直接喂 `plain_text()` | **2 条**（`settings_ui.proxies.delete_message`、`widget.watch_list.market_metadata_not_loaded`） | `settings_ui/src/pages/proxy/table.rs:692-700`、`widgets/watch_list/src/table.rs:575-586` | **在**（§6.3 契约 + 逐字符冻结） |
| C. **不经 markdown、不经 `plain_text`，原样画原始串** | **7 组 key** | `crates/kytrix/src/shell/dialogs.rs:67-94`/`:96-128` → `crates/kytrix/src/shell/ui.rs:106-127`/`:129-149` → `NodeBuilder::text`；以及 `crates/ui_kit/src/kit/alert.rs:231` | **不在**（既有缺陷，本轮不改；门禁写明覆盖边界，§10.1-17） |

C 路已核实的 7 组 key：`startup.config_reset_message`/`_no_backup`（`kytrix/src/shell/root.rs:148-160`）、`window.workspace.delete_message`（`root.rs:413-440`）、`window.workspace.add_message`/`rename_message`（`root.rs:346`/`:383`）、`window.workspace.name_required`/`name_exists`（`root.rs:516-517`）、`window.account.awaiting_authorization`/`sign_out_confirm`（`profile_ui/src/profile.rs:883` 等 → `AlertBuilder` → kit `Alert`）、`settings_ui.market_participants.clear_credentials_message`（`settings_ui/src/pages/venues/credentials.rs:675,684-685`）、`settings_ui.save_failed.message`（`root.rs:206,443-448`）。这些路径把 `<p align="center">…</p>` 原样交给纯文本节点，**用户仍会看到标签**；本轮只登记边界（§10.1-17），不收口。

### 2.7 测试与既有断言基线

- `ui_kit` 模块内 **173 条**测试的逐条处置已落成仓库内台账：`crates/ui_kit/design/markdown-test-inventory.md`（**保留并改写 101 / 删除 72**：parse 51/62、view 16/3、document 3/2、inline_flow 11/5、source 20/0；R3 复核把 2 条链接区间断言改判删除，见 §8.6），§8.6 只给组级摘要与引用。
- 模块外既有 markdown 相关断言 **23 条**（16 渲染 + 3 `plain_text` 成品 + 4 谓词/源码级），另 3 条间接相关；基线行号见 §8.5。
- `release_notes/v0.0.3/{en,zh-CN}.md` 目前**没有任何 `.rs` 引用**：`en.md` 7,047 B / 69 行 / 34 空行 / 35 内容块（1×`#` + 3×`##` + 31 段落），`zh-CN.md` 5,887 B / 84 行 / 34 空行 / 35 内容块（其中 11 个段落带段内软换行）。两份文件都**不含 `|`**（无表格）、无 HTML/列表/围栏/引用/链接/图片/实体。

---

## 3. 选型与外部方案调研

### 3.1 pulldown-cmark 0.13.6 vs comrak

**基线的诚实说明**：仓库当前锁的是 **comrak 0.59.0**（`Cargo.toml:83`、`Cargo.lock`），实测对照用的是 crates.io 上可获取的 **comrak 1.2.0**（更新的 1.x，解析路径更快）。因此下表的比值对「现状 0.59」是**保守估计**（真实差距 ≥ 表中数字）。

实测环境：Ryzen 7 9800X3D / Win11 26300 / rustc 1.97.1；release profile（与仓库一致：`lto=fat`、`codegen-units=1`、`panic=abort`）；每个数值为 14 轮交错取最优；宿主非空闲（控制组波动 ±17%~171%），**100 KB 行噪声过大不采用**。

| 文档 | 字节 | pulldown 纯解析 | pulldown + 展示模型 | comrak 1.2 解析+遍历 | comrak 1.2 + 模型 |
|---|---:|---:|---:|---:|---:|
| i18n notice（3 条真实文案） | 525 | 1.58 µs | 0.63 µs※ | 0.84 µs | 0.73 µs※ |
| `release_notes/v0.0.3/en.md` | 7,047 | **5.40 µs** | **8.10 µs** | **11.80 µs** | **13.50 µs** |
| 合成 10 KB | 14,096 | 10.4 µs | 15.3 µs | 22.1 µs | 25.5 µs |
| 合成 1 MB | 1,000,816 | 1.12 ms | 1.41 ms | 1.84 ms | 2.18 ms |

※ 小文档受 jitter 主导，以 7 KB 行为准。

分配/峰值：pulldown **4 次分配（与文档大小无关）/ 1 MB 文档峰值 1.51 MB**；comrak 1.2 **22,211 次 / 5.83 MB**（7 KB 文档 184 次）。

结论：pulldown 快 **1.55–2.19×**（逐行由表中数据算得）、分配低 3 个数量级、峰值内存低 3.9×；**解析是主成本**（展示模型层只占 pulldown 全链路 20–33%）。

#### 3.1.1 流式（D3）vs 保留展示模型

| 文档 | 用保留模型重建一次 | 重新解析+重建（主题切换代价） | 绝对差 |
|---|---:|---:|---:|
| 7 KB | 0.84 µs | 8.1 µs | **7.3 µs** |
| 1 MB | 290 µs | 1.41 ms | 1.12 ms |

对真实文档（≤7 KB）主题切换多花约 7 µs（60 Hz 一帧预算的 0.04%），而保留 IR 要付出额外数据结构、内存与失效面 → 支持 D3「不保留 IR」。

#### 3.1.2 病态输入

20 个病态用例全部正常退出（无 panic/hang/栈溢出）：200 层引用线性、100 层强调每字节退化 22×（pulldown）/31×（comrak）、1 MB 单段更快、1 MB HTML 块 pulldown 快 3.1×；**10,000 行表格是唯一帧级成本**（pulldown 5.12 ms / 9.3 MB，comrak 13.50 ms / 17.0 MB）→ 必须设渲染上限（§5.8-D15）。

#### 3.1.3 维护与安全

| 维度 | pulldown-cmark 0.13.6 | comrak 1.2.0 |
|---|---|---|
| 发布 / MSRV / edition | 2026-10-30 / 1.71.1 / 2021 | 2026-11-23 / 1.89 / 2024 |
| 反向依赖 | 2,179 | 343 |
| 直接依赖 | bitflags、memchr、unicase（可选：getopts/serde/escape） | 更多（typed-arena 等 9 个） |
| 安全通告 | 2 条 HIGH RUSTSEC，**均只在 `ENABLE_WIKILINKS`**，0.13.6 已修 | GHSA-7f8r-4rh5-vr5h（表格二次复杂度），1.2.0 已修 |
| 第三方实测 | ferromark 2026-10-01：comrak 慢 2.17×/2.27×（与本机 1.55–2.19× 同量级） | — |

`simd` 特性在 0.13.6 里同时门控 `firstpass` 的 SSSE3 扫描与转义；实测差异在噪声内，**不启用**。

#### 3.1.4 GFM 覆盖差异与用户裁决（D2）

| GFM 扩展 | 仓库现状（comrak 0.59 配置） | pulldown 0.13.6 能力 | 本轮裁决 |
|---|---|---|---|
| 表格 | ✓（`extension.table = true`，`parse.rs:127-131`） | ✓ | **实施** |
| 任务清单 | ✗（`Options::default()` 的 `tasklist` 未启用） | ✓ | **实施** |
| 删除线 | ✓（`extension.strikethrough = true`，`parse.rs:127-131`） | ✓ | **实施** |
| **扩展自动链接** | **✗（`Options::default()` 未启用 `autolink`）** | **✗**（只有尖括号 autolink） | **不实施（用户裁决）** |
| tagfilter | ✗（0.56.0 移除） | ✗（issue #608 not planned） | 不适用（§4.3） |

结论与两点如实说明：

1. **行为等价**：仓库当前的 comrak 配置本来就没有开启扩展自动链接（`parse.rs:126-133` 只显式打开 `table` 与 `strikethrough`），所以「裸 URL / 裸邮箱不会渲染成链接」这一用户可见行为在换 pulldown 后**与现状一致**。
2. **与「完整 GFM」的差距**：本轮覆盖 3/5；扩展自动链接缺失是**真实差距**，不是「等价覆盖」。v2 §5.6 计划的自实现（约 400–600 行）按 D2 删除，94 条 comrak 0.59 黄金语料归档但不入库（§12.3）。

### 3.2 pulldown-cmark 事件模型（实测事实，全部有事件转储佐证）

- `Start(Tag)` 与 `End(TagEnd)` 携带**相同**的块级区间；块区间含容器前缀、常含尾随换行、非规范化；叶子区间不连续（实体/转义/表格 `\|` 会跳空）。**不得按区间切块文本**。
- 全程借用输入（`CowStr<'a>`）：逐字节相同的 `Text`/`Code`/`Html`/`InlineHtml`/info 串/autolink 目标是 `Borrowed`；实体、智能标点、被容器前缀切分的文本才是 `Boxed`/`Inlined`（22 字节内联）。
- `Tag::List(Option<u64>)` **只**暴露有序起始号；项目符号字符、有序分隔符、紧/松都不在事件里。**紧列表的唯一可观测特征是 `Item` 下没有 `Paragraph` 事件**。
- 任务清单：`Event::TaskListMarker(bool)`；紧列表时是 `Item` 的直接子事件，松列表时是 `Paragraph` 的第一个子事件。
- 表格：`Tag::Table(Vec<Alignment>)` → **`TableHead` 直接包含 `TableCell`（没有表头 `TableRow`）** → 表体是 `TableRow`；空单元格是零长子事件；多余单元格被丢弃。
- 图片：`Tag::Image{link_type,dest_url,title,id}`，alt 是**子事件**（可含 Emphasis/Code/嵌套图片）。
- 链接：内联/引用/折叠/简写/`Autolink`/`Email`；目标已实体解码与反斜杠反转义、不做百分号编码；`Email` 目标**不带 `mailto:`**。
- 代码块：`CodeBlockKind::Fenced(info)` 的 info 是整行 trim 后的原文；literal 为逐行 `Text`，**含结尾 `\n`**；缩进代码块是另一种 kind。
- **HTML**（逐事件实测，详见 §5.6.1 与 §12.4）：`Tag::HtmlBlock` **不带 block type**；块内容按**每行一个 `Event::Html`**（含行尾换行）；**块内内容一律不解析 markdown**（raw passthrough）；type 1–5 跨空行、按结束 token 收尾，type 6/7 被空行切断（`<p align="…">x\n\n</p>` 分成**两个**块，不是三个）；容器（引用/列表项）内的块区间不连续，`&source[a..b]` 只在顶层安全；≤3 空格缩进块内会多一个 range 为空的合成 `Text("  ")`；行内标签是 `Event::InlineHtml`（只含标签本身），两侧与标签之间的文字仍是 `Text` 并照常解析 markdown。
- 换行：`SoftBreak` / `HardBreak` 分离且不带文本。
- 实体/转义把 `Text` 打碎（内容已解码）→ **必须按容器聚合**。
- 无嵌套上限：200 层 `>`、100 层 `**` 都完整产出并配对 → 构建器必须显式栈 + 硬上限。
- **扩展自动链接不存在**；**tagfilter 不存在**。

### 3.3 成熟渲染器做法（7 个项目调研，逐项引用见 `%TEMP%\markdown-refactor\renderer-patterns.md`）

| 项目 | 做法 | 取用 |
|---|---|---|
| **Zed `crates/markdown`** | 两段式：物化「源码锚定事件序列 + 深度计数生成的块边界」，再用 div 栈 + 待处理行缓冲折叠 | **折叠期不变式**：每次进出块先 flush 行内缓冲；**不取**物化事件 |
| **gpui-component** | 块树 + 叶子 `InlineNode{text, marks: Vec<(Range,TextMark)>, source_segments}` | **叶子数据形状** |
| tui-markdown | 单遍 → `Text>Line>Span`，4 个栈 | 单遍可行性佐证 |
| egui_commonmark | 每个 `Text` 事件一个 label + 全局清零间距 | 反面教材 |
| iced | `Vec<Item>` IR + 尾部增量解析 | 不取（不做增量） |
| comrak(AST) | 迭代式 Pre/Post 栈遍历 | 迭代而非递归的佐证 |
| ratatui-markdown | 手写扫描器 + 有损 IR，无法嵌套 | 反面教材 |

四条硬结论：① 紧列表项无 `Paragraph` 是公认坑；② 「块边界先 flush」是唯一必须记住的不变式；③ 列表悬挂缩进用「标记列 + 内容列」两列 flex 实现；④ 节点预算：同类项目典型文档 40–60 节点，按事件建节点会膨胀到 400–600。

### 3.4 选型结论

pulldown-cmark 满足「只读、低占用、快」：无 AST/arena、事件借用输入、首遍只建 48 B/节点的块树。代价是必须自建层次、自写 HTML 语义子集（tokenizer + 四类块模型 + 嵌套解析）；GFM 语义面少 1 个扩展（自动链接按 D2 不实施，与现状行为等价），tagfilter 不适用；因此本组件需要自己做的是**节点形状、样式映射、表格列宽、HTML 语义子集**四件事，以及一次 ui_core 引擎改造（多装饰 + run 背景）。

---

## 4. GFM 能力矩阵与渲染映射

### 4.1 块级

| GFM 构造 | 事件 | 目标渲染 | 备注 |
|---|---|---|---|
| 段落 | `Paragraph` | 1 个 text 节点（`margin-top`） | 无 wrapper |
| ATX/Setext 标题 h1–h6 | `Heading{level}` | 1 个 text 节点（字号 = `body * scale(level)`，粗体） | 比例见 §5.3 常量表；Setext 与 ATX 同构 |
| 主题分隔线 | `Rule` | 1 个 1px 背景节点 | |
| 引用块（可嵌套） | `BlockQuote(Option<Kind>)` | 容器 + 左竖条 + 内容列（3 节点 + 子块） | `ENABLE_GFM` 关闭 → `Kind` 恒 `None`（blockquote alerts 不解析） |
| 列表（有序/无序、紧/松、嵌套） | `List(Option<u64>)` / `Item` | 容器 + 每项行（标记列 + 内容列） | 紧项无 `Paragraph` → 内容列直接是 text 节点（§5.3/§5.4 约束 4） |
| 任务清单 | `TaskListMarker(bool)` | 标记列：复选框盒（+ 选中时勾文本） | 不新增行内节点 |
| 代码块（围栏/缩进） | `CodeBlock(Fenced(info)｜Indented)` | 带背景/圆角/padding 的 **1 个 text 节点**（§5.3 验证项已定稿为备选 A，见 §14.3） | info 不上屏；literal 去掉一个结尾 `\n` |
| HTML 块 | `HtmlBlock` + `Html(行)` | **四类块模型**（§5.6.3）：自包含对 / 只有开标签→乐观开窗 / 只有闭标签→关窗出栈 / 其他 | 偏差登记 §10.1-2/-3/-4 |
| 表格 | `Table(al)` / `TableHead` / `TableRow` / `TableCell` | 容器 + 每行（行容器 + 单元格 + 行分隔线） | **列宽 = §5.10 延迟建表 + 自然宽固定列宽 + flex 收缩**；**表头行在 `Start(TableHead)` 创建**，表体行才是 `TableRow`；**表头行之后也有分隔线**（现状 `view.rs:672-677`） |
| 链接引用定义 | **无事件** | 不渲染 | 需用例钉死（§8.1） |

补充事实：表格单元格**不能包含块级构造**（含硬换行——硬换行会终止当前行、退出表格），所以单元格只需处理行内事件；`setext` 标题、表格、代码块都可以出现在列表项内（事件层面已验证）。

### 4.2 行内

| GFM 构造 | 事件 | 目标渲染 | 备注 |
|---|---|---|---|
| 文本 | 多个 `Text` | 聚合为单串 + marks | 必须合并碎片 |
| 强调 / 粗体 | `Emphasis` / `Strong` | run 样式（slant / weight 700） | 可嵌套 |
| 删除线 | `Strikethrough` | run 的**删除线**（与下划线可并存） | D6-① |
| 行内代码 | `Code` | run：等宽 + code 前景色 + **run 背景**（D6-②/D19） | 无底色 → 有底色 |
| 链接（全部 link_type） | `Link{...}` | run：链接色 + 下划线；**不保留 URL**（D9） | `Email` 目标不参与渲染 |
| **扩展自动链接** | **不实施（D2）** | — | 裸 URL/邮箱保持 `Text`，与现状等价；94 条语料归档 §12.3 |
| 图片 | `Image{...}` | `file:` → inline box（`InlineBoxPaint::Quad` 占位色块，§6.4）；否则有 alt → alt 文本；否则无（D6-④） | alt 是子事件 |
| 硬换行 / 软换行 | `HardBreak` / `SoftBreak` | 文本 `\n`（D5） | 不产出 raw 字节 |
| 反斜杠转义 / 实体 | 已解码进 `Text` | 直接使用 | |
| 行内 HTML | `InlineHtml` | 白名单（§5.6.2）→ marks / 图片 / 换行；其余丢标签保文字 | `<kbd>` 只认精确 `"<kbd>"`/`"</kbd>"` |
| 角括号 autolink | `Link{Autolink/Email}` | 同链接 | |

### 4.3 tagfilter 与「GFM 覆盖」的准确边界

- GFM §6.11 tagfilter 的机制是：**HTML 输出时**把 9 个标签（`title/textarea/style/xmp/iframe/noembed/noframes/script/plaintext`）的前导 `<` 换成 `&lt;`（cmark-gfm 的 `extensions/tagfilter.c` 只被 HTML 渲染器调用）。
- 本组件**不产出 HTML**：既没有 `&lt;` 可写，也没有 HTML 解析可供过滤。该扩展**无可实施对象**；比它更严格的做法是「原始 HTML 只走白名单语义映射」（§5.6）。这是「不适用」，**不是**「白名单等价覆盖」。
- 5 个扩展的最终归口：表格 / 任务清单 / 删除线（实施 → **3/5**）、扩展自动链接（不实施，§10.1-5）、tagfilter（不适用，§10.1-1）。

### 4.4 保真度缺口：四项全部实现

| # | 缺口 | 根因 | 本轮处置 | 引擎改动 | 验收 |
|---|---|---|---|---|---|
| ① | 链接内删除线/键帽不能与下划线同时显示 | 引擎单 decoration | **实现**：`TextDecorations{underline,strikethrough}`，装饰几何改成「每个 kind 各产一条」 | 是（R1） | 同一 run 两条线的几何用例 + `inline_flow` 断言链接+删除线并存（§5.11.4） |
| ② | 行内代码/键帽无背景块 | 引擎无 per-run 背景 | **实现**：`TextBackground{color,corner_radius,padding_x}` + shaping 期生成背景矩形 + 光栅期先画 Quad（2px 内边距、2px 圆角、不改变推进宽度、覆盖行盒高度，D19） | 是（R1） | 背景 Quad 在 GlyphRun 之前（z 序）、跨行每行一个、`StylePatch.foreground` 不覆盖背景色（§5.11.4） |
| ③ | 表格列宽等分，长文本列换行多、长 token 溢出 | 引擎无共享列宽原语 | **实现（不改引擎结构）**：`measure_text(content, None)` 取自然宽 → 每列最大 + 2×padding 作为 `width`，靠 taffy 默认 `shrink=1` 按比例收缩（D18/§5.10） | 否（用现有 API） | 同列各 cell 等宽 + 宽列确实更宽 + 压缩后比例保持（§8.4 门禁 7） |
| ④ | 非 `file:` 图片（`https:`/`icon:`/`data:`）无网络通路 | 引擎无网络图片通路 | **实现**：回退为 alt 文本；无 alt 则该位置不产节点 | 否 | §8.1 行内用例 + `parse_tests` 图片组改写（台账 §1 D 组） |

---

## 5. 目标架构

> 编号说明：v2 的 §5.6（GFM 扩展自动链接自实现）按 D2 **整节删除**，因此 v2 的 §5.7–§5.10 顺移为本文件的 §5.6–§5.9；§5.10（表格列宽）、§5.11（ui_core 引擎改造）、§5.12（验证项）为 v3 新增。

### 5.1 分层

```text
Arc<str> 源文本（MarkdownNode 持有）
  ├─ [表格] 延迟建表：Start(Table)…End(Table) 只累积 TableBuf；End(Table) 时逐单元格 measure_text(None) 定列宽（§5.10）

  ├─ pulldown::Parser::new_ext(source, GFM_OPTIONS).into_offset_iter()
  │    └─ Builder 主事件循环（显式栈、块边界 flush、空行重建）
  │         ├─ 块级 HTML（深度 0 的 HtmlBlock）：html::block_tags/segments → 单程处理（§5.6.3）
  │         │    ├─ Open 有匹配 Close：Pair → 内容 1 次嵌套解析（FragmentKind 携带 align/级别）
  │         │    ├─ Open 无匹配 Close：push align_windows（Vec<Align>，只存 align；**乐观开窗，无前视**）；
  │         │    │                该开标签之后的文字 1 次嵌套解析（align 由 FragmentKind 携带，不登记）
  │         │    ├─ Close 无匹配 Open：pop 窗口（块级 `</p>` 立即出栈；行内 `</p>` 等 End(Paragraph) flush 后再出栈）
  │         │    └─ 无白名单标签：丢标签，文字段 1 次嵌套解析 → 普通段落
  │         ├─ 顶层 Paragraph：`InlineBuf::paragraph(current_align())` —— 创建时即取窗口栈顶 align（**没有登记/回写**）
  │         ├─ 关闭路径 2：段落内的 `InlineHtml("</p>")`（depth 0）→ 置 `pending_window_close`，等 End(Paragraph) flush 后出栈
  │         ├─ 块级 HTML（深度 > 0）：拼 Html/Text 子事件去标签 → 纯文本段落（不解析 markdown）
  │         └─ 行内 HTML（InlineHtml）：白名单 marks / 图片 / 换行；`</p>` 见上「关闭路径 2」
  ├─ 嵌套解析入口 = Builder::build_fragment(slice, kind, base_depth, &mut budget)：递归进同一个事件循环
  │    （唯一入口；深度计入 MAX_BLOCK_NESTING；预算 &mut 下传；fragment 状态隔离见 §5.4 约束 11）
  └─ plain_text(source)：同一套 HTML tokenizer 取文字 + 事件旁路（§6.3）
```

### 5.2 为什么不留持久 IR（D3）

见 §3.1.1 的量化：真实文档主题切换仅多花约 7 µs，而保留 IR 要多一层数据结构、内存与失效面。备选方案（保留 IR）只需在 §7-R2 增加「事件 → IR → 节点」两次遍历与 IR 类型，其余章节不变。

### 5.3 节点形状预算与布局常量

**节点形状**：

| 构造 | 节点数 | 结构 |
|---|---|---|
| 段落 / 标题 | **1** | text 节点（块间距用 `margin-top`，规则见下表；标题字号缩放 + 粗体；段落 `align` 写入 `TextStyle.alignment`） |
| **空行** | **1** | **spacer 盒子**：`width_full` + `height(正文字行高)` + 静态 id `MARKDOWN_SPACER`，无 margin、无子节点；连续 N 个空行 → N 个 spacer（§5.4 空行重建） |
| 主题分隔线 | **1** | 1px 高背景盒（`MARKDOWN_RULE`） |
| 代码块 | **1**（R2 验证项已定稿：采备选 A） | 单个 text 节点（`MARKDOWN_CODE`）+ `PaintSpec{background: code_background, corner_radius: 4px}` + `padding(8px)` + mono 样式；三条判据全部断言通过，见 §14.3。备选 B（2 节点 = 现状形状，容器 + 内部 text）**未采用** |
| 引用块 | **3 + 子块** | 行容器（`flex_row + width_full + min_width_0`）+ 竖条（`width(3px) + height_full` + 背景）+ 内容列（`flex_col + flex_1 + min_width_0`，左 padding 8px） |
| 列表 | **1 + 每项（紧 3 / 松 ≥4）+ 子块** | 列表容器（`flex_col + width_full + min_width_0`）；每项行（`flex_row + width_full + min_width_0`）＝ 标记列 + 内容列；**紧项**（`Item` 下无 `Paragraph`）：内容列就是 text 节点本身（`flex_1 + min_width_0`）→ 3 节点；**松项**（有 `Paragraph`）：内容列是容器（`flex_col + flex_1 + min_width_0`）+ 内部块 → ≥4 节点；**任务项**：标记列 = 复选框盒（14×14、圆角 2px、描边 1.5px）+ 选中时勾文本（"✓"，8px mono）→ **紧任务项 3（未选中）/ 4（选中）；松任务项 4（未选中）/ 5（选中）** |
| 表格 | **1 + Σ每行(1 + Σcell(1–2) + 1)** | 容器（`flex_col + width_full + min_width_0`）+ 每行（行容器 `flex_row` + 每 cell（cell 容器 1–2 节点）+ 行分隔线 1px）；**表头行与每个表体行之后都有分隔线**（现状 `view.rs:672-677`）；**表格行数 = 1 表头 + N 表体** |
| HTML 块级容器 | **0** | `<p>/<div>/<center>/<section>` **只作对齐/居中的作用域，不建节点**（align 写进其内段落；居中同理）；`<h1>`–`<h6>` → 标题 text 节点；`<hr>` → 分隔线节点 |
| HTML 对齐窗口 | **0** | 窗口只是 Builder 状态（`align_windows: Vec<Align>`，**只存 align**；`</p>` 即出栈，段落创建时取栈顶），不建节点 |

**布局常量表（R2 照抄；数值来自现状 `view.rs:26-35` 与 `document.rs:36-44`，标注「本轮新增」的除外）**：

| 常量 | 值 | 现状来源 | 用途 / 规则 |
|---|---|---|---|
| `LINE_HEIGHT` | 1.4 | `view.rs:26` | 行高倍数（`body_size × 1.4` = 正文字行高） |
| `BLOCK_GAP` | 8px | `view.rs:27` | 块间距：非首块且上一块**不是 spacer** 时，`margin-top = BLOCK_GAP` |
| `HEADING_SPACE_ABOVE` | 8px | `view.rs:28` | 标题**额外**上间距：`if block_index > 0 && heading { gap += HEADING_SPACE_ABOVE }`——**不被空行抑制**（现状 `view.rs:388-395`；`a_heading_after_a_blank_line_keeps_the_extra_space_above_it` 断言的就是「空行后标题上间距 == 8px」） |
| `CODE_PADDING` | 8px | `view.rs:29` | 代码块内边距（四边） |
| `CODE_RADIUS` | 4px | `view.rs:30` | 代码块圆角 |
| `QUOTE_BORDER_WIDTH` | 3px | `view.rs:31` | 引用竖条宽 |
| `QUOTE_PADDING` | 8px | `view.rs:32` | 引用内容列左内边距 |
| `LIST_MARKER_WIDTH` | 20px | `view.rs:33` | 列表标记列基础宽；有序列表取 `max(20, 8 + 7 × 数字位数)`（`1000.` = 36px）；标记文本 `NoWrap + TextAlign::Right + muted` |
| `CELL_PADDING` | 4px | `view.rs:34` | 表格单元格内边距（四边）；cell `width = 列自然宽 + 2 × CELL_PADDING` |
| `TABLE_RULE_HEIGHT` | 1px | `view.rs:674` | 表格行分隔线高（每行之后，含表头与最后一行） |
| 字号缩放 | 1.75 / 1.5 / 1.3 / 1.15 / 1.05 / 1.0 | `document.rs:36-44` | h1–h6 |
| `MONO_FAMILY` | `"Consolas"` | `inline_flow.rs:17` | 代码/键帽等宽字体 |
| `DECORATION_THICKNESS` | 1.0px | `inline_flow.rs:19` | 链接/删除线/键帽线宽 |
| spacer 高度 | `body_size × 1.4` | `view.rs:425` | 与正文字行高一致 |
| run 背景 `padding_x` / `corner_radius` | 2px / 2px | **本轮新增**（D19） | 行内代码/键帽背景 |
| 任务复选框 | 14×14、圆角 2px、描边 1.5px、勾文本 8px | **本轮新增**（D15 任务清单） | 视觉验收可在 R2 微调，改动在提交说明记一行 |

**块间距规则（完整，实施者按此写）**：维护 `block_index`（该容器内已产出的非 spacer 块数）与 `previous_blank`：

- 首个块：`margin-top = 0`；
- 非首块且 `!previous_blank`：`margin-top = BLOCK_GAP`；
- 非首块且 `previous_blank`（上一块是 spacer）：`margin-top = 0`；
- 若该块是标题且 `block_index > 0`：`margin-top += HEADING_SPACE_ABOVE`（**不受 `previous_blank` 抑制**）。

**块 id**：只保留根 `MARKDOWN` 与静态可读 id（`MARKDOWN_CODE`、`MARKDOWN_CODE_TEXT`、`MARKDOWN_QUOTE`、`MARKDOWN_QUOTE_BAR`、`MARKDOWN_QUOTE_BODY`、`MARKDOWN_LIST`、`MARKDOWN_LIST_ITEM`、`MARKDOWN_LIST_MARKER`、`MARKDOWN_LIST_CONTENT`、`MARKDOWN_TABLE`、`MARKDOWN_TABLE_ROW`、`MARKDOWN_TABLE_CELL`、`MARKDOWN_TABLE_RULE`、`MARKDOWN_RULE`、`MARKDOWN_SPACER`）；**每块编号 id 一律删除**（`MARKDOWN_BLOCK_*` / `MARKDOWN_BLANK_*`）。产品测试按 `debug_id` **前缀**匹配（`crates/widget/src/test_support.rs:523`），`Probe::Selector` 是精确匹配，重复 id 无法区分——测试改用树路径 / `NodeKind` 遍历（§8.2）。

**代码块验证项（R2 已定稿，两个备选当时都已批准）**：备选 A（1 节点）成立的三条判据，全部以测试断言：

1. text 节点带 `padding` 时 wrap 宽度 = 内容盒宽（`node/tree/layout.rs:160-176` 已扣 padding）——断言「同一代码文本在带 8px padding 的 text 节点内换行位置 == 在内容宽相同的容器内」；
2. `PaintSpec` 背景 + `rounded(4px)` 覆盖 **border-box**（含 padding 区）——断言四角像素/`QuadRecord` 覆盖到 padding 边缘；
3. 带背景 + padding 的 text 节点首帧后进入 idle（无反复重排）——断言 `FrameOutcome::Idle`。

**结论（R2 实测）**：三条判据全部绿 → 采用 A（1 节点），§8.4 门禁 3 直接断言 70 节点，无「≤2」临时口径；证据见 §14.3。备选 B（现状形状）未采用。

**release notes 节点预算推导**（`en.md` / `zh-CN.md`：35 内容块 + 34 个顶层空行，均无 `|`/无 HTML）：

| 口径 | 计算 | 节点数 |
|---|---|---|
| 现状（实测 139） | 35 内容块 × 2 + 34 空行 × 2 + 根 1 = 70 + 68 + 1 | **139** |
| v3 目标 | 35 内容块 × 1 + 34 空行 × 1（spacer）+ 根 1 = 35 + 34 + 1 | **70** |
| 门禁阈值 | 70 + 2 余量 | **≤ 72** |

> 验收硬断言：`release_notes/v0.0.3/en.md` → **恰好 34 个 `MARKDOWN_SPACER`**、总节点 **70**（§8.4 门禁 3）；zh-CN.md 同构（**11 个多行段落 / 15 个 `SoftBreak`**，软换行合并、不产节点）。最终数字由 R3 实测落档（§12.2），若偏离 70 必须回写并说明来源。

### 5.4 构建器算法（可直接落地）

```text
状态（Builder 字段）：
  app: &mut App                    // Builder 持 App 不持 tree：需要建节点时临时 window_mut(...).tree_mut()（§5.10 借用纪律）
  containers: Vec<Container>       // 容器栈：root / quote-body / item-row / item-content / table / row / cell
  skipped: usize                   // 超深容器抑制计数（§5.8）
  inline: Option<InlineBuf>        // 当前行内缓冲（None = 不在行内）
  marks: Vec<MarkSpan>             // marks 栈（End 时弹出并断言配对；行内 HTML 也操作它）
  seen_block: bool（每容器一份）    // 是否已有块（决定 margin）
  previous_blank: bool（每容器一份）// 上一块是否 spacer
  block_index: usize（每容器一份）  // 已产出的非 spacer 块数
  lines: SourceLines               // 当前解析层的源行首表（§6.1 markdown/lines.rs）
  last_block_end: Option<usize>    // 当前解析层：上一个顶层块 End 区间的 end 字节；None = 尚未产出顶层块
  align_windows: Vec<Align>        // 对齐窗口栈（**只存 align**，§5.6.3 对齐窗口规则；depth 0 专用）
  pending_window_close: bool       // 行内 </p> 触发的「flush 之后再出栈」（§5.6.3 对齐窗口规则 3）
  html_budget: HtmlBudget          // 嵌套解析次数/字节预算（&mut 下传）
  table: Option<TableBuf>          // 延迟建表缓冲（§5.10；Start(Table) 打开、End(Table) 建表）

事件处理（伪代码）：
  Start(Paragraph)        → flush_inline(); inline = Some(InlineBuf::paragraph(current_align()))
                            // current_align() 只在 depth 0（容器栈只有 root）返回 align_windows.last()，否则 None
  Start(Heading{level})   → flush_inline(); inline = Some(InlineBuf::heading(level))
  Start(BlockQuote)       → flush_inline(); push quote 容器（3 节点）
  Start(List(start))      → flush_inline(); push list 容器；记录 ordered/start
  Start(Item)             → flush_inline(); push item 行（标记列 + **惰性内容列**）；task=false
  Start(Table(al))        → flush_inline(); table = Some(TableBuf::new(al))        // 不建节点
  Start(TableHead)        → flush_inline(); table.header = Some(Vec::new()); header=true
  Start(TableRow)         → flush_inline(); table.rows.push(Vec::new())
  Start(TableCell)        → flush_inline(); 开一个 CellBuf 行内缓冲；列序号 = 当前行已有 CellBuf 数
  End(TableCell)          → flush 到当前 CellBuf（不建节点）
  End(TableRow)           → 补齐到 alignments.len()（缺失补空 CellBuf）；行超 200 的整行转 overflow 纯文本
  End(TableHead)          → header 行同上补齐
  End(Table)              → 出 TableBuf → build_table()：先对全部 CellBuf 逐个 measure_text(None)（不持 tree 借用），
                            再一次性 window_mut(...).tree_mut() 建容器/行/单元格/text（§5.10）
  Start(CodeBlock(kind))  → flush_inline(); 进入代码模式（累积 Text）
  Start(Emphasis/Strong/Strikethrough) → marks.push(...)
  Start(Link{..})         → marks.push(link)
  Start(Image{..})        → **不 flush**：开子缓冲进入 alt 模式，End(Image) 把 box/alt 追加回父 InlineBuf
  Start(HtmlBlock)        → depth == 0 → html::block_tags/segments 单程处理（§5.6.3：Pair / push 窗口 / pop 窗口 / 文字段）；
                            depth > 0  → 拼 Html/Text 子事件去标签 → 非空白则建纯文本段落
  End(Paragraph)          → flush_inline()（在这里创建 text 节点、写入当前 align）
                            然后：if pending_window_close { align_windows.pop(); pending_window_close = false }
  End(Heading/TableCell/Item/…) → 收尾：flush_inline() → 建 text 节点 / 弹 marks / 弹容器
  Text/Code               → 追加到 inline；**若 inline == None（紧列表项/隐式文本块）自动开一个 Tight 缓冲**
  SoftBreak/HardBreak     → inline.push_break()（D5）
  InlineHtml(s)           → §5.6.2：先按白名单处理（开/闭 marks、`<br>` push_break、`<img>` box）；
                            若是 depth 0 的精确 "</p>" → 只置 pending_window_close = true（**不立即出栈**，
                            等 End(Paragraph) flush 建完节点再出栈，保证该段落创建时窗口仍开启）；
                            其余丢标签
  TaskListMarker(checked) → 当前 Item 的 checkbox 状态 = checked（不写文本）
  Rule                    → flush_inline(); 建 1px 分隔节点

顶层块的空行重建（D4，**只在当前解析层的根容器**；行号口径）：
  定义（lines.rs）：
    line_of(offset) -> usize                      // **0 基**行号：offset 所在行（offset 落在行尾序列内也算该行）
    blank_lines_between(source, prev_end, next_start) -> usize
      = max(to_line - from_line - 1, 0)
      from_line = line_of(prev_end 之前**最后一个非空白字节**)   // 见下「端点口径」
      to_line   = line_of(next_start)                   // 下一块 Start 区间起点所在行
      // 端点口径（写死）：prev_end 是上一块 End 区间的 end 字节；从 prev_end 向前跳过 `\r\n`/`\n`/`\r`/空格/TAB，
      // 取「最后一个非空白字节」所在行作为 from_line；若上一块区间全为空白，则 from_line = line_of(区间起点)。
  在创建每个顶层块之前：
    if last_block_end == Some(prev):
        n = blank_lines_between(source, prev, block_start)
        对每个空行：建 1 个 spacer（height = 正文字行高，无 margin）；previous_blank = true
    last_block_end = Some(block_end)
  端点与边界（写死）：
    * block_start = Start 事件区间 start；block_end = End 事件区间 end（**都是字节**，end 为 exclusive）；
    * 首个块：last_block_end == None → 0 个 spacer；
    * 尾部：最后一个块之后没有下一个块 → 0 个；
    * 容器内部（引用/列表/表格）与 fragment 内不生成 spacer（§5.4 约束 11）；
    * HTML 自包含块天然不含空行（type 6/7 被空行切断）；跨块配对之间的空行由主解析照常生成。
  反例（实测区间，§8.1/§8.4 钉死；**不得**再引入「若 from 指向行尾序列则跳过」这类额外位移）：
    * `a\n\nb`（para a 0..2、para b 3..4）→ line_of(1)=0、line_of(3)=2 → **1 个** spacer
    * `a\n\n\n\nb`（0..2、5..6）→ line_of(1)=0、line_of(5)=4 → **3 个**
    * `a\r\n\r\nb`（0..3、5..6）→ line_of(2)=0、line_of(5)=2 → **1 个**
    * release_notes/v0.0.3/en.md → **34 个 spacer / 70 节点**；文档首尾空白行 → **0 个**
    * list 块（`End(Item)`/`End(List)` 区间末尾常是多个换行）：`- a\n- b\n\np\n` → **1 个**；`- a\n\np\n` → **1 个**；`- a\n- b\n\n\np\n` → **2 个**；`1. x\n\np\n` → **1 个**（实测）
    * 其它块型（paragraph/heading/code/table/html）不受「非空白字节」口径影响：它们的 `End` 区间末尾同样跳过换行后取内容行，结果与四例一致。
```

必须遵守的 12 条：

1. **显式栈，禁止递归**；唯一例外是 §5.6.3 嵌套解析 的嵌套解析入口，深度计入 `MAX_BLOCK_NESTING`。
2. **按容器聚合文本**（实体/转义把 `Text` 打碎）。
3. **`TaskListMarker` 不产生文本**；item 标记列在 `End(Item)`（或首次需要时）渲染：任务项 → 复选框，普通项 → bullet/number 文本。
4. **紧列表项的隐式文本块**：`Item` 下没有 `Paragraph` 时，第一个行内事件触发「惰性内容列」：内容列直接就是一个 text 节点（`flex_1 + min_width_0`），`inline` 从 `None` 自动开 `InlineKind::Tight` 缓冲；`End(Item)` 兜底 flush。**松项**（有 `Paragraph`）时内容列是 `flex_col` 容器，段落作为其子节点。
5. **表格补列（缓冲期完成）**：在 `End(TableRow)`（表头行同样）把该行 `CellBuf` 补齐到 `alignments.len()`（缺失补**空 `CellBuf`**，不建节点）；`End(Table)` 建表时补出的列产出「cell 容器 + 无 text」（**1 节点**）；多余单元格事件丢弃。
6. **图片 alt 只用于图片**；`End(Image)` 时按 §4.4-④ 与 §6.4 规则产出 inline box 或 alt 文本。
7. **唯一不变式：块边界先 flush**（含 `End(TableCell)`、`Start(TableHead)`、`Start(TableRow)`、`Start(HtmlBlock)`）。
8. **不得依赖块区间切片**；区间只用于：空行重建（D4）、HTML 块源切片（§5.6.3）、调试。
9. **空行只由「当前解析层」的根容器生成**；容器内部空行由块间距表达（差异登记 §10.1-7）。
10. **`inline == None` 时的 `Text`/`Code` 事件**必须自动开 Tight 缓冲（否则紧列表项/隐式文本块丢字）。
11. **fragment Builder 的状态隔离、`base_depth` 语义与偏移基准**：每个 `build_fragment` 在**自己的源切片**上工作——`slice` 由调用方给出（Pair 内容 / 开标签后文字 / 其他类文字），fragment 内部**重新构造 `SourceLines::new(slice)`**，所有事件 `Range<usize>` 都是**相对 `slice`** 的偏移。被隔离并「进入保存 / 退出恢复」的字段：`lines`、`last_block_end`、`block_index`、`previous_blank`、`table`、`inline`、`marks`、`align_windows`、`pending_window_close`。其中 `table` 与 `align_windows` 在 fragment 期**独立且为空**（嵌套表格各自有列宽；fragment 内**不允许开窗**，§5.6.3 对齐窗口规则 5）。`base_depth` = 外层容器深度，**只用于累加 `MAX_BLOCK_NESTING`**；判断「depth 0 的 HtmlBlock」时按 fragment 自己的解析层从 0 计（fragment 内的 HTML 块仍走四类处理与嵌套解析，不会因外层 depth > 0 被整体降级）。type 6/7 块天然不含空行 ⇒ fragment 内 spacer 恒为 0。fragment 产出的块追加到外层容器时**只读**继承外层 `block_index`/`previous_blank`（决定 margin），不写回。
12. **`End(TableCell)` 不建节点**：表格期间只累积 `CellBuf`（§5.10），`flush` 语义改为「落到 CellBuf」；表格行/列补齐与建表都在 `End(TableRow)` / `End(Table)` 完成。

### 5.5 `InlineBuf` 与样式映射

```rust
struct InlineBuf {
    text: String,                     // 聚合文本（最终交给 TextContent::new，SmolStr 转换一次）
    marks: Vec<MarkSpan>,             // (Range<usize>, Marks)
    boxes: Vec<(usize, PendingBox)>,  // **带文本字节偏移**（引擎按 index 定位 inline box）
    kind: InlineKind,                 // Paragraph{align} | Heading(u8) | Cell{align} | Tight | AltText
}

enum PendingBox {                     // 替代被删除的 ImagePaintSource（§6.4）
    Placeholder { size: LogicalSize },// file: 图片：尺寸已由 assets:: 解析，绘制为占位四边形
    Alt(SmolStr),                     // 非 file: 且 alt 非空：以文本落地
}
```

- `Marks`：`strong / emphasis / strike / code / kbd / link`（**全部是 bool，不保存 URL**，D9）。
- 每个 mark 变化切分 run；**解析后 `TextStyle` 相等的相邻 run 合并**；基础样式段落 = 零 run。
- 样式叠加（**互相独立，不再三选一**）：
  - 链接 → `with_underline(px(1.0), Some(base.link))`；
  - 删除线 → `with_strikethrough(px(1.0), Some(base.color))`；
  - 键帽 → `with_underline(px(1.0), Some(base.code))` + `family = mono` + 颜色 `code_foreground` + 背景；
  - 行内代码 → `family = mono` + 颜色 `code_foreground` + `with_background(TextBackground{ color: base.code_background, corner_radius: px(2.0), padding_x: px(2.0) })`；
  - 于是「链接内的删除线」同时有下划线与删除线，「链接内的行内代码」同时有下划线、等宽与底色。签名（R1 已落地）：`with_underline(LogicalPx, Option<Color>)` / `with_strikethrough(LogicalPx, Option<Color>)` / `with_background(TextBackground)`。
- `file:` 图片产出：`InlineBoxSpec { id, index, size（assets:: 计算或 16×16 回退）, baseline: None, align: InlineAlign::Baseline, paint: Some(InlineBoxPaint::Quad { color: style.image_placeholder, corner_radius: [px(2.0); 4] }) }`（与现状 `inline_flow.rs:96-108` 视觉一致；真实图片资源通路不复用）。
- 代码块/表格单元格文本各**去一个结尾 `\n`**（pulldown 的 literal 含尾随换行，现状实现也如此）。
- 表格：表头行 weight 700（`header` 标志只影响该行文本 weight）；列对齐 = `alignments[column]`；**列宽见 §5.10**。
- 段落对齐：`InlineKind::Paragraph{align}` 承载「对齐窗口 / `<p align>` / `<center>` / 表格单元格」的对齐，flush 时写入 `TextStyle::alignment`；**标题恒 Left**（与现状 `content_block` 语义一致）。§8.1 需 3 条对齐用例（断言 `TextContent.style.alignment` + 首字形 x 偏移）。
- 代码块文本不进 `InlineBuf`（无 marks），直接累积到 `String`。
- **容量预置**：`InlineBuf{text, marks, boxes}` 在 flush 前按 `(事件数, 文本长度)` 预估 `with_capacity`（AGENTS §2.3）。
- **flush 判定**：`text` 非空 **或** `boxes` 非空才建节点；只有 marks 没有内容 → 不建节点。
- **空文本但含 inline box**：沿用现状做法补一个 `\u{200B}`（`view.rs:120-124`）；纯 `\u{200B}` 且无 box 的块不建节点。
- **表格单元格列序号**：用「当前行已有 cell 数」计数（表头行同样适用）。

### 5.6 HTML 语义子集（D7）

**分工（精确表述）**：pulldown 只提供**块/行内边界**——`Start/End(HtmlBlock)` 的源区间、`Event::Html` 的逐行载荷、`Event::InlineHtml` 的逐标签事件。它**不提供** block type、标签树或属性解析，且**块级 HTML 的内容一律不解析 markdown**（CommonMark 0.31.2 §4.6 raw passthrough；四组 probe 逐事件核实：`<p align="center">**bold**</p>` 不产生 `Strong`，块内 `<kbd>![](...)` 连 `Image` 事件都没有）。因此：

- **块内标签的配对与属性解析由本组件 tokenizer 负责**（跨块对齐由「对齐窗口 + 关闭事件」实现，**不扫描源文本**）：迁移现有 `parse.rs:26-74` + `:381-588` 合计约 230 行（`SourceLines`、`OpenTag`/`AlignPair`、`open_tags`、`next_tag` 引号/注释感知扫描、`open_tag` 精确形态判定、`pair_bound`/`find_close_tag`/`comment_end`、`pair_blocks` 的内容解析职责），输入从 comrak AST 换成 pulldown 事件 + 源切片。
- **容器内容必须做一次嵌套解析**（唯一嵌套解析入口）：`<p align>` 内的 `<kbd>`、`![]()`、`**` 只有在二次解析后才成为事件，而生产 75 条富文本的 markdown 全在块内（§2.3）。

#### 5.6.1 pulldown 给什么、不给什么（迁移必读的 8 条实测事实）

| # | 事实 | 证据 | 对本实现的约束 |
|---|---|---|---|
| 1 | `Tag::HtmlBlock` 是 unit variant，**不带 block type** | 0.13.6 `lib.rs:186-202,297,338`；`htmlsem` 系列转储 | 自写 `is_tag_block(payload)`（≈15 行，按前缀推断 type 1–5）替换 comrak 的 `block_type < 6`（`parse.rs:154-157`） |
| 2 | `Html` 事件**每行一个**（含行尾换行）；一个块可以有多个 | `lib.rs:560-564`；`common.market_not_loaded` 是 4 行 → 4 个事件 | 不能假设「一块一个 `Html` 事件」 |
| 3 | `Start/End(HtmlBlock)` 的 offset 覆盖整块 | 单行/多行/`<details>` 例均如此 | **用 `&source[start..end]` 取原文**，不要拼接事件（否则丢 `\r`、丢行结构） |
| 4 | CRLF：`Html` 事件只含 `\n`（`\r` 被拆掉），块区间仍覆盖 `\r\n` | `htmlsem2` 的 CRLF 例 | 取块必须用源切片；行首/空行统计用 `SourceLines`（CRLF/`\r` 感知） |
| 5 | 顶层 ≤3 空格缩进的块，块内会多一个 **range 为空的合成 `Text("  ")`**；4 空格是缩进代码块 | `htmlsem2` 的 `indent-2-spaces` | 处理 `HtmlBlock` 时忽略其内部全部 `Text`/`Html` 子事件，只用源切片 |
| 6 | 容器（引用/列表项）内的 `HtmlBlock` 区间**不连续**（跳过 `> ` 前缀） | `> <div>` 例：`Html` 区间 `@2..8/@10..14/@16..22` | 源切片只在**深度 0** 安全 → 深度 > 0 走 §5.6.3 深层规则 8 的降级 |
| 7 | 七类 HTML block：type 1–5（`<pre>`/`<script>`/`<style>`/`<textarea>`、`<!--`、`<?`、`<!X`、`<![CDATA[`）**跨空行**、按结束 token 收尾；type 6/7 被**空行切断** | CommonMark §4.6 + `htmlsem` 的 `x-type*` | type 1–5 整体忽略（现状契约）；type 6/7 进入 §5.6.3 的四类分派；`<p align="…">x\n\n</p>` 是**两个块**，因此 `<p>` 的配对**必须支持跨块**（§5.6.3 对齐窗口规则 3/4） |
| 8 | 行内 HTML 只是 `InlineHtml`（只含标签本身）；两侧与标签间的文字仍是 `Text`，**标签之间的 markdown 照常解析** | `htmlsem3`：`a <b>**x**</b> b` → `InlineHtml/Strong/Text/Strong/InlineHtml` | 行内白名单只切 marks，不吞内容；**行中的 `<p align>` 是 `InlineHtml`**（markdown 照常生效），与行首 `<p align>`（raw 块）是两条互斥路径 |

#### 5.6.2 行内 HTML（`Event::InlineHtml`）

| 标签 | 映射 |
|---|---|
| `<b>`/`<strong>`、`</b>`/`</strong>` | 开/闭 `strong` mark |
| `<i>`/`<em>` | 开/闭 `emphasis` mark |
| `<del>`/`<s>` | 开/闭 `strike` mark（渲染为删除线） |
| `<code>` / `</code>` | 开/闭 `code` mark（等宽 + code 前景 + 背景） |
| `<kbd>` / `</kbd>` | **只认精确 `"<kbd>"` / `"</kbd>"`**（现状契约：`<KBD>`、`<kbd class="k">` 不算，`parse_tests` 钉死）→ 开/闭 `kbd` mark |
| `<a>` / `</a>` | 开/闭 `link` mark；**不读也不保存 href**（D9） |
| `<img src alt>` | 属性扫描（引号感知）取 `src`/`alt`：`file:` → placeholder box；否则 alt 文本；无 alt 不产节点（D6-④） |
| `<span>` / `</span>` | 透明：不改变 marks |
| `<br>` / `<br/>` / `<br />` | 插入换行（与 D5 同一处理点；**在 code/kbd mark 内也换行**） |
| 其他（含注释、`<3`、未闭合、行中 `<p>`） | **除 `<kbd>` 外**的白名单标签按**标签名大小写不敏感**识别（`<STRONG>` 等价 `<strong>`）；`<kbd>` 仅精确小写。其余**丢标签、保留两侧文字** |
| `</p>`（段落内） | **不是 mark 标签**：depth 0 时作为「对齐窗口关闭路径 2」处理（§5.6.3 对齐窗口规则 3）；depth > 0 或栈空时丢标签 |

**行内标签栈规则（写死，逐条有用例）**：

1. `marks` 用 `Vec<InlineTag>` 栈维护；`strong/emphasis/strike/code/kbd/link` 的当前值 = 栈中是否存在该 tag。
2. **孤立闭标签**（栈中无对应 tag）→ 忽略（不改 marks、不丢字）。
3. **重复开标签**（`<b><b>`）→ 允许（幂等，压两次）；关闭时按「移除最近一个同名条目」。
4. **交叉嵌套**（`<b><i></b></i>`）→ 关闭 `b` 时移除栈中最近的 `b`，`i` 保持开启；随后的 `</i>` 关闭 `i`。即**按标签名独立配对**，不报错。
5. `<br>` 不改变 marks（换行后的 run 继承当前 marks；D5 的「换行不重置 marks」）。

#### 5.6.3 块级 HTML：标签序列 + 对齐窗口（D7 主体）

**输入**：深度 0 的每个 `HtmlBlock` 的 `payload = &source[start..end]`（§5.6.1）；块内 `Html`/`Text` 子事件忽略。

**为什么不做源文本前视**：闭标签**不一定**是独立 HTML 块——实测 `<p align="center">a\n\nb</p>` → `HtmlBlock 0..20` + `Paragraph 21..26`（`</p>` 落在**段落内的 `InlineHtml`**）；`<div>x <p…>y\n\n# Heading\n\nmore </p>` → `HtmlBlock` + `Heading` + `Paragraph`。源扫描既要处理围栏/行内代码里的 `</p>` 误判、又要复刻 CommonMark 的 7 类块边界，不可靠 → **不扫描源文本**，改为「开窗 + 建节点时取窗口 align」。

**标签序列（纯函数，无状态；一个块可含多个标签）**：

```rust
enum BlockTag { Open { align: Align, level: Option<u8>, at: usize }, Close { at: usize } }
fn is_tag_block(payload: &str) -> bool;                 // type 1–5 → 整块忽略
fn block_tags(payload: &str) -> Vec<BlockTag>;          // 按源序返回**全部**精确开/闭标签
fn segments(payload: &str, tags: &[BlockTag]) -> Vec<Range<usize>>;  // 标签之间的文字段
```

- **禁止**把接口设计成「每块一个开标签」（`open_tag(payload) -> Option<_>` 是错的）：`<p><p>`（同一 type 6 块内两个开标签）必须各自入栈。
- 非精确标签不进入 `BlockTag`（按文字处理，§10.1-2）。

**单程处理（`Start(HtmlBlock)` 分支的完整规则）**：

1. `is_tag_block(payload)` 为真 → 整块忽略（type 1–5，深层规则 1）。
2. 否则取 `tags = block_tags(payload)`，按源序单程消费：
   - **Open（块内后续有配对 Close）** → **Pair**：配对内容区做 **1 次嵌套解析**，`build_fragment(content, FragmentKind::Paragraph{align} / Heading(level), depth, &mut budget)`。
   - **Open（无配对 Close）** → **开窗**：`align_windows.push(align)`；该开标签**之后的文字段**做 1 次嵌套解析，align 由 `FragmentKind` 携带（开窗即知、**立即生效**，不需要任何登记）。
   - **Close（块内无配对 Open）** → **关窗**：见「对齐窗口规则 3」的两条关闭路径。
   - **文字段**（不属于任何 Pair、也不是开标签后文字）：合并后做 **1 次**嵌套解析（align = 当前窗口 align；无窗口 → None）。
3. **类名映射（用于 §8.1 用例与 §5.6.5 计数，不是互斥分支）**：只含成对标签 → 「自包含对」；含未配对 Open → 「只有开标签」；含未配对 Close → 「只有闭标签」；无白名单块级标签 → 「其他」。**同一块可以混合**（`<p>a</p><p>b` = 一对 + 一个开窗；`<p>a</p></p>` = 一对 + 一个落单闭标签），一律按第 2 步单程处理。

**对齐只写一次（机制 A，唯一机制）**：

- 段落的 `alignment` 在**创建节点时**写入：`InlineBuf::paragraph(current_align())` → flush 时 `TextStyle::alignment`（§5.4/§5.5）。
- `current_align()`：**仅主解析层（depth 0、容器栈只有 root）** 的段落取 `align_windows.last()`；引用/列表/表格内的段落取各自容器给定的 align（None/单元格对齐）；**fragment 内的段落取 `FragmentKind` 携带的 align**。三者互不继承。
- **没有第二套机制**：不存在「登记段落 NodeId / 先不写 alignment / 关窗时回写 / 新增引擎写入口」。（历史方案见 §14.7 的 G1/J1。）

**对齐窗口规则**：

1. `align_windows: Vec<Align>`——**只存 align**，可嵌套（`<p><p>` push 两层）。
2. **push**：块级、未配对 Open（规则 2）。
3. **pop 两条路径，优先序写死**：
   - **块级 `</p>`**（顶层 HtmlBlock，类 3）→ **立即 pop**（此时不在段落内，窗口对后续块生效）。
   - **行内 `</p>`**（`Event::InlineHtml("</p>")`，depth 0）→ 先按 §5.6.2 的白名单处理（它不是 mark 标签，不做任何 mark 变化），**只置 `pending_window_close = true`**；随后 `End(Paragraph)` 的 `flush_inline()` **建完该段落节点之后**再 pop。这样该段落创建时窗口仍然开启 → 正常取到 align。
4. **EOF 仍有窗口** → 不撤销：段落已按窗口 align 建好，窗口随文档结束丢弃（§10.1-22 登记）。
5. **fragment 内不允许开窗**：`build_fragment` 期 `align_windows` 独立且为空；fragment 内未配对 Open 只作用于**它之后的文字段**（`FragmentKind::Paragraph(align)`），不 push、不影响主解析栈。
6. **不取窗口 align 的段落**：heading（恒 Left）、列表/引用/表格内段落、fragment 内段落（取 `FragmentKind`）。
7. 窗口**不消耗**嵌套解析预算。
8. **代码上下文**：代码块内一切是 `Text`、`Event::Code` 不产生 `InlineHtml`；但正文里字面 `</p>` 会被当作 `InlineHtml` → 提前关窗，后续段落失去对齐（§10.1-4）。

**顶层段落（定义）**：

- 「顶层段落」= **主解析层根容器直接产出的段落**（`depth == 0`，不在 quote/list/table 内）。
- `<p align="center">a\n\nb</p>`：`a` 在 HtmlBlock 载荷内 → `build_fragment(FragmentKind::Paragraph(Center))` 产出（直接居中）；`b` 是主解析的顶层段落 → `Start(Paragraph)` 时窗口仍开启（`</p>` 在 b 的行内、延迟出栈），创建时即写入 Center → **两段都居中**，路径不同但都用同一个 `align` 来源。

**嵌套解析（唯一入口）**：

```rust
struct HtmlBudget { parses: usize, bytes: usize }        // &mut 下传、跨递归共享
enum FragmentKind { Paragraph(Align), Heading(u8), Plain }

impl Builder<'_> {
    /// 唯一嵌套入口：把 slice 按 markdown 解析成块并追加到当前容器。
    /// - kind 决定该 fragment 内**所有顶层段落**的 align / 标题级别；
    /// - base_depth = 外层容器深度，**只用于累加 MAX_BLOCK_NESTING**；
    ///   判断是否为「depth 0 的 HtmlBlock」时，fragment 内的顶层按**它自己的解析层**从 0 计
    ///   （即 fragment 里的 HTML 块仍走四类处理与嵌套解析，不会被当成 depth > 0 而整体降级）；
    /// - fragment 期安装 slice 自己的 SourceLines、独立 block_index/previous_blank/last_block_end/
    ///   align_windows/table/inline/marks（§5.4 约束 11），退出恢复。
    fn build_fragment(&mut self, slice: &str, kind: FragmentKind, base_depth: usize, budget: &mut HtmlBudget) -> Result<(), EngineError>;
}
```

- **预算**：每次 `build_fragment` 前检查 `parses < MAX_HTML_NESTED_PARSES && bytes + slice.len() <= MAX_HTML_INNER_BYTES`，否则不解析，退化为「去标签纯文本段落」（文字不丢）。
- **计数口径**：嵌套解析次数 = 每块「标签外文字」≤ 1 次 + Σ每个「有内容的 Pair / 作用域 unit」1 次。生产 notice（单块单对、无标签外文字）= **1**；`widget.empty.hint` = 0；release notes = 0。

**深层规则**：

1. **type 1–5 整体忽略**（`is_tag_block` 为真）：块内 `<p align>`/`<kbd>` 不得识别（现状契约）；块内文字**不落地**（`<script>`/`<style>`/注释内容不该上屏；§10.1-3）。
2. **block type 推断**：`<!--`、`<?`、`<![CDATA[`、`<!` + ASCII 字母、`<pre`/`<script`/`<style`/`<textarea`（大小写不敏感，后随空格/TAB/`>`/行尾）→ type 1–5。
3. **精确形态**：`<p>`（无属性）或 `<p align="left|center|right">`（**双引号、属性名小写 `align`、值精确、无其他属性**）；`</p>` 精确。其他写法（`<P>`、`<p ALIGN="center">`、单引号、无引号、`align = "center"`、额外属性、`justify`、`</p >`）→ **非精确 → 丢标签保文字**（§10.1-2）；属性名大小写不敏感只用于**扫描**容错，不用于精确判定。
4. **`<h*>` 与 `<hr>`**：`<h1>`–`<h6>` 在块内配对：内容 1 次嵌套解析 + `FragmentKind::Heading(level)`；**`<hr>`/`<hr/>` 没有闭标签，不属于「自包含对」**，直接产出 1px 分隔线节点（不解析内容、不计入嵌套解析次数）。
5. **`<div>`/`<section>`/`<center>`**：内容 1 次嵌套解析、**不建节点**；`<center>` → `FragmentKind::Paragraph(Center)`；`<div>`/`<section>` → `FragmentKind::Paragraph(Left)`（无 align 语义）；其内部若有 `<p align>` 对，因标签按源序消费，**内层 Pair 先被消费**；fragment 内未配对 Open 只作用于其后文字段（对齐窗口规则 5）。
6. **跨块配对由关闭事件驱动**：无前视、无跨块扫描；中间块的顶层段落照常在创建时取窗口 align。
7. **配对内容与空行**：`<p align="…">a\n\n\n\nb</p>` → 开标签块（`a` 1 次嵌套解析，Center）+ **3 个空行（主解析 spacer）** + `b</p>` 段落（主解析，创建时 Center）→ 3 spacer / 两段居中，与 D4 一致。
8. **深度 > 0 的 HtmlBlock**（引用/列表项/表格单元格内）：**不嵌套解析、不建容器**；拼 `Html`/`Text` 子事件去标签，非空白则作为**一个纯文本段落**落在当前容器（`TextWrap::Wrap`、align=None、不解析 markdown、不消耗预算）。§10.1-21。
9. **绝不静默吞字**：除 type 1–5（契约性忽略）与「块内只有标签」外，任何未识别形态都必须让文字落地。
10. **`&nbsp;` 等实体**在 HTML 块内是原文，不解码、不参与匹配。
#### 5.6.4 迁移坑（每条都有事件证据）

1. **无 block type**：必须自算 1–5 类前缀，否则 `<pre>`/`<script>`/注释里的 `<p align>` 会被误识别。
2. **别按「一块一个 `Html` 事件」写**：`common.market_not_loaded` 是 4 行、4 个事件。
3. **别拼接 `Html` 事件取块内容**：CRLF 会丢 `\r`，容器内区间还不连续 → 顶层用源切片。
4. **忽略 HTML 块内的合成 `Text("  ")`**（≤3 空格缩进块）：它 range 为空，拼进 payload 会污染文本。
5. **行中 `<p>` 是 `InlineHtml`**：其内容照常解析 markdown（`text <p …>**bold**</p>` 出 `Strong`），不要当块处理。
6. **不拼「半包裹」**：`<details>`/`<summary>` 与 `</details>` 分属不同块且中间是普通段落，不要试图还原成容器（类 4 处理）。
7. **空行/CRLF 必须用 `SourceLines`**，不要 `split('\n')`；`\r` 单独出现也算行尾。
8. **不要写源文本前视/跨块扫描**：闭标签可能在段落内的 `InlineHtml` 里（实测 `<p align="center">a\n\nb</p>` → HtmlBlock + Paragraph），源扫描不可靠；关闭只认两条事件路径（§5.6.3 对齐窗口规则 3）。
9. **窗口与事件顺序**：开标签块处理时 push `align_windows`；段落创建（`Start(Paragraph)`）时由 `current_align()` 取栈顶 align **直接写入 `TextStyle.alignment`**；`</p>` 作为顶层 HTML 块时出栈、作为段落内 `InlineHtml` 时置 `pending_window_close` 并在该段落 `End(Paragraph)` flush 后出栈。**不要**在块结束后跨帧回改样式（首帧可能已 shaping）。
10. **未闭合/非精确形态**：现状是「整块丢弃」；v3 改为「丢标签 + 文字按嵌套解析落地」、窗口在 EOF 丢弃（有意变更，§10.1-2/§10.1-4）。
11. **嵌套 `<p>` 是内容不是嵌套对**：类 1 的配对在**第一个**精确闭标签处结束（沿用 `pair_bound` 语义）。
12. **同块多对**按源顺序逐对产出；对之间的空行由主解析按 D4 生成。
13. **跨块配对内容含 heading/list/code fence**：照常用 markdown 解析；**只有主解析层的顶层段落**在创建时取窗口 align，heading/list/quote 内的段落与 fragment 内段落各自用其 align 参数（不进任何「登记表」——不存在登记表）。
14. **块内缩进**：`payload` 从 `Start` 区间开始（不含前导缩进），但块内各行仍可能带 ≤3 空格缩进——去标签后的文字保留原缩进不影响 markdown 解析（`build_fragment` 用的是内容切片）。
15. **段落内字面 `</p>`**：普通正文里出现 `</p>` 会被 pulldown 当作 `InlineHtml` → 会提前关窗（对齐可能丢失）；`Event::Code`/代码块内不会出现 `InlineHtml`，无需额外防护（登记 §10.1-4）。
16. **一个块里多个开/闭标签**：`block_tags` 必须返回 `Vec`（`<p><p>`、`<p>a</p><p>b`、`<p>a</p></p>` 都是合法输入），逐个按源序入栈/出栈；不要写「每块一个标签」的单值接口。
17. **行内 `</p>` 必须延迟出栈**：`Start(Paragraph)` 时窗口必须仍在栈上，否则该段落取不到 align；`End(Paragraph)` 的 `flush_inline()` 建完节点后再 pop（§5.6.3 对齐窗口规则 3）。
18. **fragment 内段落的 align 由 `FragmentKind` 携带**：不要塞进窗口（`align_windows` 只影响主解析层顶层段落，§5.6.3 对齐窗口规则 5/6）。

#### 5.6.5 生产适配、解析次数与保真门禁

- 生产 **36（en）/ 39（zh-CN）** 条富文本：35×2 条 `<p align="center">…</p>` 是**类 1 自包含对**（单块）→ **每条 1 次嵌套解析**（内容 ≤ 322 B）；`common.market_not_loaded`（en/zh 各 1）是 4 行块、单块自包含 → 1 次嵌套解析，产出 3 个软换行 + `<kbd>` + `Image` + 文本（probe 逐事件核实）；`widget.empty.hint`（en/zh 各 1）无 `<p>` → 纯行内路径（`<kbd>` 开/闭 + `Image`）；zh 独有 3 条反引号条目共 **4 个 `Code` 事件**（`unknown_category` 一条里两段）。
- **解析次数（如实口径）**：主事件遍历 1 次 + 嵌套解析，上界 =「每块标签外文字 ≤ 1 次 + Σ每个有内容的 Pair/作用域 unit 1 次」，且 ≤ `MAX_HTML_NESTED_PARSES`（16）。生产 notice：标签外文字 0 次 + unit 1 次 = **1 次**；`widget.empty.hint` = 0；release notes = 0。**v2 的「解析次数 2 → 1」指标与「零二次解析」门禁已撤回。**
- 门禁（§8.4 门禁 4 + §8.5）：① 每文档嵌套解析次数 ≤ 1 + 容器 unit 数、且 ≤ `MAX_HTML_NESTED_PARSES`；② **含 `<p align>` 的 20 个生产 notice（19 条 `Notice::message` 富文本 + `common.market_not_loaded`）× 2 语言 = 40 个值，各 == 1 次**；③ `widget.empty.hint`（en/zh 各 1）== 0；④ release notes == 0。
- 保真门禁（§8.5）：遍历冻结夹具（en 36 / zh-CN 39）逐条渲染 → 可见文本非空 + 无残留源码标记（`<p`、`<kbd>`、`![](`、`**`、反引号）；两条 `plain_text` 生产输入逐字符冻结。

### 5.7 主题切换与 `sync`（D14）

- `MarkdownNode` 持有 `theme: ThemeHandle`、`appearance: Appearance`、`source: Arc<str>`、`assets: Box<dyn MarkdownAssets>`、`root: NodeId`。
- `sync()`：外观未变 → false（现状语义）；变化 → 删子树、重新解析 + 建树（全量重建，D14）。HTML 容器内容随主解析一起重建，不需要独立缓存。
- **根节点的外观依赖边保留**（现状 `view.rs:275-284` 在根上挂 `bind_style(theme.appearance)`，用于外观变化时把主题信号接入该帧）；**子树不得挂任何 `bind_*`**（现状 `view.rs:326-329` 的 debug 断言）。§8.4 门禁 6 只对子树断言。
- 缓存事实：零 run 段落主题切换命中 shaping 缓存；**带任意 run（链接/粗体/代码）的段落会重新 shaping**（run color 入键，`cache.rs:36-42`），带背景的 run 还会因背景入键而重 shaping（§5.11）。一次 7 KB 文档重建 = 70 个节点 + 约 35 次 shaping，属一次性毫秒以下成本。
- 备选（不采用）：让无 run 文本节点走 `bind_style` 前景色、主题切换只跑样式 pass。代价是破坏「子树无依赖边」的不变量（`view.rs:326-329`），收益（约 7 µs）不值得，**登记为 D14 的备选**。

### 5.8 嵌套与资源上限（D15）

常量：`MAX_BLOCK_NESTING = 64`、`MAX_INLINE_NESTING = 64`、`MAX_TABLE_ROWS = 200`、`MAX_TABLE_COLUMNS = 20`、`MAX_TABLE_OVERFLOW_ROWS = 100`、`MAX_HTML_NESTED_PARSES = 16`、`MAX_HTML_INNER_BYTES = 64 * 1024`。

- **计数口径**：块容器深度 = 块级容器条目数（root 不计；quote/list/item/table/row/cell 各计 1；HTML 嵌套解析的块深度从调用方深度继续累加，因此同样受 `MAX_BLOCK_NESTING` 约束）；行内 marks 另用 `MAX_INLINE_NESTING`。
- **抑制模式**：新容器深度 > 上限 → 不建节点，`skipped += 1`；此后文本事件追加到「最深允许容器内一个惰性创建的扁平 text 节点」（块之间插 `\n`）；`End` 对齐时 `skipped -= 1`，归零退出。抑制期间不创建任何节点，退出后兄弟块不受影响。
- **行内上限**：marks 栈深度 > 上限 → 进入「丢弃」模式并 `skipped_inline += 1` 直到对应 `End`；该子树文字**丢弃**（与现状 `parse.rs:599-601` 一致，`parse_tests.rs:315-341` 钉死）。
- **表格上限**：
  - 列数 > `MAX_TABLE_COLUMNS` → 只建 20 列，超出列的 `CellBuf` 文本按空格拼进第 20 列（建表时按截断后的列数测量）；
  - 行数 > `MAX_TABLE_ROWS` → 表格只建 200 行；剩余行在 `End(Table)` 转成纯文本，**挂在表格容器所在的同一父容器下、位于表格容器之后**（普通段落、吃 `BLOCK_GAP`、不受列宽约束）；
  - 溢出段落本身上限 `MAX_TABLE_OVERFLOW_ROWS = 100`，超出不渲染（§10.1-12）。
- **HTML 上限**：嵌套解析次数 / 内层字节（§5.6.3 嵌套解析），超限退化为纯文本段落（§10.1-18）；**EOF 未关窗 → 已施加的 align 保留、不撤销**（§10.1-22）。
- 所有上限都必须让**文字可见**（除行内深嵌套的既有丢弃语义、type 1–5 的契约性忽略）。
- **节点数量级**（由 §5.3 形状推导，作为 §9.3 预算；含表头行分隔线）：200 行 × 20 列表格 = 1（表容器）+ 201 行 ×（1 行容器 + 40 cell×2 + 1 分隔线）= 1 + 201×42 = **8,443**；3 列 = 1 + 201×8 = **1,609**；补列 cell 只有 1 节点，故实际更小。对比「不设上限的 1 万行 × 3 列」= 6 万+ 节点。

### 5.9 错误与降级

- pulldown 不会返回错误；`MarkdownNode::mount` 的 `EngineError` 只来自引擎节点创建（现状一致）。
- 资产缺失：`file:` 图片降级为默认占位尺寸（沿用 `assets.rs` 的 16×16）；非 `file:` 图片见 §4.4-④。
- 表格建表时降级判据 = **`App::font_stack_status() != FontStackStatus::Ready`**（`app.rs:1947`）→ 该表所有 cell **明确豁免**回退 `flex_1()` 等分（D18 只约束主路径；用注入空字体栈的测试复现，文字不受影响）。
- HTML 嵌套解析超预算 → 该 unit 退化为纯文本段落；**EOF 未关窗 → 已施加的 align 保留**（§10.1-22）。
- 任何形态都不允许出现整块文字消失（type 1–5 的忽略是有意契约）。

### 5.10 表格：延迟建表 + `End(Table)` 逐单元格测量（D18）

**目标**：同列等宽、宽度由**真实单元格内容**的无约束自然宽决定；**不引入 grid**、**不依赖布局前不可得的可用宽度**、**不需要第二遍事件遍历**、**嵌套解析里的表格天然覆盖**。

**测量语义（必须用对）**：`App::measure_text(&content, None)` 返回**无约束（`AvailableSpace::MaxContent`）的自然尺寸**（`app/canvas_text.rs:207-233`；`None → MaxContent`、`Some(w) → Definite(w)`；空内容返回零尺寸；NaN/∞/负数被归一为 `MaxContent`）。**禁止**传可用宽。

**机制：延迟建表（`TableBuf` / `CellBuf`）**

```rust
struct CellBuf { text: String, marks: Vec<MarkSpan>, boxes: Vec<(usize, PendingBox)> }
struct TableBuf {
    alignments: Vec<Align>,
    header: Option<Vec<CellBuf>>,
    rows: Vec<Vec<CellBuf>>,
    overflow: Vec<String>,        // 超出行上限后每行一条纯文本
}
```

1. `Start(Table(al))`：`flush_inline()`；push `TableBuf { alignments: al }`；**不建任何节点**。
2. `Start(TableHead)` / `Start(TableRow)`：flush；进入行模式（表头行 → `header`，表体行 → `rows`）。
3. `Start(TableCell)` … `End(TableCell)`：用**同一套**行内处理（marks / 图片 / 换行）累积到 `CellBuf`，**不建节点**；列序号 = 当前行已有 `CellBuf` 数。
4. `End(TableRow)`（含表头行）：补齐到 `alignments.len()`（缺失补**空 `CellBuf`**）；行数上限 `MAX_TABLE_ROWS = 200`（表体），超出部分转成 `overflow` 中的一条纯文本（单元格文本用单个空格连接），**不丢字**。
5. `End(Table)`：出 `TableBuf` 并建表：
   - ① 对每个 `CellBuf` 构造**与建树同样式**的 `TextContent`，`measure_text(&content, None)` 取自然宽；每列取 `max`；列宽 `width = max_natural + 2 × CELL_PADDING`（taffy `size.width` 是 **border-box**，padding 挤占内容宽；全空列取 `2 × CELL_PADDING`）。
   - ② 建节点：表格容器（`flex_col + width_full + min_width_0`）→ 表头行（`flex_row`）→ 表体行 → 每 cell（`width(列宽)` + `min_width_0` + `padding(CELL_PADDING)` + `overflow_hidden`）→ 内部 text（`TextWrap::BreakWord`）→ 每行之后行分隔线（1px，**含表头与最后一行**）。
   - ③ `overflow` 的每条：一个纯文本段落，挂在**表格容器所在的同一父容器**下、位于表格容器之后（普通段落、吃 `BLOCK_GAP`、不受列宽约束），最多 `MAX_TABLE_OVERFLOW_ROWS = 100` 条。
6. **列数上限**：`alignments.len() > MAX_TABLE_COLUMNS = 20` 时截到 20 列，超出列的 `CellBuf` 文本按空格拼进第 20 列。
7. **样式约束**：cell 容器**不要调用 `flex_none()`/`flex_1()`**——保持 taffy 默认 `grow=0 / shrink=1 / basis=auto`（`paint/style.rs` 的 `style_default_is_taffy_default`），总自然宽 > 可用宽时按 flex base 比例收缩；`min_width_0` 允许压到 0；`width()` 给 border-box 宽。
8. **嵌套与多表**：每个 Builder 实例（根 Builder + 每个 `build_fragment` 的 fragment Builder）各自持有自己的表缓冲 → 一个文档可含任意多张表、嵌套解析里的表格同样有列宽，**不需要跨实例映射**。
9. **降级（判据写死）**：`App::font_stack_status() != FontStackStatus::Ready`（`app.rs:1947`）→ **该表的所有 cell 回退 `flex_1()` 等分**（这是 D18「主路径禁用 `flex_1()`」的**明确豁免**，用注入空字体栈的测试复现，断言文字仍在）。
10. **不覆盖**：HTML 块内的 `<table>` 属白名单外标签（§5.6.3 深层规则），丢标签保文字，不做列宽。
11. **借用纪律（写死）**：Builder 持 `&mut App`、**不跨调用持 `&mut NodeTree`**。`End(Table)` 内部严格两段：① 先对全部 `CellBuf` 构造 `TextContent` 并逐个 `App::measure_text(&content, None)`（此时**不持任何 tree 借用**，得到列宽向量）；② 再 `app.window_mut(window).ok_or(...)?.tree_mut()` 一次性建容器/行/单元格/text。禁止「先取 tree 再测量」（`measure_text` 要 `&mut App`，与 `tree_mut()` 的借用无法共存）。

**成本**：① 单表内存 = `rows × cols` 个 `CellBuf`（受 200×20 上限约束）；② `End(Table)` 时每单元格一次 `measure_text`（≤ 201×20 ≈ 4,020 次，无布局、会预热 shape 缓存），mount 期一次性；③ **不需要**第二遍事件遍历、不需要源码近似、不产生额外解析。合成基准 `table_widths_200x20` 落档（§9.2）。

### 5.11 ui_core 引擎改造规格（D6-①② / D8）

> 依据：`%TEMP%\markdown-impl\engine-run-decoration-background.md` + 两份引擎改造审查（含 6 条补充，见 §14.7 的 I1–I6）；R1 已在实现中，落地形状与本节一致（行号以 R1 提交后的 §14 回写为准，本节标注的「R1 前」指 HEAD `71a5a5cc`）。
> 本节的 `rust` 片段带 `//` 注释，**只为说明语义**；实施时按 AGENTS §1.6 **不写任何注释**（代码意图靠命名/类型/结构表达）。

#### 5.11.1 数据形状（`crates/ui_core/src/text.rs`）

```rust
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextDecoration {          // 一条线（原枚举降级为参数结构）
    pub color: Option<Color>,
    pub thickness: LogicalPx,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum TextDecorationKind { Underline, Strikethrough }   // ALL = [Underline, Strikethrough]

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TextDecorations {
    pub underline: Option<TextDecoration>,
    pub strikethrough: Option<TextDecoration>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextBackground {          // run / base 底色（D19 的 2px 与 2px 随身携带）
    pub color: Color,
    pub corner_radius: LogicalPx,    // 2px
    pub padding_x: LogicalPx,        // 2px（水平内边距，不改变推进宽度）
}

impl TextBackground {
    /// 与 `TextDecoration::is_visible` 同语义：颜色全透明、`corner_radius`/`padding_x` 为 NaN/∞/负数
    /// 时视为不可见。shaping（`background_segments`）与两条 paint 路径都必须先过这一层过滤。
    #[must_use]
    pub fn is_visible(self) -> bool;
}

pub struct TextStyleExtras {
    /* 现有字段 */
    pub decorations: TextDecorations,   // 取代 Option<TextDecoration>
    pub background: Option<TextBackground>,
}

pub(crate) struct ShapedTextLine {
    pub text_range: Range<usize>,
    pub baseline: LogicalPx,
    pub line_top: LogicalPx,              // 新增：首行 = px(block_min_coord)，其后按 line_height 累加
    pub line_height: LogicalPx,
    pub advance: LogicalPx,
    pub items: Vec<ShapedTextItem>,
    pub decorations: Vec<ShapedQuad>,
    pub backgrounds: Vec<ShapedQuad>,     // 新增：rect 高度 = line_height
}

pub(crate) struct ShapedQuad {            // 原 ShapedDecoration 更名并加圆角
    pub rect: LogicalRect,
    pub color: Color,
    pub corner_radius: LogicalPx,         // 装饰线 0，run 背景 2px
}
```

- 不用 bitflags：颜色/厚度本来就按 kind 分开；`Option<TextDecoration>` 每 kind 的模型与 parley 0.12 的 `Style{underline, strikethrough}` 同构；`TextStyleExtras` 已装箱，尺寸不敏感。
- `TextStyle` 尺寸断言 `size_of::<TextStyle>() == 56` 保持不变。
- **`TextRun` 不变** → 外部构造点零改动（`ui_kit/.../inline_flow.rs`、`widgets/x/src/view.rs:2012`）。
- **`TextStyle` 的装饰访问器改名会波及 `ui_kit`**：`ui_kit/src/kit/markdown/inline_flow.rs` 有 8 处外部使用——R1 前基线 `:8`（`use …TextDecoration`）、`:301-314`（3 处 `with_decoration(TextDecoration::underline/strikethrough(...))`）、`:417`（`.decoration().is_none()`）、`:469-470`/`:479-480`/`:507-508`（3 处 `matches!(…, TextDecoration::Underline{..})` 与 `.decoration()`）、`:325`（测试 re-export）。**R1 必须一并做最小类型适配**（改成 `with_underline(…)/with_strikethrough(…)`、`decorations()`/`decorations().underline`），否则 R1 提交无法 `cargo check --workspace`（§7-R1）。
- 新增 `TextStyle` API：`decorations() -> TextDecorations`、`background() -> Option<TextBackground>`、`with_underline(LogicalPx, Option<Color>)`、`with_strikethrough(LogicalPx, Option<Color>)`、`with_background(TextBackground)`。
- **`TextBackground::is_visible()`（硬要求）**：与 `TextDecoration::is_visible` 同语义——`color.is_transparent()`、`corner_radius`/`padding_x` 非有限（NaN/∞）或 `padding_x < 0` → 不可见。**shaping（`background_segments`）与 paint（`paint/pass.rs` 的 `emit_text` 与 `window/canvas_text.rs` 的非加速路径）都必须先调用它过滤**，否则 NaN 会进 Quad 几何、透明色会被当成有背景（影响快路径判据与 `is_simple()`）。

#### 5.11.2 必须修改的 match 点与快路径

| 符号（文件） | 现状 | 改法 |
|---|---|---|
| `TextDecoration::{with_color,color,thickness,hash_into}`（`text.rs`） | 枚举上的 4 个 match | 改成单线结构上的方法；`TextDecorations::hash_into` 逐 kind 写「present + `color_bits` + thickness bits」，`TextBackground` 写「present + color + radius + padding」 |
| `TextStyle::hash_into`（`text.rs`，R1 前 `:468-502`） | `decoration()` 单一分支 | 换成 `decorations().hash_into` + `background().map(...)`；**漏补会导致缓存键碰撞**（`cache.rs:102-118` 覆盖写 → 每帧重复 shaping） |
| `TextStyle::{decoration,with_decoration}`（R1 前 `:320-322,463-466`） | 单值与单 setter | 换成 `decorations/background` + 三个 setter |
| `decoration_segments`（`shape.rs`，R1 前 `:939-1001`） | 每 run 只 `filter_map` 出一条装饰；base 兜底单 kind；变体 match 决定 offset | 「每 kind 各收集一条」（R1 抽出通用 `covered_segments`），base 兜底按 kind 外层循环，kind → offset 表（`UNDERLINE_OFFSET_EM`/`STRIKETHROUGH_OFFSET_EM` 保留） |
| `background_segments`（新增，`shape.rs`） | — | run 的 `background()` + base 背景兜底；x 复用 `decoration_span`；rect = `(left - padding_x, line_top, width + 2×padding_x, line_height)`；相邻同色相接**先合并区间再扩边** |
| `rasterize_parley`（`shape.rs`，R1 前 `:1050-1209`） | 组装 `ShapedTextLine` | 首行取 `line_top = px(metrics.block_min_coord)`、其后逐行累加 `line_height`（与 `line_height` 同源、严格相接），调用 `background_segments`，补 `backgrounds` |
| `clamp_line_decorations`（`shape.rs`，R1 前 `:1302-1317`） | 只 clamp `decorations` | 泛化为 `clamp_line_rects`，同时作用于 `backgrounds` |
| 数字快路径资格（`context.rs:168-173`） | `decoration().is_none()` | 「`decorations().is_empty()` **且** `background().is_none()`」 |
| `ShapedTextLine` 字面量 | `fast_path.rs:180`、`shape.rs:1188`、`window/canvas_text.rs:77`、`window/tests.rs:625`、`pass.rs:2921`（测试助手） | 补 `line_top`、`backgrounds` |
| `window/canvas_text.rs`（**非加速 canvas text 绘制路径**） | 只画字形与装饰线，不画 run 背景 | **R1 必须同步画背景**：与 `paint/pass.rs` 用同一份 `ShapedTextLine.backgrounds`、同一 z 序（字形之前）、同一 `corner_radius`/颜色；两条路径行为必须一致（否则软件路径与 GPU 路径观感不同） |
| `emit_text` + `emit_solid_quad`（`paint/pass.rs`，R1 前 `:1625-1769`/`:1769-1797`） | `emit_solid_quad` 原为 4 参（`context,bounds,color,opacity`）且硬编码 `corner_radius: [ZERO;4]`，**HEAD 调用点 3 处（`:1642`/`:1739`/`:1761`）** | **扩为 5 参**（新增 `corner_radius: LogicalPx`），全部调用点同步（R1 前 3 处、R1 后 5 处 = 原 3 + 背景 2）；`emit_text` 在**选区 Quad 之前**、`items` 字形循环之前插入背景 Quad 循环（传入 `background.corner_radius`；选区半透明色覆盖在背景之上是期望行为）；背景**不**走 `StylePatch.foreground` |
| `paint/pass.rs` 的私有 `struct TextDecorations`（R1 前 `:1238`，可编辑文本的选区/下划线/caret 聚合） | 与新增的公开 `text::TextDecorations` **同名但不同模块**（无编译冲突，但同名会让后续实现者混淆） | **R1 必须改名为 `EditDecorations`**（纯改名、无行为变化）；`text_decorations()` 函数名保留（它不是类型） |
| `ui_kit/.../inline_flow.rs:301-314` | `if link … else if strike … else if kbd` 三选一 | 三个独立 `if` + `if mark.code { with_background(...) }`（R1 只做类型适配，语义切换在 R2） |
| 既有测试 | `text.rs`（装饰 helper）、`cache.rs`（键区分）、`shape.rs`（装饰几何 7 组）、`pass.rs`（Quad 编码/foreground） | 按集合/背景改写断言（§5.11.4）；`shape.rs` 的 RTL 用例扩展到背景 |

#### 5.11.3 run 背景的几何与绘制顺序（D19）

- **x 区间**：复用 `decoration_span`（已处理 visual order / RTL / 双向）。
- **y 范围**：`line_top .. line_top + line_height`（行盒，含 leading）；`line_top` 是**文本局部坐标**——`shape.rs` 里从 `LogicalPx::ZERO` 起算，首行 0（等价于 `block_min_coord`，paint 期再加文本盒 origin 才是窗口坐标），之后按 `line_height` 逐行累加，**不能用 baseline 近似**。
- **水平内边距 2px + 圆角 2px**：矩形 = `(left - padding_x, line_top, width + 2 × padding_x, line_height)`，`corner_radius = 2px`；**不改变推进宽度**（不参与断行、不改 `advance`）。
- **`line_top` 与 `line_height` 必须同源（硬约束）**：相邻行的背景矩形必须**严格相接**——`line_top_{i+1} == line_top_i + line_height_i`。**实现口径 = 按行累加**：`line_top_0 = LogicalPx::ZERO`（**文本局部坐标**的首行，等价于 `block_min_coord`，不是窗口绝对坐标；`crates/ui_core/src/text/shape.rs:1163` 起算、`:1302` 累加），之后 `line_top_{i+1} = line_top_i + line_height_i`（备选「两者取同一取整基准」不采用）。**禁止**一个来自 parley 的浮点坐标、另一个来自累加/取整后的值（否则逐行背景重叠 1 设备像素或留缝）。R1/R3 必须用**分数行高**（默认主题 `body_size 14 × 1.4 = 19.6px`）的多行用例钉死「无重叠、无缝隙」。
- **分组与 padding 只加在组两端（硬约束）**：先按「**同色 + 同圆角 + 同 `padding_x`**」分组 → **组内**合并区间 → `padding_x` **只加在该组最左端与最右端**。不同颜色/圆角/内边距的相邻 run **不合并、也不各自外扩**（若各自外扩，组间会出现 2×padding 的双绘区，半透明底色在该处偏深）。
- **两条绘制路径一致**：GPU 路径（`paint/pass.rs` 的 `emit_text`）与非加速 canvas text 路径（`window/canvas_text.rs`）都必须画 run 背景，z 序相同（GlyphRun 之前）、几何数据同源。
- **z 序**：背景必须在 GlyphRun / InlineBox 之前 push（lane 保序，`paint/scene.rs:183-193`）。
- **裁剪一致性**：椭圆省略 / `max_lines` 截断时背景与装饰一样 clamp。
- **哈希**：背景与装饰集合都进 `TextStyle::hash_into`；`same_shaping_inputs` 的 `extras ==` 自动覆盖判据。

#### 5.11.4 新增 / 改写的测试（R1 交付物）

| 文件 | 测试 | 断言 |
|---|---|---|
| `text/cache.rs` | `shape_key_distinguishes_decoration_sets_and_backgrounds` | 装饰集合（underline / strike / both）两两不同键；背景按**单字段**变化（颜色 / 圆角 / 内边距各一条）都必须换键（漏哈希任一字段即变红） |
| `text/cache.rs` | `shape_key_distinguishes_base_style_decorations` | 仅基础样式 decoration 不同 ⇒ 换键 + 判据失配 |
| `text/context.rs` | `decorated_or_highlighted_numbers_bypass_the_fast_path` | 带装饰或背景的数字 NoWrap 文本 `!is_simple()`；仅装饰集合不同 ⇒ 首次 `!hit`、再次 `hit`（`Rc::ptr_eq`）；透明背景归一化为 `None` ⇒ 仍命中且 `is_simple()` |
| `text/shape.rs` | `one_run_draws_underline_and_strikethrough_together` | 同一 run 两条线、y 分布正确、厚度各自生效 |
| `text/shape.rs` | `run_background_covers_the_line_box_with_padding_on_every_line` | 跨软换行每行一个矩形，`y == line_top`、`height == line_height`、水平各外扩 `padding_x`、圆角 `corner_radius` |
| `text/shape.rs` | `background_survives_hard_line_breaks` | `\n` / `\r\n` / 空行三种硬换行各自每行一个矩形、末行宽 = advance + 2×padding |
| `text/shape.rs` | `empty_and_whitespace_background_runs_stay_bounded` | 空 run 无矩形、纯空白 run 有界、纯空白行无矩形 |
| `text/shape.rs` | `base_and_run_backgrounds_keep_their_own_spans` | base 与 run 背景各占其 span（外侧与**内侧**边界都钉死）、兜底不覆盖 run、组间接缝严格相接 |
| `text/shape.rs` | `fractional_line_height_keeps_background_boxes_touching` | 分数行高（19.6px）多行：`line_top_{i+1} == line_top_i + line_height_i` |
| `text/shape.rs` | `adjacent_different_backgrounds_do_not_double_pad_the_seam` | 相邻异色/异圆角/异 padding 两组：组间无 2×padding 双绘 |
| `text/shape.rs` | `narrow_gap_between_backgrounds_is_split_between_them` | 窄缝（`0 < gap < 2×padding_x`）时每侧取 `min(padding_x, gap/2)`，两矩形严格相接不重叠 |
| `text/shape.rs` | `rtl_background_covers_the_visual_span` | 两个不同底色 RTL run：视觉左/右归属与各自颜色、相邻严格相接；3-run 红/蓝/红 不合并被隔开的同色 |
| `text/shape.rs` | `ellipsis_clamps_background_into_the_kept_line` | 只截右端到 `line_offset + advance`，左端保留 `padding_x`（−2px） |
| `text/shape.rs` | `invisible_backgrounds_produce_no_geometry` | 不可见背景（透明色 / NaN / 负 padding）不产矩形 |
| `text.rs` | `text_decorations_carry_underline_and_strikethrough_together`、`text_decoration_lives_in_extras`、`text_background_lives_in_extras`、`text_background_visibility_rejects_transparent_and_bad_geometry` | 数据形状与可见性判据；`with_background` 对不可见背景归一化为 `None`（不写字段、清旧值、回收空 extras） |
| `paint/pass.rs` | `run_backgrounds_are_drawn_before_glyphs_and_ignore_foreground` | 背景 Quad 在选区与字形之前 push；`StylePatch.foreground` 只改字形与装饰线；组 opacity 照乘到背景色；透明 Quad 不产实例 |
| `paint/pass.rs` | `canvas_text_recipe_draws_run_backgrounds_before_glyph_runs` | 非加速 canvas 路径：背景 Quad 先于 GlyphRun，几何与 GPU 路径一致，透明 Quad 不产实例 |
| `paint/pass.rs` | `placeholder_line_draws_run_backgrounds` | 占位/回退路径也画背景（背景循环在占位文字色短路之前） |
> **`ui_kit/.../inline_flow.rs` 的三条语义改写**（链接 + 删除线并存、行内代码底色、kbd 下划线 + 底色）**归 R2**（R1 只做类型适配、保持旧 else-if 语义，见 §5.11.2 末行与 §7-R1）；对应 3 条测试随 R2 交付。

#### 5.11.5 风险与性能影响

- 风险 1：**哈希/判据漏补** → 键碰撞 + 判据失配 → 每帧重复 shaping、互相踢缓存（输出仍正确，性能悬崖）。
- 风险 2：**快路径漏排除** → 背景/装饰不画。
- 风险 3：**z 序**（背景画在字上）、**裁剪不一致**、**run 边界接缝**、**RTL/双向**。
- 性能：每个有背景的 run 每行 +1 个 Quad（无纹理、单实例）；7 KB 文档增量 < 100 Quad；塑形次数不增；缓存键每 style 约多 60–80 B。压测路径 `crates/ui_core/benches/perf.rs:222`（`text_shape_32`）。
- 尺寸：`ShapedTextLine` 每行 **+24 B**（`line_top` 4 B + `backgrounds` 24 B；现结构 80 B → 104 B，尾部 padding 合并）；16 MB 形状缓存预算不变。

### 5.12 R2 的验证项与「不需要实施者拍板」的清单

| 项 | 性质 | 落点 |
|---|---|---|
| 代码块 1 节点是否可行 | **已定：备选 A（1 节点）**，三条判据见 §5.3，结论与证据见 §14.3 | R2 首件；门禁 3 直接断言 70 节点 |
| 表格列宽的具体数值 | **不是决策**：由 `End(Table)` 的 `measure_text(None)` 与 flex 收缩决定，无人工参数 | §5.10 |
| HTML 嵌套预算的触发点 | **不是决策**：上限写死在 §5.8，超限行为写死在 §5.9/§10.1-18 | §5.6.3 |
| 任务复选框/背景的具体像素 | **不是决策**：初值见 §5.3 常量表，R2 视觉验收微调并记一行 | §5.3 |

### 5.13 引擎公开 API：除 §5.11 的双装饰线 + run 背景外**不新增入口**

v3.3 起对齐只写一次（§5.6.3「对齐只写一次」），不再有「节点建好之后回写样式」的需求，因此**除 §5.11 已批准的双装饰线 + run 背景公开面之外**（`TextDecoration` / `TextDecorations` / `TextBackground` + `TextStyle::{decorations, background, with_underline, with_strikethrough, with_background}`，最终清单见 §14.2），**本轮不新增任何 `ui_core` 公开 API**：v3.2 曾计划的 `NodeTree::set_text_style` 已按 §14.7 的 J1 删除。R1 的其余实现只使用现有公开面：

| 用途 | 现有公开 API | 证据 |
|---|---|---|
| 表格延迟建表的逐单元格测量 | `App::measure_text(&mut self, &TextContent, Option<LogicalPx>)` | `app.rs:1893` → `app/canvas_text.rs:207-233` |
| 降级判据（字体栈是否就绪） | `App::font_stack_status() -> FontStackStatus` | `app.rs:1947`、`text.rs:865` |
| 取窗口建树/建节点 | `App::window_mut(WindowId) -> Option<&mut Window>`、`Window::tree_mut() -> &mut NodeTree` | `app.rs:1483`、`window.rs:436` |
| 门禁读样式/布局盒 | `Node::style() -> &Style`、`Style::inner() -> &taffy::Style`、`Node::layout() -> LayoutBox` | `node/tree.rs:206,257`、`paint/style.rs:28`（`ui_kit` dev-deps 已开 `test-support`） |

**约束**：markdown 组件**不得**依赖 `pub(crate)` 入口（`text_content`/`set_text_content`/`invalidate_text_lines`/`mark_text_layout_dirty` 等，`node/tree.rs:834/908/921/931`）。若实施中确实需要新的写入口，必须**先回到本节登记**（签名 + 标脏要求 + 测试），不允许「顺手扩一点」。
## 6. API 规格

### 6.1 模块布局

约束与惯例：workspace lint 集里有 `definition_in_module_root = "forbid"`（root `Cargo.toml:340`）与 `pub_use = "forbid"`（`:337`，全仓 0 处 `pub use`）；仓库既有惯例是「自命名模块根文件定义公开类型（如 `ui_core/src/text.rs` 定义 `TextStyle`/`TextRun`），`mod.rs` 只声明子模块」。**实际落地**（R2 单提交 `9447be99` + R3，2026-11-26 工作树状态）：除测试根文件是 `tests.rs` 而不是 `tests/mod.rs`（理由见下，**这是与 v3 原稿唯一的强制偏差**）外，其余与设计一致：

```text
crates/ui_kit/src/kit/markdown.rs             // 自命名模块根：公开面（MarkdownAssets / MarkdownStyle / MarkdownNode / plain_text）
                                              //   + 子模块声明 + 4 个主题常量（行高 1.4 / 装饰线宽 1 / 背景内边距 2 / 圆角 2）
crates/ui_kit/src/kit/markdown/
    assets.rs      // 内禀尺寸解析（承接 source.rs 的 InlineImageSource 与其 20 条测试，原样迁移）
    lines.rs       // SourceLines（行首表）+ 字节入参的空行计数（§5.4）
    build.rs       // 事件 → 节点树（Builder、容器栈、对齐窗口、空行重建）+ 布局常量表（§5.3）
                   //   + 测试专用解析计数 seam（parse_count / reset_parse_count，仅 #[cfg(test)]）
    inline.rs      // marks → TextRun、inline box、样式映射（多装饰 + run 背景）
    html.rs        // HTML 块切片 + tokenizer（classify/units/open_tag/close_tag/is_tag_block）
                   //   + 白名单映射 + 容器内容嵌套解析（唯一允许的嵌套解析入口，受 HTML 预算约束）
    table.rs       // 延迟建表（TableBuf/CellBuf）+ End(Table) 逐单元格 measure_text(content, None) 定列宽（§5.10）
    plain.rs       // plain_text 事件旁路（复用 html tokenizer 取文字）
    tests.rs       // 测试 harness（EmbeddedAssets / mount / style / texts / tree_of / children / node_count /
                   //   bounds / cell_widths / measured_width 等）+ 子模块声明
    tests/
        corpus.rs          // §8.1 GFM 语料 + §8.3 规范一致性门禁（测试专用 HTML 发射器 + 按构造分类的语料测试）
        gates.rs           // §8.4 的七条性能与结构门禁 + 冻结夹具的渲染断言
        ledger.rs          // §8.6 台账「保留并改写」条目的实际落点断言（旧测试处置的替代，逐条对应台账编号）
        outline.rs         // §8.2 的 outline() 与节点树轮廓 helper（heading/list/table/align 的分类与文本化）
        plain.rs           // §8.5 plain_text 冻结字面量（81 条 legacy 基线 + 2 条生产输入 + 2 条 document 冻结值）
        fixtures.rs        // 冻结夹具的模块声明
        fixtures/
            i18n_markdown.rs      // **冻结夹具**：39 个 key × {en, zh} 原文 + 富文本标记 + `PRODUCTION_NOTICES`（§8.5）
            legacy_plain_text.rs  // **冻结夹具**：81 条 legacy plain_text 期望字面量 + 2 条 document 冻结值（R2 前用 comrak 管线生成）
```

- **为什么测试用 `tests.rs` + `tests/` 而不是 `tests/mod.rs`（唯一强制偏差，必须保留）**：`ui_kit` 继承 workspace lint `definition_in_module_root = "forbid"`（root `Cargo.toml:340`），即**禁止在模块根（含 `mod.rs`）里定义条目**；而测试 harness（`mount` / `style` / `texts` / outline helper 等）恰好必须定义在 `tests` 这一层，放进 `tests/mod.rs` 会被该 lint 拦下。实际形态 = 自命名根文件 `tests.rs` 定义 harness 并声明 `mod corpus; mod fixtures; mod gates; mod ledger; mod outline; mod plain;`，子目录 `tests/` 只放子模块；语义与 v3 原稿（`tests/mod.rs` + `fixtures/`）等价，只换根文件名（`self_named_module_files = "allow"`，root `Cargo.toml:336`，是该形态的前提）。
- 子模块声明放在 `markdown.rs`（`mod assets;` 等），`crates/ui_kit/src/kit/mod.rs:19` 保持 `pub mod markdown;` 不变；`mod tests;` 带 `#[cfg(test)]`。
- **没有 `autolink.rs`**（D2）、**没有 `preprocess.rs`**（不做源文本级剥壳，D7 走 tokenizer + 对齐窗口 + 嵌套解析）。
- `html.rs` 实际 686 行（v3 预估 350–450 行；超出部分来自块级四类分类、对齐窗口与嵌套解析的容错分支）。
- `SourceLines` 与空行计数的落点是 `lines.rs`（原 `parse.rs:26-74`/`:366-379`）；`parse.rs` 其余部分（comrak→IR）随 IR 一起删除。
- `source.rs` 的 20 条测试随文件迁入 `assets.rs` 的 `#[cfg(test)] mod tests`（断言不变）；`InlineImageSource` 降为 `pub(super)`（模块外不再需要）。

### 6.2 公开面

```rust
// crates/ui_kit/src/kit/markdown.rs
pub trait MarkdownAssets {
    fn load(&self, path: &str) -> Option<Cow<'static, [u8]>>;
}

#[derive(Clone, Debug, PartialEq)]
pub struct MarkdownStyle {
    // 现状 11 个字段全部保留（view.rs:37-49）：
    pub body_size: LogicalPx,
    pub line_height: f32,
    pub foreground: Color,
    pub muted: Color,
    pub link: Color,
    pub code_foreground: Color,
    pub code_background: Color,
    pub quote_border: Color,
    pub divider: Color,
    pub image_placeholder: Color,
    pub mono_family: SmolStr,
    // 新增（任务清单 / 装饰与 run 背景几何）：
    pub task_border: Color,               // = theme.role(Role::LineBorder)
    pub task_fill: Color,                 // = theme.role(Role::BrandBase)
    pub decoration_thickness: LogicalPx,  // 链接/删除线/键帽线宽（现值 1.0px，inline_flow.rs:19）
    pub background_padding_x: LogicalPx,  // run 背景水平内边距（D19：2px）
    pub background_radius: LogicalPx,     // run 背景圆角（D19：2px）
}

pub struct MarkdownNode { /* private */ }

impl MarkdownNode {
    pub fn mount<A: MarkdownAssets + 'static>(
        app: &mut App,
        window: WindowId,
        parent: NodeId,
        theme: ThemeHandle,
        source: &str,          // SmolStr 无法 Into<Arc<str>>，7 个调用点都传 &str
        assets: A,
    ) -> Result<Self, EngineError>;

    pub fn sync(&mut self, app: &mut App) -> bool;
    #[must_use] pub fn root(&self) -> NodeId;
    pub fn release(&mut self, app: &mut App) -> bool;
}

pub fn plain_text(source: &str) -> SmolStr;
```

- **`MarkdownStyle` 只放颜色 + 字号/行高 + run 背景几何 + 装饰线宽**；布局常量（`BLOCK_GAP`/`HEADING_SPACE_ABOVE`/`CODE_PADDING`/`CODE_RADIUS`/`QUOTE_BORDER_WIDTH`/`QUOTE_PADDING`/`LIST_MARKER_WIDTH`/`CELL_PADDING`/`TABLE_RULE_HEIGHT`/字号缩放）作为 `build.rs` 的 `pub(super) const`（§5.3 常量表），不进公开 API（避免 API 膨胀；需要主题化时再提升）。
- `mount` 内部：`Arc::<str>::from(source)` 存一份（一次分配，重建时复用）；`assets` 装箱为 `Box<dyn MarkdownAssets>`（对象安全，`source.rs:17-19` 已验证）。
- 公开面只剩 4 个符号 + 1 个 trait + 1 个结构体；`handles()` / `MarkdownHandles` / `rich_text` / `MarkdownView` / `Flow*` / `ImagePaintSource` 全部删除（D9）。

### 6.3 `plain_text` 契约

现状行为（`document.rs:178-232`）：块之间一个 `\n`、每个空行再加一个 `\n`、`LineBreak` → `\n`、代码块取 `code`、表格行内单元格用空格连接、行内图片完全跳过。两个生产消费者只用于**单句文案**（`settings_ui` 删除确认、`watch_list` 表格单元），其中 `watch_list` 的文案含「图标 + 双空格」这一细节（en 是 `**%{market}** ![](...)`，zh-CN 是 `![](...) **%{market}**`，顺序相反——两者都必须稳定处理）。

新契约（按块拼接，空行不计）：

1. 块级 HTML 走 §5.6.3 的 tokenizer + 容器内容嵌套解析，再取文字（`<p align>` 内的 `<kbd>`/图片/粗体都按 markdown 解析后参与拼接）。
2. 逐事件：`Text`/`Code` → 追加；`SoftBreak`/`HardBreak` → `\n`；`CodeBlock` → literal 去一个尾随 `\n`；表格单元格之间 ` `、行之间 `\n`；行内 `InlineHtml` 的 `<kbd>` 忽略、`<br>` → `\n`，其余标签丢弃；`Image` 子树完全跳过（不取 alt）。
3. 块与块之间一个 `\n`；列表项之间 `\n`；引用/列表等容器递归展开。
4. 不保留空行冗余 `\n`（与现状差异登记 §10.1-8）。**块内连续两个硬换行仍产出 `\n\n`**（新用例钉死）。
5. 契约用**冻结字面量**断言：`tests/fixtures/legacy_plain_text.rs` 存 **83 条**期望值（81 条 legacy 基线逐字符 + `document.rs` 两条冻结值），**由 R3 用 comrak 0.59 管线一次性生成后冻结入库**（R2 顺延、R3 落地；生成脚本不入库，命令与校验写进 R3 提交说明）。`common.market_not_loaded` 的四行块必须拼出 3 个 `\n`。

### 6.4 删除清单与保留清单

`parse_document`、`MarkdownDocument`、`MarkdownDocument::plain_text`、`MarkdownView`、`MarkdownHandles`（含 `link_at`）、`RichText`、`MarkdownBlock`、`MarkdownPiece`、`MarkdownListItem`、`MarkdownTableRow`、`MarkdownTableCell`、`HeadingLevel`（内部化）、`ParagraphAlign`（内部化）、`LinkId`、`KbdId`、`Flow`/`FlowStyle`/`LinkSpan`/`flow()`、`ImagePaintSource`/`PlaceholderImagePaint`/`InvisibleImages`、`rich_text`、`MONO_FAMILY` 的公开性（内部常量保留）。

**图片绘制通路的替代（删掉 `ImagePaintSource` 后写死的做法）**：

- `file:` 图片 → `InlineBoxSpec { id, index, size, baseline: None, align: InlineAlign::Baseline, paint: Some(InlineBoxPaint::Quad { color: style.image_placeholder, corner_radius: [px(2.0); 4] }) }`——与现状 `inline_flow.rs:96-108` 的占位四边形视觉一致；`InlineBoxPaint::Image{resource,modulation}`（`text.rs:750-760`）需要 App 资源注册管线，本轮**不用**。
- 尺寸：`assets::InlineImageSource::parse(uri)` 给出 `size`（显式 `?size`）或内禀尺寸；**不可加载资产 → 16×16**（沿用 `source.rs` 的 `DEFAULT_IMAGE_SIZE`）。
- 非 `file:` → `PendingBox::Alt(alt)`，有 alt 落文本、无 alt 不产节点（D6-④）。
- `InvisibleImages` 的测试替身删除后，「图片不占文本字节 / 只产 box」的断言改读 `InlineBoxSpec.paint.is_some()` 与 `size`（§8.2）。

**连带删除（AGENTS §4）**：`settings_ui` 的 `NoMarkdownAssets`（`crates/settings_ui/src/pages/proxy/table.rs:702-708`）与 `watch_list` 的 `TableMarkdownAssets`（`crates/widgets/watch_list/src/table.rs:567-573`）以及随之未使用的 `Cow` 导入（两文件各 `:1`，唯一使用点 `:705` / `:570`），否则 `clippy -D warnings` 失败。保留 `ReleaseNotesAssets`（`updater_ui/src/dialog.rs:936-942`）与 gallery `NoAssets`（`examples/gallery.rs:2752-2758`）。

**ui_core 侧替换（不是纯删除）**：`TextStyleExtras.decoration` → `decorations` + `background`；`ShapedTextLine` 增 `line_top`/`backgrounds`；`ShapedDecoration` → `ShapedQuad` + `corner_radius`；`emit_solid_quad` 增 `corner_radius` 参数；私有 `pass::TextDecorations` → `EditDecorations`（§5.11.2）。

### 6.5 调用点改造（7 处，行号已逐处核对；下表的行号是 **R2 前**的 comrak 管线基线，改造后的形态见 §14.1 的 R2 条目）

| 调用点 | 现状 | 目标 |
|---|---|---|
| `crates/widget/src/ui/placeholder.rs:86-98` | `parse_document`（`:88`）+ `mount(..., Arc::new(document))`（`:91-92`） | `mount(app, window, parent, theme, &source, EmbeddedAssets)`；`EmbeddedAssets` `:32-38`；`sync` `:131-132` |
| `crates/updater_ui/src/dialog.rs:487-490` | `parse_document`（`:488`）+ mount（`:489`）；`notes: Option<SmolStr>`（`:95`） | `mount(…, &source, ReleaseNotesAssets)`；`sync` `:293-294`、`release` `:129-130` |
| `crates/widgets/solana_wallet/src/dialog.rs:1518-1547` | `mount_markdown{,_from}` 封装（`:1528-1538`：parse `:1535`、mount `:1536`）；`root()` `:1541` | 同签名改传 `&str` + assets；wrapper 调用点 `:853`、`:857`、`:1199`、`:1310`；`sync` `:864,867` |
| `crates/widgets/ethereum_wallet/src/dialog.rs:1521-1550` | 同构（parse `:1538`、mount `:1539`、`root()` `:1544`） | 同上；wrapper 调用点 `:896`、`:900`、`:1222`、`:1333`；`sync` `:907,910` |
| `crates/settings_ui/src/pages/proxy/table.rs:692-700` | `parse_document(...).plain_text()`（`:699`）；assets `:702-708` | `markdown::plain_text(rich.as_ref())`；删 `NoMarkdownAssets` 与 `Cow` |
| `crates/widgets/watch_list/src/table.rs:575-586` | `parse_document(...).plain_text()`（`:585`）；assets `:567-573` | `markdown::plain_text(&source)`；删 `TableMarkdownAssets` 与 `Cow` |
| `crates/ui_kit/examples/gallery.rs:1313-1315` | `Arc::new(parse_document(MARKDOWN_SOURCE, &NoAssets))`（`:1313`）+ mount（`:1314-1315`）；**`:1304` 是 `section(...)`，不是 API 调用** | `mount(…, MARKDOWN_SOURCE, NoAssets)`；`sync` `:2100-2101`、`root()` `:2263-2264`、`MARKDOWN_SOURCE` `:89` 扩到覆盖 GFM 全构造、`NoAssets` `:2752-2758` |

> 类型核对：7 处都能提供 `&str`（placeholder `SmolStr`（`status.rs:72`）、updater `Option<SmolStr>`（`:95`）、钱包 `t_smol!`、settings `SmolStr`、watch_list `&source`、gallery `&'static str`）。

### 6.6 依赖与特性（已核实锁文件）

- root `Cargo.toml`：`comrak = { version = "0.59.0", default-features = false }`（原 `:83`）→ `pulldown-cmark = { version = "0.13.6", default-features = false }`（现 `:235`）。
- `crates/ui_kit/Cargo.toml`：`comrak = { workspace = true }`（原 `:11`）→ `pulldown-cmark = { workspace = true }`（现 `:12`）；新增 `[dev-dependencies] criterion = { workspace = true }` + `[[bench]] name = "markdown" / harness = false`（先例 `crates/ui_core/Cargo.toml:76-79`）。
- `default-features = false` 已实测可编译（probe 同配置）；`html` 特性只门控 `pub mod html`，`Tag::HtmlBlock`/`Event::Html`/`InlineHtml` 无条件产出，不需要该特性。
- **锁文件：R2 口径 +2 / −10，净 −8**：
  - 新增 2：`pulldown-cmark 0.13.6` + `unicase 2.10.0`（`Cargo.lock` 全量扫描无 `unicase`；pulldown 的依赖 `bitflags 2` / `memchr 2.5` / `unicase 2.6` 均非可选，仓库已有 `bitflags 2.13.2`、`memchr 2.8.3`，不新增）。
  - 移除 **10**：`comrak 0.59.0` + 其独占依赖闭包 9 个 —— `caseless`（→ `unicode-normalization`）、`entities`、`finl_unicode`、`memchr-n`（→ `fearless_simd`、`fearless_simd_macros`）、`phf_codegen`、`typed-arena`，以及**只被上述包引用的** `unicode-normalization`、`fearless_simd`、`fearless_simd_macros`。
  - **保留**：`phf`（`accesskit_atspi_common` 依赖）、`phf_generator`（`phf_macros` 依赖）、`rustc-hash`（bindgen 等）、`smallvec`（多处）。复算脚本口径：`Cargo.lock` 逆依赖闭包（`%TEMP%\markdown-impl\rev-lock3.py`，输出 `gone count: 10`）。
- `criterion` 已是 workspace 依赖（root `Cargo.toml:86`，被 `ui_core` 使用）：**R3 引入的是一条 dev-dependency 边**（`crates/ui_kit/Cargo.toml` 的 `[dev-dependencies] criterion = { workspace = true }`，即 `ui_kit → criterion`），**不新增任何包**（criterion 及其依赖闭包本来就在 `Cargo.lock` 里），`Cargo.lock` 只在 `[[package]] ui_kit` 的 dependencies 列表里 **+1 行 / −0 行**（R3 提交 `0973bda5`，`git show --numstat` 实测 `1 0 Cargo.lock`）。**两个口径不要混**：上面的 `+2 / −10 / 净 −8` 数的是**包数**（R2，`Cargo.lock` 行数 18+/91−）；R3 只动 1 行、包数不变。
- **i18n 冻结夹具不引入任何新依赖**：夹具是 `tests/fixtures/i18n_markdown.rs` 里的 `&'static str` 常量（R3 一次性从 YAML 生成，生成脚本不入库），测试侧不解析 YAML、不依赖 `i18n`/`widget`（`widget` 反向依赖 `ui_kit`，不能作为 dev-dependency）。
- **不引入任何 HTML/XML 解析依赖**：仓库只有 `quick-xml`（`wayland-scanner` 的 build 依赖）与 `roxmltree`（`usvg`/`fontique`），二者是**严格 XML 解析器**（不支持 void 元素、无引号属性、注释简写），且都不是 workspace 直接依赖；引入 `html5ever`/`html5gum` 会增加编译单元且对 `<p align>` 这种受限语法收益为负（AGENTS §1.4/§5）→ 保留自写 tokenizer。

---

## 7. 实施计划（方案 B：合并 P0+P1，D17）

> D17：**不写过渡 IR 适配层**，直接进入新架构；每个提交都必须可编译（`cargo check --workspace --all-targets`）。
> D21：验收只跑 `ui_core`/`ui_kit` 的测试；其它 crate 只要求编译；**产品侧受影响的 11 个 crate 在 R2/R4 手工补跑测试并记录结果**（命令见 §11）。

### R1 — ui_core 引擎改造（D6-①②/D8/§5.11）+ ui_kit 最小编译适配

- 交付物（ui_core）：`text.rs`（`TextDecorations`/`TextDecoration`/`TextBackground`（含 `is_visible()`）+ getter/setter/hash + `ShapedTextLine.line_top/backgrounds` + `ShapedQuad.corner_radius`）、`text/shape.rs`（双线收集 + `background_segments`（分组/两端 padding/`line_top` 同源）+ `clamp_line_rects`）、`text/cache.rs`（键区分测试）、`text/context.rs`（快路径资格）、`text/fast_path.rs` + `window/{canvas_text.rs,tests.rs}`（字面量补字段 + 非加速路径画背景）、`paint/pass.rs`（`emit_solid_quad` 增 `corner_radius` + 背景 Quad 循环 + **私有 `TextDecorations` → `EditDecorations` 改名**）。（v3.3 起**除本节列出的双装饰线 / run 背景公开面外不新增任何引擎公开 API**，见 §5.13）
- 交付物（ui_kit，**只做最小编译适配**，不改 markdown 语义）：`crates/ui_kit/src/kit/markdown/inline_flow.rs`——R1 前基线 `:8`（import）与 `:301-314`（3 处 `with_decoration(TextDecoration::underline/strikethrough(...))`）改成 `with_underline(px(1.0), Some(...))` / `with_strikethrough(...)`；`:417`（`.decoration().is_none()`）与 `:469-470`、`:479-480`、`:507-508`（`matches!(decoration, TextDecoration::Underline{..})`）改成 `decorations()`/`decorations().underline`；`:325`（测试 re-export）同步。**语义仍保持现状（三选一）**，R2 才改成独立叠加 + code 背景。
- 验收命令：`cargo test -p ui_core -p ui_kit`、`cargo check --workspace --all-targets`、`cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings`、`cargo +nightly fmt`（R1 改了 `ui_kit` 的测试代码，必须跑 `-p ui_kit`，与 D21/§11-1 一致）。
- 提交产出：一个提交；提交说明逐条列出 §5.11.4 的新增/改写测试（含 **RTL 背景用例**、**分数行高相邻行相接用例**、**组两端 padding 用例**、**非加速 canvas 路径背景用例**）、「哈希、快路径、z 序、clamp、接缝、RTL」六项风险的处理点，以及「ui_kit 只做了类型适配、语义未变」这一声明。
- **R1 明确不做**：任何 markdown 语义改动（链接+删除线共存、行内代码底色、kbd 下划线+底色归 R2，见 §5.11.4 末尾与 §7-R2）；`TextBackground::is_visible()` 的调用方过滤只在引擎内部（组件侧由 R2 用它挡无效配置）。

### R2 — markdown 组件全量替换（**一个提交**）

- **提交粒度：一个提交**（用户流程要求每轮一次提交）。若中途需要检查点，用工作树本地手段（如 `git stash create`）保留快照，**不产生额外提交**。
- 交付物：新模块布局（§6.1）、`html.rs`（标签序列 + 对齐窗口 + tokenizer 迁移）、`build.rs`（Builder + 空行重建 + 布局常量）、`inline.rs`（**多装饰叠加语义**：链接+删除线共存、行内代码底色、kbd 下划线+底色，及其 3 条测试——从 R1 移入，见 §5.11.4 末尾）、`table.rs`（延迟建表 + `End(Table)` 测量）、`plain.rs`、`assets.rs`、`lines.rs`、公开面收敛（§6.2）、`plain_text` 事件旁路（§6.3）、对齐窗口在段落创建时经 `current_align()` 写入 `TextStyle.alignment`（§5.6.3「对齐只写一次」）、7 个调用点（§6.5）、旧测试按台账逐条处置、gallery `MARKDOWN_SOURCE` 扩到覆盖 GFM 全构造；**R2 第一个动作**：用当前 comrak 管线生成 `tests/fixtures/legacy_plain_text.rs`（§6.3-5），随后再动解析器。
- 验收命令：`cargo test -p ui_core -p ui_kit`、`cargo check --workspace --all-targets`、`cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings`、`cargo +nightly fmt`；gallery 手动帧检（`cargo run -p ui_kit --example gallery`）；**产品侧补跑**（记录结果，失败需逐条说明）：`cargo test -p widget -p updater_ui -p solana_wallet -p ethereum_wallet -p force_order -p market_trade -p news -p empty -p settings_ui -p watch_list -p widget_registry`。
- 提交说明必须包含：① `markdown-test-inventory.md` 的 173 条逐条勾选（编号 + 测试名 + 处置）；② §10.1 的偏差逐条确认（R2 时 **28 条**，R3 追加 29–33 后为 **33 条**，逐条结果见 §14.4）；③ §8.5 覆盖边界（7 组原始串 key 不在范围内）；④ D21 让步声明。

**实际交付（R2 提交 `9447be99`，`refactor(ui_kit): markdown 组件切换为 pulldown-cmark 单遍管线`）**：

- **组件**（26 个文件，+5,987 / −5,673）：新增 `markdown.rs` + `markdown/{assets,build,html,inline,lines,plain,table,tests}.rs`（`tests.rs` 单文件承载当时全部测试，2,385 行）；删除旧 `document.rs` / `parse.rs` / `parse_tests.rs` / `view.rs` / `inline_flow.rs` / `mod.rs`；root 与 `ui_kit` 的 comrak 依赖删除（`Cargo.lock` +2 / −10 净 −8）。
- **调用点**：7 处（§6.5）改为 `MarkdownNode::mount(&str, assets)` / `plain_text(source)`；删除 `NoMarkdownAssets` / `TableMarkdownAssets` 及随之无用的导入；gallery `MARKDOWN_SOURCE` 扩到覆盖 GFM 全构造。
- **验收**：`cargo test -p ui_core -p ui_kit`（2,693 + 717 passed，0 failed）、`cargo check --workspace --all-targets`、`clippy -p ui_core -p ui_kit --all-targets -- -D warnings`、`cargo +nightly fmt --all -- --check` 全绿。
- **顺延到 R3 的 4 项（提交说明逐条登记，非静默丢弃）**：① `tests/fixtures/legacy_plain_text.rs` 冻结夹具与 `document.rs` 的两条冻结值；② 台账中「保留并改写」条目的对应断言；③ §8.4 门禁 4 的解析次数计数器 seam；④ GFM 语料剩余条目、测试专用 HTML 发射器、i18n 保真门禁、criterion bench。→ **R3 已全部交付**（见下）。

### R3 — 测试与门禁加固（D11/D13 + §8）

- 交付物：GFM 语料（§8.1，落地 **90 条**；设计时按 86 条排期，R3 复核后定稿 90）、`outline()` 与分类规则（§8.2）、测试专用 HTML 发射器与规范一致性门禁（§8.3）、7 条门禁（§8.4）、i18n 冻结夹具 + 保真门禁（§8.5）、`crates/ui_kit/benches/markdown.rs`（§9.2）。
- 夹具生成（一次性，脚本不入库）：`tests/fixtures/i18n_markdown.rs` 从 `crates/i18n/locales/{en,zh-CN}.yaml` 用「`has_html_tag || "](" || "**" || "`"」谓词（= `widget/src/status.rs:82-97` 逐字移植）导出 **39 个 key × {en, zh}** 原文与富文本标记（en 富 36 / zh 富 39），并把 **21 条生产可达 notice key**（19 `Notice::message` + `common.market_not_loaded` + `widget.empty.hint`）写成 `PRODUCTION_NOTICES` 常量。生成方式与脚本版本写进提交说明。
- 验收命令：`cargo test -p ui_core -p ui_kit`、`cargo bench -p ui_kit --bench markdown --no-run`（再跑一次落档 §12.2）、`cargo check --workspace --all-targets`、clippy（同 R1 范围）、fmt。
- 提交产出：一个提交；提交说明写明 bench **不纳入** `cargo xtask gates --step bench`（D20），并附实测数字（节点数 70/34 spacer、bench 中位数、门禁阈值余量）。

**实际交付（R3 提交内容）**：

- **测试拆分**：`tests.rs` 只留 harness（`EmbeddedAssets` / `mount` / `style` / `texts` / `tree_of` / `children` / `node_count` / `bounds` / `cell_widths` / `measured_width` 等）+ 子模块声明；用例拆进 `tests/{corpus,gates,ledger,outline,plain}.rs`，冻结夹具进 `tests/fixtures/{i18n_markdown,legacy_plain_text}.rs`（§6.1）。
- **夹具**（R2 顺延项 ①）：`i18n_markdown.rs`（75 条 = en 36 + zh 39，含 21 条 `PRODUCTION_NOTICES`）与 `legacy_plain_text.rs`（81 条 legacy 基线 + 2 条 document 冻结值），一次性脚本生成后冻结入库。
- **语料与门禁**（顺延项 ②④）：§8.3 语料 + `outline()` / HTML 发射器（`tests/corpus.rs`、`tests/outline.rs`）——`corpus.rs` 按 §8.1 的 8 个构造分类落齐 **90 条**用例（删除线 6 / 块级 15 / 行内 12 / 对齐 3 / 表格 12 / 任务清单 9 / HTML 语义子集 27 / 退化 6），另有 `the_gfm_conformance_corpus_covers_all_ninety_cases` 钉总数（8 个 builder 的 `len()` 求和 == 90）、5 条跳过清单逐条指向 §10.1 登记编号（运行时断言 `numbers == [3, 3, 5, 6, 16]`）；§8.4 七条门禁（`tests/gates.rs`，13 个测试）、§8.5 保真门禁、§8.6 台账落点断言（`tests/ledger.rs` / `tests/plain.rs` + `tests.rs` 的 `tests::*`）。
- **生产代码改动**：`build.rs` 补 `#[cfg(test)]` 解析计数 seam（`parse_count` / `reset_parse_count`，顺延项 ③）并修 spacer 的 `flex_shrink(0.0)`——门禁 3 实测暴露：此前 spacer 走 taffy 默认 `shrink = 1`，父容器高度受限时会被压扁（`tests/gates.rs::the_blank_line_spacer_is_flex_fixed_at_one_line_height`）。
- **bench**（顺延项 ④）：`crates/ui_kit/benches/markdown.rs` 的 **10 项**（8 项计划 + `build_notice_html_525b` / `table_widths_200x20`）+ `#[global_allocator]` 分配计数 + `[[bench]] name = "markdown"` / `harness = false` + criterion 0.8.2 dev-dependency（§9.2）；实测落档 §12.2。
- **台账**：`markdown-test-inventory.md` 补「落点（R3）」列并按 R3 复核做 2 条改判删除（101 / 72，见 §8.6）。

### R4 — 死代码清理与文档同步

- 交付物：删除 §6.4 的公开面与连带死代码（含两个 assets 结构体与未用 `Cow`）；删除 comrak 残留（root + `ui_kit`）；核对 lock（R2 的包数口径 + R3 的 dev 边 +1 行，见 §6.6）；更新 `crates/ui_core/design/custom-gui-engine-design.md` 的 **7 处**——**一律按节名定位、不写行号**（行号在 R1 后就已漂移，R4 起只认节名）：① `§4.7` 行内流描述（旧 `Flow`/`LinkSpan` → 两级映射）；② `§1.2` 的 E2 公开面表行；③ `§11.1` 的 Canvas 文本快照清单（`ShapedDecoration` → `ShapedQuad`）；④ `§11.2` 的 E2 公开面与 `TextStyle` 装饰 API（`TextDecoration` / `TextDecorations` / `TextBackground`）；⑤ `§12.1.1` 的 markdown 模块文件清单与 spacer 说明（R3 已修 flex 收缩）；⑥ `§12.4` 的上限 / 文件说明；⑦ `§13.9` R-4 行内图片状态；另外补写 `§4.5.1` 作为 E2「文本装饰与 run 背景」的规格章节。本文档 §12.2 落档 + §14 回写。
- 验收命令：`cargo test -p ui_core -p ui_kit`、`cargo check --workspace --all-targets`、clippy（同 R1 范围）、fmt；`cargo tree -p ui_kit --prefix none | Select-String -Pattern '^comrak'` 为空。
- 提交产出：一个提交（清理 + 文档）；提交说明确认 §10.1 每条偏差。

### 最终 — 全局对抗审查

按「回归／性能／正确性／偏差／文档一致性」五维度全量审查，逐项落到提交或文档修正；review 清单：① §8.4 七条门禁（13 个测试）都在 CI 可跑命令里；② §8.5 覆盖边界写明 7 组原始串 key；③ §10.1 每条「可接受性」仍成立（33 条逐条确认见 §14.4）；④ §6.5 的 7 处行号漂移则回写；⑤ §9.3 预算与实测一致（否则回写推导）；⑥ HTML 容错语义（未闭合/嵌套不配对/注释/引号/CRLF/段落内 `</p>`）都被 §8.1 的 27 条覆盖；⑦ 全文无未决项。

---

## 8. 测试计划

### 8.1 GFM 语料（新增，按构造分类，合计 **90** 条）

> 实际交付（R3 `0973bda5`，`tests/corpus.rs`）：**90 条** = 块级 15 / 行内 12 / 表格 12 / 任务清单 9 / HTML 语义子集 27 / 删除线 6 / 段落对齐 3 / 退化 6；每条按构造分类落在对应 builder 里，总数由 `the_gfm_conformance_corpus_covers_all_ninety_cases` 钉死（8 个 builder 的 `len()` 求和 == 90，各 builder 另有自己的计数断言）。

| 类别 | 用例数 | 覆盖点 |
|---|---:|---|
| 表格 | 12 | 四种对齐、空单元格、`\|` 转义、代码跨度内 `\|`、行内链接/图片、列数不齐、只有表头、表头粗体、表格在列表项内、超列/超行上限、列宽（宽列更宽）、补列 |
| 任务清单 | 9 | 紧/松、`[ ]`/`[x]`/`[X]`、嵌套、混排、空项（`- [ ]` 仍是文字，§10.1-33）、与有序列表混排、标记不上屏 |
| 删除线 | 6 | 单/双波浪线、嵌套强调、跨行、`~~a~~b~~`、表格内、**链接内（与下划线并存，D6-①）** |
| 块级 | 15 | ATX（含闭合 `##`）/Setext、嵌套引用、有序/无序嵌套、紧/松、围栏/缩进代码、分隔线、链接引用定义（0 事件）、有序起始编号、空行重建（文档首尾边界） |
| 行内 | 12 | 反斜杠转义、实体、`<kbd>` 简单形态、行内代码多重反引号/首尾空格、`file:` 图片、非 `file:` 图片回退（D6-④）、strong+em 嵌套、em、链接、链接内 strong、链接内代码 |
| 段落对齐 | 3 | 对齐窗口/`<p align>` 的居左/居中/居右（断言 `TextStyle.alignment` + 首字形 x 偏移） |
| **HTML 语义子集** | **27** | 见下方清单（逐条承接台账 §1 B 组与 C 组的容错语义） |
| 退化 | 6 | 200 层引用、100 层强调、300 层列表、10,000 字符单段、空文档、纯空白 |

**行内语料的变化（R3 复核后的事实）**：v3.4 计划里的 **硬换行 / `<br>` / `<kbd>` 只包图片**三条不再作为 canonical 用例——它们描述的正是本实现**有意不区分**的行为（`\n` 与硬换行、软换行在文本模型里都一样，D5；`<kbd>` 包图只剩图片盒），继续留在规范语料里等于「以规范之名固化」自家偏差。三条改由 **§10.1-30（硬/软换行）与 §10.1-31（`<kbd>` 包图）** 登记，专项断言落在 `tests/ledger.rs` 的 `hard_breaks_become_newline_bytes_and_reset_marks` / `the_line_break_model_does_not_distinguish_hard_from_soft` / `kbd_around_an_image_keeps_only_the_image_box`；`<br>` 仍出现在 HTML 语义子集的复合用例 `html:inline-del-code-and-break` 里（标签 → `\n` 的一处映射），不再单独成条。

**HTML 语义子集 27 条**（§5.6；括号内是承接的现有契约。下面 24 组契约落成 27 条用例：`<p align="left">` / `<p align="right">` 拆成两条、`<center>` / `<section>` 拆成两条，另加 1 条交叉容器 `<div><p>a</div>b`）：

1. 自包含单对（生产形态 `<p align="center">x</p>`，单行）；
2. 自包含多行 4 行块（`common.market_not_loaded`：3 个真换行 + `<kbd>` + `![]()`）；
3. `<p>` 无属性 → 左对齐段落；
4. `<p align="left">` / `<p align="right">`；
5. 同块多对（同行 / 跨行）→ 多个段落，对间不凭空插空行；**同块混合**：`<p>a</p><p>b` → 一对 + 一个开窗（`</p>` 在后续块关闭）、`<p>a</p></p>` → 一对 + 一个落单闭标签（丢标签）；
6. **跨块配对：关闭标签在段落内的 `InlineHtml`**——`<p align="center">a\n\nb</p>` → a、b 两段都居中 + 1 spacer（迁移 `an_align_tag_pair_may_span_blank_lines`）；
7. **跨多个块**：开标签独占一行 + `# Heading` + 段落 + 关闭标签独占一行 → 窗口内**顶层段落**全部居中、标题恒 Left（迁移 `an_align_tag_pair_may_span_several_paragraphs` / `an_open_tag_inside_an_html_block_is_a_pair` 第 3 段）；
8. 多空行：`a\n\n\n\nb` → 3 spacer（迁移 `blank_lines_inside_an_align_tag_pair_are_kept`）；
9. 嵌套窗口：`<p><p>`（**同一块内两个开标签**）→ 两次 push；后续两个 `</p>`（任何顶层块内，作为 HTML 块或 `<InlineHtml>`）→ 两次 pop，各**出栈**一次（pop 不写样式——align 在段落创建时已写入）；**EOF 仍开窗 → 已施加的 align 保留**（§10.1-22）；
10. 未配对开标签（EOF 未关窗）：丢标签、块内文字按 markdown 落地（现状「整块丢弃」，§10.1-2）；
11. 落单闭标签：丢标签（栈空）；
12. 段落内字面 `</p>` 提前关窗：之后的段落左对齐（§10.1-4）；
13. `<div>…</div>` 容器（内容按 markdown；内部 `<p align>` 对优先）；
14. `<center>…</center>` → 内容段落居中；`<section>…</section>` → 段落容器；
15. `<h1>`–`<h6>` → 标题（含内部 markdown）；`<hr>`/`<hr/>` → 分隔线；
16. 精确形态与属性容错：`<p align='center'>`、`<p align=center>`、`<p align = "center">`、`<p ALIGN="center">`、`<P align="center">`、`</p >`、额外属性、`justify`、`<palign=…>` → 一律非精确（丢标签保文字）（§10.1-2）；
17. 注释块 `<!-- -->` 内的 `<p align>` 不得识别；`<!-->`/`<!--->` 就地结束；
18. CRLF 与单独 `\r` 行尾的空行计数；
19. type 1–5 块（`<pre>`/`<script>`/`<style>`/`<textarea>`/`<?`/`<![CDATA[`/`<!X`）整体忽略，内部 `<p align>` 不得识别、文字不上屏（§10.1-3）；
20. 行内 `<b>/<strong>/<i>/<em>`（含标签之间 markdown 照常解析）+ 交叉嵌套 `<b><i></b></i>`、孤立闭标签、重复开标签；
21. 行内 `<del>/<s>/<code>/<span>/<br>`（`<br>` 在 code mark 内也换行）；
22. 行内 `<a>`（不保留 URL）+ `<img src>`（`file:` → box、`https:` → alt、无 alt → 无节点）+ 行中 `<p>` 是行内标签（markdown 照常生效）；
23. `<kbd>` 内图片（生产形态，落在 `html:production-multiline-pair`；kbd 样式无宿主可挂 → 只留图片盒，见 §10.1-31）+ `<kbd>` 只认精确小写（`<KBD>`/`<kbd class>` 走普通文字）；
24. 深度 > 0 的 HtmlBlock（引用/列表内）→ 去标签纯文本段落、不解析 markdown（§10.1-21）+ 多字节源末尾未闭合注释不 panic。

### 8.2 渲染结构测试（替代旧 IR 断言）

**`outline()` 文本格式（写死，便于断言与 diff）**：每个节点一行，缩进 = 2 空格/层，格式 `<kind>[<attrs>]`：

| kind | attrs | 判定规则（§5.3 形状 + `NodeKind`） |
|---|---|---|
| `p` | `align=left\|center\|right, runs=N` | text 节点，字号 == `body_size` |
| `h1`…`h6` | `align=left, runs=N` | text 节点，字号 == `body_size × scale(level)`（1.75/1.5/1.3/1.15/1.05/1.0），weight 700 |
| `code` | `bg=1, mono=1` | 节点 id == `MARKDOWN_CODE`（**已实现：备选 A 的 1 个 text 节点**；`MARKDOWN_CODE_TEXT` 不存在，参见 §14.3） |
| `quote` | — | id == `MARKDOWN_QUOTE` |
| `list` | `ordered\|bullet, start=N` | id == `MARKDOWN_LIST`；有序由 marker 文本以 `.` 结尾判定，无序为 `•` |
| `item` | `tight\|loose` | id == `MARKDOWN_LIST_ITEM`；**内容列是 text 节点 → tight；是容器 → loose** |
| `table` | `cols=N` | id == `MARKDOWN_TABLE` |
| `row` | `header\|body` | id == `MARKDOWN_TABLE_ROW`；header 判定 = 该行 cell 文本 weight == 700 |
| `cell` | `align=…, w=NN, has_text=0\|1` | id == `MARKDOWN_TABLE_CELL`；`w` 读 `Node::layout().bounds.width`（border-box），内容宽 = `w - 2×padding` |
| `rule` | — | id == `MARKDOWN_RULE`（分隔线）或 `MARKDOWN_TABLE_RULE`（表格行线，高 1px） |
| `spacer` | `h=19.6`（默认主题 body_size 14px × 1.4；示例值随 `body_size`） | id == `MARKDOWN_SPACER`；高度 == `body_size × 1.4` |
| `box` | `id=<debug_id>` | 其他容器（list/marker/content/quote-bar/…）按 debug id 输出 |
| `image` | `w×h` | text 节点的 `inline_boxes` 中 `paint.is_some()` 的 box |

- `text` 节点的 run 断言：`content.runs()` 的区间落在字符边界（`normalize_runs` 会重写区间）、每个 run 的 `decorations()`（underline/strikethrough 是否并存）、`background()` 是否为 `Some`。
- 图元断言（`frame.quads` / `PrimitiveKind`）：链接下划线 Quad、删除线 Quad、run 背景 Quad（在 GlyphRun 之前）、代码块背景、分隔线、复选框盒、同列 cell 的 `w` 相等。
- **分类器只依赖公开 API**（`NodeKind` + `TextStyle` + `Node::layout()` + `Node::style()` + debug id），不新增访问器、不加 `#[cfg(test)]` 旁路（零注释/无死代码约束下不留测试专用字段）。

### 8.3 规范一致性门禁（新增）

- 测试专用「outline → 规范化 HTML 片段」发射器（只用于断言，不进入产品）。元素映射：

| outline kind | 发射 |
|---|---|
| `p[align=X]` | `<p align="X">…</p>`（align=left 时省略属性） |
| `h1`–`h6` | `<h1>`…`<h6>` |
| `list[bullet]` / `list[ordered,start=N]` | `<ul>` / `<ol start="N">` |
| `item` | `<li>`（tight：内容不加 `<p>`；loose：内容包 `<p>`——由 §8.2 的 tight/loose 判定驱动） |
| `code` | `<pre><code>…</code></pre>`（语言 info 不上屏 → 不发射 `class`） |
| `table`/`row[header]`/`row[body]`/`cell[align]` | `<table>`/`<thead><tr>`（首 body 行前闭 `</thead><tbody>`）/`<tr>`/`<th align>`\|`<td align>` |
| `rule` | `<hr>` |
| `quote` | `<blockquote>` |
| `spacer` | **不发射**（一致性语料不含需要保留空行的用例；D4 的空行渲染是已知偏差 §10.1-6） |
| run 的 strong/em/del/code/kbd/link | `<strong>/<em>/<del>/<code>/<kbd>/<a>`（**无 href**，D9） |
| `image` | `<img>`（无 src，占位四边形的等价物） |

- 语料（R3 实际交付）：**90 条**，分类与逐类计数见 §8.1；期望值是**规范输出**（归一化空白与属性顺序），由测试专用发射器把节点树渲染成规范化 HTML 后逐字符比对。发射器、8 个 corpus builder 与总数断言都在 `tests/corpus.rs`。
- **不覆盖项**：① 扩展自动链接规范例（622–635）——D2 不实施（§12.3）；② 原始 HTML 规范例——D7 已登记偏差，改由 §8.1 的 HTML 语料守自家契约；③ 链接 `href`（§10.1-16）；④ 空行 spacer（§10.1-6）。
- **显式 skip 列表 = 5 条**（`tests/corpus.rs` 的 `SKIPPED`，运行时由 `the_conformance_corpus_skips_are_registered_deviations` 校验：每条 reason 非空、每条编号必须在 §10.1 表内真实存在，排序后断言 `numbers == [3, 3, 5, 6, 16]`）：扩展自动链接 → §10.1-5（D2）；原始行内 HTML → §10.1-3（D7）；原始 HTML 块（type 1–5）→ §10.1-3（D7）；链接 `href` → §10.1-16（D9）；空行 spacer → §10.1-6（D4）。
- **三条 canonical 用例已移出语料**：硬换行、`<br>`、`<kbd>` 只包图片不再作为规范期望值固化（理由与落点见 §8.1 末段、§10.1-30/-31）——规范一致性门禁只固化「本实现确实按 GFM 契约输出」的部分，不把已登记偏差写成期望值。
- CommonMark 全量示例的**一次性 diff 报告**（与 comrak 0.59 输出对比）在 R2 前生成并存档到 `%TEMP%`（不入库；§10.1 的差异登记以它为依据）。

### 8.4 性能与结构门禁（7 条；落地在 `tests/gates.rs`，共 **13 个测试**：门禁 3 = 3 个、门禁 4 = 3 个、其余 5 条各 1 个，另加 §8.5 保真门禁 2 个）

| # | 门禁 | 断言方式（实际测试名 + 实测口径） | 依据 |
|---|---|---|---|
| 1 | 稳定帧零重排 | `gates::gate_one_a_stable_frame_commits_nothing`：先 `drain()` 产出提交帧（`frame_record().rendered == true`）；再 `drain()` 断言 `FrameOutcome::Idle`（`frame_record().rendered == false`）。**不能**在 idle 帧读 `FrameStats`（它只在提交帧填充，`app/frame.rs:68,80,86,113,230-265`） | §2.5 |
| 2 | 稳定帧零 shape | `gates::gate_two_a_repaint_only_frame_shapes_and_lays_out_nothing`（**阳性对照 + 负样本两段，同一测试内**）：① 先读 mount 首帧的 `FrameStats`，断言 `text.shaped == 5 && text_layout.laid == 6`——证明这一帧确实有 5 个文本参与形状、6 个参与排版，后面的零值不是「报告没填」；② 再 `invalidate_all_lanes()` 制造纯重绘帧并 drain，断言该帧 `text.shaped == 0 && text_layout.laid == 0`，同时 `record.rendered == true` 保证它是真重绘帧而非 idle 帧 | `text.rs` 的 `TextPassReport`/`TextLayoutReport`、`app/frame.rs` |
| 3 | 节点预算 + 空行 | `gates::gate_three_the_release_notes_stay_inside_the_node_budget`：`release_notes/v0.0.3/{en,zh-CN}.md` → 节点总数 **== 70**（先断言 `<= 72` 再断言 `== 70`）、**恰好 34 个 `MARKDOWN_SPACER`**，每个 spacer 的布局高度 == 行高（默认根下 `body_size 14 × 1.4 = 19.6px`）且**声明样式 `flex_shrink == 0` / `flex_grow == 0`**（R3 修复：此前是 taffy 默认 `shrink = 1`，父容器受限时空行会被压扁）；根的直接子节点 69（35 text + 34 spacer），`1 + 35 + 34 == 70`。单行 spacer 形态另由 `gates::the_blank_line_spacer_is_flex_fixed_at_one_line_height` 钉死；zh 的软换行合并由 `gates::the_zh_release_notes_merge_fifteen_soft_breaks_into_eleven_paragraphs` 钉死（35 段 / 11 个多行段 / 15 个 `\n`，节点仍 70） | §5.3、§5.4、§9.3 |
| 4 | HTML 嵌套解析计数（替代 v2「零二次解析」） | 计数器 seam 为 `build.rs` 的 `#[cfg(test)] parse_count()` / `reset_parse_count()`（仅测试构建存在），口径 = **解析调用总次数**（主解析 + 嵌套解析）。① `gates::gate_four_the_nested_parse_count_follows_the_html_container_units`：release notes == 1；单条 `<p align>` notice == 2（主 1 + 嵌套 1）；行内 `<kbd>` 图片 == 1；混合 `<div>`（3 个容器 unit）== 4 且 `<= 1 + 3`、`<= 16`；② `gates::gate_four_every_production_rich_text_parses_exactly_once_or_once_nested`：`tests/fixtures/i18n_markdown.rs` 全 75 条中 70 条含 `<p align>` == 2、5 条 == 1；③ `gates::gate_four_plain_text_counts_the_nested_html_parse`：`plain_text` 共用同一 tokenizer 与预算 | §5.6.5 |
| 5 | 无旧形状残留 | `gates::gate_five_no_legacy_block_or_blank_ids_survive`：全树遍历，不存在 `MARKDOWN_BLOCK*` / `MARKDOWN_BLANK*` 前缀的 debug id；根 id 仍是 `MARKDOWN` 且全树只有 1 个 | §6.4 |
| 6 | 无依赖边（子树） | `gates::gate_six_the_markdown_subtree_holds_no_dependency_edges`：markdown **子树**逐个 `remove` 返回空依赖边集合；根节点 `remove` 返回 1 条自持外观依赖边（沿用 `view.rs:1426` 的回归测试口径） | §5.7、§2.5 |
| 7 | 表格列宽（延迟建表） | `gates::gate_seven_table_columns_take_their_measured_natural_widths`：① 同列各 cell 的 `bounds.width` 相等；② 两列自然宽差异大时渲染宽**不相等**且宽度比 ≈ 自然宽比（±5%）；③ **仅在该表总自然宽 ≤ 可用宽（未触发 flex 收缩）时**断言：列内容宽（`bounds.width − 2×CELL_PADDING`，padding 读 `node.style().inner().padding`）== 该列 `max` 自然宽（±1px）；触发收缩时只断言 ①② 并额外断言「收缩列 < 自然宽 − 1px、两列合计 ≤ 可用宽」；④ 嵌套解析（HTML 容器内容）里的表格同样满足 ①②③ | §5.10、§4.4-③ |

### 8.5 生产文案保真门禁（冻结夹具）

- **夹具**：`crates/ui_kit/src/kit/markdown/tests/fixtures/i18n_markdown.rs`——39 个 key × `{ key, en, zh, en_rich, zh_rich }`（en 富文本 36 / zh-CN 39）+ `PRODUCTION_NOTICES: &[&str]`（21 条）。夹具由 R3 从 `crates/i18n/locales/{en,zh-CN}.yaml` 一次性生成（谓词 = `has_html_tag || "](" || "**" || "`"`，逐字移植 `widget/src/status.rs:82-97`；生成脚本与校验命令写进提交说明）。
- **门禁**：① 遍历所有 `*_rich == true` 的值渲染 → 可见文本非空 + 无残留源码标记（`<p`、`<kbd>`、`![](`、`**`、反引号）；② 计数断言 `en_rich == 36 && zh_rich == 39`；③ `PRODUCTION_NOTICES.len() == 21` 且每条在夹具中可查；④ 两条 `plain_text` 生产输入（`widget.watch_list.market_metadata_not_loaded`、`settings_ui.proxies.delete_message`，en/zh 各一组）与 `legacy_plain_text.rs` 冻结字面量逐字符相等。
- **覆盖边界（写进断言消息与提交说明，不写代码注释）**：① 21 条走 markdown 组件的 notice（§2.6-A）在范围内；② 2 条 `plain_text()` 生产输入在范围内；③ **7 组「不经 markdown、不经 `plain_text`，把 `<p align>` 原样交给 `ui::label`/kit `Alert`」的 key 不在范围内**（§2.6-C，既有缺陷，§10.1-17）；④ 产品侧 4 条谓词/源码级断言（`widget/src/status.rs:425`、`widget/src/venue.rs:222`、`widget/src/venue.rs:267`、`perpetual_statistics/.../pane_tests.rs:509`）不经过渲染，改造后应保持绿或收紧。
- 真实长文档：`release_notes/v0.0.3/{en,zh-CN}.md` 渲染后节点数、文本总量、无 panic；**zh-CN.md 的 11 个多行段落 / 15 个 `SoftBreak` 必须合并**（D5），单独断言。

### 8.6 旧测试处置（台账在仓库内）

- 逐条台账：`crates/ui_kit/design/markdown-test-inventory.md`（**173 条**：保留/改写 **101** + 删除 **72**；parse 51/62、view 16/3、document 3/2、inline_flow 11/5、source 20/0）。**R3 复核把 2 条改判删除**，依据 D9（`LinkId` / `LinkSpan` / `link_at` 已删，`InlineBoxSpec` 不承载链接区间 ⇒ 「链接区间」断言在新架构不可观察）：`parse_tests.rs` #32 `an_image_inside_a_link_carries_the_link`（D 组，13 → 12 改写 + 1 删除）与 `inline_flow.rs` #15 `links_around_images_keep_their_ranges`（inline_flow 12/4 → 11/5）。台账的「落点（R3）」列已按实际落点测试名逐行入库（删除项写「删除：…」+ 理由），本节与 §10.1 的计数以台账为准。
- 组级摘要：A 嵌套上限 11 改写；B `<p align>`/HTML 方言 34 **删除测试函数但容错语义迁入 §8.1 的 HTML 语义子集（27 条用例）**；C `<kbd>` 6 改写 / 6 删除（保留名单见台账）；D 图片 12 改写 / 1 删除（链接区间改判，见上）；E 空行 10 删除（语义迁 §8.4 门禁 3 + §8.1）；F IR 内部 7 删除；G sweep 3 删除；H 块构造 14 改写；I 行内标记 5 改写 / 1 删除；J 其他 3 改写。`view.rs`：5 保留（断言不变）/ 11 改写 / 3 删除（依赖被删公开面）。`document.rs`：3 改写（含 `heading_levels_map_to_scales` 迁移、比例值不变）/ 2 删除。`inline_flow.rs`：11 改写 / 5 删除（含 `empty_pieces_make_an_empty_flow` 改判改写、`image_paint_source_can_choose_image_or_skip` 删除、`links_around_images_keep_their_ranges` 改判删除，理由见台账 §6/§7）。`source.rs`：20 全部保留（随文件迁入 `assets.rs`）。
- R2 提交说明必须按台账逐条勾选（编号 + 测试名 + 处置 + 替代落点），**不允许静默丢弃**；R3 复核补齐的落点测试名同样逐行可查。

---

## 9. 性能预算与测量

### 9.1 目标口径

| 场景 | 现状（comrak 0.59，未实测；1.2 量级见 §3.1） | 目标 | 验证方式 |
|---|---|---|---|
| 挂载 7 KB release notes | 解析 + IR + 建树（139 节点） | 1 次主遍历（5.4 µs）+ 建树 ≤ bench 记录值；节点 ≤ 72（推导 70，34 spacer） | R3 bench 落档 |
| 挂载单条 i18n notice | 2 次解析（主 + 二次） | 1 次主遍历 + **1 次嵌套解析**（≤ 322 B，< 1 µs）；无表格 → 无 `End(Table)` 测量 | §8.4 门禁 4 |
| 含表格文档 | 1 次解析（列宽等分） | **1 次事件遍历**（`End(Table)` 延迟建表 + 每单元格 1 次 `measure_text(None)`，≤4,020 次） | §5.10、§9.2 bench（`table_widths_200x20` = 4,020 次测量、53.004 ms） |
| 主题切换重建 7 KB | 删+建 139 节点 | 重建 70 节点，release 实测 **92.73 µs**（§12.2 `theme_rebuild_7kb`） | §12.2 |
| 稳定帧 | 0 重排 / 0 shape | 保持（门禁） | §8.4 门禁 1/2 |
| `plain_text` 热路径 | 解析整树 + 拍平 | 纯事件旁路（HTML 容器仍走一次嵌套解析） | R3 bench（10.782 µs） |
| 病态 1 万行表格 | 13.5 ms / 17 MB | 上限截断 → 节点 ≤ 8,443（20 列）/ 1,609（3 列）+ 溢出段落 ≤ 100 | §5.8 + bench（`build_table_10k` = 1,710 节点、8.7201 ms、18.05 MB） |

**分配口径**：§3.1 的「pulldown 4 次分配」指**解析器自身**；建树阶段分配（`InlineBuf` 扩容、`TextContent`/`TextRun`/`InlineBoxSpec` 装箱、节点槽位）另计。**R3 已做分配计数**：`benches/markdown.rs` 挂 `#[global_allocator]`（`CountingAllocator` 统计 alloc / alloc_zeroed / realloc 调用次数、累计字节与峰值存活字节），实测见 §12.2——7 KB release notes 的 mount 是 **440 次 / 185,220 B**（量级比早先估计的 100–200 次高，因为每个 text 节点还要装箱 `TextContent`/run/inline box）；主题重建 **612 次 / 145,852 B**；单条 notice **45 次 / 36,793 B**（峰值 35,934 B）；首帧 **5,003 次 / 4.84 MB**（峰值 4.25 MB）；100 KB 文档 **67,999 次 / 40.74 MB**；10,000×3 表格 **132,427 次 / 18.05 MB**。延迟建表的额外分配 = 每表 `rows × cols` 个 `CellBuf`（受 200×20 上限约束，仅含表格的文档；`table_widths_200x20` 的 219,214 次 / 36.71 MB 就是「200 行 × 20 列 + 4,020 次 cell 测量」的总量）。

### 9.2 基准方案

`crates/ui_kit/benches/markdown.rs`（criterion **0.8.2**，取 workspace 统一版本 `Cargo.toml:86`；`crates/ui_kit/Cargo.toml` 的 `[[bench]] name = "markdown"` + `harness = false`，criterion 只在 `[dev-dependencies]`）：**10 个基准**——「§9.2 原计划的 7 项 + `parse_only_100kb` + R3 追加的 `build_notice_html_525b` / `table_widths_200x20`」，并挂 `#[global_allocator]` 做分配计数。

| 基准 | 测什么 | 实测中位数（完整数据见 §12.2） |
|---|---|---|
| `parse_only_7kb` | 只跑 pulldown 事件遍历，不建节点（`release_notes/v0.0.3/en.md`，7,047 B） | 6.0129 µs |
| `parse_only_100kb` | 同上，合成 ≈100 KB 文档（`SYNTHETIC_BYTES = 100 × 1024` = 102,600 B） | 1.4111 ms |
| `plain_text_7kb` | `plain_text()` 事件旁路（release notes） | 10.782 µs |
| `build_7kb` | `MarkdownNode::mount` = parse + build（不渲染） | 55.907 µs；nodes 70 / spacers 34 / runs 19；分配 440 次 / 185,220 B |
| `build_notice_html_525b` | 单条生产 notice 的 mount（夹具里最长的 `<p align>` en 原文，322 B，含 1 个 HTML 容器 unit） | 3.6145 µs；nodes 2；嵌套解析 1；分配 45 / 36,793 B / 峰值 35,934 B |
| `frame_after_mount` | 挂载后的首帧 `TestApp::drain()`（layout + text + paint + encode） | 1.0736 ms；shaped 35 / laid 35 / primitives 88；分配 5,003 / 4.84 MB / 峰值 4.25 MB |
| `theme_rebuild_7kb` | 只计 `MarkdownNode::sync` 的重建段（`ThemeHandle::set_appearance` 不在计时内） | 92.73 µs；分配 612 / 145,852 B |
| `build_100kb` | 合成 ≈100 KB 文档的 mount | 17.646 ms；nodes 20,250 / runs 2,250；分配 67,999 / 40.74 MB / 峰值 36.49 MB |
| `build_table_10k` | 10,000×3 表格 mount（验证行上限截断） | 8.7201 ms；nodes 1,710；分配 132,427 / 18.05 MB / 峰值 8.76 MB |
| `table_widths_200x20` | 200×20 宽表 mount（验证 `End(Table)` 逐单元格测量的上限） | 53.004 ms；源 36,394 B、nodes 8,444、measured cells 4,020；分配 219,214 / 36.71 MB / 峰值 19.75 MB |

- fixture：`include_str!` 引入真实 release notes、i18n 冻结夹具（`build_notice_html_525b` 的源从夹具里取最长 `<p align>` en 原文）与程序合成的 100 KB / 表格源；headless 用 `ui_core::test_support::app::TestApp`（与 `tests.rs` 的 mount 路径同源）。
- criterion 配置：`sample_size(10)`、`warm_up_time(300 ms)`、`measurement_time(2 s)`；7 个基准（`build_7kb` / `build_notice_html_525b` / `frame_after_mount` / `theme_rebuild_7kb` / `build_100kb` / `build_table_10k` / `table_widths_200x20`）用 `iter_custom` 只计被测段，节点释放与拆除不计时；3 个 `parse_only_*` / `plain_text_7kb` 用 `b.iter`。
- **分配计数（R3 已落地，不是留待）**：bench 文件顶部定义 `CountingAllocator` 并 `#[global_allocator]` 注册，统计 `alloc` / `alloc_zeroed` / `realloc` 的**调用次数**、**累计字节**与**峰值存活字节**；`report()` 在 mount 前后各取一次基线，`eprintln!` 打出 `allocations / alloc_bytes / peak_bytes`。R3 之前担心的「bench 内不挂 allocator」已被这条实现替代。
- 环境与落档：机器 Ryzen 7 9800X3D / 111.6 GB / Win11 26300、rustc 1.97.1、profile=bench（release + `lto="fat"` + `codegen-units=1`）、criterion `sample_size(10)`、日期 2026-11-26；全部数字见 §12.2。

> **bench 不纳入 `cargo xtask gates --step bench`**（D20）：`Step::Bench`（`xtask/src/gates.rs:599`）走 `bench_commands()`（`:701-706`，只返回 label `bench(ui_core,perf)` 一条），而 `bench_args()`（`:770-779`）硬编码 `bench -p ui_core --features test-support --bench perf --no-run`；本轮不改 xtask，`benches/markdown.rs` 只在 R3 提交说明与本文档声明。

### 9.3 结构性预算（门禁口径）

| 指标 | 现状 | 目标 |
|---|---|---|
| release notes 节点总数 | 139（35×2 + 34×2 + 根 1） | **70**（35×1 + 34×1 + 根 1），门禁 **≤ 72** |
| release notes spacer 数 | 68（每个空行 2 节点） | **34**（每个空行 1 节点） |
| 每段/标题节点数 | 2 | 1 |
| 每空行节点数 | 2 | **1** |
| 任务项节点数 | — | 紧 3（未选中）/ 4（选中）；松 4（未选中）/ 5（选中） |
| HTML 容器节点数 | 2（wrapper + 内容） | **0**（不建节点，align 写进段落） |
| 每文档嵌套解析次数 | 1 per `<p align>` 对 | ≤ 1 + 容器 unit 数（生产 notice = 1），≤ 16 |
| 每块 id 构造 | 1 | 0（仅根 + 静态可读 id） |
| 段落 run 数（基础样式段） | ≥ 1 | 0 |
| 每 run 装饰条数 | 1 | ≤ 2（下划线 + 删除线） |
| 每个 code/kbd run 的背景矩形 | 0 | 1/行（跨行 N 个） |
| 表格列宽来源 | `flex_1` 等分 | `End(Table)` 延迟建表 + `measure_text(None)` 自然宽 + flex 收缩 |
| 病态 1 万行表节点数 | 无上限（6 万+） | ≤ 8,443（20 列）/ 1,609（3 列）+ 溢出段落 ≤ 100 |

---

## 10. 风险、偏差与自检

### 10.1 已登记的偏差（共 33 条；1–22 为 v3.4 前登记，23–28 为 R2 落地时新增，29–33 为 R3 落地时新增。逐条确认结果见 §14.4）

| # | 偏差 | 来源 | 可接受性 |
|---|---|---|---|
| 1 | tagfilter 不适用（不产出 HTML） | GFM §6.11 | 事实性结论；比它更严格 |
| 2 | **块级 HTML 容器内容按 markdown 解析**（GitHub 是 raw passthrough）；且「未闭合/非精确形态」由现状的「整块丢弃」改为「丢标签保文字」 | D7 | 生产依赖容器内解析；文字不丢；现有 34 条 align 测试的**期望**变了，但容错**输入**迁入 §8.1 |
| 3 | HTML 语义映射的其余差异：白名单外标签丢标签保文字、不传 HTML 属性/样式、`align="justify"` → Left、**type 1–5 块整块忽略（文字不上屏）**、`<div>/<center>/<section>` 不建节点、跨块「半包裹」（`<details>`…`</details>`）不还原 | D7 | 生产只用到 `<p align>`/`<kbd>`；异常形态文字仍可见（type 1–5 例外，见左） |
| 4 | **对齐窗口的边界**：只有主解析层顶层段落取窗口 align（heading/list/quote 内与 fragment 内段落各用其 align 参数）；正文里字面 `</p>` 会提前关窗、导致后续段落失去对齐；`<p ALIGN=…>`/`<P>` 等非精确形态按文字处理 | D7 | 生产 70 个 `<p align>` 全部成对、无字面 `</p>`，无影响 |
| 5 | **不支持 GFM 扩展自动链接**（裸 URL/邮箱不成为链接） | D2 | 与当前 comrak 配置行为等价（现状也未启用）；与「完整 GFM」差 1 个扩展；94 条语料归档 §12.3 |
| 6 | **保留源码空行占位节点**：每个空行仍占一行行高（与 GitHub 块间距语义不同） | D4 | 视觉与现状一致；节点预算按 1 节点/空行重算 |
| 7 | 容器内部（引用/列表/表格内）空行不生成 spacer，由块间距表达 | D4 | 生产语料空行只在顶层 |
| 8 | `plain_text` 不再为空行输出多余 `\n` | §6.3 | 两个生产消费者无空行 |
| 9 | CommonMark 版本：pulldown 跟 0.31.2，GFM 基于 0.29 | 库事实 | 差异为勘误/澄清（**未逐条验证**，R3 抽查） |
| 10 | 列表项目符号字符/有序分隔符不可得 | 事件模型缺失 | 统一 `•` / `1.` |
| 11 | 表格内 `\|` 已核对，**与 GFM 输出一致，不再是偏差**（R3 结论；编号保留以免下游引用漂移）：表格语料 12 条里 `table:escaped-pipe` 覆盖单元格内 `\|`、`table:code-span-pipe` 覆盖代码跨度内 `\|`，两者都进规范一致性门禁 | GFM §4.10，R3 实测 | 结论句：已覆盖 |
| 12 | 表格溢出段落上限（100 行）；溢出段落挂在表格容器之后、不受列宽约束 | D15 | 极端输入下尾行不显示 |
| 13 | 表格列宽 = 延迟建表 + 自然宽 basis + flex 收缩：**不做总宽归一化**、压缩后不保证等于自然宽；单表内存 = `rows × cols` 个 `CellBuf`（≤200×20）；`measure_text` 不可用时豁免回退 `flex_1()` 等分；长 token 用 `BreakWord`；不支持跨列 | D18 | 同列等宽、宽列更宽、嵌套表格同样生效；与浏览器自动布局仍有差异 |
| 14 | 装饰线 offset/thickness 用固定 EM 比例（0.10/0.30），不改用字体度量 | §5.11 | 与现状一致 |
| 15 | run 背景按**行盒**绘制（含 leading），跨行每行一个圆角矩形 | D19 | 与浏览器 `background` 盒模型不同 |
| 16 | 链接不保留 URL → 规范发射器只能输出无 `href` 的 `<a>` | D9 | 本轮不做点击交互 |
| 17 | **7 组 `<p align>` 文案不经 markdown 组件**（走 `ui::label`/kit `Alert` 原样画源码标签） | §2.6-C | 既有缺陷，本轮不收口 |
| 18 | HTML 嵌套解析预算（16 次 / 64 KB）触发时该 unit 退化为纯文本段落 | §5.6.3 嵌套解析 | 极端输入降级，文字不丢 |
| 19 | **HTML 块内 pair 之外的文字由「隐藏」改为「可见」**：现状 `<p align="center">a</p>\ntail` 只画 a（`tail` 属同一 raw 块而被吞掉），v3 把 `tail` 作为段落渲染 | D7-类 1 | 符合「绝不静默吞字」；现有测试第 2 断言（`several_align_tag_pairs_in_one_html_block_become_several_paragraphs`）期望变更，已随台账删除 |
| 20 | 列表标记列固定宽：`>9999` 项时编号可能溢出标记列（不额外加裁剪节点，以保持紧项 3 节点） | §5.3 | 极端输入；现状亦无裁剪 |
| 21 | **深度 > 0 的 HTML 块不做语义映射**：去标签后作为一个纯文本段落，不解析 markdown | D7-规则 8 | 现状 `content_block` 对容器内 `HtmlBlock` 直接丢弃（更差）；本方案改进为保留文字 |
| 22 | **未闭合的 `<p align>` 开标签对后续顶层段落仍然生效**（EOF 不撤销已施加的 align） | D7/J1 | 与现状「未配对则完全不做对齐」不同；生产 70 个 `<p align>` 全部成对，无影响；`<p align=…>` 未闭合时按整篇生效是可解释的行为 |
| 23 | **HTML 块配对上限**：单块标签 ≤ 4096 / 单块 payload ≤ 256KB / 每文档参与配对的标签总数 ≤ 16384；超限整块「丢标签保文字」 | §5.6.3 实现需要（性能优先） | 极端恶意输入下对齐/结构丢失但**文字不丢**；正常文档（493KB 纯 markdown、70 个 `<p align>`）不会触发 |
| 24 | 表格列宽测量只取单元格**前 1024 字符**；溢出行的空 cell 保留位 | §5.10 | 超长单元格列宽可能小于真实自然宽（门禁 7 用中度收缩验证）；溢出行结构不变 |
| 25 | 同一 HTML 块内多个「标签外文字」区间合并为 **1 次**解析并统一落在配对之后 | §5.6.3 | 配对两侧都有尾巴的极端混合块文字顺序可能与源码顺序不同；解析次数 ≤ 1 + unit 数 |
| 26 | 空**松**列表项的 loose 由 `Item` 事件区间是否含换行推断 | pulldown 不为空 item 发 `Paragraph` | 仅影响空项的间距，视觉差异极小 |
| 27 | fragment 的 `block_index` / `previous_blank` 写回外层容器 | §5.4 约束 11 字面为「隔离」 | 写回才能让 fragment 之后的块间距正确（否则 fragment 后首块丢 margin） |
| 28 | `plain_text` 代码块尾随空行折叠、极端多段顺序变化 | §6.3-4 | 两个生产消费方均无代码块/空行，无可见影响 |
| 29 | **嵌套列表的紧/松视觉间距不区分**：节点形态上紧/松可区分——纯紧项没有内容列（文本直接挂 item 行，`tight_lists_mount_markers_without_a_content_column` 断言 `MARKDOWN_LIST_CONTENT == 0`），需要内容列时（嵌套子块等）按 tight/loose 给不同 id（`MARKDOWN_LIST_CONTENT_TIGHT` / `MARKDOWN_LIST_CONTENT`，见 `build.rs::ensure_content`）；但渲染器**不在列表项内建模段落间距**，紧与松在屏幕上只差块间距语义 | R3 落地时发现（`ledger.rs::tight_and_loose_nested_lists_take_different_shapes` 暴露） | 节点形态与 outline 可区分（`item[tight]` / `item[loose]`，两种嵌套形态节点数都是 10），生产语料没有「同一列表里嵌套紧松混排且依赖段距」的形态；仅视觉间距与浏览器不同 |
| 30 | **硬换行与软换行在文本模型上不区分**：两种都渲染为 `\n`（D5），文本内容/节点数/outline 完全一致，只有源码里的标记不同；`<br>` 同样落成 `\n` | D5；R3 复核（`the_line_break_model_does_not_distinguish_hard_from_soft`、`hard_breaks_become_newline_bytes_and_reset_marks`） | 与 D5 裁决一致（软换行必须合并、硬换行不额外断行）；`plain_text` 与可编辑文本都不依赖二者差异；视觉上是同一行盒 |
| 31 | **`<kbd>` 包住图片时只保留图片 inline box**：`<kbd>![](file://…)</kbd>` 渲染为一个图片盒（`InlineBoxSpec`），kbd 的 mono 字体、下划线与底色**无处附着**（该 run 没有文字），视觉上只剩图片 | §5.6.2 HTML 映射 + D19 run 背景；R3 复核（`kbd_around_an_image_keeps_only_the_image_box`） | 生产 notice 的 `<kbd>![]()</kbd>` 正是这个形态：图片盒可见、文字不丢、kbd 语义无宿主可挂；不新增「盒装饰」抽象（AGENTS §1.2） |
| 32 | **表格 200×20 上限内的 mount 成本**：53.0 ms / 219,214 次分配 / 36.7 MB 峰值 19.75 MB，其中 `End(Table)` 的 4,020 次 cell `measure_text(None)` 主导（§12.2 `table_widths_200x20`） | §5.10 + D15 上限；R3 bench 实测 | 200×20 是硬上限、属于极端输入；一次性 mount 53 ms 可接受，且测量次数被 ≤4,020 硬约束（§5.8 三上限）锁死，不会随行数线性失控 |
| 33 | **空任务项 `- [ ]` 不产生 task marker**：pulldown 不为「只有标记、没有内容」的项发 `TaskListMarker`，于是渲染成普通文字 `[ ]`（无复选框） | pulldown 事件模型 + GFM §5.3；R3 语料 `task:empty-item-stays-text` | **与 GFM 输出一致**（`- [ ]` 没有段落内容，本就不是 task list item），`<li>[ ]</li>` 与 cmark-gfm 相同；文字不丢。注：带尾随空白的 `- [ ] ` 仍会产生 marker（`tests.rs::an_empty_task_item_still_shows_its_checkbox`） |

> **R2 提交时顺延到 R3 的交付物（已在提交说明逐条登记，不属静默丢弃）**：① `tests/fixtures/legacy_plain_text.rs` 冻结夹具与 `document.rs` 的两条冻结值；② 台账中 23 条「保留并改写」条目的对应断言；③ §8.4 门禁 4 的解析次数计数器 seam；④ GFM 语料剩余条目、测试专用 HTML 发射器、i18n 保真门禁、criterion bench。

### 10.2 风险

| # | 风险 | 影响 | 缓解 |
|---|---|---|---|
| R1 | 视觉变化（块间距分布、表格样式） | 设计验收 | D4/D5 已裁决为「与现状一致」；R2 同步更新既有渲染断言并核对 |
| R2 | **ui_core 引擎改动回归**：哈希/判据漏补 → 缓存抖动；快路径漏排除 → 背景不画；z 序 → 背景盖字；clamp 不同步；run 接缝；RTL 错位 | 引擎级回归 | §5.11.4 的 14 条测试 + `cargo test -p ui_core` + `text_shape_32` bench 对照 |
| R3 | **HTML tokenizer 容错面**：未闭合/嵌套不配对/属性引号异常/注释简写/CRLF/多字节 | 文字丢失或标签上屏 | §8.1 的 27 条 HTML 语料（逐条承接台账 B/C 组）+ §5.6.4 的 15 条迁移坑 |
| R4 | **块类推断错误**（type 1–5 当 6/7） | 把代码/注释当正文渲染 | `is_tag_block` 用例（§8.1-19） |
| R5 | **对齐窗口时序**：行内 `</p>` 出栈过早 → 该段落取不到 align；块级 `</p>` 出栈过晚 → 后续段落误取 align | 对齐错 | §5.6.3 对齐窗口规则 3/4 写死两条关闭路径与延迟出栈时机；§8.1-6/7/9/12 四条用例；门禁断言 `TextStyle.alignment` |
| R6 | 段落内字面 `</p>` 提前关窗 | 后续段落失对齐 | 登记 §10.1-4；用例 §8.1-12 |
| R7 | 表格列宽收缩比例与自然宽比例不一致（taffy 的 shrink 按 flex base 分配） | 观感 | 门禁 7 的 **±5%** 容差（与 §8.4 门禁 7 一致）；`table_widths_200x20` 已落档 **53.004 ms**（4,020 次 cell 测量，§12.2），列宽来源与测量成本都不再是未知量 |
| R8 | 紧列表项/隐式文本块在 `inline == None` 时丢字 | 内容缺失 | §5.4 约束 4/10 + §8.1 块级用例 |
| R9 | 深度上限与抑制模式 | 恶意输入仍产大节点树 | 双上限 + 表格/HTML 上限 + 退化用例 |
| R10 | 主题切换重新解析 | 长文档切主题延迟 | release 实测 **92.73 µs**（`theme_rebuild_7kb`，70 节点，§12.2）——比早先按 D3 估算的「7 µs 量级」高一个数量级（含子树 remove + 重建 + 主题色重解析），但仍远低于一帧预算；MB 级再评估 IR（D3 备选） |
| R11 | 表格延迟建表成本（`CellBuf` 内存 + `End(Table)` 测量） | 大表格 mount 变慢 | 上限 200×20：≤ 4,020 次测量、`CellBuf` 内存受行×列约束；bench 落档 |
| R12 | 删除公开面（D9） | 未来点击需重加 | AGENTS §1.1 |
| R13 | 删除公开面导致产品侧断言变红（23 条基线） | 产品回归 | R2 补跑产品侧 11 个 crate 的测试并记录；断言只允许同步更新、不允许删除 |
| R14 | 冻结夹具与 YAML 漂移（文案改动后门禁仍绿） | 保真门禁失真 | 夹具由 R3 生成，`en_rich/zh_rich` 计数断言（36/39）会在文案增删时变红；提交说明记录生成时间 |
| R15 | 其它 crate 只做 `cargo check`（D21） | 行为回归漏检 | 提交说明写明让步；R2/R4 各跑一次 `cargo check --workspace --all-targets` + 补跑产品侧测试 |

**已消除的二次复杂度风险（R2 实测，debug 构建、同输入；数字取 R2 提交说明）**：

| 原风险 | 消除机制 | 实测（修复前 → 后） |
|---|---|---|
| **空行计数每块重扫全文（O(n²)）→ 已解决** | `lines.rs` 的 `SourceLines`：行首表一次 O(n) 建好（`lines.rs:9-28`），`line_of` 用 `partition_point` 二分（`:30-37`），`blank_lines_between` 复用同一张表（`:39-53`）；主解析与 fragment 各装一份（`build.rs`），不再有 `parse.rs` 时代的全文重扫 | 16k 段 / 181 KB mount **5,905 ms → 151 ms**（近线性） |
| **HTML 标签配对 O(n²) → 已解决（单遍配对 + 三上限）** | `html.rs` 的单遍配对（按 `TagName::slot` 分栈，一次 O(T)，`html.rs:106-123`）+ 三个硬上限（单块标签 4,096 / 单块 payload 256 KB / 每文档参与配对的标签总数 16,384，超限整块「丢标签保文字」，§10.1-23）；`plain_text` 走同一 tokenizer 与同一预算（§8.4 门禁 4 钉计数） | 20k `<p align>` / 360 KB mount **663 / 1,077 ms → 7 ms**；32k 层 `<div>` 的 `plain_text` **1,785 ms → 8 ms** |
| **表格超列逐格处理（O(n²)）→ 已解决** | `table.rs` 在 `finish_row` 里把第 21 列起**一次性合并**进第 20 列（单次遍历，非逐格搬迁，§5.10 第 6 条） | 32k 列表格 mount **3,880 ms → 63 ms** |

> 同批实测旁证：493 KB 纯 markdown（release en × 70）mount 27.76 ms / 4,969 节点、无降级；16k 层 `<div>` 的 `plain_text` 19.46 ms（有界）。

### 10.3 同类项目踩过的坑（实施自检清单）

1. 块间距无状态机 → 嵌套段落/引用多出一倍间距或首块带边距（§5.3 块间距规则）。
2. **标题额外 8px 被空行抑制** → `a_heading_after_a_blank_line_keeps_the_extra_space_above_it` 必挂（§5.3 规则：`HEADING_SPACE_ABOVE` 不受 `previous_blank` 抑制）。
3. marks 未按栈弹出 → 样式泄漏（栈 + `End` 断言配对，§5.6.2）。
4. 紧列表项当段落 → 项间出现段距；紧项内容列**惰性创建为 text 节点**（§5.4 约束 4）。
5. 软换行策略散落多处 → 视觉行数不一致（只允许一处 `SoftBreak` 分支）。
6. 代码块内容被行内解析（代码模式不进 `InlineBuf`）。
7. 图片 alt 渲染两遍（§5.4 约束 6）。
8. 原始 HTML 未处理 → 文字消失或标签上屏（§5.6 + §8.5 非空断言）。
9. 链接引用定义被当正文（pulldown 不产事件；用例钉死）。
10. 单元格数与表头不齐 → 错位（`End(TableRow)` 补列，§5.4 约束 5）。
11. 递归实现 → 恶意输入爆栈（显式栈；嵌套解析另设预算）。
12. 背景画在字形之后 → 文字被底色盖住（§5.11.3 z 序）。
13. 装饰/背景没进 `hash_into` → 缓存键碰撞 + 每帧重复 shaping（§5.11.2）。
14. 数字快路径没排除背景（`context.rs:168-173`）。
15. run 背景不合并 → strong/em 拆 run 后接缝与重叠透明度（§5.11.3）。
16. **空行按字节差切** → 端点未规范到行界 → off-by-N 打穿 70/≤72（§5.4 端点规范化）。
17. **拼接 `Html` 事件取块内容** → 丢 `\r`、丢行结构（§5.6.4-2/3）。
18. **把行中 `<p>` 当块** → 行内 markdown 不再生效（§5.6.1-8）。
19. **忘了 type 1–5 排除** → `<pre>`/注释里的 `<p align>` 被识别（§5.6.3 深层规则 2）。
20. **只实现「`</p>` 作为 HTML 块」的关闭路径** → 段落内的 `InlineHtml("</p>")` 漏处理，跨块对齐全丢（§5.6.3 对齐窗口规则 3）。
21. **事后回改段落 alignment** → 对齐只在建节点时写一次（§5.6.3「对齐只写一次」）；`ShapeKey`/`same_shaping_inputs` 不含 color/alignment，事后改样式还要处理 shaped line 失效，属不必要复杂度。
22. **fragment Builder 串用外层状态** → 空行/间距错位、`last_line_end` 指向错误偏移（§5.4 约束 11）。
23. **表格缓冲期提前建节点** → 列宽无法确定，只能等 `End(Table)`；`End(TableCell)`/`End(TableRow)` 一律不建节点（§5.10）。
24. **行内 `</p>` 忘了延迟出栈** → 该段落 `Start(Paragraph)` 时窗口已关、取不到 align（§5.6.3 对齐窗口规则 3）。
25. **嵌套解析里的表格没有列宽** → 每个 Builder 实例各自持有表缓冲；不要用「每文档一份 `column_widths`」的跨实例映射（§5.10 第 8 条）。

---

## 11. 验收标准

1. **测试范围（D21）**：`cargo test -p ui_core -p ui_kit` 通过（含 §8.1–§8.6）；其它 crate 只要求 `cargo check --workspace --all-targets` 通过（写进提交说明）。R3 实测：**2,693 passed / 1 ignored**（`ui_core`）+ **779 passed**（`ui_kit`，其中 markdown 189），见 §14.3。
2. **产品侧补跑**（R2/R4 各一次，记录结果；失败逐条说明）：`cargo test -p widget -p updater_ui -p solana_wallet -p ethereum_wallet -p force_order -p market_trade -p news -p empty -p settings_ui -p watch_list -p widget_registry`。
3. **clippy（D21）**：`cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings` 通过（全仓 clippy 不在本轮门禁内；既有门禁用 `--all-targets`，`xtask/src/gates.rs:745-755`）。
4. `cargo +nightly fmt` 无 diff。
5. `cargo bench -p ui_kit --bench markdown --no-run` 通过，且 **10 个基准已实跑落档 §12.2**；bench **不纳入** `cargo xtask gates --step bench`（D20，已声明）。
6. 7 个外部调用点行为等价（§6.5）；gallery 手动帧检。
7. 性能门禁 **7 条**（13 个测试）全绿（§8.4），基准落档 §12.2。
8. `Cargo.lock`：R2 = **+2 / −10（包数，净 −8）**；R3 追加 `ui_kit → criterion` 的 dev-dependency 边（**不新增包**，`Cargo.lock` **+1 行**），两个口径见 §6.6，R4 核对。
9. `crates/ui_core/design/custom-gui-engine-design.md` 的 **7 处**已更新（§7-R4；一律按节名定位，不再写行号）。
10. 仓库内台账 `crates/ui_kit/design/markdown-test-inventory.md` 的 173 条在 R2 提交说明中逐条勾选。
11. 冻结夹具 `tests/fixtures/{i18n_markdown.rs, legacy_plain_text.rs}` 已入库，§8.5 门禁全绿。
12. §10.1 的 **33 条**偏差全部确认（逐条结果见 §14.4）；全文无未决项；§14 已回写（提交序列、公开 API 清单、测试与门禁基线、偏差确认、实测基准、决策变更日志）。

---

## 12. 附录

### 12.1 pulldown-cmark 实测事件样本

完整转储：`%TEMP%\markdown-refactor\pulldown-events-full.txt`（**1,365 行 / 1,272 非空行 / 93 个 fixture**，含 `CowStr` 变体、字节区间与源切片）；HTML 专项转储见 §12.4。

> 偏移基准：主解析用**原始源文本**的偏移（v3 不做源文本级剥壳）；HTML 容器内容嵌套解析时，偏移以该容器的内容切片为基准。关键样本：

```text
# 紧列表 + 任务清单：Item 下没有 Paragraph，TaskListMarker 是 Item 的直接子事件
- [ ] one
Start(List(None)) @0..28 / Start(Item) @0..10 / TaskListMarker(false) @2..5 / Text("one") @6..9

# 松列表：Paragraph 出现，TaskListMarker 成为 Paragraph 的第一个子事件
Start(Item) / Start(Paragraph) / TaskListMarker(false) @2..5 / Text("one")

# 表格：TableHead **直接**包含 TableCell（没有表头 TableRow）
Start(Table([None,None,None])) @0..39 / Start(TableHead) @0..13 / Start(TableCell) @1..4 / Text("a") …

# 行内 HTML：标签只有 InlineHtml 事件，标签间文字仍是 Text（markdown 照常解析）
text <b>**x**</b> text
Text("text ") / InlineHtml("<b>") / Start(Strong) / Text("x") / End(Strong) / InlineHtml("</b>") / Text(" text")

# 块级 HTML：整块是 raw，内容不解析 markdown；多行块每行一个 Html 事件
<p align="center">Hello <kbd>Ctrl</kbd></p>
Start(HtmlBlock) @0..43 / Html("<p align=\"center\">Hello <kbd>Ctrl</kbd></p>") @0..43 / End(HtmlBlock) @0..43

# 跨块配对的关键事实（关闭标签在段落内的 InlineHtml）：
<p align="center">a\n\nb</p>
Start(HtmlBlock) @0..20 / Html("<p align=\"center\">a\n") @0..20 / End(HtmlBlock) @0..20
Start(Paragraph) @21..26 / Text("b") @21..22 / InlineHtml("</p>") @22..26 / End(Paragraph) @21..26

# 实体/转义把 Text 打碎：必须按容器聚合
&amp; &copy; &#42; \*literal\* &lt;tag&gt;
Text(Borrowed("&")) @0..5 / Text(Inlined("*")) @13..18 / Text(Borrowed("*literal")) @20..28 …

# 扩展自动链接缺失（D2 的证据）：三个候选都留在同一个 Text 里
www.example.com https://bare.example/x?a=1 john@example.com
Text(Borrowed("www.example.com https://bare.example/x?a=1 john@example.com ")) @0..60
```

### 12.2 基准数据（10 项，2026-11-26 实测）

数据来源：`crates/ui_kit/benches/markdown.rs` 的 **10 个基准**在全量 run 下的 criterion 中位数（`sample_size(10)` 估计值）；附带的节点 / run / shape 计数与**分配计数**由 bench 的 `eprintln!`（`Metrics` 汇总 + `#[global_allocator]` 的 `CountingAllocator`）给出。早先 comrak / pulldown 对照的方法学与原始 TSV 见 `%TEMP%\markdown-refactor\perf-pulldown-vs-comrak.md`（汇总在 §3.1）。

- 机器：AMD Ryzen 7 9800X3D（8C/16T）/ RAM 111.6 GB / Windows 11 专业版 10.0.26300
- 工具链：rustc 1.97.1（`8bab26f4f` 2026-07-14）/ cargo 1.97.1（`c980f4866` 2026-06-30）
- 命令：`cargo bench -p ui_kit --bench markdown`（`[[bench]] name = "markdown"` / `harness = false`）
- profile：`bench`（继承 `release`：`opt-level=3`、`lto="fat"`、`codegen-units=1`、`panic="abort"`、`strip="debuginfo"`）
- criterion 配置：`sample_size(10)`、`warm_up_time(300 ms)`、`measurement_time(2 s)`
- 日期：2026-11-26；分配口径 = `#[global_allocator]` 统计的 `alloc` / `alloc_zeroed` / `realloc` **调用次数 / 累计字节 / 峰值存活字节**（`report()` 在 mount 前后取基线）

| 基准 | 中位数 | 计数与分配（附注） |
|---|---:|---|
| `parse_only_7kb` | 6.0129 µs | 全量事件遍历、不建节点；源 7,047 B |
| `parse_only_100kb` | 1.4111 ms | 合成 102,600 B（`SYNTHETIC_BYTES = 100 × 1024`） |
| `plain_text_7kb` | 10.782 µs | release notes → 纯文本（事件旁路，不建节点） |
| `build_7kb` | 55.907 µs | nodes 70 / spacers 34 / runs 19；分配 **440 次 / 185,220 B** |
| `theme_rebuild_7kb` | 92.73 µs | 仅 `MarkdownNode::sync` 的重建段，70 节点；分配 **612 次 / 145,852 B** |
| `build_notice_html_525b` | 3.6145 µs | 源 **322 B**（夹具里最长的 `<p align>` en 原文）；nodes 2（根 + 段落）；**嵌套解析 1 次**；分配 **45 次 / 36,793 B / 峰值 35,934 B** |
| `frame_after_mount` | 1.0736 ms | 挂载后首帧（layout + text + paint + encode）；shaped 35 / laid 35 / primitives 88；分配 **5,003 次 / 4.84 MB / 峰值 4.25 MB** |
| `build_100kb` | 17.646 ms | 合成 102,600 B；nodes 20,250 / runs 2,250；分配 **67,999 次 / 40.74 MB / 峰值 36.49 MB** |
| `build_table_10k` | 8.7201 ms | 10,000×3 表格（行上限生效）；nodes **1,710 = 根 1 + 表格 1,609 + 溢出 100**；分配 **132,427 次 / 18.05 MB / 峰值 8.76 MB** |
| `table_widths_200x20` | **53.004 ms** | 200×20 宽表；源 36,394 B；nodes **8,444**；**measured cells 4,020**（`(200+1)×20`，逐单元格 `measure_text(None)` 主导）；分配 **219,214 次 / 36.71 MB / 峰值 19.75 MB** |

**与 §9.1 目标对照**：

| 场景 | §9.1 目标 | 实测 | 结论 |
|---|---|---|---|
| 挂载 7 KB release notes | 1 次主遍历（预算 5.4 µs）+ 建树 ≤ bench 记录值；节点 ≤ 72（推导 70、34 spacer） | parse_only 6.0129 µs（单遍，+11% vs 预算 5.4 µs）、build 55.907 µs、nodes 70、spacers 34、分配 440 次 | 节点 / 空行命中；解析仍是 µs 级单遍 |
| 挂载单条 i18n notice | 1 次主遍历 + **1 次嵌套解析**（≤ 322 B）；无表格 → 无 `End(Table)` 测量 | 3.6145 µs、nodes 2、嵌套解析 1、源 322 B、分配 45 次 | 命中（HTML 容器只多一次 ≤322 B 的嵌套解析） |
| 主题切换重建 7 KB | 重建 70 节点，耗时以 R3 落档 | **92.73 µs**（含子树 remove + 重建）、70 节点、分配 612 次 | 落档（≈1.66× `build_7kb`） |
| `plain_text` 热路径 | 纯事件旁路 | 10.782 µs（parse 6.0129 µs + 旁路开销 ≈4.8 µs） | 落档 |
| 病态 1 万行表格 | 上限截断 → 3 列节点 ≤ 1,609 + 溢出段落 ≤ 100 | nodes=1,710 = 根 1 + 表格 1,609 + 溢出 100；8.7201 ms、18.05 MB | 命中（对照 comrak 现状 13.5 ms / 17 MB） |
| 200×20 宽表（测量上限） | 每单元格 1 次 `measure_text(None)`，≤4,020 次 | 4,020 次、53.004 ms、nodes 8,444、36.71 MB | 命中（§5.8 三上限锁死测量次数） |
| 100 KB 合成文档 | §9.2 合成 100 KB | 17.646 ms / 20,250 节点 / 2,250 run / 40.74 MB | 落档 |
| 首帧（挂载后） | 帧管线（layout + text + paint + encode） | 1.0736 ms、shaped 35 / laid 35 / primitives 88、4.84 MB | 落档 |
| 稳定帧 | 0 重排 / 0 shape（§8.4 门禁 1/2） | 不属本 bench（门禁覆盖） | — |

**`build_table_10k` 的 1,710 节点推导**：表格容器 1 + 201 行（表头 + 200 行）×（1 行容器 + 3 cell×2 + 1 分隔线）= 1,609，再加 100 条溢出纯文本段落 + 根 1 → **1,710**（§5.8 的表格三上限全部生效：10,000 行只建 200 行 + 100 条溢出）。

**`table_widths_200x20` 的 8,444 节点推导**：表格容器 1 + 201 行 ×（1 行容器 + 20 cell×2 + 1 分隔线）= 1 + 201×42 = **8,443**，再加根 1 → **8,444**（与 §5.8「200×20 = 8,443」的推导同源，差额就是根节点）。

**全部 10 项都已落档**（含 R3 追加的 `build_notice_html_525b` / `table_widths_200x20` 与分配计数）；bench 本身不纳入 `cargo xtask gates --step bench`（D20，见 §9.2 末尾）。

### 12.3 自动链接语料：已归档、本轮不实施（D2）

按 D2 不实现 GFM 扩展自动链接；v2 §5.6 的方案（区间驱动 + 源码回验，约 400–600 行）整体删除。comrak 0.59.0 的 94 条黄金语料与偏差分析**归档但不入库**：

| 归档物 | 路径 | 内容 |
|---|---|---|
| 黄金语料（人读） | `%TEMP%\markdown-impl\autolink-golden.md` | 94 条（S = GFM 例 622–635 共 32、E = cmark-gfm 22、D = 规则推导 16、X = 边界 24） |
| 黄金语料（常量） | `%TEMP%\markdown-impl\autolink_corpus.rs` | 94 条 `AUTOLINK_GOLDEN`（comrak 0.59.0 实测） |
| 偏差分析 | `%TEMP%\markdown-impl\autolink-deviations.md` | 13 条（8 改期望 / 5 skip）、xmpp/尾随 `_`/实体邮箱的 0.59 缺陷分析、8 KB 窗口上限实测 |
| 探针原始输出 | `%TEMP%\markdown-impl\probe-results.jsonl`、`comrak-probe\`、`pulldown-events.txt` | (a)/(b) 两侧链接、sourcepos、字节区间、HTML |
| 上游跟踪 | pulldown issue #518 / #695 开启、#608 not planned | 上游实现后可重启该特性 |

**行为说明**：当前 comrak 配置未启用 `extension.autolink`（`parse.rs:126-133`），因此「裸 URL/邮箱不成为链接」在换 pulldown 前后**用户可见行为一致**；本偏差记录的是「与 GFM 规范的能力差距」，不是回归。

### 12.4 HTML 事件语义调研（§5.6 的证据来源）

完整报告：`%TEMP%\markdown-impl\html-subset-research.md`（408 行）与四组 probe 转储 `%TEMP%\pulldown-probe\htmlsem{,2,3,4}-events.txt`；另有 `review6` 转储（跨块配对：`<p align="center">a\n\nb</p>` → HtmlBlock + Paragraph、`</p>` 在 `InlineHtml`）。要点：

- **块级 HTML 内容不解析 markdown**；`Tag::HtmlBlock` 无 block type；`Html` 每行一个；`Start/End` 区间覆盖整块 → **取块用源切片**；CRLF 的 `\r` 不在 `Html` 事件里；≤3 空格缩进块内有 range 为空的合成 `Text("  ")`；容器内块区间不连续（只处理顶层）。
- type 1–5 跨空行、type 6/7 被空行切断（`<p align="…">x\n\n</p>` = **两个**块）。
- **闭标签可能出现在 `Paragraph`/`Heading` 内的 `InlineHtml` 里** → 对齐只能由事件驱动的关闭路径处理（§5.6.3）。
- 行内 HTML 只有标签本身；标签之间/两侧文字仍是 `Text`，markdown 照常解析；行中 `<p align>` 是行内标签。
- 仓库没有可复用的 HTML 解析依赖（`quick-xml` 是 `wayland-scanner` 的 build 依赖、`roxmltree` 属 `usvg`/`fontique`，均为严格 XML），不新增依赖（§6.6）。

---

## 13. 迁移与删除清单（AGENTS §1.1：不留兼容层）

| 项 | 处置 |
|---|---|
| `comrak` 依赖 | **删除**（root `Cargo.toml:83` + `crates/ui_kit/Cargo.toml:11`），替换为 `pulldown-cmark`（D1/D12）；lock **+2/−10 净 −8**（**包数**，R2 口径） |
| GFM 扩展自动链接（v2 §5.6） | **不实施**（D2）；94 条语料归档 §12.3，不入库；无 `autolink.rs` |
| `<p align>` tokenizer | **迁移保留**：`parse.rs:26-74`（`SourceLines` → `markdown/lines.rs`）+ `:381-588`（`OpenTag`/`AlignPair`/`open_tags`/`next_tag`/`open_tag`/`pair_bound`/`find_close_tag`/`comment_end` → `markdown/html.rs`），合计约 230 行；`pair_blocks` 的「内容再解析」职责改由 `build_fragment` 承担 |
| HTML 语义 | **扩为四类块模型 + 对齐窗口 + 白名单映射**（D7/§5.6）：类 1 自包含 / 类 2 乐观开窗 / 类 3 关窗应用 / 类 4 丢标签保文字；`<div>/<center>/<section>` 不建节点；深度 > 0 去标签纯文本段落 |
| 源文本级「剥外层 `<p align>`」预处理 | **不做**（v2 §5.7-1 方案作废）：pulldown 的块级 HTML 内容不解析 markdown，剥壳无法覆盖未闭合/多对/尾巴文本，且会破坏现有 align/HTML 与 kbd 契约（台账 B/C 组 46 条） |
| `MarkdownDocument` / `MarkdownPiece` / `MarkdownBlock` / `MarkdownListItem` / `MarkdownTableRow` / `MarkdownTableCell` / `LinkId` / `KbdId` / `HeadingLevel` / `ParagraphAlign` 等 IR | **删除**（`HeadingLevel` 的 scale 作为内部常量保留） |
| `parse_document` / `MarkdownNode::mount(Arc<MarkdownDocument>)` | **删除**，改 `mount(…, source: &str, assets)` |
| `MarkdownView` / `MarkdownHandles` / `link_at` / `RichText` / `rich_text` / `Flow*` / `LinkSpan` / `ImagePaintSource` / `PlaceholderImagePaint` / `InvisibleImages` | **删除**（D9）；`file:` 图片改用 `InlineBoxPaint::Quad`（§6.4） |
| `NoMarkdownAssets`（`table.rs:702-708`）/ `TableMarkdownAssets`（`watch_list/table.rs:567-573`）+ 两处未用 `Cow` | **删除** |
| `MARKDOWN_BLOCK_*` / `MARKDOWN_BLANK*` debug id | **删除**（保留根 `MARKDOWN`、静态可读 id、新增 `MARKDOWN_SPACER`） |
| `markdown/table.rs`（新增） | **延迟建表**：`TableBuf`/`CellBuf` 缓冲 + `End(Table)` 逐单元格 `measure_text(None)` → 固定列宽（§5.10） |
| `markdown/lines.rs`（新增） | `SourceLines`（迁自 `parse.rs:26-74`）+ 行号口径 `line_of` / `blank_lines_between`（§5.4） |
| `NodeTree::set_text_style` | **不新增**（v3.3 废弃关窗回写；对齐只在节点创建时写一次，§5.6.3/§5.13） |
| 空行占位节点 | **保留**（D4）：2 节点 → **1 个 spacer**，高度不变（= 正文字行高）；`SourceLines` 的空行计数改为字节入参（§5.4） |
| ui_core `TextStyleExtras.decoration: Option<TextDecoration>` | **替换**为 `decorations: TextDecorations` + `background: Option<TextBackground>`；`TextDecoration` 枚举 → 单线结构；`ShapedDecoration` → `ShapedQuad`（+`corner_radius`）；`ShapedTextLine` 增 `line_top`/`backgrounds`；`emit_solid_quad` 增 `corner_radius`；私有 `pass::TextDecorations` → `EditDecorations`（§5.11） |
| `ui_kit/.../inline_flow.rs` | R1 做**最小编译适配**（8 处装饰 API：`:8`/`:301-314`/`:417`/`:469-470`/`:479-480`/`:507-508`/`:325`），R2 整体重写为 `inline.rs`（marks→run + 多装饰叠加 + code 背景） |
| `MarkdownAssets` trait / `file:` URI 解析与内禀尺寸（`InlineImageSource` → `assets.rs`，20 条测试随迁）/ mono 字体常量 | **保留**（语义不变） |
| `MarkdownStyle` | **保留但字段变化**：现有 11 字段不变，新增 `task_border` / `task_fill` / `decoration_thickness` / `background_padding_x` / `background_radius`（§6.2） |
| `ReleaseNotesAssets`（updater）/ gallery `NoAssets` | **保留** |
| 设计文档 | `crates/ui_core/design/custom-gui-engine-design.md` **7 处**更新（§7-R4）；`.docs/markdown-redesign-design.md` **删除**（D10）；新增 `crates/ui_kit/design/markdown-test-inventory.md` |

---

## 14. 实施结果回写（v4 已完成）

实现与文档已收口。以下 6 节是实施结果的事实回写，来源 = 提交 `9d3b04ce` / `9a4d3e8d` / `9447be99` / `0973bda5` 与 2026-11-26 工作树的实测（`cargo test` / `cargo bench` / `git show --numstat` / `grep`）。**本节所在的收口提交在 §14.1 的记录之后产生**，因此它自己的 SHA 不在文中引用。

### 14.1 提交序列

| 提交 | 说明 |
|---|---|
| `9d3b04ce` | docs(markdown): 设计定稿 v3.4（pulldown-cmark 单遍管线 + ui_core 引擎改造 + HTML 语义子集） |
| `9a4d3e8d` | feat(ui_core): 文本引擎支持双装饰线与 run 背景——R1，`TextDecorations` / `TextBackground` / `TextDecoration` 单线结构 / `ShapedQuad` / 私有 `EditDecorations`；`ui_core` 侧只落地 §5.11 已批准的公开面，没有 `NodeTree::set_text_style` 之类的额外入口（§5.13） |
| `9447be99` | refactor(ui_kit): markdown 组件切换为 pulldown-cmark 单遍管线——R2，新模块布局 + 公开面收敛 + 7 个调用点 + comrak 删除（`Cargo.lock` +2/−10 净 −8 包） |
| `0973bda5` | test(markdown): 冻结夹具 + 七条门禁（13 个测试）+ 90 条规范语料 + criterion 10 项基准——R3；另修 spacer `flex_shrink(0.0)`、`ui_kit → criterion` dev 边（`Cargo.lock` +1 行） |
| **本节所在提交** | docs(markdown): 文档收口——§8.1 / §8.3 / §8.4 / §9.1 / §9.2 / §10.1 / §10.2 / §11 / §12.2 / §13 / §14 与实现对齐；**在 §14.1 的记录之后产生**，SHA 不在此引用 |

### 14.2 最终公开 API 清单

| crate / 模块 | 公开面（= 实现后的最终形态） |
|---|---|
| `ui_kit::kit::markdown` | `MarkdownAssets`（trait，`load(&self, path: &str) -> Option<Cow<'static, [u8]>>`）、`MarkdownStyle`（16 个 `pub` 字段 = 原 11 + `task_border` / `task_fill` / `decoration_thickness` / `background_padding_x` / `background_radius`）、`MarkdownNode::{mount, sync, root, release}`、`plain_text(source: &str) -> SmolStr`。模块根 `markdown.rs`（186 行）只有这 4 个公开条目 + 私有子模块声明；4 个布局常量 `LINE_HEIGHT` / `DECORATION_THICKNESS` / `BACKGROUND_PADDING_X` / `BACKGROUND_RADIUS` 是私有 `const` |
| `ui_core::text` | 新类型 `TextDecoration`（单线条参数 `{ color: Option<Color>, thickness: LogicalPx }`）、`TextDecorations`（`{ underline, strikethrough }`，同一 run 可两条线共存）、`TextBackground`（`{ color, corner_radius, padding_x }`）；`TextStyle::{decorations, background, with_underline, with_strikethrough, with_background}`。**`ui_core` 本轮没有其它新增公开入口**：对齐只写一次、没有 `NodeTree::set_text_style`；`ShapedTextLine` / `ShapedTextItem` / `ShapedQuad` / `ShapedGlyphRun` / `ShapedInlineBox` 全部 `pub(crate)` |

删除清单见 §13；markdown 侧旧公开面（`MarkdownDocument` / `MarkdownBlock` / `MarkdownPiece` / `MarkdownView` / `MarkdownHandles` / `link_at` / `rich_text` / `Flow*` / `LinkSpan` / `LinkId` / `KbdId`）在 `crates/` 内已无定义。

### 14.3 测试与门禁基线

环境：AMD Ryzen 7 9800X3D / RAM 111.6 GB / Windows 11 专业版 10.0.26300；rustc 1.97.1；日期 2026-11-26；`cargo test` 用默认 debug profile，bench 用 `profile=bench`（§12.2）。

| 命令 | 实测结果 |
|---|---|
| `cargo test -p ui_core -p ui_kit` | **2,693 passed / 1 ignored**（`ui_core` lib）+ **779 passed**（`ui_kit` lib，其中 `kit::markdown` **189** 条）；同一命令下所有 `test result:` 行均为 ok（lib / 集成 / example 目标，0 failed），本轮复跑 exit code 0 |
| `cargo check --workspace --all-targets` | 通过（D21 让步：其它 crate 只要求编译） |
| `cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings` | 通过（0 warning） |
| `cargo +nightly fmt --all -- --check` | 无 diff |
| `cargo bench -p ui_kit --bench markdown` | **10 个基准**全部跑通并落档 §12.2；**不纳入** `cargo xtask gates --step bench`（D20，见 §9.2 末尾） |

**§5.3 代码块节点形状验证项 → 备选 A（1 节点）**：代码块是单个 `MARKDOWN_CODE` text 节点（`code_background` + `padding(8px)` + `corner_radius(4px)` + mono），三条判据全部断言通过——① wrap 宽度 = 内容盒宽：`tests.rs::a_code_block_wraps_at_its_padded_content_width`（与同内容宽容器逐行比较）；② 背景 Quad 覆盖 border-box：`tests.rs::code_blocks_mount_one_text_node_with_background_padding_and_mono`（`quad.bounds == code_bounds`、四角 4px、`MARKDOWN_CODE_TEXT` 计数 0、整树 2 节点 = 根 + 代码）；③ 首帧后 idle：`tests.rs::a_code_block_settles_into_an_idle_frame`（第二帧 `rendered == false`）。因此门禁 3 不需要「≤2」的临时口径，节点预算直接断言 `== 70`。

**门禁 3 的 spacer 修正（R3）**：R2 时 spacer 只有 `width_full + height(行高)`，走 taffy 默认 `shrink = 1`，父容器受限时会被压扁；R3 在 `build.rs` 的 `emit_blank_spacers` 里补 `flex_shrink(0.0)`（`flex_grow` 保持默认 0）。门禁 3 与 `gates.rs::the_blank_line_spacer_is_flex_fixed_at_one_line_height` 同时断言 `flex_shrink == 0.0 && flex_grow == 0.0`、高度 == `body_size × 1.4 = 19.6px`（默认 800×600 根下 34 个 spacer 各 19.6px）。

### 14.4 §10.1 的 33 条偏差确认结果

| # | 确认 | 验证方式（测试 / 实现证据） |
|---|---|---|
| 1 | ✅ 事实成立 | `Options` 只开 tables / strikethrough / tasklists（`build.rs`），组件从不产出 HTML ⇒ tagfilter 无作用对象 |
| 2 | ✅ 确认 | `corpus.rs::html:division-container-parses-markdown`、`html:crossed-containers-keep-their-text`、`html:non-exact-attributes-keep-only-text`；`tests.rs::an_html_division_parses_its_content_as_markdown` |
| 3 | ✅ 确认 | `corpus.rs::html:type-one-to-five-blocks-are-ignored`、`html:center-container-centers-its-content`、`html:section-container-scopes-its-content`；§8.3 的 skip 2/3 指向本行 |
| 4 | ✅ 确认 | `corpus.rs::html:literal-closing-tag-ends-the-window`、`html:pair-across-blocks`、`html:nested-pair-windows`；`tests.rs::a_literal_closing_tag_in_a_paragraph_closes_the_window_early` |
| 5 | ✅ 确认 | `corpus.rs` 的 `SKIPPED` 第 1 条指向本行；§12.3 的 94 条语料只归档不入库；全仓无 `autolink.rs` |
| 6 | ✅ 确认 | `gates.rs::gate_three_the_release_notes_stay_inside_the_node_budget`（70 节点 / 34 spacer / 各 19.6px）；`plain.rs::plain_text_deviation_08_folds_*` 两条 |
| 7 | ✅ 确认 | `tests.rs::container_blocks_do_not_add_spacers_inside_themselves` |
| 8 | ✅ 确认 | `plain.rs::plain_text_deviation_08_folds_the_release_note_blank_lines`、`plain_text_deviation_08_folds_the_document_baseline_blank_lines`；`tests/fixtures/legacy_plain_text.rs` 的 81 条冻结字面量 |
| 9 | ✅ 确认（保留「未逐条验证」口径） | 90 条规范语料 + §12.1 的事件转储；本条是 CommonMark 版本差的**登记项**，不逐条验证，门禁只覆盖语料里实际出现的用例（与 §10.1-9 的登记一致） |
| 10 | ✅ 确认 | `tests.rs::tight_lists_mount_markers_without_a_content_column`、`ordered_markers_start_at_the_parsed_number`（标记统一 `•` / `1.`） |
| 11 | ✅ 已核对（结论句） | `corpus.rs::table:escaped-pipe`、`table:code-span-pipe`；`tests.rs::escaped_pipes_keep_their_text_in_cells_and_code_spans` |
| 12 | ✅ 确认 | `tests.rs::rows_beyond_the_table_cap_become_one_paragraph_after_the_table`、`a_table_is_capped_at_twenty_columns`；bench `build_table_10k`（1,710 节点） |
| 13 | ✅ 确认 | `gates.rs::gate_seven_table_columns_take_their_measured_natural_widths`；`tests.rs::same_column_cells_share_the_measured_natural_width`、`a_wider_cell_gets_a_wider_column`、`a_table_falls_back_to_equal_columns_without_a_ready_font_stack` |
| 14 | ✅ 确认 | ui_core `text/shape.rs` 的 0.10 / 0.30 em 固定比例（§5.11.1 证据表）；R1 的 RTL / 硬换行装饰用例（`decoration_survives_hard_line_breaks`、`rtl_decoration_covers_the_full_visual_span`） |
| 15 | ✅ 确认 | ui_core `text/shape.rs` 背景按行盒（`line_top..line_top + line_height`）；分数行高 19.6px 的相邻行严格相接用例（§5.11.4） |
| 16 | ✅ 确认 | `corpus.rs` 的 `SKIPPED` 第 4 条指向本行；`tests.rs::inline_anchor_and_image_tags_map_to_marks_and_boxes`（`<a>` 不带 href） |
| 17 | ✅ 确认（范围外） | §2.6-C 的 7 组原始串 key；§8.5 覆盖边界③ 明确不在范围内，本轮不收口 |
| 18 | ✅ 确认 | `tests.rs::html_nesting_beyond_the_parse_budget_degrades_to_plain_text`、`the_document_nested_parse_budget_degrades_later_html_units_without_losing_text` |
| 19 | ✅ 确认 | `tests.rs::several_html_tag_pairs_in_one_block_become_separate_paragraphs`、`an_html_pair_inside_a_center_keeps_its_own_alignment_and_the_tail_centers` |
| 20 | ✅ 确认 | `tests.rs::ordered_markers_start_at_the_parsed_number` + `LIST_MARKER_WIDTH`（20px 基础宽，`>9999` 项不额外裁剪） |
| 21 | ✅ 确认 | `tests.rs::html_blocks_inside_containers_degrade_to_one_plain_paragraph` |
| 22 | ✅ 确认 | `tests.rs::an_unclosed_alignment_tag_keeps_its_alignment_until_the_end` |
| 23 | ✅ 确认 | `tests.rs::an_html_block_beyond_the_tag_cap_degrades_to_plain_text`、`an_html_block_beyond_the_payload_cap_degrades_but_keeps_its_text`、`the_document_tag_budget_degrades_only_the_blocks_beyond_it` |
| 24 | ✅ 确认 | `tests.rs::a_cell_beyond_the_measured_character_cap_keeps_its_text_and_measures_a_prefix` |
| 25 | ✅ 确认 | `tests.rs::several_html_tag_pairs_in_one_block_become_separate_paragraphs`（尾巴统一落在配对之后）+ `gates.rs::gate_four_the_nested_parse_count_follows_the_html_container_units`（混合 `<div>` 3 unit == 4 次） |
| 26 | ✅ 确认 | `tests.rs::empty_loose_items_keep_a_content_column`、`empty_list_items_keep_the_body_line_height` |
| 27 | ✅ 确认 | `tests.rs::an_html_pair_inside_a_center_keeps_its_own_alignment_and_the_tail_centers`（fragment 之后首块间距正确）+ §8.1 的空行语料 |
| 28 | ✅ 确认 | `tests/fixtures/legacy_plain_text.rs` 的 2 条已登记偏差；`plain.rs::plain_text_deviation_08_folds_*` |
| 29 | ✅ 确认 | `ledger.rs::tight_and_loose_nested_lists_take_different_shapes`（id 区分 `MARKDOWN_LIST_CONTENT_TIGHT` / `MARKDOWN_LIST_CONTENT`、outline `item[tight]` vs `item[loose]`；两种形态节点数都是 10，差别不在节点数） |
| 30 | ✅ 确认 | `ledger.rs::the_line_break_model_does_not_distinguish_hard_from_soft`（`a\nb` 与 `a\\\nb` 的文本 / 节点数 / outline 全等）、`hard_breaks_become_newline_bytes_and_reset_marks` |
| 31 | ✅ 确认 | `ledger.rs::kbd_around_an_image_keeps_only_the_image_box`（text 为 `\u{200B}`、1 个 inline box、runs 为空） |
| 32 | ✅ 实测 | §12.2 `table_widths_200x20`：53.004 ms / 219,214 次分配 / 36.71 MB（峰值 19.75 MB）/ 4,020 次 cell 测量 |
| 33 | ✅ 确认 | `corpus.rs::task:empty-item-stays-text`（`- [ ]` → `<ul><li>[ ]</li></ul>`）；带尾随空白的 `- [ ] ` 仍出 marker，由 `tests.rs::an_empty_task_item_still_shows_its_checkbox` 覆盖 |

### 14.5 实测基准（§12.2）

10 项中位数（完整环境、分配与计数见 §12.2）：`parse_only_7kb` 6.0129 µs、`parse_only_100kb` 1.4111 ms、`plain_text_7kb` 10.782 µs、`build_7kb` 55.907 µs（70 节点 / 34 spacer / 19 run / 440 次分配）、`theme_rebuild_7kb` 92.73 µs、`build_notice_html_525b` 3.6145 µs（源 322 B / 2 节点 / 嵌套解析 1）、`frame_after_mount` 1.0736 ms（shaped 35 / laid 35 / primitives 88）、`build_100kb` 17.646 ms（20,250 节点 / 2,250 run）、`build_table_10k` 8.7201 ms（1,710 节点）、`table_widths_200x20` 53.004 ms（8,444 节点 / 4,020 次测量）。**bench 不纳入** `cargo xtask gates --step bench`（D20）。

### 14.6 决策变更日志（v2 → v3.4）

| 决策 | 变化点 | v3.4 定稿（= 已实现） | 证据 |
|---|---|---|---|
| D2 GFM 扩展集 | v2 计划自实现 GFM 扩展自动链接（v2 §5.6，约 400–600 行） | **不实施**：只开 tables / strikethrough / tasklists，GFM 覆盖 3/5；94 条 comrak 语料只归档 | §3.1.4、§10.1-5、§12.3、`SKIPPED` 首条 |
| D4 空行语义 | v2 决定保留空行占位（视觉与现状一致），但形状从「2 节点/空行」收成 1 个 | 每空行 **1 个 `MARKDOWN_SPACER`**（高 = `body_size × 1.4` = 19.6px、`flex_shrink = 0`），只在顶层容器生成 | D4、§5.3、§5.4、门禁 3（70 / 34） |
| D6 保真度缺口 | v2 原案 ①②③ 都要改引擎，与 §4.4-③「不改引擎」冲突（F5） | **四项全实现**：①② 改引擎（同一 run 双装饰线 + per-run 背景）、③④ 用现有 API（延迟建表 `measure_text` / 非 `file:` alt 回退） | D6、§4.4、§5.11、R1 `9a4d3e8d` |
| D7 HTML 语义 | v2 的「源文本级剥外层 `<p align>`」+「关窗回写样式」方案 | 语义映射子集 + 四类块模型 + 对齐窗口；align 在建节点时**写一次**，块级 `</p>` 立即出栈、段落内 `</p>` 延迟到 `End(Paragraph)`、EOF 不撤销 | D7、§5.6.3、J1/J2、§10.1-2/-3/-4/-22 |
| D17 阶段 | v2 的 P0（解析内核）/ P1（渲染）分段推进 | **方案 B**：合并 P0+P1，不写过渡 IR 适配层，每个提交都可编译 | D17、§7（R1 → R2 单提交） |
| D18 表格列宽 | v2 的事件预扫 `column_widths`（G2 原案） | **延迟建表**：`End(Table)` 对每个 `CellBuf` 调 `measure_text(content, None)` 取自然宽；主路径不调 `flex_none()`/`flex_1()`、不做总宽归一化 | D18、§5.10、门禁 7、bench `table_widths_200x20` |
| D19 run 背景 | v2 无 per-run 背景（D6-② 是新增项） | 2px 水平内边距 + 2px 圆角 + **行盒高度**、不改变推进宽度；`padding_x` 只加在同色/同圆角/同内边距组的**两端** | D19、§5.11.3、ui_core 文档 §4.5.1、R1 `9a4d3e8d` |
| D20 bench 与 xtask | 曾计划把 markdown bench 纳入 `cargo xtask gates --step bench` | **不纳入**（该步骤硬编码 `-p ui_core --features test-support --bench perf`）；只在提交说明与 §12.2 声明 | D20、§9.2 末尾、`xtask/src/gates.rs:599`、`:701-706`、`:770-779` |
| D21 测试范围 | 曾计划全仓跑测试 | 只跑 `ui_core` + `ui_kit`；其它 crate 只要求 `cargo check --workspace --all-targets`（让步写进提交说明，产品侧手工补跑） | D21、§11-1/-3、R2/R3 提交说明 |

### 14.7 正文修订日志（v3 → v3.4）

v3 → v3.1（可落地性对抗审查后的机制修正；每条都已落进正文对应章节）：

| # | 原表述（v3） | 新表述（v3.1） | 原因 |
|---|---|---|---|
| A1 | 只有开标签的块做「单次源文本前视」找闭标签；配对窗口 = 连续顶层 HTML 块序列；被消费块需跳过 | **取消前视**：开窗（`align_windows: Vec<Align>`）；v3.3 进一步改为「建节点时写 align + 行内 `</p>` 延迟出栈」（见 J1，登记/回写已删） | 闭标签可能藏在 `Paragraph`/`Heading` 内的 `InlineHtml` 里（`<p align="center">a\n\nb</p>` 实测 = HtmlBlock + Paragraph）；源扫描不可靠且与事件流互斥 |
| A2 | 嵌套解析计数口径含「tail unit」含糊表述 | 计数 = 1（每块标签外文字，最多 1 次）+ Σ每个有内容的容器 unit 1 次；门禁 = 「20 条含 `<p align>` 的 notice 各 == 1、`empty.hint` == 0」 | 修掉「21 条各 ==1 且 empty.hint ==0」的自相矛盾 |
| B | 表格列宽：`measure_text(Some(可用宽))` + 夹紧 + 归一化到可用宽；gate 7 断言「等于预扫值 ±1px」 | `measure_text(None)` 取**自然宽**；cell `width = 自然宽 + 2×padding` + `min_width_0` + `overflow_hidden`，**不调用 `flex_none()`/`flex_1()`**（taffy 默认 `shrink=1` 允许按比例压缩）；**不做总宽归一化**（可用宽度建树前不可得）；gate 7 改断言「同列等宽 + 宽列更宽 + 比例一致」 | `Some(w)` 是限宽测量不是自然宽；可用宽在 mount 前不可得；`StyleBuilder` 没有 `flex_basis/grow/shrink` setter |
| C | 深度 > 0 的 HtmlBlock 无分支（文字被静默丢弃） | 深度 > 0 → 拼 `Html`/`Text` 子事件去标签 → 非空白则纯文本段落（不解析 markdown、不消耗预算）；登记 §10.1-21 | 原伪代码只有 depth 0 分支，与「绝不吞字」冲突 |
| D | 空行按 `source[last_block_end..block_start]` 字节片段计数 | 改为**行号口径**：`last_line_end` 端点规范化到行界、`SourceLines::blank_lines_between(字节入参)`、首个块前不生成、尾部不生成、容器内不生成；验收 `en.md` → 34 spacer / 70 节点 | 字节差端点在「块区间含尾随换行」时 off-by-N，打穿 70/≤72 门禁 |
| E1 | R1「ui_core 之外不改任何代码」 | R1 允许对 `ui_kit/.../inline_flow.rs` 做**最小编译适配**（6 处装饰 API，语义不变），并列出基线行号；R1 验收补 `-p ui_kit` | 否则 R1 提交 `cargo check --workspace` 必失败，违反 D17「每个提交可编译」 |
| E2 | 代码块「1 或 2 节点」写成留待 R2 现场判定 | 明确为**两个都已批准的备选 + 三条可执行判据 + 结论回写 §14**（§5.3/§5.12）；门禁先按 ≤2 写 | 消除「无未决项」与验证项表述冲突 |
| E3 | 列表项形状「每项 3 或 4 + 子块」含糊 | 紧项 = 内容列即 text 节点（3 节点）；松项 = `flex_col` 内容容器 + 段落（≥4）；任务项标记列 = 复选框盒 +（选中）勾文本 | 需写死 `inline == None` 时的惰性内容列规则 |
| E4 | 布局常量只散落在 §2.3 | 新增 §5.3 **完整常量表**（block gap 8、heading 额外 8 且不被空行抑制、code padding 8/radius 4、quote bar 3/padding 8、marker 20、cell padding 4、rule 1、字号缩放、mono、装饰线宽…）+ 块间距规则写死 | 实施者照抄；修掉「空行后标题额外 8px」缺失导致既有断言必挂 |
| E5 | 图片绘制通路只写「`file:` → inline box」 | 写死 `InlineBoxPaint::Quad{color: image_placeholder, corner_radius:[2px;4]}` + 尺寸来源 + 16×16 回退 + 非 `file:` 的 alt 规则（§6.4） | 删掉 `ImagePaintSource` 后必须有替代规格 |
| E6 | i18n 门禁「遍历 YAML」 | 改为**仓库内冻结夹具** `tests/fixtures/i18n_markdown.rs`（39 key × en/zh + 21 条 `PRODUCTION_NOTICES`），R3 一次性生成；不新增依赖 | `ui_kit` 不能反向依赖 `widget`/`i18n` |
| E7 | 173 条处置只在设计文档组级描述 | 新增仓库内台账 `crates/ui_kit/design/markdown-test-inventory.md`（173 条逐条 + 处置 + 轮次） | 「不允许静默丢弃」必须可审计 |
| E8 | §8.2/§8.3 未给 outline 分类与发射映射 | 新增 `outline()` 文本格式、kind/attrs 定义、tight/loose `li` 判定、发射器元素映射表（§8.2/§8.3） | 否则一致性门禁会自证 |
| E9 | R2「可拆分 3 个提交」 | **一个提交**（检查点用 `git stash create`） | 用户流程要求每轮一次提交 |
| F1 | `Cargo.lock` +2/−7 净 −5 | **+2/−10 净 −8**（补 `fearless_simd`、`fearless_simd_macros`、`unicode-normalization`） | 逆依赖闭包复算（`rev-lock3.py`） |
| F2 | 「嵌套 `<p>` 内容在第一个闭标签结束」等表述保留 | 保留并补：`</p>` 两条关闭路径、EOF 丢弃、字面 `</p>` 提前关窗（§10.1-4） | 事实核对：闭标签可能是段落内 `InlineHtml` |
| F3 | `<kbd>` 精确 vs「标签名大小写不敏感」自相矛盾 | 除 `<kbd>` 外大小写不敏感；`<kbd>` 仅精确小写（§5.6.2）；精确形态判定要求小写 `align=`（§5.6.3 对齐窗口规则 3） | 与保留的 `only_the_exact_kbd_tags_count`/`only_the_exact_align_tag_pair_counts` 契约一致 |
| F4 | 「pair 外尾巴文本按 markdown 落地」未登记 | 登记 §10.1-19（现状隐藏 → v3 可见） | 现状测试第 2 断言期望变更 |
| F5 | D6「①②③ 需改引擎」与 §4.4-③「不改引擎」冲突 | D6 改为「①② 需改引擎；③④ 用现有 API」 | 决策表是实施入口，必须准确 |
| F6 | 生产代码 3,897 行 / `+32 B` / 事件转储 1,272 行 / `rasterize_parley :1050-1196` / 「约 230 行 = :381-588」 / zh 反引号「3」 | 2,176 行 / +24 B / 1,365 行（1,272 非空）/ `:1050-1209` / 「`:26-74` + `:381-588`」 / 「3 条（4 个行内代码段）」 | 事实复核修正 |

v3.1 → v3.2（第三轮独立复审 17 项 + 用户 4 项裁决 + 引擎审查 6 条补充；O-3/N-7 两行补在 H18/H19）：

| # | 原表述（v3.1） | 新表述（v3.2） | 原因 |
|---|---|---|---|
| G1 | 对齐窗口「直接改写 `TextContent.style`」 | 曾计划新增引擎 API `NodeTree::set_text_style`（§5.11.6，v3.2）；**v3.3 起整条废弃**：对齐只在建节点时写一次（J1/J2），不新增任何 API（§5.13） | `TextContent.style` 是 crate 私有入口；且 `ShapeKey` 不含 color/alignment，回写还要处理 shaped line 失效——不值得 |
| G2 | 表格列宽 = 事件预扫（无 `\|` 跳过、每文档一份 `column_widths`） | **延迟建表**：`TableBuf`/`CellBuf` 缓冲到 `End(Table)`，再逐单元格 `measure_text(None)` 定列宽；每个 Builder 实例各自缓冲 → 多表与嵌套解析里的表格天然覆盖（§5.10） | 预扫是「源码近似 + 额外一遍事件遍历」，且覆盖不了嵌套解析里的表格 |
| G3 | 空行计数依赖 `SourceLines` 的行索引基 | **字节口径** `blank_lines_between(source, from_byte, to_byte)` + `next_line_start` 行尾规范化 + 四条反例（`a\n\n\n\nb` → 3、`a\n\nb` → 1、release notes → 34、首尾 → 0）（§5.4） | 行索引基（0/1 基、`next_start-1`）不可执行、易 off-by-N |
| G4 | `<p><p>` 嵌套窗口无处理；`open_tag` 是单值接口 | **`block_tags(payload) -> Vec<BlockTag>`** 按源序返回全部标签；同一块可混合（成对 + 未配对开 + 落单闭）并单程处理（§5.6.3） | 一个块可含多个开/闭标签，单值接口会丢窗口 |
| H1 | §5.6.3 内「第 N 条」悬空引用（`-2/-3/-4/-5/-7/-8`） | 全部改为按**小节标签**引用（「分类」「对齐窗口」「嵌套解析」「深层规则」），不再用序号 | 复审发现 5 处指向不存在的条目 |
| H2 | `inline_flow.rs` 外部使用点 `:322`、共 6 处 | `:8` / `:301-314` / `:417` / `:469-470` / `:479-480` / `:507-508` / `:325`，共 **8 处** | 按 `git show HEAD` 实测 |
| H3 | `emit_solid_quad` R1 前 4 处调用点 | R1 前 **3 处**（`:1642`/`:1739`/`:1761`）、R1 后 5 处 | 实测 |
| H4 | 自定义引擎文档更新 7 处里的 `:2945` | `:2943`（`ShapedDecoration` 散文所在段） | 实测 |
| H5 | §8.2 的 spacer 示例 `h=16.8` | `h=19.6`（默认 body_size 14 × 1.4） | 16.8 是 12px 字号的值 |
| H6 | §5.10 降级「回退 `flex_1`」与 D18「禁 `flex_1`」冲突 | D18/§5.10 明确：**主路径禁用**；`measure_text` 不可用时**豁免**回退 `flex_1()` 等分（用注入空字体栈的测试复现） | 规格自相矛盾 |
| H7 | `<hr>` 列入「类 1 自包含对」示例 | `<hr>` 无闭标签、**不属于自包含对**；直接产出分隔线、不计入嵌套解析次数（§5.6.3 深层规则 4） | 分类错误会误导实现 |
| H8 | §5.6.5「1 + 容器 unit 数；生产 notice = 1」字面矛盾 | 改为「标签外文字 ≤ 1 + Σ有内容 unit；生产 notice：0 + 1 = 1」 | 字面相加不等于 1 |
| H9 | §10.1 条目数仍写 17 | 已更新为 **21 条**并补 `-18`（嵌套解析预算退化）/`-20`（列表标记溢出）；§5.6.3/§8.1/§5.9/§5.12 的引用同步 | 复审发现计数与引用漂移 |
| H10 | §5.6.3「嵌套解析」小节的 `§5.6.3-5` 等 5 处悬空引用 | 见 H1（改为标签引用）；§5.13 新增「需要新增的引擎公开 API」唯一清单 | 引用不可达 |
| H11 | R1 验收只跑 `-p ui_core` | 改为 `cargo test -p ui_core -p ui_kit`（R1 改了 ui_kit 的测试代码） | 与 D21/§11-1 一致 |
| H12 | §5.4 未写 fragment Builder 的状态隔离与偏移基准 | 新增约束 11：fragment 用**自己的源切片**、自己安装 `SourceLines`、独立 `block_index/previous_blank/last_line_end`，退出恢复；所有区间相对该切片 | 状态串用会导致空行/间距错位 |
| H13 | §5.4 未写 `TableBuf` 缓冲期的列/行补齐时机 | 改为 `End(TableRow)` 补齐 `CellBuf`、`End(Table)` 建表；`End(TableCell)` 不建节点 | 与 G2 配套 |
| H14 | §5.10 未写多表/嵌套表格的列宽隔离 | 每 Builder 实例独立表缓冲（§5.10 第 8 条） | 复审 O-1 |
| H15 | §8.2 未给 cell 内容宽的读取方式 | 写死 `bounds.width − 2×CELL_PADDING`（padding 读 `node.style().inner().padding`） | 门禁 7③ 可执行性 |
| H16 | §10.3 自检清单缺 4 条 | 补 22–25：fragment 状态隔离、`TableBuf` 缓冲期不建节点、`set_text_style` 必须标脏、嵌套表格列宽隔离 | 前三轮踩坑汇总 |
| H17 | A2 的「无竖线则跳过表格预扫」触发条件 | 随 G2 **取消**（延迟建表没有触发条件，也不需要 `contains` 判断竖线） | 机制替换后原触发条件作废，避免实施者继续实现预扫 |
| H18 | **O-3**：§5.3 与 §9.3 的任务项节点数一处写 4/≥5、一处写 3/4 | 统一为「紧任务项 3（未选中）/4（选中）；松任务项 4/5」（§5.3、§9.3） | 同一形状两套计数 |
| H19 | **N-7**：`AlignWindow{align, depth}`（§5.3）与 `AlignWindow{align, paragraphs}`（§5.6.3）不一致，`depth` 无用途 | 统一为 `align_windows: Vec<Align>`（只存 align，v3.3 起无 `paragraphs`），§5.3/§5.4/§5.6.3 一致 | 字段定义漂移 |

引擎改造审查补充（6 条，落在 §5.11/§7-R1）：

| # | 内容 | 落点 |
|---|---|---|
| I1 | `TextBackground::is_visible()`（透明色 / NaN / 负 padding 过滤），shaping 与两条 paint 路径都必须先调用 | §5.11.1 |
| I2 | `paint/pass.rs` 私有 `TextDecorations` **必须**改名为 `EditDecorations` | §5.11.2、§7-R1 |
| I3 | `line_top` 与 `line_height` **同源且严格相接**（分数行高 19.6px 用例钉死无重叠/无缝隙） | §5.11.3、§5.11.4 |
| I4 | 背景分组：按「同色 + 同圆角 + 同 padding」分组，**padding 只加在组两端**（防异组 2×padding 双绘） | §5.11.3、§5.11.4 |
| I5 | 非加速 canvas text 路径（`window/canvas_text.rs`）也必须画 run 背景，z 序与 GPU 路径一致 | §5.11.2、§5.11.3、§7-R1 |
| I6 | R1 只做最小编译适配（保持旧 else-if 语义）；三条语义改写（链接+删除线共存、code 底色、kbd 下划线+底色）及测试**归 R2** | §5.11.4 末尾、§7-R1、§7-R2 |

v3.2 → v3.3（第四轮收敛：废弃关窗回写、统一行号空行口径、补借用纪律）：

| # | 原表述（v3.2） | 新表述（v3.3） | 原因 |
|---|---|---|---|
| J1 | 机制 A（`InlineBuf::paragraph(current_align())`）**与**机制 B（登记段落 NodeId、关窗 `set_text_style` 回写）并存 | **只保留机制 A**：align 在 `Start(Paragraph)` 时由 `current_align()` 写入 `TextStyle`；窗口 = `align_windows: Vec<Align>`（只存 align）；块级 `</p>` **立即出栈**，段落内 `InlineHtml("</p>")` **置 `pending_window_close`、等 `End(Paragraph)` flush 建完节点后再出栈**；**EOF 不撤销已施加 align**（新增偏差 §10.1-22）；删除 §5.11.6 整节、§5.13 的 `set_text_style` 项、§7-R1 该项交付、§13 对应行 | 段落节点在 `End(Paragraph)` 的 `flush_inline()` 才创建，而 `</p>`（`InlineHtml`）先于它到达 → 登记表必然为空，机制 B 不成立；回写还需新引擎 API 且要处理 `ShapeKey` 不含 color/alignment 的失效问题 |
| J2 | R1 新增 `NodeTree::set_text_style`（含两条测试、`-> bool`） | **整节删除、不新增该 API**；§5.13 改为「本轮不需要新增任何引擎公开 API」并列出只用的现有公开面 | 与 J1 配套；`-> bool` 也不符合引擎同类 mutator 的 `Result<(), EngineError>` 惯例 |
| J3 | 关窗侧 `set_text_style(node, node.style().with_alignment(align))` | 随 J1 删除（`node.style()` 是 paint `Style`，没有 `with_alignment`；`TextStyle` 读入口是 `pub(crate)`） | 该写法本就不可编译 |
| J4 | `blank_lines_between` 的字节口径 + 「`from_byte` 若指向行尾序列则跳过」+ 反例区间 `5..7` | **行号口径**：`blank_lines_between = max(to_line - from_line - 1, 0)`，`from_line = line_of(prev_end − 1)`（上一块 End 区间最后一个字节所在行）、`to_line = line_of(next_start)`；**删除 `next_line_start` 条款**；反例按实测区间改为 `a\n\nb`（0..2/3..4）→1、`a\n\n\n\nb`（0..2/5..6）→3、`a\r\n\r\nb`→1、en.md→34 | 「跳过行尾序列」把「上一块恰好结束于空行行首」的 1 个空行也跳过 → 常见场景少 1，打穿 34/70 门禁 |
| J5 | 约束 11 只隔离 `lines/last_line_end/block_index/previous_blank`；`base_depth` 无语义 | 隔离字段补全为 `lines`/`last_block_end`/`block_index`/`previous_blank`/`table`/`inline`/`marks`/`align_windows`/`pending_window_close`；`base_depth` **只用于累加 `MAX_BLOCK_NESTING`**，fragment 内 depth 按自身解析层从 0 计；fragment 期 `align_windows`/`table` 独立且为空、**fragment 内不允许开窗** | 共享 `align_windows` 会让 fragment 内的 `<p align>` 污染主解析栈；`base_depth` 无语义则 fragment 内 HTML 块会被误判为 depth > 0 而整体降级 |
| J6 | `End(Table)`「先测量再建节点」未写借用纪律 | §5.10 新增：Builder 持 `&mut App`、**不跨调用持 tree 借用**；`End(Table)` 先对全部 `CellBuf` 调 `measure_text(None)`（此时不持 tree），再一次性 `window_mut(...).tree_mut()` 建表；降级判据 = `App::font_stack_status() != Ready` | 照现有 `tree: &mut NodeTree` 一路下传的形态，`End(Table)` 里调不到 `measure_text` |
| J7 | §10.2 R7「±2%/±4px」vs §8.4 门禁 7「±5%」 | 统一 **±5%**（门禁 7 与 R7 一致） | 同一门禁两套容差 |
| J8 | §10.2 R2「§5.11.4 的 11 条测试」 | **14 条**（§5.11.4 表现有行数；不含已删除的 `set_text_style` 两条） | 计数漂移 |
| J9 | §5.4 反例区间「para b 5..7」 | **5..6**（`a\n\n\n\nb` 实测；5..7 只出现在带尾随换行的源） | 实测区间错 |
| J10 | §5.1 流程图两行重复「顶层 Paragraph：创建时若窗口非空 → 登记」 | 合并为一行「`InlineBuf::paragraph(current_align())`——创建时即取窗口栈顶 align」 | 重复行 |
| J11 | §5.1 `build_fragment(slice, kind, &mut budget)`（3 参）vs §5.6.3（4 参） | 统一 **4 参**：`build_fragment(slice, kind, base_depth, &mut budget)` | 形参数量不一致 |
| J12 | §8.4 门禁 7③ 未写前提 | 加前提「**仅在该表总自然宽 ≤ 可用宽、未触发 flex 收缩时**」；触发收缩时只断言 ①② | 收缩后内容宽 ≠ 自然宽，③ 必红 |
| J13 | `set_text_style -> bool` 与引擎 mutator 惯例不一致 | 随 J2 删除（不再新增该 API） | 惯例不符 |
| J14 | §14.7 v3.1→v3.2 块缺 O-3/N-7 两行、H17 排在引擎表后 | 补 **H18（O-3 任务项节点数）/ H19（N-7 AlignWindow 字段）**；H17 移回 H16 之后；标题标注两行位置 | 复审发现计数与顺序断裂 |
| J15 | 降级判据写作「`measure_text` 不可用（无字体栈等）」 | 写死 **`App::font_stack_status() != FontStackStatus::Ready`**（`app.rs:1947`） | `measure_text` 返回 `LogicalSize`、无失败路径，判据必须点名公开入口 |
| J16 | 「片段注释不进代码」免责只写在本节（§5.11） | 扩到**头部总则**（全文所有 `rust`/`text` 片段的 `//` 注释只为说明） | §5.10/§5.5 也有片段注释，照抄会违反 AGENTS §1.6 |

v3.3 → v3.4（终审 8 条清理，无设计变更）：

| # | 原表述（v3.3） | 新表述（v3.4） | 原因 |
|---|---|---|---|
| K1 | §5.6.4 第 9/13 条仍是机制 B 处方（「登记 id 但不写 align」「关闭事件到达时统一写 align」「进窗口登记表」）；§8.1-9「各应用一次」；§5.8/§5.9 两处「EOF 未关窗 → 丢弃登记 / 段落保持默认对齐（§10.1-4）」；§10.2 R5「延迟应用：登记后忘记应用」 | 全部改为机制 A：段落创建时 `InlineBuf::paragraph(current_align())` 即写入 `TextStyle.alignment`，窗口只 push/pop、pop **不「应用」任何东西**（§8.1-9 改「各出栈一次」）；EOF 两处统一为「**不撤销已施加的 align**」并引用 **§10.1-22**；R5 改为「**时序**：行内 `</p>` 出栈过早 → 段落取不到 align；块级 `</p>` 出栈过晚 → 后续段落误取 align」 | 两套机制并存，照做即错；EOF 表述与新语义相反且引用错 |
| K2 | 空行 `from_line = line_of(prev_end − 1)`（「最后一个字节」）在 List 上少算（`End(Item)`/`End(List)` 区间末尾常是多个换行，`prev_end−1` 落到空行） | 口径改为「上一块 `End` 区间内**最后一个非空白字节**（跳过 `\r\n`/`\n`/`\r`/空格/TAB）所在行；若区间全为空白，退回 `line_of(区间起点)`」；`to_line` 与公式不变；补 4 条 list 反例（`- a\n- b\n\np\n`→1、`- a\n\np\n`→1、`- a\n- b\n\n\np\n`→2、`1. x\n\np\n`→1），并注明其它块型不受影响 | 文档公式在列表上比实测少 1 |
| K3 | §7-R2 提交说明「§10.1 的 21 条偏差」 | **22 条**（与当时的 §10.1 表头、实际行数一致；R2 落地时新增 23–28，R3 再新增 29–33，故现在是 **33 条**，见 §10.1 表头与 §14.4） | 计数漂移 |
| K4 | J16 声称「注释免责已扩到头部总则」，实际只在 §5.11 | 头部新增总则：**全文所有 `rust`/`text` 片段的 `//` 注释只为说明语义，实施按 AGENTS §1.6 不写注释**（§5.11 原句保留） | 免责未落地 |
| K5 | §10.1 表内第 21 行与第 22 行之间有一个空行，`§10.1-22` 被挤出表格 | 删除该空行，22 行全部在表内 | 表格结构破损 |
| K6 | §2.6-C 行引用 `§10.1-16` | `§10.1-17`（7 组原始串 key） | 引用错位 |
| K7 | D21 段与 §10.2 R13 写「产品侧 10 个 crate」 | **11 个**（与 §11-2 命令实列一致） | 数量不符 |
| K8 | §5.3 验收断言与 §8.5 写「zh-CN.md 的 11 个段内软换行」 | 改为「**11 个多行段落 / 15 个 `SoftBreak`**」（实测），语义仍是软换行必须合并、不产节点 | 把「段落数」误写成「SoftBreak 数」 |
