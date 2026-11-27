# P1 终审对抗性审查（完整性 / AGENTS / 性能 / 文档 / 门禁）

> **本报告已被 `review-p1-final-quality-r2.md`（第 2 轮收口复核，2026-11-27 10:47）取代**：Q1-01…Q1-10 的最终处置（fixed / deferred / 文档再同步）以 r2 §2 为准；本文件保留为第 1 轮审查记录。

- 审查对象：worktree `E:\kytrix\.agents\worktrees\dsh-1d175414-1a41`，基线 `d9949aca`，P1 全部未提交改动（111 个 tracked 文件 + 7 个 untracked 路径）。
- 审查时间：2026-11-27 08:49–09:16（CST）。**审查期间 P1 回写由另一 agent 并行落盘**：设计文档 `§12` 于 09:02–09:05 出现、`p1-summary.md` 于 09:08 出现（本报告以 09:05/09:08 落盘版本核对，09:10 复读确认）；行号可能继续漂移，引用以符号/章节为准。
- 只读约束：未修改任何源码；`git status --short` 与审查开始时一致（本报告位于 `.docs`，被 `.gitignore` 忽略）。
- 严重度口径（沿用 P0/P1 审查）：**P0=正确性缺陷/硬要求违反/回归/门禁虚假通过；P1=重大健壮性或测试可靠性缺口；P2=次要问题；P3=风格/文档/测试缺口。**

## 1. 范围与方法

### 1.1 必读与核对对象

- 设计：`.docs/popup-overlay-redesign-design.md` §5.1/§5.2/§5.3、§6 P1-10/P1-11/P1-12、§8、§9、§10、§11、**§12（P1 回写，审查中途出现）**、附录 C。
- 实现记录：`implementation/p1-migration-inventory.md`（§5.3 组合模式、§1 消费者清单）、`implementation/p1-cleanup-w7.md`（W7 删除/测试重写/gallery 迁移，§6 门禁自证）、`implementation/p1-summary.md`（09:08 由另一 agent 落盘，已核对）、早审报告 `review-p1-engine-api.md`（A-01…A-34）与 `review-p1-engine-behavior.md`（B-01…B-17）。
- `AGENTS.md` 全条；`crates/widgets/AGENTS.md`（本轮修改）、`crates/ui_core/design/custom-gui-engine-design.md`（历史设计，未改）、`.docs/gui-migration/spec-deltas/kit-gallery.md`（未同步）。
- 代码：P1 新增 `ui_core/src/popup.rs` + `popup/`、`app/popup.rs`、`a11y/popup.rs`、`a11y/popup_tests.rs`、`tests/popup_behavior.rs`、`ui_kit/src/kit/popup_panel.rs`；`git diff d9949aca` 全量；宿主侧 `widget`/`kytrix`/5 个业务 ui crate/15 个 widgets crate/测试支持/gallery。

### 1.2 实际执行的验证

| # | 命令/方法 | 结果 |
|---|---|---|
| 1 | `cargo test -p ui_core -p ui_kit --lib --tests`（`CARGO_TARGET_DIR=E:\kytrix\target`） | **PASS，3734 passed / 0 failed / 2 ignored**：ui_core lib 2804（+1 ignored）、frame_record 8、layout 4、popup_behavior 92、snapshot 14、demo example 39、ui_kit lib 773；gui_smoke 0 passed/1 ignored。与 §12.5 完全一致 |
| 2 | `cargo test -p ui_core -p ui_kit`（完整门禁命令） | **PASS，exit 0，3734 passed / 0 failed / 2 ignored**（09:09 复跑，与 #1 计数一致）。中间 3 次尝试曾在 link `ui_kit` example `gallery` 阶段报 `LNK1104: cannot open file 'E:\kytrix\target\debug\examples\gallery.exe'`，原因是另一 agent 的 gallery 进程（PID 34476，09:01–09:08）占用同一 target 产物；其退出后完整命令通过，属环境争用而非仓库缺陷（`p1-summary.md` §9 也提醒了同一现象） |
| 3 | `cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings` | **PASS**（无 warning） |
| 4 | `cargo +nightly fmt --all -- --check` | **PASS**（exit 0） |
| 5 | `cargo check --workspace --all-targets` | **PASS**（含 widgets/business/kytrix 全 target 编译） |
| 6 | `cargo run -p ui_kit --example gallery -- --pixel-check` | **PASS，exit 0**：`checks=93 sampled_frames=32 open_animation_frames=8 close_animation_frames=12 damage_full_frames=0`，`first_visible==stable=(496,91,300×375)`，`moved=(496,4,300×375)`；30‰ 覆盖下限 + 2 点注入自检通过（WARP） |
| 7 | `cargo run -p ui_kit --example gallery -- --debug-stack --auto-close=9` | **PASS，exit 0**：menu 度量 `(6, 180.0, 127.5125)`、picker children=4、最终 `entries=0`、`overlay-check PASS`；首/末帧 `popup_stack` 快照可见（menu 首帧 `Opening`、次帧 `Open` 且 rect 一致） |
| 8 | `rg` 等零引用复跑 | 见 §3（8 组模式，全部 0；1 个保留词命中已解释） |
| 9 | 静态核查 | 新增代码注释/`#[allow]`/`#[expect]`/`unwrap`/`serde_json`/`#[path]`/新依赖/生产 `format!` 全为 0；`git diff` 新增 6478 行中注释行 0；测试 `#[ignore]`/`should_panic` 新增 0；测试净增不减（kit 组件 51 条随旧文件删除，组件+PopupPanel 净增 55 条；`interaction_tests` 74→65 与 W7 的 6 改写 + 9 删除一致） |

> 说明：`--debug-stack` 的 `menu_frames`/`picker_frames` 是逐帧采样计数，运行间有浮动（本次 293/426，§12.5 记录另一次 436/388），断言只要求 >0，不构成偏差。

## 2. Findings（Q1-01…Q1-10）

**统计：P0 = 0，P1 = 0，P2 = 3，P3 = 7。**（另见并行的 `review-p1-final-kit.md`：P0=0/P1=0/P2=5/P3=8，其中 K1-01/K1-03 与本报告 Q1-09/Q1-10 同源；K1-02/K1-05 属需 lead 裁决的 parity 项，见 §8-7。）

### Q1-01（P2）P1-4「`PopupPanel` 确定高度内部滚动」未落地，§12.2 完成口径不实

- 证据：
  - 设计 P1-4（`popup-overlay-redesign-design.md` §6，当前行 766）要求「运行时 `set_max_size` + `PopupPanel` 内部滚动（确定高度，不靠 max-height 裁）」；§4.5 进一步要求「`PopupPanel` 在 `resized=true` 时把内容放进**确定高度** viewport（`height=max_height`，`overflow_hidden` + 内部 List/ScrollBar）」。
  - 引擎侧已交付：`settle.rs` 施加 `MaxSize`、把结果写入 `PopupEntry.constrained_size`（`popup/settle.rs:86-95,167-210,371-391`），并有测试 `a_constrained_popup_exposes_its_applied_size_without_unsettled_churn`（`tests/popup_behavior.rs:3551`）。
  - **kit 侧零消费者**：全仓 `constrained_size` 的读取只出现在引擎快照构造（`app/popup.rs:715`）、gallery dump（`examples/gallery.rs:2851`）与测试；`ui_kit` 生产代码（含 `popup_panel.rs`/`menu.rs`）没有把 `constrained_size` 转成内容高度。
  - `PopupPanel` 只写宽度（`popup_panel.rs:580-612`），从不设置 body/viewport 高度；`ItemPanel` 的列表高度来自**静态** `MenuOptions.max_height`（默认 320px）：`menu.rs:504-516`（`content_height().min(max_height - padding*2)`）、`menu.rs:530-544`（把该高度写进 surface style）。§12.2 P1-4 与 `p1-summary.md` §2 P1-4 都写「`PopupPanel` 在 `constrained_size` 下把内容放进确定高度 viewport（内部 List/ScrollBar）」，与代码不符。
  - 覆盖缺口：`popup_behavior` 只有 rect/诊断断言；kit 无「超长菜单在小视口缩小 + 内部滚动 + 末行可达」用例（§7.1-4 的验收未锁）。
- 影响：窗口高度小于「面板开销 + `max_height`」时（菜单/下拉/选择器/对话框内容），panel 被引擎 clamp 且 `overflow_hidden`，内部 List 的 viewport 仍是静态高度 → 滚动范围按旧的 320px 计算，**末行可能被裁在可视区之外不可达**；设计 §4.5 的“不靠 max-height 裁”没有实现。正常 1200×800 下不可见（320+开销 < 800），属边缘视口/超长内容缺陷。
- 修法（二选一，必须在提交前定）：
  1. 实现：`PopupPanel`/`ItemPanel` 读取 `PopupEntry.constrained_size`（或 kit 侧 `App::popup_rect`/诊断），把 body/surface 高度设为 `constrained - chrome`，让 List 的 viewport 与 clamp 后高度一致，并补「小视口超长菜单可滚到末行」测试；
  2. 或把 §12.2 P1-4 改为如实口径（引擎 API+`constrained_size` 已落地；kit 确定性高度内部滚动未实现），登记为 P2-3/P3-5 遗留。
- 是否阻塞：**提交前必须处理（代码或回写文字）**；代码本身不是 P0 回归（正常视口无影响）。

### Q1-02（P3，AGENTS 硬规则）`Anchor::Node.edge` / `AnchorEdge` 生产无读取者（死字段）

- 证据：`ui_core/src/popup.rs:402-415` 定义 `AnchorEdge` 与 `Anchor::Node { node, edge }`；唯一解析点 `popup/anchor.rs:11` 用 `Anchor::Node { node, .. }`，**edge 被忽略**；全仓 edge 只在构造点出现（kit Menu/Dropdown/Select/Tooltip/ColorPicker/Dialog/PopupPanel、business crates、gallery、测试），无 `.edge` 读取。
- 影响：违反 AGENTS §2.1「生产代码中的数据结构不得定义未使用字段」与 §4「不再使用的代码、类型、字段应直接删除」；同时 `Anchor::Node` 的 `edge` 与 `PlacementSpec.side` 构成两份语义，P2 子菜单 API 容易误用。
- 现状：§12.6「待复核 1」已登记，但未处置。
- 修法：删除 `Anchor::Node.edge` 与 `AnchorEdge`（同步 kit/测试构造点；边语义统一由 `PlacementSpec.side` 表达），或明确接入边语义并补测试。若 lead 裁决保留，必须在 §12.6 写明理由与 P3 编号。
- 是否阻塞：严格 AGENTS 合规下**否不可提交**；至少需 lead 明确裁决并记录，否则应在提交前删除。

### Q1-03（P3）生产 `expect` 兜底仍存在（已登记，未闭环）

- 证据：`app/popup.rs:135,154,169,189,251,267,389,396`（`expect("window was just resolved")`，均为同一函数内已解析窗口后的私有 helper）、`popup/stack.rs:252,256,291,373`（长度/索引不变量，`373` 为 `"just checked"` 的二次查找）、`popup/placement.rs:204`（候选序列恒非空）。公开 API 不可达（已逐条确认：helper 调用点均在同一 `&mut App` 借用期间，窗口不可能被移除）。
- 影响：与设计 §4.13「禁止 unwrap/静默 clamp」及 AGENTS §3 强类型错误精神有落差；若未来 helper 被复用/重构，可能变成可达 panic。§12.6「待复核 8」已如实登记。
- 修法（低优先）：把 8 处窗口 helper 改为接收 `&mut Window`（调用方已解析，零成本），或返回 `PopupError::UnknownWindow`；`stack.rs:373` 合并为单次 `get`。
- 是否阻塞：否（P3，登记后 P3 跟进）。

### Q1-04（P3）widget 层把全部 `DialogError::Popup(_)` 折叠为 `EngineError::UnknownWindow`，错误语义丢失

- 证据：`widgets/financial_index/src/dialog.rs:1441-1445`、`x/src/dialog.rs:3073-3077`、`hyperliquid_wallet/src/dialog.rs:1207-1211`、`solana_wallet/src/dialog.rs:1602-1606`、`ethereum_wallet/src/dialog.rs:1605-1609`、`updater_ui/src/dialog.rs:957-961`、`profile_ui/src/profile.rs:1351-1356`：`ContentBuild/StackSync` 保留 source，其余 `Popup(_)` 一律映射为 `UnknownWindow`（含 `LimitReached/DepthExceeded/InvalidPlacement/StalePopup/UnknownPopup/InvalidScrim/HintBlockedByModal`）。
- 影响：AGENTS §3 要求上层能按错误类型决定处理策略；这些错误被伪装成“窗口不存在”，日志/用户提示不可区分。与旧代码相比映射面变大（旧只映射 `PlacementConfig|NotOpen`）。
- 修法：为 `DialogError::Popup` 建立到 `EngineError` 的穷举映射（`UnknownWindow` 只对应真正的窗口错误，其余映射到对应 `EngineError`/新强类型变体），或用 `DialogError` 原样上抛；6 个 widget 共用一处函数避免重复。
- 是否阻塞：否（P3；错误路径在正常使用中罕见，但属新代码引入的规范回退）。

### Q1-05（P3）§12.1 回写数字系统性偏小（7 条 untracked 路径展开为 14 个文件）

- 证据（以当前工作树实测；设计 §12.1 与 `p1-summary.md` §1 是同一组数字）：
  - 删除文件：`kit/overlay.rs` 2871（回写 2701）、`kit/layer.rs` 964（896）、`app/overlay.rs` 184（177）、`interaction/overlay.rs` 331（296）；合计 **4350**（回写 4070）。
  - 新增文件：`popup.rs` 633（回写 551）、`popup/anchor.rs` 21（20）、`animation.rs` 187（182）、`dismiss.rs` 582（561）、`focus.rs` 276（267）、`placement.rs` 1146（1070）、`settle.rs` 408（398）、`stack.rs` 759（687）、`popup/tests.rs` 1540（1429）、`app/popup.rs` 809（775）、`a11y/popup.rs` 352（333）、`a11y/popup_tests.rs` 1719（1663）、`popup_behavior.rs` 4068（3947）、`popup_panel.rs` 1283（1201）。
  - 「新增文件（7 条路径）」是 `git status` 的 7 条 untracked 条目（其中 `popup/` 目录展开为 9 个文件，实际 14 个文件）；行数全部偏小，应说明口径（7 条路径 vs 14 文件）。
- 影响：§12 是 P1 的审计记录与复现口径，行数错误会让后续审查误以为回写对应旧快照；`popup_behavior` 等数字差异较大（121 行）。
- 修法：用 `git show d9949aca:<file> | Measure-Object -Line` / `Get-Content | Measure` 重算（或删除精确行数只保留 `git diff --stat`），并把「7 条路径」改为实际计数。
- 是否阻塞：否（P3 文档），但建议**提交前修正**，因为 §12 是本次提交的说明来源。

### Q1-06（P3）§12.9 第 2 项结果自相矛盾；§8 仍保留未实现的 `PopupHost` / `cx.popup_host()`；`kit-gallery.md` 未同步

- 证据：
  - §12.9 第 2 行模式 `kit::layer|layer::\{|Layer::new|LayerSlot|Band::` 写「0（唯一命中是 `CAMetalLayer::new()`…）」，自相矛盾；实测原始模式 **1 命中**（`ui_core/src/render/macos/swapchain.rs:26`，纹理 layer，保留），用 `\bLayer::new` 才为 0（W7 §5 已说明判据收紧）。其余 7 组实测 0。
  - 设计 §8 表两行仍写「替换为 `PopupHost`/`PopupPanel` 能力」「替换为 `cx.popup_host()`（新 API）」，但 P1 最终实现**没有** `PopupHost`/`cx.popup_host()`（组件构造只收 `theme`；`WidgetServices` 只保留 `frame_router`），§5.2/§12.4 的最终 API 也不是该形态；§8 下方「P1 完成情况」却称全部完成。
  - `.docs/gui-migration/spec-deltas/kit-gallery.md` 仍大量使用 `--dump-overlay`，设计 §10-7/§12.6 已登记「P3-7 同步」，但 P3-6 三端命令若照旧文档执行会命中 Q1-07。
- 修法：§12.9 写「1 命中（保留词）/判据改为 `\bLayer::new` 后 0」；§8 两行改为最终形态（无新 API，组件只收 theme）；kit-gallery 同步登记到 P3-7 并在 P3-6 命令中显式写 `--debug-stack`。
- 是否阻塞：否（P3 文档）。

### Q1-07（P3）`--dump-overlay` 被静默忽略：旧验收命令会 exit 0 且不执行 overlay-check（假绿风险）

- 证据：`ui_kit/examples/gallery.rs:139-165` 的 `parse_flags` 对未知参数只 `println!("gallery: ignoring unknown argument {arg}")` 后继续；`--dump-overlay` 已从 flag 列表删除（改名 `--debug-stack`）。旧脚本（设计 §7.4/§11.5、`kit-gallery.md`、三端历史脚本）传 `--dump-overlay --auto-close=9` 时：只打印忽略行、正常开窗、9s 后关窗、**退出码 0，overlay-check 从不运行**。
- 影响：P3-6 三端验收若沿用旧命令会得到假绿；AGENTS §1.1 虽禁止兼容层，但“未知参数静默成功”本身也不是显式失败。
- 修法：未知参数打印后 `std::process::exit(2)`（或返回错误），让旧命令显式失败；P3-6 文档/脚本统一 `--debug-stack --auto-close=9`。
- 是否阻塞：否（P3；仓库内当前无脚本使用旧名，只有文档）。

### Q1-08（P3）`--pixel-check` 108→93 的覆盖差异未在 §12 说明；`sampled-animation-floor` 口径需明示

- 证据：P0 口径 `108 checks = 45 采样断言 + 20 idle-frames + 20 background-residue + 3 restored + 2 close-frames + 18 具名`（设计 §11.3-9）；P1 实测 `93 = 32 采样断言 + 16 background-residue + 45 静态`（本次复跑与 §12.5 一致）。差异来源：场景迁到 `PopupPanel` 后 idle tick 不再出帧（`PixelCheckRun::observe` 在 `PixelFrame::Idle` 提前 return，不再执行 idle 期间的 `background-residue` 逐像素比较；idle 由 20 条 `idle-frames` 断言覆盖）。§12.3-9 只登记了「108→93、45→32、新增 2 条 floor」，未说明 **idle 期 background-residue 不再执行**这一语义变化；且 `sampled-animation-floor` 实为「settle/close 窗口内已提交帧增量 ≥4/≥3」（W7 §7.1 自述），不是“每帧动画”下界。
- 影响：后续审查可能误判 93 checks 与 108 等价；P3-5 性能/像素基线对比时口径不清。
- 修法：§12.3-9 补一句覆盖差异与 floor 口径；或把 idle 期背景比较改为在 idle tick 也执行一次（不改断言数即可）。
- 是否阻塞：否（P3 文档/测试口径）。

### Q1-09（P2）设计 §4.7「菜单/右键菜单祖先滚动或 resize 关闭整链」矩阵未实现，且 kit 测试断言了相反行为（与 `review-p1-final-kit.md` K1-01 同源，本报告独立复核）

- 证据：
  - 设计 §4.7「组件级 anchor/scroll/resize 矩阵」（`popup-overlay-redesign-design.md:513`）要求：菜单/右键菜单在祖先滚动或窗口 resize 时**关闭整链**；Dropdown/Select/ColorPicker 才同帧 re-settle。
  - 实现：`Menu`/`ContextMenu` 的 `MenuInner`/`ContextMenuInner` 没有“祖先滚动/resize → 关闭”的检测；滚动置 geometry dirty 后引擎重新 settle（`popup/settle.rs:121-129`），Menu 跟随锚点行、`Anchor::WindowRect` 的 ContextMenu 仍钉在旧窗口点位；`DismissPolicy::Chain.anchor_lost` 只在 `resolve_anchor` 返回 `AnchorNodeMissing` 时生效（`popup/settle.rs:258-266,394-396`），节点仍存在则不触发。
  - 相反行为被固化为测试：`ui_kit/src/kit/context_menu.rs:762-777` `the_page_scrolls_under_an_open_context_menu` 滚动后断言 `menu.is_open(...)` 为 true（消息“背景滚动后菜单仍打开（WindowRect 锚点）”）；`menu.rs` 无祖先滚动/窗口 resize 用例。
  - §12.6/§12.3 未登记该矩阵缺口（只登记了 `Dialog::recenter`、菜单链 P2 等）。
- 影响：滚动页面时菜单/右键菜单悬停在不再对应内容的位置（或跟随锚点行），违反设计 §4.7 验收矩阵；P2 `MenuChain` 若基于此实现会把偏离固化。属**设计矩阵缺口**（旧 kit 的锚点滚出视口行为同样未闭环，见 kit 审计 K14，不能称为旧行为回归）。
- 修法：kit 在 `sync` 中对菜单/右键菜单比较祖先滚动 offset 与窗口尺寸快照，变化时 `close_chain`（Dropdown/Select/ColorPicker 保持 re-settle）；把 `the_page_scrolls_under_an_open_context_menu` 改为“滚动后关链”或由 lead 明确裁决登记设计变更。
- 是否阻塞：**提交前需修或由 lead 明确裁决并登记**（P2）。

### Q1-10（P2）Dropdown / Select 的「首可见帧 == 稳定帧」测试是空断言（先 settle 再采样；与 `review-p1-final-kit.md` K1-03 同源，本报告独立复核）

- 证据：`ui_kit/src/kit/dropdown.rs:1205-1224` 与 `select.rs:2166-2185` 的 `the_first_visible_frame_already_has_the_settled_rect`：`harness.click()` 内部（`dropdown.rs:947-958`）先调用 `settle()`（6×40ms + 200ms 帧 + drain，`dropdown.rs:914-924`），之后才 `popup_entry()` 采样所谓“首帧”，再 `settle()` 比较——两次采样都在完全 settle 后，无法发现“首可见帧错位/晚一帧”。同一断言在 PopupPanel（`popup_panel.rs:906-928`，单帧 drain）、Menu（`menu.rs:2122-2133`，open 后单帧）、ContextMenu（`context_menu.rs:498-514`，单帧）、Dialog（`dialog.rs:1234-1265`，open 后单帧）、ColorPicker（`color_picker.rs:3336-3358`，open→1 帧→sync→1 帧）是真断言；仅 Dropdown/Select 是空的。
- 影响：P1-12 的核心测试要求（首可见帧断言）在生产使用最广的两个组件上没有真实覆盖；K17/设计 §7.3 的首帧回归无法被这两条测试发现（引擎侧 `popup_behavior` 仍有覆盖，但组件层 gap 存在）。
- 修法：click 后**只 drain 一帧**（不调用 `settle`）即读取 `popup_entry().hit_rect`，再 settle 比较；或给 Harness 增加 `click_without_settle`。
- 是否阻塞：**提交前应修**（测试可靠性，P2）。

## 3. `rg` 零引用复跑表（inventory §5.3 口径）

工具说明：环境无独立 `rg` CLI（`rg` 不在 PATH、VS Code 内置路径不存在），使用本会话 grep（ripgrep 语义）逐条复跑；范围 `crates/**/*.rs`。

| # | 模式 | 命中 | 判定 |
|---|---|---:|---|
| 1 | `kit::overlay\|overlay::\{\|OverlayOptions\|OverlaySurface\|OverlayA11y\|OverlayError\|AnimationRouter` | 0 | PASS |
| 2 | `kit::layer\|layer::\{\|Layer::new\|LayerSlot\|Band::` | **1**：`ui_core/src/render/macos/swapchain.rs:26` `CAMetalLayer::new()`（macOS 纹理 layer，保留词，inventory 已登记） | PASS（排除后 0；`\bLayer::new` = 0） |
| 3 | `create_overlay\|close_overlay\|place_overlay\|bring_overlay_to_front\|resolve_overlay_placements\|dismiss_overlay\|\.overlays\(` | 0 | PASS |
| 4 | `OverlaySlot\|OverlayDismiss\|OverlayPlacement\|overlay_style\|note_overlay_container\|overlay_containers\|overlay_slot` | 0 | PASS |
| 5 | `\.layer\(\)\|\.animation\(\)\|animation_router\|services\.layer` | 0 | PASS |
| 6 | `overlay_wrapper\|MenuMode::ListBox` | 0 | PASS |
| 7 | `dump-overlay\|dump_overlay` | 0 | PASS |
| 8 | `\bOverlay\b`（单词级，排除 `DamageOverlay`/`A11yPopupOverlay` 子串） | 0 | PASS |

保留词复核：`A11yPopupOverlay` 在 `ui_core/src/a11y/popup.rs`/`a11y/tree.rs`（新投影，保留）；`DamageOverlay` 在 `ui_core/src/test_support/diagnostics.rs`（与浮层无关，保留）；`ItemPanel` 为新形态（`pub(crate)`，基于 `PopupPanel`+`List`，`ui_kit/src/kit/{menu,dropdown,select}.rs`）；`CAMetalLayer` 保留。旧文件 `ui_kit/src/kit/{overlay,layer}.rs`、`ui_core/src/{app,interaction}/overlay.rs` 均已删除（`git status` 4 个 `D`）；旧测试 helper `overlay_wrapper`、`declare_overlay`、`App::overlays` 零引用。

## 4. AGENTS 对照

| 条目 | 结果 | 证据/说明 |
|---|---|---|
| §1.1 不保留兼容/migration/fallback/过渡 | **通过** | 新增 6478 行中无 legacy/compat/deprecated/过渡 代码；旧 API 直接删除；仅 `PlacementSpec.fallbacks` 是功能词；`App::modal_scope` 保留是语义收窄（D-P1-20 登记）而非兼容层 |
| §1.2 保持简单 | **基本通过** | 无新增配置层/开关；但留有未消费公开面（`AnchorEdge`、`match_anchor_width`、`Window::set_max_size`、`FadeSlide`）——见 Q1-02/Q1-01/§12.6 待复核 3 |
| §1.3 模块布局、无 `#[path]` | **通过** | `popup.rs` + `popup/`（无 `mod.rs`）、`app/popup.rs`、`a11y/popup.rs`、`kit/popup_panel.rs`；`#[path]` 新增 0 |
| §1.4 复用成熟依赖 | **通过** | workspace `Cargo.toml` 零改动；仅 `ui_core/Cargo.toml` 新增 `[[test]] popup_behavior required-features=["test-support"]`；沿用 thiserror/smallvec/smol_str/ahash |
| §1.6 不写注释 | **通过** | `git diff` 新增 6478 行中 `//`/`/*` 0 行；7 个新文件与 `popup/*.rs` 全文注释 0；`crates/ui_core/src` 无注释 |
| §2.1 Serde / 无未使用字段 | **部分违反** | 无 `serde_json`/`RawValue`/`json!`/`deny_unknown_fields`/`rename_all`；但 `Anchor::Node.edge` 写而不读（Q1-02）；`match_anchor_width` 引擎字段无行为消费者（§12.6 待复核 3）；`Window::set_max_size` 仅测试消费（Q1-01） |
| §2.2 SmolStr | **通过** | 新增生产代码使用 `SmolStr`/`format_smolstr!`；新增生产 `format!` 0（diff 中唯一 `format!` 在既有测试 helper） |
| §2.3 集合类型/预分配 | **通过** | 新增使用 `SmallVec`/`with_capacity`（settle/popup/placement）；a11y 侧表用 `ahash::HashMap`；无 `std::collections::HashMap` 新增 |
| §2.4 字节数据 | n/a | 无字节缓冲改动 |
| §3 强类型错误 | **部分违反** | 引擎 `PopupError` 13 变体齐全、无字符串错误；但 widget 层 7 处把 `DialogError::Popup(_)` 折叠为 `UnknownWindow`（Q1-04） |
| §4 Warning/死代码/无 `allow`/`expect` 属性 | **通过（属性）** | 新增 `#[allow]`/`#[expect]`/`#[allow(dead_code)]` 0；clippy `-D warnings` PASS；生产 `expect` 兜底见 Q1-03（已登记） |
| §5 依赖管理 | **通过** | workspace 根未改，子 crate 未新增独立版本声明 |
| §6 小型函数/无意义抽象 | **通过（抽查）** | 未发现“只返回常量/单表达式”的包装函数；helper 均有复用/降复杂度作用 |
| §8 验收 | **部分** | `cargo test`（workspace）、clippy、fmt 为 AGENTS 要求；本轮只跑 `ui_core`/`ui_kit`（用户限制），workspace 测试与 widgets/business 测试未执行 → §9 残余风险 |

## 5. 性能观察（无新增实测基线）

- **空闲帧**：`app/frame.rs` 只在帧内调用 `popup::settle`；`settle::settle` 的 `needs` 在 `popup_dirty/视口/动画/layout_ran` 全假时立即返回（`popup/settle.rs:110-132`），`idle_frames_do_no_popup_settle_work`（`popup_behavior.rs:477`）锁定 `popup_settle_rounds==0`。**无 popup 时零 popup 工作**。
- **open/close 帧成本**：无实测数据（P3-5，§12.6 未执行 3 如实登记）。代码事实：打开首帧 settle ≤2 轮（1 次 max-size 重排 + 1 次 inset 重排，`popup/settle.rs:161-345`）；关闭动画帧走 `popup/animation.rs::tick`（每帧 mark paint popup 子树，无 layout）；非模态 popup 开关不 `damage_full`（`opening_and_closing_a_non_modal_popup_never_forces_a_full_repaint`）；模态/Scrim 打开 `force_full_publish`（`app/popup.rs:403-414`），与设计允许一致。
- **动画期间 lane 重建**：`animation::tick` 对 popup 节点写 `PopupTransform` 并 mark paint → 每个动画帧重建 popup 子树 lane（局部，无 layout）；8+12 帧的 120/90ms 动画属于预期成本，但没有帧耗时数据。
- **O(n) 路径**：`sync_order` 只在 `order_dirty` 时执行（`sort_order` O(k log k) + `reorder_children` O(子树)）；`sync_rects` O(entries)；settle 每次 O(entries·placement)；`settle` 触发含 `layout_ran && !stack.is_empty()`（主树任一布局帧重解 popup，宽于设计纯 dirty 闸门，B-16/§12.3-15 已登记）。a11y 每次投递全树 DFS（A-32，已登记 P3）。
- **内存**：`PopupStack` per-window 侧表（entries/generations/handlers/free_slots/order/animations/focus/max_sizes）按栈条目计，上限 1024；`NodeTree` 新增两张每节点侧表 `popup_transforms: Vec<Option<PopupTransform>>`（约 20–24 B/节点）+ `popup_pointer_transparent: Vec<bool>`（1 B/节点），40k 节点约 0.8–1.0 MB（估算）。§9 R9 只登记了 P0 的 44 B/节点，P1 这两张表未计入回写（P3 文档）。
- **结论**：代码层无“空闲帧做 popup 工作”或“动画帧触发布局”的明显热点；但**没有 open/close 帧预算实测**，P3-5 必须补基线；`settle` 的 `layout_ran` 触发条件建议在 P3-5 一并评估是否收紧。

## 6. 文档/门禁核对

| 对象 | 结论 |
|---|---|
| P0 §11 | **未被不准确改写**：头部仅加「P1 回写见 §12」交叉引用；§11.3-9 的 108 条/旧命令/§11.4/§11.7 保留为历史记录（实测 108 仍在 §11.2/§11.3-9）。 |
| P1 §12 出现与完整性 | 审查中途（09:02–09:05）由另一 agent 落盘，含 12.1–12.9：任务结果、D-P1-1…24 偏差、最终 API、测试/门禁基线、未执行项/待复核、P2/P3 衔接、早审 A/B 处置、对齐摘要、rg 表。**未发现与最终代码冲突的功能性声明**（抽查 A-01…A-34/B-01…B-17 的测试名 20 条全部存在）；但 §12.1 数字错误（Q1-05）、P1-4 口径不实（Q1-01）、§12.9 第 2 项矛盾（Q1-06）。 |
| 独立 P1 summary | `implementation/p1-summary.md` 已于 09:08 由另一 agent 落盘，已核对：任务结果/偏差/最终 API/门禁/遗留项与设计 §12 对齐（抽查无冲突）；但 §1 规模行沿用 §12.1 的错误行数（Q1-05）、§2 P1-4 沿用不实口径（Q1-01）。 |
| 命令可复现 | §12.5 的 6 条命令全部可复制执行；本次逐条复跑得到一致结果（完整 `cargo test` 因并发 gallery 占用 target 产物中途失败 3 次，进程退出后 09:09 PASS，见 §1.2 #2）。§12.5 的 `--debug-stack` 帧数（436/388）与本次（293/426）是运行间浮动。 |
| gallery 新命令语义/退出码 | `--pixel-check`：headless 持久增量像素门禁，exit 0/3，93 checks（采样帧/动画下限/覆盖下限/注入自检），运行时会打印完整语义串（`gallery.rs:3407-3422`）；`--debug-stack`：真实窗口 `popup_stack` 快照 + `overlay-check`（栈条目=1、tier、菜单度量、首可见帧==稳定帧、关闭无残留、结束栈空），失败 exit 3（`main` 合并 `failed`）。命令改名只在 W7/§12.3-8 登记，`kit-gallery.md` 未同步、未知参数静默（Q1-06/Q1-07）。 |
| 三端验收准备（P3 待办） | P3-6（真实窗口像素/三端）仍「未开始」，P3-5（性能）「未开始」，P3-7 文档「kit-gallery 未同步」——均在 §12.6 的 P2/P3 衔接表逐项登记；命令名/93-check 基线/性能热点需在 P3-6/P3-5 承接。**未发现 P3 待办漏登记**；建议在 P3-6 条目里补「`--debug-stack` 为唯一真实窗口命令、`kit-gallery.md` 需同步」。 |
| `crates/widgets/AGENTS.md` | 已更新且与代码一致（§5 删三单例、§13 构造只收 theme、§16 断言改 `popup_stack/popup_rect`、`WidgetServices` 表删 layer/animation_router 行）；未发现残留旧 API 指引。 |
| 其它文档 | `crates/ui_core/design/custom-gui-engine-design.md` 仍描述旧 `Overlay/Layer/AnimationRouter`（历史设计文档，非本轮范围，§10-7 未要求）；`kit-gallery.md` 为 P3-7 待同步。 |

## 7. 已核实正确点（正面证据）

1. **旧 API 删除完整**：4 个旧文件删除；`App::{create_overlay,close_overlay,place_overlay,bring_overlay_to_front,overlays,dismiss_overlay}`、`interaction::{Overlay,OverlayOptions,OverlayPlacement,OverlayDismiss,OverlaySlot,AnchorSide,resolve_placement}`、`overlay_style`、`note_overlay_container/overlay_containers/overlay_slot`、`kit::{overlay,layer}` 全类型、`AnimationRouter`、`MenuMode::ListBox`、`overlay_wrapper`、旧 a11y/focus scope 回落零引用（§3）。
2. **宿主迁移完整**：`WidgetServices` 删 `layer/animation_router` 字段与 builder 参数（`widget/src/services.rs`，12 处 builder 调用全部更新）、`WidgetCx::layer()/animation()` 删除、`binding/test_support` 删自建单例、`kytrix` composition/dialogs 去两单例、15 个 widgets crate 与 5 个业务 ui crate 的构造点全部改为 theme-only、4 个业务 test_support + market_trade 测试改 `popup_stack`、`ui_core/tests/hud.rs` 改 `open_popup`；workspace check PASS 证明无漏改。
3. **测试未静默减少**：kit 旧 overlay 37 + layer 14 = 51 条随文件删除，组件测试净增 55 条（menu 16→21、dropdown 18→23、select 23→29、tooltip 20→26、context 12→14、dialog 38→48、color_picker 73→78、新增 PopupPanel 16）；`interaction_tests` 74→65（6 改写 + 9 删除，逐项有覆盖去向，W7 §3.1）；`a11y/tree` 3 条、`interaction/focus` 2 条删除有去向；新增 `#[ignore]/should_panic` 0。首可见帧断言在 PopupPanel/Menu/ContextMenu/Dialog/ColorPicker 为真断言；**Dropdown/Select 两条为空断言**（Q1-10）。
4. **早审 P0/P1 大部分闭环**（代码+测试复核）：A-01 侧表清理（`arena.rs:596-599` + 测试）、A-02 modal a11y 挂 root（`a11y/popup.rs:255-257` + `assert_no_hidden_ancestors`）、A-03 ZPolicy 回滚（`invalid_z_policy_open_leaves_no_residue_and_next_open_works`）、A-04 required-features、A-05 Scrim 全视口（`settle.rs:191-196,242-250,325-334`）、A-07 `HintBlockedByModal`、A-08 popup scope（`a11y/action.rs:462-472` + `in_input_scope`）、A-10 子菜单方向（`app/popup.rs:53-79` + 测试）、A-11/A-12 a11y 父（`resolve_parent` 上溯 Container + 单通道）、A-13 Scrim 拒绝伪造（`app/popup.rs:217-228`）、A-14/A-15/A-16/A-18/A-19/A-20/A-21/A-23/A-28/A-30 均有对应代码/测试（§12.7 抽查 20 条测试名全部存在）；**例外**：A-09（`match_anchor_width` 转 kit，引擎字段无行为消费者）、A-22（IME caret 不随 paint_offset，§12.6 待复核 5）已如实登记为遗留。
5. **门禁真实**（以 §1.2 为准）：`3734 passed/0 failed/2 ignored`、clippy/fmt/workspace check PASS、`--pixel-check` 93 checks/0 damage_full/首帧==稳定帧、`--debug-stack` overlay-check PASS，全部为本审查独立复跑（非引用 W7 自证）。
6. **AGENTS §1.6/§2.1 无注释/无 allow/unwrap**：新增 6478 行注释 0、`#[allow]/#[expect]` 0、生产 `unwrap()` 0、`serde_json` 0、新依赖 0、`#[path]` 0。
7. **P1-0 原型结论成立**：0×0 popup root + auto-size 内容 + `HitFocusSkip::Container` 三处一致（hit/focus/a11y），`opening_a_popup_does_not_change_the_main_tree_layout`、`the_popup_root_never_wins_the_hit_test`、`tab_focus_skips_the_popup_root_container` 锁定。

## 8. 残余风险

1. **workspace 测试未跑**（用户本轮限制只跑 `ui_core`/`ui_kit`）：`widget`/`widgets`/业务 ui crate 的集成测试未执行；P1 改了 15 个 widgets crate 与 5 个业务 crate 的组件构造签名，虽有 workspace check 与 W7 自证 `cargo test -p widget` 182 passed，但 AGENTS §8 的 `cargo test`（workspace）仍需 lead 在合并前补跑。
2. **真实窗口像素/三端未做**（P3-6）：残影门禁仍是 headless `--pixel-check`；真实窗口 `--debug-stack` 只断言栈/rect。
3. **性能无基线**（P3-5）：open/close 帧预算、settle `layout_ran` 触发面、a11y 全树 DFS 均未测。
4. **并发环境风险**：本 worktree 的 target 目录被多个 agent 共用，gallery example 链接会被并发 `gallery.exe` 占用（LNK1104）；不影响提交，但 lead 收口时应在无并发进程时复跑一次完整 `cargo test -p ui_core -p ui_kit`。
5. **`--dump-overlay` 旧脚本假绿**（Q1-07）在三端脚本迁移前仍存在。
6. **P2/P3 规格遗留**：2–3 级菜单/`MenuChain`（P2）、kit 未接 `match_anchor_width`/`FadeSlide`/a11y 静态 haspopup（已有登记）、`Dialog::recenter`（P3）、`FrameRecord.popup_damage/z_changes`（P3）。
7. **需 lead 裁决的 parity 项（参考并行的 kit 终审）**：`review-p1-final-kit.md` K1-02（Menu/ContextMenu/Dropdown/Select 从“模态 + outside Consume”改为“非模态 + Replay”，inventory §7.1 曾列为需 lead/用户裁决；§12.3 D-P1-1/D-P1-2 已登记为偏差但未记录用户裁决）与 K1-05（D16 默认按钮退化为“第一个 Button”，自定义 footer 的初始焦点需产品确认）。本报告未单独计数，提交前应由 lead 明确批准或调整实现。

## 9. 结论

- **无 P0、无 P1**：旧 overlay/layer 机制删除完整、`rg` 零引用、宿主迁移完整、门禁全部独立复跑通过（3734/0/2 + clippy + fmt + workspace check + pixel-check + debug-stack）。
- **P2 × 3**：Q1-01（P1-4 的 `PopupPanel` 确定高度内部滚动未落地，§12.2 完成口径不实；kit 无 `constrained_size` 消费者，边缘视口下超长内容可能不可达）、Q1-09（§4.7 菜单/右键菜单祖先滚动或 resize 关闭整链矩阵未实现，测试固化相反行为）、Q1-10（Dropdown/Select 首可见帧测试为空断言）。
- **P3 × 7**：Q1-02（`AnchorEdge` 死字段，AGENTS §2.1，已登记待复核）、Q1-03（生产 `expect` 兜底，已登记）、Q1-04（widget 错误映射丢语义）、Q1-05（§12.1 行数错误）、Q1-06（§12.9 矛盾 + §8 `popup_host` 残留 + kit-gallery 未同步）、Q1-07（`--dump-overlay` 静默假绿）、Q1-08（93 checks 覆盖差异未说明）。
- **P1 是否可提交**：
  - 代码质量与门禁层面 **可以提交**（无正确性阻塞项；P1 验收 ①②③④ 均有独立证据）。
  - **提交前应处理**：① Q1-01 二选一（实现或改 §12.2/`p1-summary.md` 文字并登记 P2 遗留）；② Q1-09（实现滚动/resize 关链或 lead 明确裁决设计变更）；③ Q1-10（修 Dropdown/Select 首帧测试）；④ Q1-02 由 lead 裁决删除 `AnchorEdge` 或记录保留理由（否则与 AGENTS §2.1 冲突）；⑤ Q1-05/Q1-06/Q1-08 的文档修正（§12 与 `p1-summary.md` 是本次提交说明来源，错误数字/不实口径会污染审计记录）。
  - Q1-03/Q1-04/Q1-07 与 P3-5/P3-6/P3-7 可在提交后按已登记的 P2/P3 队列跟进。
  - 复跑建议：lead 收口时在无并发 `gallery.exe` 的干净 target 上再跑一次 §12.5 的 6 条命令（本次已全部独立通过，仅有并发争用造成的中间失败记录）。
