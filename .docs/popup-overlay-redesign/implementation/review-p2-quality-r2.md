# P2 收口复核（第 2 轮）— p2-review-quality

- 对象：worktree `E:\kytrix\.agents\worktrees\dsh-1d175414-1a41`，基线 `152616b1`，P2 未提交工作树（task-58 + 收口修复）。
- 快照：代码最后 mtime **14:55:15**（`menu.rs`；14:55:15–15:03 稳定），代码指纹（排除 `.docs`）`git diff 152616b1 -- . ':(exclude).docs' | git hash-object --stdin` = **`e4ebe19359b2fd814c527080345ce52330a1628a`**，`menu_chain.rs` = `4607d471d3d5f2302ce9ff4ef0e198687b964fd3`。审查期间 14:49–14:55 有并发修复轮（M2-02R：`last_hovered` 复位 + 2 条 Esc/重开测试、typeahead 跨层 timer、flip/root-Opening 测试）；文档 mtime 仍为 `p2-summary` 14:38:32 / 设计 §13 14:40:12 / kit-gallery 14:38——**早于最后代码**。
- 只读；仓库零改动（本文件除外，`.docs` 被 ignore）。

## 0. 结论

| 项 | 结果 |
|---|---|
| P0 / P1 / P2 | **0 / 0 / 0**（代码/测试层面；另见下方 debug-stack 退出码待验证） |
| P3 | 若干（多数为已登记项 + 本轮新增观察，见 §3） |
| Q2-01 | **关闭** |
| Q2-02 | **基本关闭**：原 7 项已按代码修正；**但文档数字/两处文案在 14:49–14:55 修复轮后过期**（P3，见 §2.1） |
| Q2-03 | **关闭**（6 点桥接几何已删；改为 grace ∪ 即时 safe triangle，测试改名/新增） |
| 提交判定 | **可提交**，前提：① 按最终代码重跑一次门禁并回填文档数字（3839/858 等，见 §2.1）；② 在干净会话复跑 `--debug-stack` 确认 exit 0（本会话 log 断言 PASS，但进程停在 “closing window”；同会话旧 build 亦复现 → 会话/GPU 环境问题，见 §4）。P3 项登记即可。 |

## 1. Q2-01 / Q2-03 与 M2-02R 复核

| 项 | 状态 | 证据（最终代码） |
|---|---|---|
| Q2-01 引擎取消关闭/失败重试 | **关闭** | `sync_own` Opening\|Open → `reconcile_chain`（`menu.rs:1576-1635`）：Closing→`reopen`、Closed→root `open_root`/submenu `revive`，并清 `last_hovered/trigger_over/active_descendant`；DismissPending\|Closing 只 `mark_closing` 不再直接 `close`；`open_focused` 失败 root `chain.reset()` + `level=None`（:1412-1424）。测试 `a_cancelled_close_restores_the_chain_and_hover_cascade`、`a_failed_open_resets_the_chain_and_the_retry_keeps_submenus_working` 通过。 |
| Q2-03 桥接几何 | **关闭** | `bridge_polygon/bridge_contains` 删除；`MenuChain::safe_area_contains` = `grace_contains` ∪ `derived_safe_triangle`（`menu_chain.rs:127-139,599-631`），`dismiss_outcome` 使用它（`menu.rs:1728-1733`）。测试改为 `a_press_inside_the_safe_triangle_keeps_the_chain_open` / `a_press_outside_the_safe_triangle_closes_the_chain_and_replays`；新增 flip 镜像测试 `a_flipped_submenu_keeps_the_chain_through_the_mirrored_safe_triangle`。 |
| M2-02R（menu 审查 r2 的 P2，并发修复中） | **已修/已测（建议 menu 审查确认后关闭）** | `on_level_closed` 复位 `last_hovered/trigger_over/active_descendant`（`menu.rs:1637-1654`）；submenu `on_close` 条件复位父层（:2668-2683）；`reconcile_chain` 两个分支均复位（:1604-1635）。新增 `escaping_a_hover_opened_submenu_then_reentering_the_row_reopens_it`（对应探针 [J]）与 `a_programmatic_reopen_after_two_escapes_reopens_the_submenu_row`（[K]），本轮 858 条 ui_kit 测试通过。 |
| typeahead 跨层 timer（menu r2 R2-a） | **已修/已测** | timer 回调改为 `clear_typeahead_for(level)`（`menu.rs:2429-2446`；`menu_chain.rs:641-671`）；新增 `a_stale_typeahead_timer_does_not_clear_a_newer_layers_buffer`。 |
| root Opening anchor / flip（R2-b/R2-c） | **已补测试** | `submenus_opened_during_the_root_opening_animation_survive`、`a_flipped_submenu_keeps_the_chain_through_the_mirrored_safe_triangle`。 |

## 2. Q2-02 文档复核（task-60 之后又发生代码变更）

原 7 项（bind_trigger 左键、open_on 底边中点、surface_action 归因、“只关子级”、bridge 桥接、PopupError +2、a11y +2 测试）均已按代码修正；`design §4.8/§13`、`p2-summary §8` 已改“仅键盘/底边左端点/无桥接几何/整链关闭/PopupError 13 变体/+1 测试”。**但最终代码（14:55）比文档（14:38/14:40）多出 5 条测试与 M2-02R 修复，当前数字全部过期：**

| 文档位置 | 文档值（14:38/14:40） | 最终代码/本轮实测 |
|---|---|---|
| `p2-summary:11`、`design §13.1:1441` | +4915/−1510 | **+5070/−1483**（排除 `.docs`；含 14 tracked 文件 + untracked `menu_chain.rs` 921 行） |
| `p2-summary:12`、`design §13.1:1442` | menu.rs 5697 行/68 测试、menu_chain 911 行/11 测试；context 1031/17 | **menu.rs 5879 行/73 测试、menu_chain 921 行/11 测试**；context 1031/17 不变 |
| `p2-summary:91/98`、`design §13.6:1507`、`kit-gallery §13.6:746` | 3834 passed / 2 ignored，ui_kit 853；menu-family 96 条 | **3839 passed / 2 ignored，ui_kit 858**；menu-family **101 条**（73+11+17） |
| `design §7.3:857` | 96 条 `#[test]`；文案仍含“**桥接 Ignore**” | 101 条；桥接几何已删除，应改为“safe triangle/grace Ignore” |
| `p2-summary:5`、`design §13:1437` | “P2-1…P2-7 **全部落地**” | 同文件/同节 P2-6 为“部分 done（右键入口 D-P2-4）”，自相矛盾 |
| `p2-summary:62` | “grace 过期后**不再保留多边形**” | `safe_area_contains` 始终计算 `derived_safe_triangle`（`menu_chain.rs:599-613`），过期后仍保留一条约 bleed 宽的窄带；设计 §13.4:1478 的“即时 safe triangle”表述正确，summary 该句需改 |
| `p2-summary:63`、`design §13.4:1480` | “`Scrolled` 仅 ContextMenu 显式关链” | 代码为 `Scrolled if scroll_closes && !internal_scroll`（`menu.rs:2859-2870`），少了“非内部滚动”限定（精度问题） |

> 结论：task-60 需以**最终代码快照**重跑门禁并回填上表（预计 3839/858、101 条、+5070/−1483、menu.rs 5879/73、menu_chain 921）。这是当前唯一的 Q2-02 残余，P3。

## 3. 修复引入的新问题复扫（本轮）

### 3.1 已确认无问题

- **chain 状态计时/grace 过期**：`arm_grace_timer`（open）/`rearm_grace_timer`（几何变化）均先取消旧 timer；`the_safe_triangle_expires_and_the_sibling_replaces_it` 通过；无 timer 泄漏（`cancel_timers` 覆盖 4 类）。
- **失败重试**：`open_focused` 失败清 level/链，重试后 root `open_root`/`Right` 开子菜单正常（测试通过）；无自引用槽。
- **重复 bind_trigger**：Menu/ContextMenu 均先 `off_action(previous)`；`rebinding_the_trigger_replaces_the_previous_action` 通过。
- **共享 sink**：`Menu::on_activate/on_close` 仅 Root 生效（`menu.rs:1488-1496`）；`a_submenu_cannot_replace_the_shared_activation_sink` 通过。
- **`on_item_click` 新签名**：ContextMenu 回调携带 `Option<MenuCheckState>`（`context_menu.rs:240-246, 265-282`）。
- **`on_dismiss` 可见性**：已改为 `pub(crate)`（`popup_panel.rs:279`）。
- **gallery `chain_menu.sync`**：已加入帧循环（gallery.rs:2540）；否则关闭动画不推进。
- **每帧分配/a11y 缓存**：`active_descendant` Cell 缓存只在 active 变化时 `set_a11y`（`menu.rs:1475-1490`）；仍保留每帧 `open_levels()` Vec（P3 性能观察，无实测基线，P3-5 已登记）。
- **遗留调试输出**：14:52 快照曾在 `on_level_closed`/`dismiss_outcome` 出现两条 `eprintln!("DBG ...")`；14:55 最终快照已删除。最终新增行注释/`#[allow]`/`eprintln` 均为 **0**（5070 行新增扫描）。

### 3.2 新增 P3 观察（不阻塞，建议登记）

1. **`derived_safe_triangle` 未被隔离测试**：`a_press_inside_the_safe_triangle...` 的落点同时落在 grace 三角内（测试未先让 grace 过期），因此 `safe_area_contains` 中“即时 safe triangle”分支未被单独证伪；该三角形在 `gap=0` 时仅比子面板多出一条 ≤5px 窄带。建议补“grace 过期后三角内 Ignore/三角外 Close”一条。
2. **`a_failed_open...` 未覆盖 level 已设置后的失败分支**：测试用已关闭 window，错误发生在 `resolve_icons`（早于 `level` 设置）；`open_focused` 中 `panel.open` 失败 → `chain.reset()+level=None`（`menu.rs:1412-1424`）未被测试命中。可用“有效窗口 + 已删除 anchor 节点”构造。
3. **ContextMenu 新 API 测试缺口**：`open_on` 的 `AnchorNodeMissing` 错误路径、ContextMenu 重复 `bind_trigger`、`on_item_click` 的 check 状态传递均无测试（Menu 侧有对应测试）。
4. **`reconcile_chain` 与 pointer-close 的 Closing 状态**：`pointer_left` 把链层标 `Closing` 后，下一帧 `sync_own` 的 reconcile 会立即 `reopen`（因为 panel 仍是 Open）；grace 关闭实际由 `close_timer/closing_root` 承担，`Closing` 状态几乎只存活一帧。无用户可见问题，但状态机冗余/churn，建议 reconcile 仅在 panel `Opening` 晚于引擎取消关闭时执行，或删除该状态。
5. **`Menu::on_activate/on_close` 仅 Root 生效、`open_submenu` 句柄只应 open/close/sync** 未写入公开 API 文档（menu r2 R2-f）；`on_level_closed` 清 `last_hovered` 的修复已关闭 [J]/[K]。
6. **menu r2 的 P3 残留**：`R2-d` 恒真断言仍在（`menu.rs:4484` `assert_ne!(level2.id, root_entry.id)`）；`R2-e`（设计 §4.2 WindowRect 1×1 vs 实现 0-size point anchor）仍是文档精度项。未在 `p2-summary §7` 登记。
7. **既有登记项**：R2-02 关闭 reason 仍固定 `Programmatic`（`popup_panel.rs:457-472`）；E-12 弱测试（typeahead 追加断言/分页自证）；无性能基线（P3-5）；右键自动绑定 D-P2-4；M2-07 `set_popup_a11y` 幂等观察。

## 4. 门禁实测（最终代码快照）

| 命令 | 结果 |
|---|---|
| `cargo test -p ui_core -p ui_kit`（精确命令，锁释放后复跑） | **PASS**：3839 passed / 0 failed / **2 ignored**（ui_core lib 2805+1 ignored、frame_record 8、gui_smoke 0+1 ignored、layout 4、popup_behavior 111、snapshot 14、demo 39、**ui_kit 858**），**exit 0** |
| 同上首次尝试 | 另一 agent 的 `gallery.exe` 占用 target，链接 `LNK1104`（与 P1 审查同现象）；锁释放后精确重跑 exit 0 |
| `cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings` | exit 0 |
| `cargo +nightly fmt --all -- --check` | exit 0（输出为空） |
| `cargo check --workspace --all-targets` | exit 0（覆盖 gallery example 编译） |
| `cargo run -p ui_kit --example gallery -- --pixel-check`（精确命令） | **PASS**：`checks=94`、`sampled_frames=32`、`close 6+6`、`damage_full_frames=0`，**exit 0** |
| `gallery --debug-stack` | **log 断言 PASS**：chain-open entries=3/z=[2,3,4]、chain-closed entries=0、`overlay-check PASS`（final frame=49）；**退出码未取得**：进程停在 “closing window”。同会话内用 **14:34 旧 build**（14:44 曾 exit 0，当前代码变更只涉及 menu 状态机）复跑同样停在 “closing window” → 属**会话/GPU 退出环境问题**而非本轮代码回归；仍需在干净会话复跑确认 exit 0（若单实例仍不退出则按 P1 门禁例外处理）。 |

日志：`E:\kytrix\target\p2q-r2g-{test,pixel}.log`（精确命令）、`p2q-r2f-{clippy,fmt,wscheck,debugstack-copy}.log`、`p2q-r2-oldbuild-debugstack.log`（旧 build 差分退出）、`p2q-r2f-idlecheck.log`（窗口关闭路径 exit 0 对照）、`p2q-r2f-gallery.exe` / `p2q-r2-gallery.exe`。

## 5. 结论（回复 lead）

- **Q2-01 = 关闭；Q2-03 = 关闭；Q2-02 = 原有 7 项已关，但最终代码比文档多 5 条测试/M2-02R 修复，数字与 2 处文案过期（P3，需 task-60 按最终快照重填）。**
- **P0/P1/P2 = 0**（代码/测试）；**P2 可提交**，但需：① 重跑最终门禁并回填文档数字（3839/858、101 条、+5070/−1483、menu.rs 5879/73）；② 干净会话复跑 `gallery --debug-stack` 确认 exit 0（本会话 log 断言 PASS，退出停在 “closing window”；旧 build 同复现，判为会话/GPU 环境问题，但 lead 需在提交前拿到 exit 0 证据）；③ 将 §3.2 的 P3 观察登记进 `p2-summary §7`/`§13.7`（或由 menu 审查统一收口）。
- 本轮修复轮已把 menu 审查 r2 的唯一 P2（M2-02R）修掉并补测试（858 条通过）；建议 lead 在提交说明/§13 中同时记录该轮修复，避免 menu r2 文档停留在“不可提交”状态。
