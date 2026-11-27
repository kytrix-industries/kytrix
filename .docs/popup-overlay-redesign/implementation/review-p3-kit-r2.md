# P3 收口复核 r2（kit lens）— P3K-01/02/03、P3E-08 关闭核对

- 审查员：子 agent `p3-review-kit`；对象：worktree `E:\kytrix\.agents\worktrees\dsh-1d175414-1a41`，基线 `f24a3441`（P2）。
- **r2 快照**：`git diff f24a3441 -- crates | git hash-object --stdin` = **`e9140fc3e90a23bbc2bacdd9808c25154ae27c38`**（含 `git diff` 全量（除 .docs）= `209c7c87cba71e69060b1c6d813e9f339b7e9fef`）；`crates` 24 文件 **+4354/−131**；代码最新 mtime `pass.rs 17:56:07`、`menu.rs 17:06:09`、`popup_panel.rs 17:09:24`；`tools/*.sh` 已 staged（100755），`tools/real-pixel-check-windows.ps1` 也已 staged。设计 §14 / p3-summary 于 17:55:49 定稿，kit-gallery §14 于 17:40:50。
- 方法：只读（未改任何代码/文档受版本控制文件；仅新增本 r2 文件）+ 复用 r1 仓库外探针 `E:\kytrix\target\p3-kit-probe` 复测 scrim/Dropdown + 全量门禁复跑 + 逐修复点读码。命令统一 `CARGO_TARGET_DIR=E:\kytrix\target`。
- **结论：r2 P0 = 0，P1 = 0，P2 = 0，P3 = 1（仅 §14.3 一处过期描述，非代码）→ kit 侧代码可提交**；r1 的 8 条 finding 中 6 条已修复、2 条按 §14.7 转 P4 登记。

---

## 1. r1 finding 关闭核对

| r1 finding | r2 结论 | 证据（r2 实测） |
|---|---|---|
| **P3K-01 [P2] scrim 关闭期缩放** | **已关闭** | `PopupEntry::is_fade_only()`（`popup.rs:572-574`：`is_modal() \|\| Modal \|\| Scrim`）取代 `is_modal()\|\|tier==Modal`；`animation.rs:42-57`、`dismiss.rs:206-214` 均改用该判定。新增引擎测试 `fade_scale_modal_scrim_is_fade_only`（`popup_behavior.rs:6051-6112`）断言打开/关闭帧 modal+scrim `paint_scale==1.0` 且关闭后移除。**kit 探针复测**：`DialogBuilder::new("Scrim probe")` 关闭第 1/2 帧 scrim `paint_scale=1`、实际绘制 quad 均为视口 `(0,0 800×600)`（r1 为 0.991/0.982 且 inset 3.55/7.07）；引擎直开对照组同样 `(0,0 800×600)`。设计 §14.6 D-P3-2 已改写并记录 P3E-02/P3K-01 修复 |
| **P3K-02 [P3] D-P3-5 文档与代码相反** | **已关闭** | 新增 `dropdown::the_dropdown_slides_in_without_moving_the_settled_rect`、`select::the_select_slides_in_without_moving_the_settled_rect`：断言打开帧 `paint_offset=(0,4)±0.5`、`paint_scale==1`、局部 damage、settle 后 offset 归零；探针复测 Dropdown 打开帧 `(0,4)`。文档全部改对：§14.1“ItemPanel 打开动画改 FadeSlide，Menu/ContextMenu/Dropdown/Select 共用”、§5.2、§14.6（D-P3-5 已改为真实窗口像素条目）、p3-summary、kit-gallery §14“Dropdown/Select 也经 ItemPanel 使用 FadeSlide” |
| **P3K-03 [P3] D-P2-10 理由不成立** | **已关闭** | `menu.rs` 分组头改 `role: Some(A11yRole::Group)`；`group_and_separator_rows_expose_their_a11y_roles` 断言 `A11yRole::Group` + label + 不可聚焦；`A11yRole::Group`→accesskit `Role::Group`（`a11y/mapping.rs:22`），符合设计 §4.10；Separator 仍 `A11yRole::Separator`→accesskit `Splitter`+`hidden`=设计本意。设计 §13.4 D-P2-10、§13.7 行、§14.7“fixed（P3K-03）”均已回写 |
| **P3E-08 [P3] close_with_reason stale reason** | **已关闭** | `popup_panel.rs:464-477`：先 `begin_close_popup`，仅当 `closing==true` 才写 `inner.reason`；dismiss handler 也改为仅 `outcome != Ignore` 时写 reason（`popup_panel.rs:424-437`）。新增负向测试 `a_refused_close_does_not_leak_its_reason_into_a_later_close`：`on_dismiss(Ignore)` 下 `close_with_reason(AnchorLost)` 返回 false、保持 Open，随后 `close_now` 上报 `Programmatic`（不泄漏）；原 `closing_with_an_explicit_reason_reports_that_reason` 仍断言恰好一次 |
| **P3K-04 [P3] compose 不可达+公式错** | **已关闭** | `paint/pass.rs` 中 `compose` 已删除，仅保留 `inherited_scale`（祖先→后代继承；popup 树平铺，嵌套 transform 不可达）；§14.3 已删除“外层×内层 compose”表述，p3-summary 同步 |
| **P3K-05 [P3] paint_scales 未清理** | **已关闭** | `arena.rs:610-612` 移除时清 `paint_scales[index]=(1.0, ZERO)`；侧表改为 `(factor, pivot)` 并进入 `paint_state` reuse 键（`content.rs:741,776,813-826`），新增单测 `the_paint_reuse_key_includes_the_scale_pivot`（ui_core lib +1，2805→2806） |
| **P3K-06 [P3] primitive 覆盖/CanvasText** | **部分修复 + 余项登记** | 新增 `fade_scale_scales_glyph_runs_and_images_in_animation_frames`（GlyphRun/Image 按 scale 缩放渲染断言）；CanvasText 只缩放 origin 的余项已按 §14.7“P3K-06 → P4”登记（不阻塞） |
| **P3K-07 / P3K-08 [P3] scale 契约未登记 / 可观测门禁缺口** | **已登记 P4** | §14.7 两行 P3K-07（scale 不作用于 hit/a11y/IME 的契约）、P3K-08（debug-stack 无 `paint_scale`、像素门禁不采样动画帧）→ P4 |
| （engine lens P3E-01 Shadow.offset） | **已关闭** | `pass.rs` 中 `shadow.offset = LogicalPoint::new(self.length(offset.x), self.length(offset.y))`（向量 ×factor，不再套 pivot）；新增 `fade_scale_scales_the_shadow_offset_as_a_vector`（含“buggy 公式必产生 >2px 漂移”回归守卫 + GPU 实例 kind==3 断言）。ColorPicker 阴影漂移的用户可见影响随之消失 |

> r1 另外两条组合风险（P3E-03 性能口径、P3E-04 空闲帧恒真）在 r2 一并处理：`frame_cpu` 改为 `frame_started.elapsed()`（`frame.rs:287` 覆盖 `App::frame` 全程）、新增 `popup_animation_records` 并在 open 帧断言 `>0`、性能守卫改为**中位帧 ≤ idle p95 + 2 ms**（设计目标，`popup_behavior.rs` 预算段），与 §14.6 D-P3-7 文档一致。

## 2. r2 门禁（本机 Windows 11 / D3D12 WARP（headless）/ RTX 5080（真实窗口）/ 125% DPI）

| 命令 | 结果 |
|---|---|
| `cargo test -p ui_core -p ui_kit` | **PASS：3872 passed / 0 failed / 2 ignored**（ui_core lib **2806**+1 ignored、frame_record 8、gui_smoke 0+1 ignored、layout 4、popup_behavior **123**、snapshot 14、demo 39、ui_kit lib **878**）= 与 §14.5 逐二进制一致；相对 P2 3847 **+25**（引擎 +12、ui_core lib +1、kit +12） |
| `cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings` | PASS |
| `cargo +nightly fmt --all -- --check` | PASS |
| `cargo check --workspace --all-targets` | PASS |
| `gallery --pixel-check` | **PASS** 94 checks / 32 sampled / open 8 / close 6+6 / `damage_full_frames=0` / 首可见帧 rect==稳定帧；exit 0 |
| `gallery --debug-stack` | **PASS**：3 级链 entries 3→0、z/parent 正确、picker 1→0、`overlay-check PASS`（menu=(9,270,180.075)、retries=0）；exit 0 |
| `gallery --real-pixel-check` | **PASS 55 checks / failures=0 / 15.867 s**（修复后新增 dialog-scrim-theme/composite/alpha + scrim/panel fault 自检；证据 `target/real-pixels/windows/`，report `source_diff_sha=96742b4a…`、`last_frame=95`、`scrim_alpha=0.55`）；exit 0 |

**关于一次真实窗口 FAIL 的说明（非代码回归）**：复核期间 acceptance 侧同时在同机跑 `gallery` 各门禁（证据目录里有 17:48–17:54 的 `run.log`/`real-pixel-check.log`/`debug-stack-2.log`），我并行执行的 `--real-pixel-check` 出现 `baseline-stable differing=46 max_delta=29` + `PickerOpening` 20 s 超时（checks=11）。acceptance 独占窗口日志先有 **PASS 55 checks**；我随后**独占桌面重跑 PASS 55 / 15.87 s**，且该次运行已重写 `report.txt`（PASS）。结论：门禁对并发窗口敏感，非 P3 代码问题；建议 §14.4 补一句“`--real-pixel-check` 需独占桌面、不要与其他 gallery/always-on-top 窗口并行”（归 acceptance/P4 文档）。

## 3. r2 新发现（1 条 P3，非代码）

### P3K-R2-01 [P3] §14.3 仍留一处过期的 modal 判定描述

- §14.3（设计 `:1593`）：`**modal fade-only**：FadeScale 在 is_modal() || tier==Modal 时 scale 固定 1.0…`
- 实际（r2 修复后）：`PopupEntry::is_fade_only()` = `is_modal() || PopupTier::Modal || PopupTier::Scrim`（`popup.rs:572-574`）；D-P3-2 与 p3-summary 已按新口径写，只有 §14.3 这一句未同步。
- 影响：文档内部不一致，且旧句子恰好是 P3K-01 缺陷的成因描述（scrim 落在两个条件之外）；建议提交前把该句改为 `is_fade_only()（modal/Modal/Scrim）`。
- 阻塞否：不阻塞代码；仅文档一行。

### 其他 r2 观察（不计 finding）

1. **跨 lens（engine 卫生，P4）**：`settle.rs:418` 的 `set_popup_settle_rounds(outcome.rounds)`（r2 新增 `window/record.rs` 的 saturating_add API）在 paint 帧末尾会被 `frame.rs:260-294` 构造的 `FrameStats{popup_settle_rounds: settlement.rounds}` + `set_frame_stats` 整体覆盖 ⇒ 该累加在常规帧实际无效。当前无用户影响、无测试依赖，建议 engine lens 记为 P4 清理项（要么删 API，要么把统计改到 `set_frame_stats` 之后的累加路径）。
2. **提交形态**：`tools/` 四个脚本已 staged（`.sh` 模式 100755 ✓，`real-pixel-check-windows.ps1` 也已 staged）；`.docs` 仍被 `.gitignore` 的 `.*` 忽略（含本报告），如需归档 review/文档需 `git add -f`。
3. **r1 残余风险仍在**：workspace 测试未跑（`cargo check` 绿）；macOS/Ubuntu 真实像素证据为修复前 50 checks（§14.4 已标“需复跑”）；`--pixel-check` 仍只覆盖 ColorPicker、不采样动画帧（P3K-08 → P4）。

## 4. 测试真实性（r2 增量）

- 无放宽：r2 对被修改测试的删除行仅限“代码语义替换”（`A11yRole::Label→Group`、`close_with_reason` 预写 reason 删除、`Fade→FadeSlide`、`close→close_with_reason`、recenter 旧测试），未见断言被删除/弱化。
- 新增/强化断言均能证伪：scrim `paint_scale==1` + 移除；shadow 偏移“bug 公式必漂移 >2px”守卫 + GPU 实例；GlyphRun/Image 缩放渲染 rect；性能守卫改 **median ≤ idle p95+2ms** 且 open 帧断言 `popup_animation_records>0`；`paint_state` reuse 键含 pivot 的单测；Dropdown/Select slide 帧（`paint_offset=(0,4)`、无 scale、局部 damage、settle 归零）；refused-close reason 不泄漏。
- 逐文件 `#[test]` 计数与 §14.5 完全一致：menu 79 / menu_chain 13 / context_menu 21 / popup_panel 20 / dropdown 29 / select 34 / dialog 55 / tooltip 28 / alert 11 / color_picker 81；popup_behavior 123。

## 5. r2 结论

| 严重度 | 数量 | 说明 |
|---|---|---|
| P0 | 0 | — |
| P1 | 0 | — |
| P2 | **0** | r1 的 P3K-01（scrim）与 engine P3E-01（shadow）均已修复并有测试/探针复核 |
| P3 | **1** | P3K-R2-01：§14.3 一行过期 modal 判定描述（非代码） |

- **r1 → r2 关闭率：8/8**（6 修复 + 2 转 P4 登记；P3K-06 部分修复+余项登记）。
- **kit 是否可提交：可提交**（kit 侧代码 0 finding；测试 3872/878、clippy/fmt/workspace/pixel-check/debug-stack/real-pixel-check（Windows 55）全绿）。提交前建议只做两件非阻塞小事：(1) 改 §14.3 那一句为 `is_fade_only()`；(2) acceptance 侧在 §14.4 注明真实窗口门禁需独占桌面（并发会假红）。macOS/Ubuntu 真实像素需按 §14.4 用当前构建复跑（已登记）。
