# P2 对抗性审查：菜单链 / 交互 / a11y（p2-review-menu）

- **审查对象**：worktree `E:\kytrix\.agents\worktrees\dsh-1d175414-1a41`，基线 `152616b1`（P1 提交），P2 未提交工作树（14 tracked 文件 + 新文件 `crates/ui_kit/src/kit/menu_chain.rs`）。
- **审查 lens**：菜单交互 / 键盘 / safe triangle / 链式 dismiss / a11y（`active_descendant` 投影）。P0/P1 渲染与引擎不变量不在本轮 lens 内（仅在发现关联风险时引用）。
- **用户裁决基线**：D5/D6/D13/D14（Auto 非模态 + outside Replay、Esc 逐层、hover 不夺焦/键盘夺焦、tooltip 优先）。
- **方法**：只读静态审查 + 全量测试 + gallery 两门禁 + 外部探针（`E:\kytrix\target\p2-probe`，不修改仓库）验证状态机/定时器/焦点。仓库代码零改动；本文件为唯一新增。
- **审查日期**：2026-11-27。

## 0. 结论摘要

| 严重度 | 数量 | 编号 |
|---|---|---|
| P0（崩溃/数据丢失/不可用） | 0 | — |
| P1（主路径功能失效） | 0 | — |
| P2（用户可见的行为缺陷/设计契约违反） | **3** | M2-01、M2-02、M2-03 |
| P3（边界/文档/测试质量/偏差登记） | **7** | M2-04…M2-10 |

**提交结论：P2 暂不可提交。** 3 条 P2 均位于菜单链状态机与定时器生命周期上，修复面小（`menu.rs` 的 `sync_own`/`update_hover`/`fire_pending_open` + 新增 1 个 chain 恢复 API），但其中 M2-01 恰好是设计 §4.7 明示的 Toggle/Closing 场景，且现有测试 `a_trigger_press_during_the_close_transaction_reopens_the_menu` **只断言 `is_open`，掩盖了链失效**；修复必须同时补断言，否则 P2 的“链式状态机已落地”结论不成立。P3 可在修复轮一并处理（文档/Self-report 纠偏 + 测试缺口）。

**最关键 3 条**：

1. **M2-01（P2）**：关闭动画期间由引擎 Toggle/取消关闭重开同一 popup 后，`MenuChain` root 层仍停在 `Closed`，hover 级联与 safe triangle 永久失效（探针：hover 子菜单行 260ms 后仍只有 1 层；同一操作在新开菜单为 2 层）。
2. **M2-02（P2）**：`last_hovered` 只在 `on_level_closed` 复位；子级因 300ms grace 自动关闭、指针重新回到同一 submenu 行时，`update_hover` 因 `hovered == last_hovered` 提前返回，`PendingOpen` 不再启动（探针：重入同行为 1 层；换一行再回来为 2 层）。
3. **M2-03（P2）**：`PendingOpen` 定时器在 root 进入 `DismissPending/Closing` 后未被取消，仍会在 closing parent 下真的 open 一个子菜单 popup（探针：Esc 后 60ms stack 1→2、子菜单短暂挂载，约 40–80ms 后消失），属悬空定时器 + 关闭事务内新开 popup。

## 1. 范围、环境与命令结果

### 1.1 审查范围

| 区域 | 文件 | 关注点 |
|---|---|---|
| 新增链状态机 | `crates/ui_kit/src/kit/menu_chain.rs`（942 行，14 tests） | per-level 状态/定时器绑定/slot 复用/children-first/grace/桥接多边形/typeahead 缓冲 |
| 菜单集成 | `crates/ui_kit/src/kit/menu.rs`（5116 行，56 tests；+3591/−1512） | `sync_own`/`sync_chain`/`update_hover`/`pointer_left`/`fire_close_grace`/键盘表/checkable/a11y |
| ContextMenu | `crates/ui_kit/src/kit/context_menu.rs`（1028 行，17 tests） | WindowRect/节点锚/Keyboard 入口/共享链/Replay |
| 宿主/演示 | `crates/ui_kit/examples/gallery.rs` | 3 级链 overlay-check（chain-open/stack-size/z-order/tier/parent/close/entries-zero） |
| 引擎 a11y 小补 | `ui_core/src/popup.rs`、`app/popup.rs`、`a11y/popup.rs`、`a11y/tree.rs`、`a11y.rs` | `PopupA11y.active_descendant` 投影/校验/诊断 |
| 引擎测试 | `a11y/popup_tests.rs`(+1)、`tests/popup_behavior.rs`(+1) | active_descendant 投影/更新/清空/非法目标 |
| 面板小改 | `popup_panel.rs`（`on_dismiss`）、`dropdown.rs`/`select.rs`（`surface_action: None`）、`mod.rs` | 回调/接口贯通 |

### 1.2 环境

Windows 11 / 125% DPI；`CARGO_TARGET_DIR=E:\kytrix\target`。真实窗口门禁用 NVIDIA RTX 5080 D3D12；headless 像素门禁用 D3D12 WARP。

### 1.3 命令与结果（均为本次实测）

| 命令 | 结果 |
|---|---|
| `cargo test -p ui_core -p ui_kit` | **PASS**：ui_core lib 2805 passed / 1 ignored；`tests/popup_behavior.rs` 111 passed；`tests/frame_record` 8；`tests/layout` 4；`tests/snapshot` 14；ui_kit lib **844 passed**；demo example 39；0 failed |
| `cargo check --workspace --all-targets` | **PASS**（exit 0） |
| `cargo run -p ui_kit --example gallery -- --pixel-check` | **PASS** exit 0：94 checks，sampled_frames=32，open=8，close=6+6，`damage_full_frames=0`；首可见 rect == 稳定 rect；含 2 点注入故障自检 |
| `cargo run -p ui_kit --example gallery -- --debug-stack` | **PASS** exit 0：3 级链 entries=3、z=[2,3,4]、parent 链正确、`chain-close` 接受、关闭后 entries=0；最终 `overlay-check PASS`（menu_frames=345/picker_frames=329/retries=0） |
| `git status --short` | 仅 P2 工作树改动（14 M + 1 ?? `menu_chain.rs`）+ 本 review 文件；无审查者代码改动 |

### 1.4 外部探针（仓库外，`E:\kytrix\target\p2-probe`）

为验证状态机/定时器/焦点行为，用公开 API（`TestApp` + `Menu`/`ContextMenu`）在仓库外建最小探针 crate（path 依赖工作树，不写仓库）。探针覆盖：Toggle 重开、gutter 点击、内部滚轮、Shift+Tab、hover→键盘、`bind_trigger`、重入同一行、closing 期 PendingOpen、safe triangle 过期。结果作为 Finding 证据（编号 [A]…[I] 与正文一致）。

## 2. Findings

> 严重度口径：P2 = 用户可见交互缺陷或设计契约/API 不成立（不阻断其它模块，但不应带病提交）；P3 = 边界/文档/Self-report/测试有效性/登记缺失。“阻塞”列表示是否建议阻塞 P2 提交。

### M2-01（P2，阻塞）：引擎取消关闭/Toggle 重开后 `MenuChain` root 层不恢复，hover 级联与 safe triangle 永久失效

- **位置**：`crates/ui_kit/src/kit/menu.rs:1556-1577`（`sync_own`）、`:1355-1432`（`open_focused`，尤其 `:1386-1393`）、`crates/ui_kit/src/kit/menu_chain.rs:282-303`（`confirm_open` 只接受 `Opening|Open`）。
- **证据**：
  - 静态链路：root `DismissPending/Closing` → `sync_own` 调 `chain.close(level)`（root level 变 `Closed`）→ 引擎 Toggle 取消关闭、同一 PopupId 回到 `Opening/Open` → `sync_own` 的 `Opening|Open` 分支不做任何恢复 → `open_focused` 仅在 `self.level()` 为 `None` 时 `open_root`，此处 `Some(0)` 被复用 → `confirm_open` 因 `Closed` 直接返回 `false`，root 级永远停在 `Closed`。
  - 探针 [A]：Esc → 点 opener 重开（`open=true`）→ hover submenu 行 +260ms：`stack=1`；全新打开的对照：`stack=2`。重开后键盘 `Down`+`Right` 仍能开子菜单（`stack=2`），说明是“链几何/指针跟踪死”，不是菜单整体不可用。
  - 现有测试 `menu.rs:3869-3880` 只断言 `menu.is_open`，未断言 hover/grace/键盘链状态，故 CI 全绿。
- **影响**：设计 §4.7 明示的“Closing 中按 opener → Toggle 同 id 取消关闭并回 Opening”场景（也是用户快速双击/连点的常见路径）后，菜单虽可见但：hover 打开子菜单失效、safe triangle/grace 失效、`pointer_entered` 语义失效；子菜单只能用键盘或鼠标点击行打开。ContextMenu 走同一 `Menu` 代码，同样受影响。
- **修法**：二选一或组合：
  1. `sync_own` 在 `Opening|Open` 分支检测 `chain.state(level) == Closed` 时自愈：root → `open_root` + `register_level`；submenu → 以 `begin_open`/新增 `MenuChain::revive(level, rect, anchor, exit, source)` 恢复 `Open` 并重算 grace；
  2. `DismissPending/Closing` 时改为 `chain.mark_closing(level)`（保留 level/关系），`Opening` 时 `chain.reopen(level)`；只有 popup 真正 `Closed`/移除时才 `chain.close`。
  同时重置 `last_hovered`/`trigger_over`/typeahead（见 M2-02），并补测试：重开后 hover 级联、Esc 逐层、outside press 各一条；现有 toggle 测试必须加链断言。
- **阻塞**：是（P2 链式状态机落地结论依赖此行为）。

### M2-02（P2，阻塞）：`last_hovered` 未在子级因 grace 关闭/指针离开链时复位，重新进入同一行不再启动 `PendingOpen`

- **位置**：`menu.rs:1754-1802`（`update_hover`，提前返回在 `:1781`）、`:1579-1593`（`on_level_closed` 只复位本菜单）、`:1910-1967`（`pointer_left`/`fire_close_grace` 不复位父级）、`:2101-2115`（`close_submenus` 不复位）。
- **证据**：探针 [G]：hover 行 1 打开子菜单（stack=2）→ 指针移到链外 500ms（子级按 grace 关闭，stack=1，root 保留）→ 重新 hover **同一行** +300ms：`stack=1`（未重开）；改为 hover 另一行再回同一行 +300ms：`stack=2`。
- **影响**：设计 §4.8 `on pointer_enter(item)` 契约（hover submenu 行应重新起 `PendingOpen`）被破坏；鼠标用户“离开菜单再回来”这一自然路径上子菜单不出现，容易被当作“菜单坏了”。
- **修法**：在子级关闭（`close_submenu_row`/`fire_close_grace`/`close_submenus`）时复位父级 `last_hovered`（以及 `trigger_over`）；或在 `update_hover` 的去重条件中同时要求“本层当前无 child 且无 pending 且当前无 grace”，使 `hovered` 相同但意图状态变化时仍继续执行。补回归测试（对应探针 [G]）。
- **阻塞**：是。

### M2-03（P2，阻塞）：root 进入 Closing 后 `PendingOpen` 定时器仍触发，在 closing parent 下真的 open 子菜单（短暂闪现）

- **位置**：`menu.rs:1853-1869`（`fire_pending_open` 未校验 parent panel 仍 dismissable）、`:1564-1568`（Closing 分支只 `chain.close`）、`:1629-1643`（`cancel_timers` 只在 `Closed`/release 调用）。
- **证据**：探针 [H]：hover 子菜单行 → +150ms（PendingOpen 已 armed）→ Esc（`open=false`，stack=1）→ 按 20ms 步进：**stack 1→2、`probe:sub:1:panel:surface` 已挂载 2 帧**（`open=false`），随后级联回收为 0。即定时器在 `DismissPending/Closing` 期间落地，引擎接受了一个 parent 处于 Closing 的子 popup。
- **影响**：关闭动画中“闪一下”子菜单；在 I8/§4.6 语义下，Closing 层不应继续派生新 popup，虽被级联回收但属于关闭事务不干净；gallery 时序敏感路径可能出偶发 diff。
- **修法**：`sync_own` 进入 `DismissPending/Closing` 时 `cancel_pending_opens`（递归，含 grace timers）+ 该层 `close`；`fire_pending_open` 追加 `panel.state(app).is_dismissable()` 与 `self.level()` 仍 Open 的校验；`start_pending_open` 不应对不 dismissable 的父级起 timer。补测试（Esc 时 PendingOpen 已 armed 的时序）。
- **阻塞**：是（虽影响面小于 M2-01/02，但属“悬空定时器 + 关闭事务内新开 popup”，应随本轮修）。

### M2-04（P3，不阻塞）：`bind_trigger` 未绑定左键 action；普通 trigger 不可聚焦导致 Alt+F10 不可达；Self-report 与实现不符

- **位置**：`menu.rs:1498-1522`（`bind_trigger` 只 `app.on_action` + `bind_key_scoped(F10|ALT)`）；`context_menu.rs:196-235`（只注册 Shift+F10/ContextMenu/F10 三个 scoped 键）；可聚焦判据 `ui_core/src/interaction/focus.rs:14-24`（需节点有 action 或 explicitly interactive）。
- **证据**：
  - 探针 [F]：无 action 的 plain trigger → `set_focus` 失败（`focused=None`）、Alt+F10 `open=false`；宿主自行加 `on_click` 的 trigger → Alt+F10 `open=true`；`bind_trigger` 后左键点击始终 `open=false`。
  - `p2-summary.md:37`（Menu `bind_trigger` = “左键 action + Alt+F10 键”）、`:44`、`:25`（ContextMenu “绑定左键 action + Shift+F10/ContextMenu/F10”）、`:56`（“ContextMenu 把左键 action 挂到 panel surface”）与实现不符；`:25` 的“未内建 secondary-button”与“绑定左键 action”自相矛盾。
  - grep：仓库内 `bind_trigger` 仅 2 处调用（均为测试），且测试 helper 自带 `on_click`，掩盖了 trigger 自身不可聚焦的问题。
- **影响**：P2 新公开 API 名不副实；宿主按 summary 只调 `bind_trigger` 会既无鼠标入口、Alt+F10 也不生效；`trigger_action` 的实际唯一来源是键盘绑定。当前无生产调用者，故 P3。
- **修法**：实现“trigger 绑定”的完整语义（为既有节点设 click action 或暴露 action 供宿主 `on_click`；Menu 另加 Shift+F10/ContextMenu 视需求），或修 summary/文档明确“trigger 必须已可聚焦/可点击，`bind_trigger` 仅补键盘入口”；同时删除 summary 中不实的“绑定左键”。`ContextMenu` 的“未内建右键绑定”既有 D-P2-4 登记，但 summary 的“左键 action”应删除。

### M2-05（P3，不阻塞）：内部 List 滚动触发的 anchor “Moved” 会关整链；D-P2-3 的“内部滚动不误关整链/只关子级”不实，测试名与断言相反

- **位置**：`menu.rs:2753-2770`（`sync_anchor`：`Moved → close_chain`；仅 `Scrolled` 分支用 `sync_internal_scroll` 豁免）、`popup_panel.rs:123-150`（`AnchorWatch::observe` 先比 rect → 滚动导致的位移先报 `Moved`，`Scrolled` 分支不可达）、测试 `menu.rs:4990-5022`。
- **证据**：探针 [C]/[C2]：root+子菜单（stack=2）→ 在 root 面板上滚轮滚动（列表动画生效后）→ `open=false`、两 popup 进入 Closing；滚动动画帧前 `open=true`，动画生效后关闭，符合“submenu anchor 位移 → Moved → close_chain”。仓库测试 `scrolling_the_parent_row_out_of_view_closes_the_submenu` 实际断言 `!menu.is_open() && stack.is_empty()`（整链关闭），与测试名“closes the submenu”及 `p2-summary.md:62`（“内部滚动不会误关整链；父行滚出视口只关子级”）矛盾。
- **影响**：长菜单 + 子菜单时滚轮/键盘滚动关闭整链（设计 §4.7 允许“祖先滚动关整链”，但 §4.8 说“父项滚出视口 → 关闭该子级（或整链，按菜单策略）”，D-P2-3 又声称豁免）；文档/测试名误导验收。运行时影响有限（该组合为鼠标滚轮跨层滚动）。
- **修法**：二选一并登记：① 实现 D-P2-3 语义——`sync_anchor` 的 `Moved` 分支用“窗口 scroll epoch + 本菜单内部 offset”区分内部滚动，内部滚动只重定位子级/关闭子级；② 若接受整链关闭，则修 `p2-summary.md:62`、测试名与断言说明，并在设计偏差登记“内部滚动=关整链”。

### M2-06（P3，不阻塞）：safe triangle 的 300ms 过期未实现（open 路径的 grace 从不 arm timer）

- **位置**：`menu.rs:1811-1828`（`arm_grace_timer` 只在 `update_hover` 的 `!has_grace && was_over` 分支调用）、`:1770-1773`、`:2060`（open 时 `compute_grace` 但不 arm）、`:1734-1737`（`refresh_geometry` 重算不 arm）。
- **证据**：探针 [I]：hover 行 1 打开长子菜单（8 项）→ 指针移到三角形内、靠子菜单近边的行 2 位置 → +400ms 后再移到行 3（仍在同一多边形内）→ 子菜单仍是同一个（`same_child=true`）。若 300ms 过期生效，行 3 应触发 sibling replacement。现有测试 `pointer_through_the_safe_triangle_keeps_the_child_and_ignores_siblings` 在三角形内 `advance(400ms)` 后仍断言保留，无法区分“未过期”和“过期后指针仍在多边形/无兄弟行”。
- **影响**：违反设计 §4.8“每次子菜单 open/flip/scroll 后重算，300ms 过期”的数值契约；stale 多边形在慢速移动场景可长期抑制兄弟替换。键盘/主路径不受影响。
- **修法**：`compute_grace` 成功后按 level arm 单层 300ms 过期 timer（或在 Polygon 存储 deadline，`grace_contains` 先判过期），过期清多边形；补“过期后移动到三角形内的兄弟行 → 替换”的负向测试。

### M2-07（P3，不阻塞）：引擎 `active_descendant` 补丁与 Self-report 不符；`set_popup_a11y` 对非法目标非幂等返回

- **位置**：`p2-summary.md:50`（声称 `PopupError` 13→15，新增 `A11yTargetMissing/A11yTargetOutOfScope`，open/set 强类型拒绝、settle 期清空并诊断）；实际：`ui_core/src/popup.rs:474+` 无这两个变体，`app/popup.rs:286-312`（open 时诊断+清空）、`:851-883`（set 时诊断+清空），`a11y/popup.rs:156-159`（仅在树投影时过滤，不写回 entry，也无 settle 诊断）；新增 `A11yDiagnostic::InvalidActiveDescendant`。
- **证据**：grep `A11yTarget` 无结果；`cargo test` 全绿，新测试 `popup_a11y_active_descendant_is_validated_and_cleared`（`tests/popup_behavior.rs:5280`）与 `a_declared_active_descendant_is_projected_updated_and_cleared`（`a11y/popup_tests.rs:1722`）验证的是 **诊断+清空**，非强类型错误。
- **影响**：Self-report/验收依据不可靠（P2 摘要声称的错误 API 不存在）；`set_popup_a11y` 在 `app/popup.rs:864-875` 用**请求值**与已存（已 sanitize）值比较，再 sanitize → 调用方若持续申报非法目标，每次 `Ok(true)` + `request_frame()`，存在每帧重绘循环风险（Menu 不会触发：`active_row_node` 只会给有效节点或 `None`）。
- **修法**：改 summary 或实现错误变体；`set_popup_a11y` 先 sanitize 再比较、仅 sanitized 变化返回 `true`；若要 settle 期清理，补实现与诊断；否则删除 summary 该行。

### M2-08（P3，不阻塞）：测试有效性与覆盖缺口（∅断言/名不副实/关键路径无负向自检）

- **位置/证据**：
  - `menu.rs:4369` `assert_ne!(level2.id, harness.root_entry(&menu).id)`：恒真（root 与 child 的 PopupId 必然不同），无意义断言。
  - `menu.rs:4305-4327`：点 `(child.hit_rect.origin.x-2, row2.top+2)` 实际落在 safe triangle 内（几何核算：x=498/exit=395/upper≈270.3/lower≈513.6，y=344 在多边形内；探针同构 geometrie 也成立），断言“grace 期间保持父项高亮”成立但不验证 sibling-replacement 的负向；同文件 `:5053` 的“bridge area”点击点（`(child.left-2, panel.max+40)`）同时落在 grace 与 bridge 内，`dismiss_outcome` 先短路 grace，**桥接多边形分支未被隔离**。
  - `menu.rs:4860-4878`：测试名 “keeps its own active item while the pointer moves”，实际只 hover 子菜单 row0 并断言 highlight=0/仍 open；未验证“指针移动”和“父层 active 不被改写”。
  - `menu.rs:4611-4616`：断言消息“禁用项 Beta 被跳过”实际原因是 buffer 变 `"gb"` 无匹配，非 disabled skip；同样 `:4601-4610` 循环匹配成立，但“跳过 disabled”未被独立断言。
  - `menu.rs:4990`：见 M2-05，测试名与断言相反。
  - **缺失（lens 必查）**：子菜单 Right→Left flip 的集成与 grace 重算；hover-open 后“第一次键盘事件把焦点移入更深层并移动其 highlight”（实现正确，探针 [E] 证明；仓库无测试）；指针离开后**重入同一行**（M2-02）；toggle 重开后 hover/Esc/outside（M2-01）；disabled 子菜单父项 hover/Right 不得打开（设计 §4.8 明确要求，代码 `menu.rs:2007-2009` 有检查但无测试）；Shift+Tab 关闭链并越过 opener（实现正确，探针 [D] 证明；仓库只有 Tab 测试）；radio 组边界（Group/Separator 截断）；PageUp/PgDn/Home/End 在子层的键盘 target；typeahead 跨层 timer（见 §6）。
  - **计数**：新增 test fn 实测 **46**（menu 27 + menu_chain 14 + context_menu 3 + a11y popup_tests 1 + popup_behavior 1；P1 29→56 等），另有 gallery `overlay-check` 8 个 chain 检查项；任务描述中的“43 条”为近似口径。
- **影响**：P2 的核心承诺（safe triangle/grace/链式状态机/键盘表）在 CI 中缺少能失败的负向自检，M2-01/02/03 均因此漏网。
- **修法**：补齐上述用例；桥接用例需构造 grace 不存在（过期后/手动 clear）且仅 bridge 命中的场景，否则删除 `bridge_contains` 的独立分支或明确其为“grace 冗余兜底”；把 `assert_ne!` 换成 root PopupId 稳定性 + child 关系断言。

### M2-09（P3，不阻塞）：a11y role 偏差未登记（Separator vs Splitter；Group 行用 Label）

- **位置**：`menu.rs:2528-2537`（`MenuEntry::Group → A11yRole::Label`；`MenuEntry::Separator → A11yRole::Separator` + `hidden`）。
- **证据**：设计 §4.10 角色表含 `Group`，并要求“菜单分隔线用 `Role::Splitter`…并在设计记录该平台限制”；§4.8 又写“分隔线不暴露为 splitter（accesskit 限制）”——设计自相矛盾；p2-summary 未在 D-P2-x 登记该偏差。
- **影响**：hidden 使运行时影响低，但设计与实现记录不一致，后续 P3 文档回写会再次踩坑。
- **修法**：统一设计正文与实现（推荐保留 `Separator+hidden` 并在 D-P2-10 登记“accesskit 限制下的偏差”），Group 行同理登记。

### M2-10（P3，不阻塞）：ContextMenu 锚点细节与 summary 不符

- **位置**：`context_menu.rs:180-193`（`open_on` 取 `(bounds.origin.x, bounds.max().y)`，即左下角）；`:166-177`（`open_at` 用 `Anchor::WindowRect(LogicalRect::from_origin_size(point, LogicalSize::ZERO))`）。
- **证据/影响**：`p2-summary.md:44` 写“节点底边中点”，设计 §4.2 写“指针点扩展的 1×1 rect”；实现为零尺寸 + 左下角。`AnchorWatch` 对 point anchor 有特判（`popup_panel.rs:132-137`），零尺寸可用，无功能错误；键盘打开右键菜单横向偏左。
- **修法**：修文档或实现（中点/1×1），二者选一并登记。

## 3. 设计 §4.8 逐项核对表（菜单链状态机）

> ✅ 符合；⚠️ 偏差已登记且可接受；❌ 偏差/缺陷；— 不在本 P2 lens。

| # | §4.8 要求 | 结果 | 证据/说明 |
|---|---|---|---|
| 1 | 每层 `Closed/Opening/Open/Closing` 状态 | ✅（D-P2-7 命名偏差已登记） | `menu_chain.rs:43-49`，`LevelSlot.state` |
| 2 | PendingOpen/PendingClose 定时器绑定 level（不用全局 topmost） | ✅ | `menu.rs:1136-1141` timer 字段；`start_pending_open`/`pointer_left` 捕获 `LevelId` |
| 3 | `on pointer_enter`: grace 内忽略兄弟替换 | ✅ | `update_hover:1774-1777`；测试 `pointer_through_the_safe_triangle...` |
| 4 | `on pointer_enter`: submenu 行重新起 PendingOpen | ❌ **M2-02** | 重入同一行不启动（探针 [G]） |
| 5 | `on pointer_enter`: 普通/disabled/group/separator 关子级 + 取消 timer | ✅ | `update_hover:1789-1799`；`close_submenus` 取消 pending open/grace |
| 6 | `on pointer_leave(chain)`: 300ms PendingClose，绑定 deepest hover-opened level | ✅ | `pointer_left:1910-1942`；`deepest_kept` 边界；测试 `leaving_the_chain...` |
| 7 | PendingClose 期间重入链任意 rect/多边形取消 | ✅ | `sync_chain:1688` + `cancel_pending_close`；测试 `reentering...` |
| 8 | PendingOpen fire: close siblings/descendants、open child、z 递增、键盘聚焦首项 | ✅（但缺 parent 存活校验 → M2-03） | `open_submenu_row:1986-2075`；测试 3 级链 |
| 9 | PendingOpen/PendingClose 在 DismissPending/Closing 期间不应落地 | ❌ **M2-03** | 探针 [H] |
| 10 | 引擎 Toggle/Closing 重开 → 链恢复 | ❌ **M2-01** | 探针 [A] |
| 11 | safe triangle：5 点、左右镜像、翻面重算 | ✅（几何单测齐；flip 集成测试缺） | `menu_chain.rs:64-104`；单测 3 条镜像/边界/非有限值 |
| 12 | safe triangle 300ms 过期 | ❌ **M2-06** | open 路径从不 arm timer |
| 13 | 指针桥/bridge band 用于 outside press Ignore | ⚠️ 测试未隔离、4px 间隙下与 grace 重叠（M2-08） | `bridge_contains` 先判 grace；测试点同时在 grace 内 |
| 14 | 键盘表：↑↓/Home/End/PgUp/PgDn | ✅ | `bind_extra_keys:2779-2858`；测试 `the_keyboard_table_pages_and_jumps...`、`arrow_keys...` |
| 15 | ←/→/Enter：打开/关闭子菜单、聚焦首项、父项不 toggle | ✅ | `right_opens...`、`key_open_submenu`/`close_to_parent`；键盘源 `OpenSource::Keyboard` |
| 16 | Esc：引擎逐层、焦点逐层归还 | ✅ | `escape_closes_one_level_at_a_time_children_first`、`escape_returns_focus_to_the_parent_level` |
| 17 | Tab/Shift+Tab：关整链 + 焦点过 opener/前一 tab stop | ✅（Tab 有测试；Shift+Tab 仅探针） | `tab_out:2300`；测试 `tab_closes...`；探针 [D] |
| 18 | typeahead：2s、按层、跳 disabled/sep、循环匹配 | ✅（跨层 timer 见 §6） | `typeahead`/`typeahead_row`；测试 2 条 |
| 19 | typeahead 不消费 text/IME | ✅（D-P2-5） | `a_text_event_never_feeds_the_typeahead_buffer` |
| 20 | checkable：Space 不关、Enter 关、radio 互斥、checked 投影 | ✅ | `toggle_checked:2181`、`radio_run:2229`；测试 2 条 |
| 21 | hover 不夺焦；首次键盘事件移入最深打开层 | ✅（无仓库测试 → M2-08） | `open_focused` take_focus 按 source；`keyboard_target:2140`；探针 [E] |
| 22 | `active_descendant` 只引用本层 item | ✅ | `sync_active_descendant:1466`；测试 `the_active_item_is_projected...` |
| 23 | 2–3 级链：打开/关闭/兄弟替换/外部/Esc/滚动/翻面 | ⚠️（3 级打开/关闭/Esc/兄弟/外部 ✓；flip 缺） | `a_three_level_hover_cascade...`、`a_context_menu_chain...` |
| 24 | 长菜单内部滚动；父项滚出视口关子级/整链 | ⚠️ **M2-05** | 内部滚动（Moved）实际关整链；测试名/断言相反 |
| 25 | 链式 dismiss：祖先前按下只关后代、outside Replay/Consume、tooltip 优先 | ✅ | 引擎 P1 + `a_press_on_an_ancestor_level...`、`a_press_outside_the_bridge...replays`、`a_consuming_menu...` |
| 26 | ContextMenu：WindowRect/节点锚/Shift+F10/ContextMenu/子菜单共享链 | ⚠️（D-P2-4 无右键自动绑定；bind_trigger 见 M2-04；锚点 M2-10） | 测试 `shift_f10...`、`a_context_menu_chain_opens_three_levels...` |
| 27 | 关闭先子后父、最新层最上、根保留 | ✅ | `the_chain_assigns_states_per_level_and_closes_children_first`、`leaving...`；debug-stack z=[2,3,4] |
| 28 | 兄弟替换：grace 外立即 close 旧子级 + PendingOpen 新行 | ✅ | `a_pointer_outside_the_triangle_replaces_the_open_sibling` |
| 29 | `deepest_kept`（保留 click/keyboard-opened，关闭 pointer-opened） | ✅ | `deepest_kept:451`；`leaving...` 断言根保留 |
| 30 | 2–3 级 a11y 嵌套与 expanded 投影 | ✅ | `a_three_level_a11y_chain_nests_and_projects_expanded`；引擎 overlay `expanded_of.opener` |

## 4. 测试有效性评估

### 4.1 正面

- 测试全部跑在真实引擎 popup 栈上（不是 mock）：popup stack entries/z/parent、a11y snapshot、damage_full、焦点真实节点 id 均有断言；时间用 `TestApp::advance` 推进虚拟时钟并 drain，无 sleep/忙等，硬编码 220/260/400ms 与 `HoverTimings` 200/300ms 一致。
- 关键几何有负向自检：三角形外/远端外/非有限值拒绝（`menu_chain.rs` 单测）；桥接无垂直重叠拒绝；兄弟替换与 grace 保留成对出现。
- a11y 侧有真实投影测试（active_descendant 跟随键盘/行卸载清空/非法目标诊断/2–3 级 parent）与真实窗口 debug-stack 三链路验证。
- gallery `--pixel-check` 94 checks 含 2 点注入故障自检，`damage_full_frames=0`；`--debug-stack` 3 级链 z/parent/关闭后 entries=0。
- 全量测试 0 failed，workspace check 通过。

### 4.2 弱断言/名不副实（详见 M2-08）

| 位置 | 问题 |
|---|---|
| `menu.rs:4369` | 恒真 `assert_ne!(child.id, root.id)` |
| `menu.rs:4305` | 三角形内点保持 child 成立，但未隔离 sibling-replacement 的负向；未验证 300ms 过期 |
| `menu.rs:5053` | “bridge” 点击同时命中 grace（几何核算），桥接分支未被隔离 |
| `menu.rs:4860` | 名称承诺指针移动/自身 active，实际只验证 open+highlight |
| `menu.rs:4611` | 注释“Beta disabled 被跳过”与真实原因（buffer 追加为 "gb"）不符 |
| `menu.rs:4990` | 测试名/摘要说“只关子级”，断言整链关闭 |
| `menu.rs:3869` | Toggle 重开只断言 `is_open`，未断言链/焦点/后续交互 |

### 4.3 覆盖缺口（按风险排序）

1. Toggle/Closing 重开的链状态（M2-01）——无。
2. 子级 grace 关闭后重入同一行（M2-02）——无。
3. DismissPending 时 PendingOpen 已 armed（M2-03）——无。
4. safe triangle 过期（M2-06）——无（现有 400ms 用例反而以“仍保留”断言，不能证明过期）。
5. 子菜单 Right→Left flip + 翻面后 grace 重算 —— 无集成测试。
6. hover-open 后首次键盘事件夺焦并作用于深层 —— 无（实现正确）。
7. Shift+Tab —— 无（实现正确）。
8. disabled 子菜单父项 hover/Right 不打开 —— 无。
9. radio 组被 Group/Separator 截断 —— 无。
10. typeahead 跨层 timer 竞态（§6 R4）—— 无。
11. ContextMenu 鼠标 hover 级联（现有 3 级用程序化 `open_submenu`，未测 hover/grace）—— 无。

## 5. 已核实正确点（无需修改）

- **链结构/层序**：3 级链 parent/z/最新在上；子级先关；click-opened root 在 pointer-leave 后保留（`deepest_kept` 边界）；Esc 逐层且焦点归还父层；关闭只上报 root `on_close` 一次。
- **safe triangle 几何**：5 点顺序 `exit+bleed → 近端上 → 远端上 → 远端下 → 近端下`；Right 用 `-5`、Left 用 `+5` 镜像正确；非有限输入拒绝；点在多边形内判定边界（远端外/基线下/退出点左侧）正确。
- **grace/指针**：sibling 替换在 grace 外立即发生并重开；进入三角形保持 child；离开链 300ms 后关 pointer-opened 层、root 保留；grace 内重入取消关闭。
- **键盘**：↑↓/Home/End/PgUp/PgDn/←/→/Enter/Space/Esc/Tab/Shift+Tab/Alt+F10 全部可用（Enter/Space 经 focused node 的 action_target → surface action，Space 依 `key_down` 区分 keep-open）；Tab/Shift+Tab 关闭后焦点越过/回到 opener 前后 tab stop（探针 [D] 与仓库 Tab 测试）；hover 不移动 DOM 焦点；首次键盘事件移入 hover-opened 深层（探针 [E]）。
- **checkable**：Space on checkbox 切换不关、Enter 关链、checkbox 状态+回调；radio 同层互斥（连续 radio run，Group/Separator 截断）；`MenuActivation.check` 透传；a11y role CheckBox/Radio + checked 投影。
- **a11y**：`active_descendant` 落在 role=Menu 的 popup content 根（不是 surface）；跟随键盘 active_item；行卸载（虚拟列表）清空；2–3 级 a11y 父=父 menuitem；expanded 由引擎按 opener/dismissable 投影（关闭同帧 false）；非法目标（越界/不存在）强类型诊断+清空，不投影到树；owns 内目标可投影；声明值优先于焦点推导。
- **dismiss**：outside press 默认 Replay（D14）、显式 Consume 不 replay、桥接/grace 区 Ignore；按祖先层只关后代；ContextMenu 页面滚动关整链；Esc 逐层；ContextMenu 3 级共享链。
- **渲染/性能**：菜单开/关不 `damage_full`；gallery pixel-check 94 checks PASS、debug-stack 3 级 PASS。

## 6. 残余风险（不单独立 finding，但建议 P3 文档/后续处理）

- **R1 内部滚动矩阵**：Dropdown/Select 在内部滚动时按 P1 语义重新 settle；Menu 内部滚动在“有子菜单”时关闭整链（M2-05）。若 P3 要统一“内部滚动豁免”，需同时覆盖 `Moved` 分支与 `Scrolled` 分支，并在设计登记。
- **R2 typeahead timer 跨层**：buffer 是 chain 级（`Typeahead{level,buffer}`），timer 是 per-menu（`menu.rs:2328-2342`）。层 A 起 timer 后切到层 B 输入，层 A 的旧 timer 会在 2s 时调用 `chain.clear_typeahead()` 清掉层 B 较新的 buffer（提前过期）。建议 timer 捕获 level 并在回调中校验 `typeahead.level == level`。
- **R3 `set_popup_a11y` 幂等性**：见 M2-07；Menu 不受影响，其它声明型调用方可能每帧 `Ok(true)`。
- **R4 grace 无 timer 路径**（M2-06）与 `trigger_over` latch：一旦实现过期，需回归 `pointer_through_the_safe_triangle...`（它在 400ms 内断言保留，若同时不移动则不冲突，但要确认不引入误关）。
- **R5 焦点归属非模态**：菜单非模态但 root 打开即 `Take`；若宿主把焦点放回背景可编辑控件，菜单键盘表失效（scope binding 不在焦点链），Esc 仍引擎关闭——符合非模态语义但值得写入文档。
- **R6 a11y 树中的 surface 容器**：surface 是 `role=GenericContainer` + `tab_stop`，AT 焦点可能落在该容器上（设计接受，但 P3 可考虑更贴近 ARIA menu 的结构）。
- **R7 设计文档无 §13/P2 回写**：本轮只有 `implementation/p2-summary.md`；按 §6 P3-7 的文档回写尚未执行，M2-04/M2-05/M2-07/M2-09/M2-10 的 summary 偏差需在回写时纠正（或在修复轮先改 summary，避免审查依据继续漂移）。

## 7. 附录：证据索引

- **静态**：`git diff 152616b1`（14 文件 +4339/−1530）；`crates/ui_kit/src/kit/menu_chain.rs`（942 行）；`menu.rs` 56 tests/`menu_chain.rs` 14/`context_menu.rs` 17。
- **测试/门禁输出**：§1.3 表（ui_core 2805+111；ui_kit 844；gallery pixel-check 94 checks/0 full；debug-stack PASS 3-level z=[2,3,4]）。
- **探针（仓库外）**：`E:\kytrix\target\p2-probe`（`src\main.rs`，公开 API；可删除）。输出摘要：
  - `[A] toggle reopen: open=true stack=1`；`[A] hover submenu row after reopen: stack=1`；`[A] control fresh hover: stack=2`。
  - `[C2] ... +120ms: open=false stack=2`（内部滚动后整链关闭）。
  - `[D] shift+tab: open=false focused=Some("probe-before")`；`tab: focused=Some("probe-after2")`。
  - `[E] first keyboard event: focus=Some("probe:sub:1:panel:surface")`。
  - `[F] plain trigger: focus=None / alt+F10 open=false`；`clickable trigger: left click open=false / alt+F10 open=true`。
  - `[G] re-enter same row +300ms: stack=1`；`move other row then back: stack=2`。
  - `[H] step2 +60ms: open=false stack=2 submenu_mounted=true`（closing 期子菜单闪现）。
  - `[I] after move to 丙 row inside triangle: stack=2 same_child=true`（grace 未过期）。
- **始终未改动仓库**：`git status --short` 仅 P2 工作树 + 本 review 文件。

---

**最终判定（供 lead 汇总）**：P0=0、P1=0、P2=3（M2-01/02/03）、P3=7（M2-04…M2-10）。**P2 暂不可提交**：先修 3 条 P2 并补对应负向测试；P3 中 M2-04/M2-05/M2-07 涉及 Self-report/文档误导，建议同轮纠正（或至少在提交说明中登记）。门禁本身全绿（测试/check/pixel-check/debug-stack），问题集中在链状态机与定时器生命周期的边界时序，以及测试未能失败。


