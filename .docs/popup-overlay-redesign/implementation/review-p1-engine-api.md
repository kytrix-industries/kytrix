# P1 引擎数据 / API / a11y 面 对抗性审查（review-p1-engine-api）

- 审查对象：worktree `E:\kytrix\.agents\worktrees\dsh-1d175414-1a41`，基线提交 `d9949aca`（P0 收口提交），P1 未提交改动。
- 审查范围：`crates/ui_core` 的 popup 数据模型/API/错误与回滚/调试面/a11y，覆盖 `popup.rs`、`popup/{stack,settle,dismiss,focus,animation,anchor,placement,tests}.rs`、`app/popup.rs`、`app/{frame,focus,input}.rs`、`window/{,a11y,paint,focus}.rs`、`node/tree/*`、`layout/hit_test.rs`、`a11y/**`、`tests/popup_behavior.rs`、`Cargo.toml`。**kit 迁移（`ui_kit`/宿主）由其它 agent 进行中，不属本审查范围；“kit 未迁移”不作为 finding。**
- 设计基准：`.docs/popup-overlay-redesign-design.md` §4.1/§4.2、§4.3、§4.4/§4.5、§4.6、§4.7、§4.9、§4.10、§4.11、§4.12、§4.13、§4.14、§5.1、§7.1、§11。
- 审查快照：**审查基线**为引擎行为层 agent 首轮结束（约 23:25），基线 hash（SHA256 前 16 位）：`app/popup.rs A1153674DEFED528`、`popup/stack.rs C7BCCECB29FD4CA8`、`popup/settle.rs 0785D83DDF854838`、`popup/dismiss.rs D80B5AFCA67B2A44`、`popup/focus.rs 1A95C9400E8F3458`、`popup/placement.rs 5651432F47207383`、`a11y/popup.rs 51E0D1914B636AAD`、`a11y/action.rs 6CB5CA74E956B586`、`a11y/tree.rs CE5A3511CB57AC07`、`layout/hit_test.rs 5303A75DF37176A4`、`node/tree/arena.rs 9F185AF34152D85F`、`node/tree.rs DB8DEED72512F263`、`tests/popup_behavior.rs 0B1D32780EF5D10C`、`Cargo.toml 36B0A66C0B2922F2`。**23:35–23:41 实现方并发修复**改动：`app/popup.rs E79B87F638FEB770`、`popup/settle.rs EF523C1776C0BE08`、`popup/dismiss.rs 65D42FE1E90B4D5A`、`popup/stack.rs 7EF0BBBDA069E025`、`node/tree/arena.rs CE0399277772AE92`、`node/tree/layout.rs 8DAA1E5A0FDAECE2`、`node/tree/tests.rs 7CBB9A85E210A2FC`、`Cargo.toml A3F67C058E7B0DAD`（未变文件仍按基线 hash）；修复状态见 §1.1。行号以基线为准，修复后的行号已另行标注；若实现方继续修改需按最终代码重对齐。
- 方法：只读代码审查 + 只跑 `ui_core` 测试 + 仓库外探针（`E:\kytrix\target\p1-review\exp`，不修改仓库）。三条独立子审查由本审查员派发并汇总：placement（`E:\kytrix\target\p1-review\placement.md`）、node tree/hit/focus（`tree-hit.md`）、a11y（`a11y.md`）；本报告对 P0 及关键 P1 做了独立复核/复现。未修改任何仓库文件（本报告是唯一交付物）。

严重度口径（与 P0 审查一致）：**P0=正确性缺陷/硬要求违反/回归/门禁虚假通过；P1=重大健壮性或测试可靠性缺口；P2=次要问题；P3=风格/文档/测试缺口。**

## 1. 门禁与验证结果（本机，2026-11-26 23:2x–23:4x）

| 命令 | 结果 |
|---|---|
| `cargo test -p ui_core --features test-support` | **PASS**（全量 lib + 集成测试 exit 0） |
| `cargo test -p ui_core --features test-support --test popup_behavior` | 基线 **55 passed / 0 failed**；修复后 23:42 **62 passed / 0 failed** |
| `cargo test -p ui_core --features test-support --lib popup` | **94 passed / 0 failed**（popup::tests + placement） |
| `cargo test -p ui_core --features test-support --lib a11y::popup` | **10 passed / 0 failed** |
| `cargo test -p ui_core --features test-support --lib node::tree`（子审查） | 59 passed / 0 failed |
| `cargo check -p ui_core --all-targets --features test-support` | **PASS** |
| `cargo test -p ui_core --no-run`（默认 feature，门禁口径） | 审查基线 **FAIL**（A-04）；**23:35 并发修复后 23:40 复跑 PASS**（`Cargo.toml` 新增 `[[test]] popup_behavior required-features=["test-support"]`） |
| `cargo run -p ui_kit --example gallery -- --pixel-check` / `--dump-overlay` | **未执行**：gallery/kit 仍绑定旧 `Overlay` API，正在由 P1 kit/P1-12 迁移；本阶段按用户要求只跑 `ui_core`（P0 §11.5 口径）。不得把该未执行当作绿。 |

仓库外探针（`E:\kytrix\target\p1-review\exp`，只依赖 `ui_core` + `test-support`）用于复现 P0-A-01/A-03 与 A-05/A-06/A-07/A-13，结果逐条列在 finding 中；实验设计=“打开 modal/scrim 或 popup → drain 两帧 → advance/输入 → drain → 读取 `popup_stack`/`popup_rect`/`probe().hit`/`FrameRecord`”。

### 1.1 审查期间并发修复（p1-engine-behavior，23:35–23:41，均以仓库外探针/命令行复核）

审查是“早审”：报告落盘期间实现方在并发修 findings。以下为已知落地状态，**未列出的 finding 仍按基线有效**；收口前必须按当前代码重跑全部门禁与探针（行号会随修复漂移）。

| finding | 状态 | 复核证据 |
|---|---|---|
| A-01 侧表 stale transform | **已修复** | `arena.rs:596-601` remove 清 `popup_transforms`/`popup_pointer_transparent`；`node/tree/tests.rs:117-162` 新增 remove/槽位复用测试；探针第三次槽位复用时 `hit=Some(n4.2)`（基线为 `Some(root)`） |
| A-04 默认测试门禁 | **已修复** | `Cargo.toml:52-55` 新增 `[[test]] popup_behavior required-features=["test-support"]`；`cargo test -p ui_core --no-run` 23:40 复跑 PASS |
| A-05 Scrim 全视口 | **已修复** | `settle.rs:175-181,228-236,307-315` 对 `tier==Scrim` 跳过 max-size/placement，直接 `set_absolute_size(viewport)` + `set_absolute_inset(ZERO)`；探针 scrim `hit=(0,0,800,600)`（基线 `(4,4,792,592)`） |
| A-06 modal+scrim 关闭事务 | **已修复（首帧态）** | 探针 `begin_close_popup` 后同一同步点 entries=`[(Scrim,DismissPending),(Modal,DismissPending)]`（基线为空）；动画结束移除需再验 |
| A-13 Scrim density/绘制 | **部分修复** | `app/popup.rs:38-43 scrim_paint(density)` + `create_scrim_node(..., density)`；但 `open_popup` 仍接受调用方直接传 `tier=Scrim`（探针仍 `Ok` 且栈中 `[(Scrim,Open)]`） |
| A-14 EscapePolicy::CloseChain | **已修复** | `dismiss.rs:448-460` 按 policy 调 `begin_close_chain(chain_root)` |
| A-16 Closing 中 Toggle 取消关闭 | **已修复** | `dismiss.rs:296-313` `opener_chain_closing` + `cancel_close`（`:498-519`） |
| A-02/A-03/A-07…A-12、其余 P2/P3 | **未修复（截至 23:41）** | a11y 文件 mtime 22:40–22:50 未变；A-03 探针仍复现 children 0→1 + panic（当前行号 `app/popup.rs:161`）；A-07 hint 仍 z=2>modal=1 |

（本表时间戳 2026-11-26 23:41；若实现方继续修复，以最终代码与复跑结果为准。）

## 2. Findings（P0-P3，A-01…A-34）

**统计（基线→当前）**：审查基线 **P0 3 / P1 9 / P2 17 / P3 5 = 34**；截至 23:41 并发修复已落地 **A-01、A-04、A-05、A-06、A-14、A-16**（6 条），当前待处理 **P0 2 / P1 6 / P2 约 14（A-13 部分修复） / P3 5**。下文中标题带“审查期间已修复”的条目保留原始证据供追溯，不再计入当前阻塞项。

### P0（审查基线 3 条；A-01 已在审查期间修复，当前 2 条仍阻塞 P1 收口）

#### A-01（原 P0，审查期间已修复，复核见 §1.1）popup 侧表在节点移除时不清理 → arena 槽位复用后新节点继承 stale `blocked`/`opacity`/`pointer_transparent`，不可命中/不可聚焦/不可见

- 证据链：
  - 侧表按 `node.index()` 下标存储：`node/tree.rs:393-394`（`popup_transforms`、`popup_pointer_transparent`）；写入 `popup/dismiss.rs:100-107`（`block_entry` 无条件写 `blocked:true`）、`popup/animation.rs:143-151`。
  - 清理只发生在动画正常结束路径：`popup/dismiss.rs:212-228`（`finalize_close` 先 `set_popup_transform(node,None)` 再 `close_popup_now`）。
  - 立即关闭路径不经该清理：`dismiss.rs:170-183`（`animation==None` 时 `block_entry` 后直接 `close_popup_now`）→ `app/popup.rs:466-493` 的 `close_entry` 只清 `popup_pointer_transparent`，**未清 `popup_transform`** → `NodeTree::remove`。
  - `arena.rs:560-651` 的移除清理列表含 `text_slots/editable_slots/debug_ids/clip_keys/baked_screen_bounds/painted_extents/paint_marks/scroll_slot_of/a11y_slots`，**不含 `popup_transforms`/`popup_pointer_transparent`**；`node/id.rs:172-188` 复用 free-list 槽位。
  - 读取处：`layout/hit_test.rs:360-373`（`blocked` → 整棵子树 `None`）、`node/tree.rs:1280-1288`→`interaction/focus.rs:18`（不可聚焦）、`paint/pass.rs:398-407`（stale offset/opacity 继续乘入绘制）。
  - **高可达路径：Scrim 的 `animation` 恒为 `PopupAnimation::None`（`app/popup.rs:307-308`）**；modal 经 Esc/外部/AT 关闭时 `begin_close` 把 scrim 列为首个候选（`dismiss.rs:145-167`）→ `block_entry(scrim)` → `animation==None` → 立即移除，scrim 槽位留下 `blocked:true`。此外，带 Fade 的 modal 内容节点在 `start_open` 写入 `opacity:0` 后若走 scrim 立即关闭路径，槽位留下 `opacity:0`。
- 独立复现（仓库外探针，三次槽位复用）：打开 modal+scrim → drain → Esc（`consumed=true`，entries=0）→ 在 window root 新建 3 个白底 100×100 节点 → `probe().hit` 结果：`n2.2 → Some(n2.2)`、`n3.2 → Some(n3.2)`、**`n4.2 → Some(n0.1=root)`（期望 n4.2）**。即 scrim 槽位上的新节点被 stale `blocked` 标成不可命中；槽位 2 的 stale `opacity:0` 在代码路径上会使新节点不可见（绘制透明度乘 0，命中不受影响）。这与 tree-hit 子审查的外部探针结论一致（`tree-hit.md` P1-01）。
- 影响：正常关闭一次 modal/scrim 后，后续新建节点/子树（尤其重开 dialog、重建菜单/列表行）可能点不到、Tab 跳过、或整体透明；不是每帧必现，但属于“正确性缺陷 + 回归”。
- 修法：在 `arena::remove` 的清理块补 `popup_transforms[index]=None`、`popup_pointer_transparent[index]=false`（或把 transform 并入 `Node`/带 generation 的 side table）；`close_entry` 在 `remove_node` 前也显式 `set_popup_transform(node,None)`；补“关闭后同槽位新节点可命中/可聚焦/可见”的回归测试。
- 是否阻塞：**是（P0）**。

#### A-02（P0）Trap（模态）popup 的 a11y 节点挂在被 `hidden` 的 trigger 下 → AccessKit 整棵模态子树被排除，AT 看不到对话框/模态菜单

- 证据链：
  - 逻辑父解析：`a11y/popup.rs:232-254` 依次取 `a11y.a11y_parent` → 父 popup 内容根 → `entry.opener`；**Trap 模态无特殊处理**。
  - `a11y/popup.rs:139-143` 将 popup 节点 `adopt` 到该父；`a11y/tree.rs:1216-1227` 把 adopted 子节点 append 到父的 children；`a11y/tree.rs:500-508` 用该 parent 分配 `A11yNode.parent`。
  - 模态 scope 下 scope 外节点被标 hidden：`a11y/tree.rs:247-251`（`hidden_outside_modal_scope`）、`:1283-1294`（仅 window root 例外）。trigger 在主树、不在 modal scope（scope=模态内容节点∪`owner_modal` 后代，`a11y/popup.rs:202-214`）→ `trigger.hidden=true`；对话框自身 `hidden=false`，但位于 hidden trigger 的子树内。
  - AccessKit 语义：`hidden` 为继承标志，子树整体排除。子审查核对了 `accesskit_consumer-0.39.2/src/node.rs:440-441`（`is_hidden` 沿 parent 继承）、`filters.rs:20-24`（`ExcludeSubtree`）；Windows/macOS/Unix 适配器共用该 filter。
- 触发面：inventory §2.4 计划 root menu 用 `FocusPolicy::{Trap|Take}` + `opener:Some(trigger)`；Dialog 从按钮打开时 opener 自然存在；嵌套 Dialog（父 popup 内容根同样被外层 scope 隐藏）同样踩中。
- 影响：AT（NVDA/JAWS/VoiceOver/Orca）完全看不到模态对话框/模态菜单；初始焦点与 aria-modal 也一起丢失。这是 a11y 的“错误暴露”级别。
- 现有测试缺口：`a11y/popup_tests.rs:598-666` 只断言 dialog record `hidden==false`，不断言“`hidden==false` 的节点不得有 hidden 祖先”；`ShadowTree` 不建模 hidden 继承。
- 修法：`resolve_logical_parent` 对 `entry.is_modal()`（Trap）且无显式 `a11y_parent` 时返回 `None`（挂 window root，设计 §4.10 允许）；或 `hidden_outside_modal_scope` 对“scope 成员 a11y 祖先闭包”不置 hidden；补“可见节点祖先链无 hidden”契约测试。
- 是否阻塞：**是（P0）**。来源：a11y 子审查 P0，本审查员按代码链复核。

#### A-03（P0）`open_popup` 错误回滚缺口：`ZPolicy::Above` 失败路径遗留孤儿 content node，下一次 open 触发生产 `expect` panic

- 证据链：`app/popup.rs:218-225` 先创建 content node 并执行内容闭包；随后 `app/popup.rs:228-255` 才 `reserve_z_pair(z)?` / `reserve_z(z)?`。若 `ZPolicy::Above(id)` 的 `id` 已关闭/未知（`stack.rs:215-227` → `entry(id)?` 返回 `UnknownPopup/StalePopup`），`?` 直接返回，**content node 未删除、未插入 entry**。`stack.rs:341-349` 的 `sync_order` 之后调用 `tree.reorder_children(popup_root, nodes)`，而 `arena.rs:431-462` 要求 nodes 是 popup root 子节点的**全排列**；孤儿节点使下一次 `finalize_open` 的 `sync_order(...).expect("popup root children always mirror the popup stack")`（`app/popup.rs:146-148`）panic。
- 独立复现：打开 popup A → 关闭 → 以 `z=ZPolicy::Above(A)` 打开 popup B：
  - B 返回 `Err(UnknownPopup)`（预期）；
  - popup root children 从 `0` 变 `1`（遗留 content node）；
  - 再打开 popup C → **panic `popup root children always mirror the popup stack: NodeSpecInvalid ... requires a full permutation` @ app/popup.rs:148**。
- 影响：公开 API 的正常错误输入（stale id 是 `ZPolicy::Above` 的自然生命周期用法）导致库 panic；且设计 §4.13 明确要求“open 失败回滚：不留下 entry/node/focus/a11y”。同类风险也存在于 `close_entry`/`raise_popup` 的 `.expect`（`:503-504`、`:545-547`）与内容闭包直接删除/重挂 content node 的场景（tree-hit P2-05）。
- 修法：`reserve_z` 移到创建 content node 之前（或在所有 `?` 路径统一 `cleanup_nodes` + `popups.remove`）；`sync_order` 改 `?` 并返回 `PopupError`，不得 panic；`open_popup` 结尾校验 entry/content node 仍存在；补“invalid z rollback 后栈/tree 不变、下一次 open 正常”的测试。
- 是否阻塞：**是（P0）**。

### P1（审查基线 9 条；A-04/A-05/A-06 已修复，当前 6 条仍阻塞/需 lead 裁决后收口）

#### A-04（原 P1，门禁，审查期间已修复）默认 `cargo test -p ui_core` 编译失败：新集成测试未声明 `required-features`

- 证据：`crates/ui_core/tests/popup_behavior.rs:17` 无条件 `use ui_core::{..., test_support::app::TestApp}`；`test_support` 由 `lib.rs:16-17` 的 `#[cfg(feature="test-support")]` 门控。`Cargo.toml` 现有 4 个 `[[test]]`（frame_record/snapshot/layout/hud）都写了 `required-features`，**没有 popup_behavior 条目**。实测 `cargo test -p ui_core --no-run` → `error[E0433]: cannot find test_support in ui_core ... could not compile ui_core (test "popup_behavior")`。`cargo check -p ui_core --all-targets --features test-support` 才 PASS。
- 影响：违反 §7.5 的 `cargo test`（workspace）门禁与 P1 验收④；若 CI/lead 用 P0 基线命令 `cargo test -p ui_core -p ui_kit`，会直接红。*（注意：`cargo test -p ui_core --features test-support` 全绿，51→55 条 popup_behavior 均通过，说明只是 target 声明缺失。）*
- 修法：`Cargo.toml` 增加 `[[test]] name="popup_behavior" path="tests/popup_behavior.rs" required-features=["test-support"]`（或文件首行 `#![cfg(feature="test-support")]`，与 `tests/gui_smoke.rs` 一致）。补跑默认 `cargo test -p ui_core`。
- 是否阻塞：**是（门禁）**。

#### A-05（原 P1，审查期间已修复）引擎把 Scrim 当普通 popup 参与 max-size/placement/inset → scrim 不是全视口（4px 缺口），命中/绘制/damage 都缺边

- 证据：`app/popup.rs:287-330` 合成 Scrim entry 时复制模态的 `placement`；`popup/settle.rs:108-182` 对 `depth_ordered()` 中**所有** entry（含 Scrim）执行 `max_size_for` + `set_max_size`；`:204-299` 对所有 `Opening|Open` entry（含 Scrim）跑 `solve_placement` 并 `set_absolute_inset`；`stack.rs:366-381` 的 `sync_rects` 以 layout bounds 为 hit/paint rect。Scrim 节点样式是 `size(viewport)`（`app/popup.rs:25-32`），但 max-size 被夹成 `viewport-2*margin`，再被 solve/clamp 到 `margin`。
- 独立复现（TestApp viewport 800×600，margin=4）：scrim `hit = (4,4,792,592)`，期望 `(0,0,800,600)`；modal 正常 `(200,224,200,120)`。设计 §4.6 要求“全视口、pointer-consuming、damage=全视口”，§4.11 要求模态/scrim damage 全视口；当前四边各缺 4px，scrim 的 damage 也不覆盖边缘。
- 额外后果：Scrim 的 `solve_placement` 结果 `resized` 可能为 true（首轮），经 `settle.rs:262-265` 记为 `Unsettled`（见 A-20），且把 scrim 的 origin 从 (0,0) 改成 (4,4)。
- 修法：`settle` 对 `tier==Scrim` 跳过 max-size/solve/inset（scrim rect 由引擎直接写为 `(0,0,viewport)`），或给 Scrim 独立几何通道；补 scrim rect/damage 全视口测试。
- 是否阻塞：**是（设计硬要求，用户可见）**。与 A-06/A-13 同属 scrim 子系统。

#### A-06（原 P1，审查期间已修复首帧态）modal+scrim 关闭事务绕过 `DismissPending`/`Closing`/淡出动画：scrim 立即连带删除 modal

- 证据：`dismiss.rs:118-167` 的 `begin_close_inner` 先收集 `scrims` 并放在 `ordered`（子级→父级）**之前**；`:170-206` 对 scrim 执行 `block_entry` 后因 `entry.animation == PopupAnimation::None`（scrim 恒 None，`app/popup.rs:307-308`）直接 `close_popup_now(scrim)`；`app/popup.rs:515-523` 的 `close_entry(scrim)` 又因 `scrim_for=Some(modal)` 立即 `close_entry(modal)`。因此 modal 的 Fade 关闭动画、`Closing` 状态和 90ms 窗口被完全跳过（设计 §4.6/§4.12）。
- 独立复现：modal（animation=Fade, scrim）打开并稳定后调 `begin_close_popup(modal, Escape)`：返回 `true`，**同步**打印 stack entries 已为空（无 DismissPending/Closing 帧）；`dismiss_topmost(A11yDismiss)` 同样立即删。现有测试 `a_modal_scrim_closes_with_the_modal`（popup_behavior）只等 95ms 后断言空，不检查关闭中间态，故未抓到。
- 修法：scrim 与 modal 同一关闭事务：scrim 也进入 `DismissPending`/`Closing`（或至少不因自身 animation=None 立即 `close_popup_now`，由 modal 的动画结束统一移除）；`close_entry(scrim)` 不应反向立即关闭 modal；补“关闭开始同帧 modal/scrim 均 DismissPending、动画结束同帧移除”的测试。
- 是否阻塞：**是（设计硬要求，P1-8 动画交付项）**。

#### A-07（P1）tier 规则缺口：modal 打开后仍可打开 Hint，且 Hint 获得更高 z 叠在 modal 之上

- 证据：`app/popup.rs:184-190` 只在**打开 Modal 时** `close_hints`；`open_popup` 对 `tier==Hint` 没有任何“存在 topmost modal 时拒绝/隐藏”检查，`reserve_z` 默认 `Newest`（`stack.rs:215-227`）→ 新 hint z 更大。
- 独立复现：先开 Modal（z=1），再开 Hint（z=2，`popup_rect(hint).is_some()`）→ hint 在 modal 之上。设计 §4.6 明确“Hint（Tooltip）… **modal 打开时一律隐藏/关闭**（用户打开对话框时旧 tooltip 不得悬在模态之上；modal 关闭后由 hover 重新触发）”。
- 影响：违反 z/模态层级硬规则；tooltip 会盖在对话框/遮罩上；a11y/命中 scope 也矛盾（hint 不在 modal scope，却视觉在上）。
- 修法：`open_popup` 对 Hint 在存在 `topmost_modal()` 时返回错误或拒绝；或在 modal 打开期间把 Hint 强制 `hidden`/关闭。补测试“modal 打开后 hint 不出现/被关闭”。
- 是否阻塞：**是（设计硬要求；可豁免需登记偏差）**。

#### A-08（P1）AT 动作的 scope 判定走旧 overlay 物理链，`owner_modal` 后代 popup（Dialog 内 Select/ColorPicker）所有 AT 动作被拒

- 证据：`a11y/action.rs:42-52` 用 `app.modal_scope(window)` + `interaction::focus::in_scope`；`app/focus.rs:27-47` 的 `focus_scope` 在有 popup modal 时返回**模态内容节点**；`interaction/focus.rs:27-29` → `interaction/overlay.rs:32-53` 的 `in_modal_scope` 只沿**物理父链**/旧 `overlay_slot.opener` 链匹配。Dialog 内 Select 的 popup 节点物理父是 `popup_root`，不是 dialog 内容节点，也无旧 overlay slot → 判为 scope 外 → `UnhandledAction`（`:46-52`）。仓库已有正确实现 `popup/stack.rs:567-586 node_in_popup_scope` + `app/focus.rs:37-47 in_input_scope`，但 a11y dispatch 未接。
- 影响：设计 §4.6/§4.9 明确“Dialog 内 Select/ColorPicker 属于该 modal 的 a11y/输入 scope、可用”；当前键盘/鼠标可用但 AT 用户的 Click/Focus/Dismiss 全被静默拒绝。a11y 子审查独立确认（A11Y-02）。
- 修法：dispatch 的 scope 判定改用 `in_input_scope` 等价路径（或 `A11yPopupContext::in_modal_scope`）；补“Dialog 内 Select 的 AT Click/Focus”测试。
- 是否阻塞：**是（设计明确要求）**。

#### A-09（P1）`match_anchor_width` 只有纯函数计算，引擎落地通路缺失 → Select/Dropdown 宽度匹配 trigger 无法实现

- 证据（placement 子审查 R-01）：`popup/placement.rs:34-46` 把 `max(anchor width, content)` 写进 `PlacementResult.rect.size`，但 `popup/settle.rs:252-261,274-299` 只写 `set_absolute_inset(node, rect.origin)`，**从不写宽度**；`stack.rs:366-381` 的 hit/paint rect 来自 layout bounds；`settle.rs:85-94` 只施加 `MaxSize`（上限，不是宽度设定）。设计 §5.1 也没有“写回宽度”的 API。
- 影响：设计 §4.4“match_anchor_width：Select/Dropdown 宽度 = max(anchor 宽度, 内容最小宽度)，不超过视口可用宽度”在引擎侧不可实现；§5.2/§7.3 的宽度验收无通路。PopupPanel 只能自己在内容闭包中用 style 宽度逼近（但无法拿到 offsetWidth 反馈）。
- 修法：引擎在 settle 中把求解宽度作为 popup 内容节点/面板的显式宽度（例如新增 `set_popup_size` 或把 `PlacementResult.rect.size` 通过 `Style`/max-size 下传），并在 round 2 layout 中生效；补“anchor 200px、内容 100px → popup 宽 200”的集成测试。
- 是否阻塞：**是（P1 parity 硬需求；若本阶段不交付 match width 需登记偏差并告知 kit）**。

#### A-10（P1）子菜单 `Right→Left`（禁止 Top/Bottom）无任何强制点；唯一单测是恒真断言

- 证据（placement 子审查 R-02）：`placement.rs:119-129 candidate_placements` 完全由 `spec.side + fallbacks` 决定，纯函数不知道“这是子菜单”；`popup.rs` 没有 `PlacementSpec::submenu()` 构造入口；`app/popup.rs:49-78,160-177` 的 `validate_open` 不校验 `parent.is_some()` 时的候选方向；`tests/popup_behavior.rs:91-97 menu_placement()` 仍是 `PlacementSpec::default()`（fallback=[Top]，来自 `popup.rs:435`）。`placement.rs:763-778` 的子菜单测试把 `spec.side` 直接设为 Right、fallbacks 设 Left，然后断言结果 side ∈ {Right,Left}——**恒真**，不检验任何引擎规则。
- 影响：P2 菜单链的“翻面只 Right→Left”无法由引擎保证；现有 P1 测试会给出假绿。设计 §4.4/§4.8 明确。
- 修法：在 `PopupSpec`/`PlacementSpec` 增加明确的 submenu 语义（如 `PlacementSpec::submenu()` 或 `is_submenu` 标志），或在 `open_popup` 对 `parent.is_some()` 校验 `side/fallbacks ⊆ {Right,Left}`（返回 `InvalidPlacement`）；把恒真测试改成“给 submenu spec 非法 Top fallback 必须被拒/被忽略 + avoid 相交时翻左”。
- 是否阻塞：**是（P2 起必须；P1 若交付 Menu 子菜单结构需先定 API）**。

#### A-11（P1）`a11y_parent` 可指向 `HitFocusSkip::Container`（popup root）→ 节点在 a11y 树中无父，违反 AccessKit `TreeUpdate` 契约

- 证据（a11y 子审查 A11Y-03）：`a11y/popup.rs:256-274 valid_parent` 只检查“parent 不是本节点的物理后代”，不检查 parent 是否生成 a11y 节点；popup root 是 `HitFocusSkip::Container`，构建器直接跳过且不生成记录（`a11y/tree.rs:212-214,1239-1257`）；但 `a11y/tree.rs:500-508` 仍为 parent 分配 id，children 也不会包含该节点。popup root 的 NodeId 可通过公开 `App::popup_stack().root` 取得（`popup.rs:311-313`、`app.rs:1315`），把 `a11y_parent=Some(stack.root)` 当“挂窗口根”是自然误用。同一漏洞适用于 `A11yPatch.a11y_parent`。
- 影响：a11y 树出现无父节点；平台适配器可能丢弃该 popup 或对非法 TreeUpdate 报错；无 `InvalidA11yParent` 诊断（诊断只在显式 patch 路径的 `valid_parent` 失败时产生，见 `tree.rs:1048-1056`）。
- 修法：`valid_parent` 拒绝 skip-container（或自动提升到最近非 Container 祖先，与 `lifted_skip_container` 一致并 push 诊断）；补负例测试。
- 是否阻塞：**是（a11y 契约；可由 lead 决定是否随 P1 修）**。

#### A-12（P1）两套 a11y 父通道可同时生效 → 同一节点被 append 到两个父（多父），破坏“每个节点唯一父”

- 证据（a11y 子审查 A11Y-04）：popup 通道 `a11y/popup.rs:139-143` 把 `adopt` 写进 `adopted`；patch 通道 `a11y/tree.rs:33-52` 建立 `patch_parents`；构建时 `a11y/tree.rs:1216-1227` 对两者**无条件 append** 到各自父的 children，只做同父去重；effective parent 优先级却是 patch > popup > physical（`tree.rs:1058-1071`）。场景：popup 打开时默认挂 parent/opener（或显式 a11y_parent=X），随后 `set_a11y_patch(popup_node, A11yPatch{a11y_parent:Y})` → 节点同时出现在 X、Y 的 children。ShadowTree 本可报“multiple parents”（`test_support/a11y.rs:101-118`），但无测试。
- 影响：非法多父树，AT 侧结构错乱/重复节点。设计 §4.10 明确“每个节点唯一父”。
- 修法：统一 `effective_a11y_parent` 解析，adopted/patch children 生成前校验；或写 patch 时同步覆盖 popup 通道并只保留一个；补混用测试。
- 是否阻塞：**是（a11y 契约；同上）**。

### P2（17 条；A-14/A-16 已修复、A-13 部分修复，当前约 14 条待处理）

1. **A-13（P2，部分修复）Scrim 的公开规格可伪造 + `ScrimSpec.density` 未使用**：*（审查期间已修 density/绘制：`app/popup.rs:38-43` 用 density 生成黑色遮罩 PaintSpec 并传入 `create_scrim_node`；但 tier 校验仍缺，见下）*。`open_popup` 只校验 `spec.scrim.is_some()` 的组合（`app/popup.rs:178-183`），但调用方可直接传 `tier=PopupTier::Scrim` 而不设 `spec.scrim`；独立复现（23:41 代码）：这样 open 返回 `Ok(slot 0)`，栈中留下 `[(Scrim, Open)]`，无 `scrim_for`/无 z 配对，永远不被 hit/dismiss/auto/modal 选中，只能程序化关闭。建议：拒绝用户构造 `Scrim` tier；补测试。
2. **A-14（原 P2，审查期间已修复）`EscapePolicy::CloseChain` 在引擎 `escape()` 未实现**：*（23:37 已修：`dismiss.rs:448-460` 处理 CloseChain 并调 `begin_close_chain(chain_root)`）*。
3. **A-15（P2）`DismissPolicy::Chain.anchor_lost` 的“true=关整链”未实现**：`dismiss.rs:521-538` 取 `anchor_lost` 布尔后 `dispatch + apply_outcome`，默认 `Close` 只关该 entry 的子树；`settle.rs:382-384` 也只对单个 id 调 `anchor_lost`。设计 §4.7 矩阵菜单/右键链锚点消失应关整链。修法：`anchor_lost=true` 时走 `close_popup_chain`/`begin_close_chain`；补链测试。
4. **A-16（原 P2，审查期间已修复）Closing 中点击 opener 不取消关闭回 Opening**：*（23:37 已修：`dismiss.rs:296-313` `opener_chain_closing` + `:498-519 cancel_close`；`on_pointer_down` 命中 Closing 链先 cancel 并消费）*。设计 §4.7 的这一条现已实现，收口前补回归测试。
5. **A-17（P2）`owner_modal` 传递缺口**：`validate_open`（`app/popup.rs:61-76`）对 `opener` 只检查其 `enclosing` entry 自身是否 `is_modal()`，未用 `nearest_modal`/`owner_modal` 上溯。若 opener 位于一个“由 modal 内打开的 Dropdown”内容中，新 popup 的 `owner_modal=None`，会被 modal scope 判为外部（输入/a11y 阻断）。修法：opener 路径改为 `popups.nearest_modal(Some(enclosing))`（与 parent 路径一致）；补嵌套测试。
6. **A-18（P2）`dismiss_topmost`/AT Dismiss 绕过关闭事务**：`app/popup.rs:688-737` 直接 `close_popup`/`close_popup_chain`（立即删节点），与 `apply_outcome`→`begin_close` 的动画关闭不一致；a11y `A11yAction::Dismiss` 走 `dismiss_topmost`（`a11y/action.rs:90`）→ AT 关闭 dialog/menu 无淡出、无 DismissPending。a11y 子审查 A11Y-10/A11Y-11 另指出 `CustomAction(-1)` 被无条件劫持、`HideTooltip` 非顶层时走立即 close 且无 `A11yDismiss`。修法：dismiss_topmost 统一走 `request_close`；AT Dismiss 映射到目标节点所在 popup；补测试。
7. **A-19（P2）limitShift 与“clamp origin 到 margin”冲突**：placement 子审查 R-05：`placement.rs:73-83` 非 fit 分支只 clamp 主轴，`:290-314` 的 limitShift 可撤销视口内位移，最终 rect 越出 viewport margin；in-tree 测试 `placement.rs:613-631` 直接断言 `origin.x == -41`。与 §4.4 rule 5、§4.11 I7「所有 rect 满足 viewport margin」冲突，且无 I7 测试。需设计裁决：limitShift 以“不脱锚”为优先还是 margin 为硬约束；建议返回显式状态或记录偏差。
8. **A-20（P2）`settle` 的 `resized` 信号链断裂 + `resized` 漏置**：placement 子审查 R-03/R-04：`settle.rs:262-265` 把 `result.resized` 记 `Unsettled`（诊断 + 每帧重 settle），却没有把 `resized/实际尺寸` 暴露给 kit；`PopupPanel` 无法按 §4.5 设置“确定高度 viewport + 内部滚动”。若 resize 状态持续为真还会导致永远不进 `Open`（`settle.rs:316-329` 只在非 unsettled 才 set_state(Open)）并每帧 `request_frame`。另外 `match_anchor_width` 夹到可用宽度时不置 `resized`（`placement.rs:39-46`），可返回“宽<内容且 resized=false”。修法：settle 结果向 kit 暴露尺寸约束（如 `PopupRect` 增加 constrained size 或 API），区分“已应用 max-size”与“未收敛”；补超长 popup 缩小/内部滚动测试。
9. **A-21（P2）`reorder_children` 的 damage 不含后代 painted extent 溢出**：tree-hit P2-01：`arena.rs:431-462` 对每个直接子节点只 union `subtree_bounds`（纯布局）+ 子节点**自身** `painted_extent`；后代 shadow/ring/Canvas 溢出（如 kit 面板阴影由子节点绘制）未覆盖。设计 §4.11:569/§4.6:463 要求 z 变化 damage = 受影响 popup 的 paint_rect 并。`raise_popup`/`ZPolicy::Above` 改序时可能残留环带。修法：对受影响子树遍历 union `painted_extent`，与 `sync_rects` 的 paint_rect 口径统一；补像素/伤害断言。
10. **A-22（P2）`paint_offset` 不进入 anchor/IME/焦点环等派生几何**：tree-hit P2-02/P2-03：hit/paint 已跟随 offset（`hit_test.rs:360-373`、`pass.rs:398-407`），但 `window/focus.rs:25-42 text_caret_rect`（IME 候选窗）、`app/focus.rs:88-108`（焦点环 damage）按 settle 位置计算；`node_screen_rect`（`node/tree.rs:593-610`）与 `Anchor::NodeRect`（`anchor.rs:15-22`）不含祖先 `paint_offset`，`NodeRect` 未减节点自身 scroll。Slide 4px/120ms 内子 popup/IME 错位。修法：提供“沿祖先累加 transform offset”的只读查询并统一到 caret/ring/anchor；或明确文档化仅命中跟随并登记偏差。
11. **A-23（P2）`sync_order`/`prune_missing` 的静默失败**：`stack.rs:351-364` 把“节点已被外部删除”的 entry 静默 `remove`；`app/popup.rs:146-148` 仍返回成功 id（`open_popup` 末尾不校验 entry 存在）。tree-hit P2-05 指出内容闭包删除 content node 后 `open_popup` 返回幽灵 id、`start_open/take_focus_on_open` 直接 return。与 A-03 同一族；修法：`prune_missing` 返回被清理 id 并让 open 失败/诊断；`sync_order` 去掉 expect。
12. **A-24（P2，过渡期）legacy overlay 固定层序与 `pin_tail_child` damage**：tree-hit P2-06/P2-07：`app/overlay.rs:46,144` 仍对旧 overlay 调用 `pin_tail_child`，root children 变为 `[..., popup_root, legacy_overlay, debug_hud]`，旧 overlay 会盖住新 popup；且 `arena.rs:401-424 pin_tail_child` 移动已存在子节点不做 damage，旧 `bring_overlay_to_front` 可能残影。P1-11 删除旧 overlay 后前者自然消失；在删除前应拒绝/前插 pin 并补 damage。
13. **A-25（P2）a11y `has_popup/controls/expanded` 为瞬态 + `controls` 与嵌套双份暴露**：a11y 子审查 A11Y-06/A11Y-07：`a11y/popup.rs:162-189` 由“当前打开的 popup”派生，关闭开始后 trigger 的 `has_popup/controls` 立即消失、关闭完成后 opener 无 `aria-haspopup`；`controls` 与 popup 作为 child 同时暴露（`tree.rs:1115-1123`），违反 §4.10“owns/controls 只作为适配器兜底，不重复暴露”。修法：`has_popup` 作为 trigger 的持久 a11y 属性（由 kit 的通用 patch 写），expanded/controls 按 open 状态清除时保留 haspopup；不重复暴露 controls。
14. **A-26（P2）混合 `a11y_parent` 链环未检测**：a11y 子审查 A11Y-09：`a11y/tree.rs:33-69 patch_parent_cycle` 只沿 patch 链；popup 逻辑父 + patch 交叉可成环（每节点唯一父但整体非树），AT 无法从 root 到达，无诊断。修法：统一 effective parent 图做可达性/环检测并 push 诊断。
15. **A-27（P2）两个公共 `MaxSize` 类型 + popup max-size 缓存可被绕过**：placement 子审查 R-06/R-07、tree-hit P2-04：`popup.rs:382-400 MaxSize`（策略）与 `node/tree.rs:49-85 MaxSize`（数值）同名不同义；`PopupStack::max_sizes`（`stack.rs:56-73`）只是 settle 的缓存，kit 通过公共 `NodeTree::set_max_size`（`tree.rs:582`）改节点后，下一次 settle 会按 `entry.placement.max_size` 覆盖且缓存不失效。修法：重命名策略枚举（如 `SizeConstraint`）、在 `NodeTree::set_max_size` 处通知 popup stack 或禁止 kit 直接改 popup 节点 max size；补“外部 set_max_size 与 settle 交互”测试。
16. **A-28（P2）placement 极端有限输入产生 NaN rect**：placement 子审查 R-08：`placement.rs:207-240` 中 `place` 可因 gap≈f32::MAX 产生 inf，`clamp_shift` 对 inf 做 `inf + (-inf)` 得 NaN，`overflow_axis`（`:316-320`）的 `NaN.max(0)` 吞成 0 → `fitted=true` 并返回 NaN rect，而不是 `InvalidGeometry`。修法：在 `select/evaluate` 后校验 rect 有限，否则返回 `InvalidGeometry`；补负例。
17. **A-29（P2，inventory 已登记 P2 推迟，需 lead 裁决）子菜单 a11y 默认父是“父 popup 内容根”而非父 menuitem**：a11y 子审查 A11Y-05：`a11y/popup.rs:238-254` 顺序为 a11y_parent → 父 popup 内容根 → opener；测试 `a11y/popup_tests.rs:255-269` 固化了内容根行为；设计 §4.10 要求“子菜单挂到父 menuitem”。inventory §2.4 把子菜单 a11y 父链列为 P2 推迟，因此本阶段可豁免，但需 lead 明确并在 P2 修正（否则 Menu 角色嵌套违反 ARIA required owned elements）。

### P3（5 条，风格/文档/测试/性能）

1. **A-30（P3）生产数据结构/诊断的死字段**：`PopupEntry.settled_size`、`PopupEntry.solved_origin` 全仓只写不读；`current_origin` 仅测试读取；`PopupDiagnostic::AnchorLost` 从未被 push（`dismiss.rs` 只用 `DismissReason::AnchorLost`）；`PopupId::slot/generation`、`PopupStack::next_z` 等 getter 无生产消费者。违反 AGENTS §2.1“生产代码中的数据结构不得定义未使用字段/不再使用的代码直接删除”。修法：删除或接入（settled_size 本应用于 A-20 的约束回传，可借此转正）。
2. **A-31（P3）调试/可观测缺口（§4.14）**：`FrameStats` 只加了 `popup_settle_rounds/popup_unsettled`（`test_support/record.rs:131-132`、`app/frame.rs:277-278`），设计 §4.14 还要求 `FrameRecord.popup_damage`/`z_changes`；gallery/`--dump-overlay` 的 `debug_stack` 名称在引擎不存在（实际 API 为 `App::popup_stack`），P1-12 迁移时必须对齐命名/契约；`popup_diagnostics` 仅按帧返回 Unsettled，不进入 FrameRecord，故障不可回放。修法：补字段或在 §11 登记偏差；P1-12 以 `popup_stack` 为唯一 dump 面并补 z/damage 记录。
3. **A-32（P3）a11y 性能热点**：a11y 子审查 A11Y-12：每次 a11y 投递 `collect_patch_parents` 全树 DFS（`a11y/tree.rs:33-69`）、`logical_depth_of` 排序 O(n·depth)、`a11y/adapter.rs:236,253-259` 每帧/每 pending 调用 `project`（`window/a11y.rs:20-24`）并 clone 整个 `A11yPopupContext`。popup 规模小（≤1024）时风险有限，但 P3 性能验收前需测；可用“仅 popup 相关节点增量索引”或缓存。
4. **A-33（P3）测试真实性缺口（重要，见下节详列）**：`slide_animation_offsets_paint_and_hit_without_moving_the_settle_rect`（popup_behavior:849）名字含 paint，但只断言 hit/settle rect，未断言 `FrameRecord.primitives` 的实际绘制位置（本次用探针补验了绘制=+offset，但测试本身无此断言）；`a_modal_scrim_closes_with_the_modal`（:940）不检查关闭中间态/rect；placement 子菜单测试恒真（`placement.rs:763-778`）；`a11y_oracle_diff` 返回值被丢弃（`a11y/popup_tests.rs:826,830`）；分隔线“不可聚焦”断言空转（:692-698）；`popup_settle_rounds_never_exceed_two` 只查计数不查内容；`the_popup_root_is_the_last_root_child` 未含 HUD（HUD 由 `tests/hud.rs:381` 补，但独立测试名与断言范围不一致）。
5. **A-34（P3）规范核查（通过项 + 残余风险）**：新增 popup/a11y 生产文件**无注释**（符合 AGENTS §1.6）、**无 `#[allow]`/`#[expect]`/`unwrap`**、无新增 workspace 依赖（thiserror/smallvec/smol_str 均为既有）；残余风险是生产 `expect`：`app/popup.rs:148,504,547`（sync_order，已由 A-03/A-23 证明可 panic）、`window.rs:323`（install 不变量）、`stack.rs:250-290,371`（索引/长度不变量，上限 1024 下安全）。**当前 `cargo check -p ui_core --all-targets --features test-support` 有 1 个 `unused variable: window` warning（`tests/popup_behavior.rs:2514`，为修复轮新增测试），会红 `clippy -D warnings`，需修**；`cargo +nightly clippy -D warnings` 未在本轮执行（用户本轮只要求 ui_core/ui_kit 测试；但 clippy 是 §7.5 门禁，需在收口前补跑）。

## 3. 数据模型 / API 逐项核对表

| 设计要求（§） | 实现 | 结论 |
|---|---|---|
| `PopupId{window,slot,generation}` 防 ABA/跨窗口（§4.2） | `popup.rs:29-59`；`stack.rs:502-517 resolve` 校验 window+generation+tombstone | ✔ 基本正确；但“刚关闭、槽未复用”的 id 返回 `UnknownPopup` 而非 `StalePopup`，语义可与设计对齐（P3 文档） |
| entries 墓碑/槽复用/generation；墓碑清理（§4.2/§7.1-11） | `entries:Vec<Option>`、`generations`、`free_slots`、`handlers/focus/max_sizes` 清理；`insert` 复用槽 +1 generation | ✔；基线曾缺 popup transform/pointer transparent 侧表清理（A-01），**23:35 已修复并补测试**（`arena.rs:596-601`、`node/tree/tests.rs:117-162`） |
| order 与 z 单调一致；raise 子树（§4.2/§4.6） | `sort_order` 按 z；`reserve_above`/`raise` 更新 z 并排序；测试 `popup/tests.rs:114-235` | ✔；`order==z`、scrim pair 连续、Above 位移均有单测 |
| parent/child 每父至多一个打开子级（§4.2/I7） | `set_child`；`close_popup` 先子后父；测试 `:1041-1064` | ✔；父处于 Closing/DismissPending 时仍允许 open（未校验），P3 |
| `scrim_for` 与 modal 生命周期严格绑定（§4.6） | 合成时 `scrim_for=Some(modal)`；`close_entry` 双向回收；测试 `:1181-1290` | ✔ 正常路径；基线 A-05（几何）/A-06（关闭事务）**已修复**（§1.1）；公开 API 可伪造 Scrim（A-13 P2 仍开） |
| `owner_modal` 最近 modal 祖先；modal 关闭递归 `owner_modal` 后代（§4.6） | parent 路径 `nearest_modal`；opener 路径只查 immediate enclosing（`app/popup.rs:70-76`）；`owner_modal_descendants` 递归关闭 | ⚠ parent 路径 ✔、opener 嵌套缺口（A-17 P2）；`remove` 不清 `owner_modal==id` 的悬挂引用（P3） |
| tier 规则 Auto/Hint/Modal/Scrim（§4.6） | 开 Modal：关 scope 外 Auto + 关 Hint；开 Auto root：关其它 Auto；Hint 不关任何 | ⚠ Auto/Modal 主路径 ✔；**Hint 在 modal 打开后仍可打开且叠在上（A-07 P1）**；Scrim 仅引擎合成未强制（A-13 P2） |
| 状态机 Opening/Open/DismissPending/Closing/Closed 与 API 返回语义（§4.2、§4.7、§5.1） | open→Open；动画→Opening；settle→Open；begin_close→DismissPending→Closing→移除；`close_popup` 立即子树关闭；`popup_state->Option` | ⚠ 主路径 ✔；A-06（scrim 跳过关闭态）/A-16（Closing Toggle）**已修复**（§1.1）；Closed 仍仅瞬时存在（`close_entry` set 后 remove，不可观测；A-18 dismiss_topmost 仍立即关闭） |
| popup root 0×0 绝对定位、Container skip 三处一致（§4.6） | `stack.rs:20-53 install`；`hit_test.rs:440-442`、`interaction/focus.rs:14-24`、`a11y/tree.rs:211-214,1239-1257`；测试 `popup/tests.rs:93-111,622-745` | ✔（子审查已用探针验证 0×0 容器内 Text 仍量得非零尺寸；主树布局不变） |
| 固定层序 `[...内容, popup_root, debug_hud]`、HUD 最上穿透（§4.6） | `pin_tail_child` + `child_insert_position`；`tests/hud.rs:381-430` | ✔ 无 legacy overlay 时；旧 overlay 仍 pin 在两者之间（A-24 P2，P1-11 删除后消失） |
| `reorder_children` 不触发 structure epoch、damage 正确（§4.6/§4.11） | `arena.rs:426-475`：只改 children/order_dirty/mark_paint_without_damage，不 mark_structure；damage 见 A-21 | ✔ 不触发 structure epoch（测试 `popup/tests.rs:1137`、`popup_behavior` 无 damage_full）；⚠ damage 不含后代 painted 外延（A-21 P2） |
| placement 候选/avoid/bestFit/flip/shift/resize/clamp/子菜单/宽度（§4.4/§4.5） | `placement.rs` 纯函数 + 单测 35 条；`settle` 调用 | ⚠ 大部分规则正确（子审查已逐条核实）；**match width 未落地（A-09 P1）、子菜单方向未强制（A-10 P1）、limitShift 越界（A-19）、NaN（A-28）、resized 信号（A-20）** |
| `PopupError` 全变体路径 + open 失败回滚（§4.13） | 变体齐全；`open_popup_rejects_invalid_inputs`/`open_popup_rolls_back...`/depth/limit 测试 | ⚠ 变体与 content 回滚 ✔；**ZPolicy 错误路径泄漏/panic（A-03 P0）**、已关闭旧 popup 不可回滚（P3） |
| a11y 链嵌套/唯一父/expanded/has_popup/owns/controls/described_by/active_descendant/aria-modal/scrim hidden/AT dismiss（§4.10） | `a11y/popup.rs project` + `a11y/tree.rs` 接线 + `a11y/action.rs` 映射；popup_tests 10 条 | ⚠ 字段接线基本齐全、scrim hidden/expanded/described_by/AT custom action 有测试；**modal hidden 祖先 P0（A-02）、AT scope A-08、container/parent 唯一性 A-11/A-12、瞬态 A-25、环 A-26、子菜单父 A-29** |
| 调试面：`popup_stack/popup_rect/popup_pointer/FrameStats(settle_rounds,unsettled)`（§4.14） | `app/popup.rs:603-658`、`test_support/record.rs:131-132`、`app/frame.rs:277-278`；`popup_diagnostics` 另加 | ✔ 可用且语义正确（snapshot 含 id/tier/parent/z/state/rect/owner_modal/focus/paint_offset）；⚠ 缺 `popup_damage/z_changes`、`debug_stack` 命名（A-31） |
| 旧 API 删除/新 API 唯一机制（§5.1/§8，P1） | 新 API 全在；旧 `create_overlay/overlays/kit::overlay` 仍存 | ⚠ P1-11 未落地（本审查范围外，按任务不把 kit 未迁移当 finding）；删除前 A-24/A-04 相关门禁需收口 |

## 4. 已核实正确点（正面证据）

1. **popup root 的 0×0 绝对定位 + `HitFocusSkip::Container`** 在命中/焦点/a11y 三处一致；主树布局不变；root 自身不命中、不聚焦、不生成 a11y 节点（`popup/tests.rs:622-745`；子审查外部探针验证 0×0 容器内子节点自动尺寸正常）。
2. **z/order/父子链/墓碑/ABA**：`opening_popups_keeps_a_strictly_increasing_z_order`、`opening_above...`、`reused_slots_bump_the_generation_and_invalidate_old_ids`、`chain_reports_depth_and_cycle_errors`、`a_scrim_pair_reserves_two_consecutive_z_values`、`the_stack_limit_is_enforced` 全绿；`raise` 子级随父升序（`:227-235`、`:1144-1177`）。
3. **settle 收敛**：首帧 rounds=2、稳定帧 0（`opening_a_popup_settles_geometry_in_the_first_frame`）；内容增长同帧、无第二帧位移；滚动锚点同帧跟随；resize 重夹；idle 帧 0 popup 工作；总轮数 ≤2 有测试；报告合并进 FrameStats（`app/frame.rs:213-214,277-278`）；settle 插入点正确（canvas text eviction 之后、ensure_visible 之前，`app/frame.rs:197-217`）。
4. **Slide 的 paint/hit/damage 一致（本审查员独立探针）**：settle rect y=124、paint_offset=(0,1.6875) 时，`FrameRecord.primitives` 中 Quad/ClipPush y=125.6875 = 124+offset，damage y0=124..209 覆盖 → 设计 §4.12 的“绘制/hit 同偏移、damage 覆盖偏移区”成立（尽管 A-22 指出 IME/环/anchor 不跟随）。
5. **关闭不可命中/不可聚焦**：`closing_animation_blocks_hit_immediately_then_removes_the_node` + `popup_input_blocked` 接线；`escape_skips_closing_entries_and_closes_the_next_open_level` 验证 Esc 跳过 Closing。
6. **焦点事务**：Take 取焦/关一次恢复、opener 消失 fallback、`RestoreFocus::{Opener,Previous,RootFallback}` 分支均有测试（popup_behavior `restore_*` 5 条）；Dialog Tab 闭环；开 popup 从 editable 抢焦取消 composition；IME composition 中 Esc 先取消不关 popup；`closing_a_popup_does_not_steal_the_newer_popup_focus`。
7. **dismiss 主路径**：modal scope 外按下消费、Explicit outside 关链消费、默认 Replay 重派 press+release、同 opener Toggle 消费 press+release、点祖先关闭后代、Esc 只关一层、tooltip Esc 只关自身、窗口失焦关 Auto 保留 Modal、`begin_close` Ignore/幂等/未知 id 强类型错误（popup_behavior 相关用例全绿）。
8. **placement 纯函数核心规则**（子审查逐条核实）：候选=首选+fallbacks 去重保序；avoid 下 Right/Left 取 avoid 外缘、纵向按 anchor；avoid 严格相交判定；首个 fit 胜出 + bestFit 严格 `<` + 平局优先不覆盖 avoid；翻面后仍 shift；Limit 保 1px 不脱锚、None 无限制；max-size=viewport−2·margin 与 Viewport/Width/Height 的 shrink 语义正确；`validate` 无静默归零；纯函数/无副作用/无 panic 路径。35 条单测通过。
9. **错误类型**：`PopupError` 11 个变体均有构造/测试路径（`UnknownWindow/UnknownPopup/StalePopup/InvalidGeometry/InvalidPlacement/InvalidScrim/Cycle/DepthExceeded/AnchorNodeMissing/ContentBuild/LimitReached`），`open_popup_rejects_invalid_inputs`、`open_popup_enforces_the_depth_limit`、`open_popup_rolls_back_when_the_content_closure_fails` 等锁定了大部分边界。
10. **a11y 核心投影**：scrim hidden、background hidden、expanded 开启/关闭开始清除、described_by、`HasPopup` 从 role 派生、`A11yAction::Dismiss` 的 `CustomAction(-1)`→`A11yDismiss` 路径、HideTooltip 只关 tooltip（非顶层路径除外，A-18）均有测试（`a11y::popup` 10 passed）。
11. **无新增注释/allow/unwrap/新依赖**：popup/a11y 新增生产文件扫描无 `//`/`/*`、无 `#[allow]`/`#[expect]`、无 `unwrap()`；新增依赖仅为既有 thiserror/smallvec/smol_str（AGENTS §1.6/§3/§5 通过项）。
12. **审查期间并发修复复核（§1.1）**：A-01（侧表清理 + 槽位复用测试）、A-04（Cargo test target）、A-05（scrim 全视口 rect）、A-06（modal/scrim 同帧 DismissPending）、A-14（EscapePolicy::CloseChain）、A-16（cancel_close/Toggle）已在 23:35–23:41 落地；本审查员用仓库外探针逐项复核了可观测结果（scrim rect、关闭中间态、第三次槽位复用 hit、hint、z-rollback）。

## 5. 测试真实性评估（关键句）

- **强（可复现、断言到外部可观测面）**：popup/tests 的数据模型/栈不变量与 damage 用例；popup_behavior 的首帧/稳定帧 rect、settle rounds、滚动锚点、dismiss 矩阵、焦点事务、`damage_full==false`、HUD 顺序；node/tree 的 skip/reorder 测试；a11y popup 的 scrim hidden/expanded/described_by/AT dismiss。
- **弱/自证（需补）**：placement 子菜单方向测试是恒真断言（A-10）；`slide_animation_offsets_paint_and_hit...` 只验 hit 不验 paint（本次探针补验了 paint 正确，但测试未锁）；`a_modal_scrim_closes_with_the_modal` 只验最终清空，不验 DismissPending/Closing/90ms 窗口（A-06 因此漏网）；`slide_animation_damage_covers...` 的 damage 断言只覆盖 code 自己 push 的 +offset 区域（与 A-22 的派生几何缺口无关）；`a11y_oracle_diff` 返回值被丢弃（`a11y/popup_tests.rs:826,830`）；分隔线不可聚焦断言空转（`:692-698`）；`ShadowTree` 不建模 hidden 继承（A-02 漏网）。
- **缺测清单（按严重度）**：
  - P0：~~关闭 popup 后同槽位新节点可命中/可聚焦/可见（A-01）~~（**23:35 已补回归测试**）；`hidden==false` 节点祖先链无 hidden（A-02，**仍缺**）；invalid `ZPolicy` open 失败后栈/树无残留且下一次 open 不 panic（A-03，**仍缺**）；~~默认 `cargo test -p ui_core` 门禁（A-04）~~（**23:40 已 PASS**）。
  - P1：~~scrim rect/damage 全视口（A-05）~~、~~modal+scrim 关闭中间态/动画时序（A-06）~~（**已修复，需补测试锁**）；modal 打开后 Hint 不可见（A-07，仍缺）；Dialog 内 Select 的 AT Click/Focus（A-08，仍缺）；match_anchor_width 集成（A-09，仍缺）；子菜单非法 fallback 被拒（A-10，仍缺）；a11y parent 指向 Container / 双通道多父（A-11/A-12，仍缺）。
  - P2：`EscapePolicy::CloseChain`、Closing Toggle（**已修**）、`anchor_lost` 关链（仍缺）、`owner_modal` 嵌套、resized/超长 popup 内部滚动、reorder damage 后代外延、paint_offset 下的 IME/anchor、`set_max_size` 外部写入与 settle 交互、placement NaN/极值。
- **测试规模与门禁如实口径**：`--features test-support` 下 ui_core 全绿（popup_behavior 基线 55/修复后 62、popup lib 94、a11y::popup 10）；默认 feature 的 `cargo test -p ui_core --no-run` 在 A-04 修复后 23:40 PASS（带 1 个 unused-variable warning，见 A-34）；`cargo clippy`/gallery 像素门禁本轮未跑（kit 迁移中）。

## 6. 残余风险与收口结论

- **能否进入 P1 收口：当前不能。** 审查基线 P0×3 + P1×9；审查期间已修复 A-01/A-04/A-05/A-06（并有 A-14/A-16 两条 P2）。**仍阻塞项：P0 A-02（模态 a11y 被 hidden 祖先排除）、A-03（ZPolicy 回滚 panic）；P1 A-07（Hint 叠 modal）、A-08（AT scope 拒绝 Dialog 内 popup）、A-09（match width 无落地）、A-10（子菜单方向无强制）、A-11/A-12（a11y 无父/多父）**。A-09/A-10 若本阶段不交付对应组件/子菜单，可由 lead 登记偏差降级；A-02/A-03/A-08/A-11/A-12 不应降级（正确性/AT 可用性）。
- **建议修复顺序**：① A-03（低风险，纯引擎回滚）+ A-02（a11y 逻辑父/hidden 闭包）；② A-08 + A-11/A-12（a11y scope/唯一父/无父）；③ A-07（tier 规则，一条 open_popup 校验）；④ A-09/A-10（P1 若交付 Select/Menu 子菜单则必须；否则登记 P2 并调整 P1 验收措辞）。修复后必须重跑默认 `cargo test -p ui_core`、`cargo test -p ui_core -p ui_kit`、clippy，并复跑 A-02/A-03/A-07/A-08 探针（本报告 §1.1 时间戳后可能继续变化）。
- **需 lead/设计裁决**：A-19 limitShift vs I7 margin（设计冲突）；A-20 resized 信号如何暴露给 kit（与 §4.5 internal scroll 绑定）；A-24 旧 overlay 层序（P1-11 删除前是否接受过渡期）；A-29 子菜单 a11y 父链（inventory 已登记 P2 推迟）；A-13 scrim 视觉/density 归属（引擎绘制 vs kit）。
- **残余风险**：审查基线固定在 23:25、部分修复在 23:35–23:41（§1.1），kit/宿主仍在迁移；P1 收口前需按上述 P0/P1 修复后重跑默认 `cargo test -p ui_core` + `cargo test -p ui_core -p ui_kit` + clippy，并复核 A-01/A-02/A-03/A-05/A-06 的回归测试。gallery `--pixel-check` 仍是旧 Overlay 通路（P1-12 迁移），本阶段不能作为新 popup 的像素门禁。

## 7. 附：子审查报告与实验位置

- `E:\kytrix\target\p1-review\placement.md`（35 tests，19 findings：P1×2/P2×6/P3×11）
- `E:\kytrix\target\p1-review\tree-hit.md`（23:25 快照 + diff；16 findings：P1×1/P2×8/P3×7）
- `E:\kytrix\target\p1-review\a11y.md`（14 findings：P0×1/P1×4/P2×6/P3×3）
- `E:\kytrix\target\p1-review\exp`（本审查员探针 crate，只读依赖仓库，不修改仓库）；`engine-hashes-2320.txt`（被审快照 hash）。
