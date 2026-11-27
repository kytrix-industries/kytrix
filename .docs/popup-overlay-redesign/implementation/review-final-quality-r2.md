# 最终对抗性审查 B r2：质量 / 完整性 / 测试 / 文档 / 规范

- 审查人：`final-review-quality`（独立子 agent，只读）
- 时间：2026-11-27 21:42–22:02（Asia/Shanghai）
- 对象：`E:\kytrix\.agents\worktrees\dsh-1d175414-1a41`，HEAD **`00927ff9`**（`docs(kit-gallery): P3 验收/真实窗口像素/动画回写`），其父 `5374c4c8`（`fix(gallery): real-pixel resize 断言平台无关`）；被测代码主体为 `5df859b8`（`fix(popup): 最终审查修复...`），基线 `fa41987c`
- 只读：未修改源码/提交；仅写 `E:\kytrix\target\final-review-quality-r2\` 与本报告。
- 说明：本 r2 主要复核 FQ-01…FQ-07；另发现 FQ-08/09/10/11。P4 已登记项不计入 finding 计数。
- 复核末工作树状态：`?? tools/source-id.txt`（内容 `revision=00927ff9`）；`.docs` 设计/implementation 仍未跟踪；`5df859b8` 之后的 `5374c4c8`（gallery resize 断言）与 `00927ff9`（kit-gallery docs）已提交。最终 Windows report 仍为 `source_revision=5df859b8`（见 FQ-01/FQ-11）。

---

## 1. 方法

1. `git diff fa41987c..5df859b8`（25 文件、+2848/−660）逐文件复核；重点：dismiss `Ignore→Consume`、非模态 Tab/Shift+Tab owner-scope 离开、`PopupState::Closing` 真实状态、API 变体删除、settings Alert `ok_action`、真实窗口交互门禁、per-run source-id。
2. 测试：`cargo test -p ui_core -p ui_kit`（canonical）、`cargo test --workspace`、`cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings`、`cargo +nightly fmt --all -- --check`、`cargo check --workspace --all-targets`；新增测试逐条读锚定断言。
3. 证据：`target/real-pixels/windows/20261127T134244Z-e69de29b/` 的 report/run.log/PNG 与交互 07–17 阶段；旧 mac/linux 证据；source-id。
4. 文档：FQ-02/04/05/06/07 回写状态；`.docs` gitignore/跟踪状态；`git status`。
5. AGENTS 新增行扫描（注释/allow/expect/path/依赖/panic/动态 JSON）。

---

## 2. FQ 复核状态

### FQ-01 [P3] 证据可对账 —— **Windows 106/0 PASS，但证据仍指向 `5df859b8`；final HEAD `00927ff9` 未重跑**

- Windows 新 per-run：`target/real-pixels/windows/20261127T134244Z-e69de29b/report.txt`
  - `source_revision=5df859b8`、`source_status=clean`、`source_diff_sha=e69de29bb2d1d6434b8b29ae775ad8c2e48c5391`（clean tree 的空 diff hash）、`source_identity=source-id-file`；
  - **`checks=106`、`failures=0`、`result=PASS`**、`last_frame=282`、`elapsed_ms=43111`；
  - `latest.txt` 指向该目录；`run.log` 记录 pixel-check 94 / real-pixel 106 / idle / theme / debug-stack（attempt1 timeout、attempt2 PASS）`gates_done fail=0`；PNG 07–17 为交互阶段。
- **未关闭**：最终 HEAD 已是 `00927ff9`（含 `5374c4c8` 的 gallery resize 断言修复与 `00927ff9` docs），`tools/source-id.txt` 也已重生成 `revision=00927ff9`；但 report 仍为 `source_revision=5df859b8`，文档 §14.4/14.5/p3-summary/kit-gallery 仍写 `5df859b8`。因此最终代码树（`00927ff9`）没有被真实窗口 report 直接覆盖；`5374c4c8` 只是验收断言放宽（产品运行时无变化），但按“final tree 证据”标准仍需重跑 106 checks 或明确说明该提交不影响验收。
- mac/Linux 本地 `macos-remote`/`linux-remote` 仍是旧 `f24a3441 dirty(30)`/`0a9a…`/55 checks；文档已改为“待 acceptance 复跑回填”，但三端 final 尚未成立。
- 备注：`e69de29` 是 git 空 blob 的 SHA-1（clean tree 的 diff hash），不是树哈希；身份以 `source_revision` 为准。

### FQ-02 [P3] pivot rationale —— **已关闭**

- design §14.6 D-P3-4 现为：“pivot 是纯绘制派生值，**但安全前提是同帧 tick 已 mark_paint**；缓存键含 pivot 保证不误用旧 pivot；settle 同步 pivot 不额外 mark_paint 以免 `place_subtree` 迁移 damage 并入同帧导致 `damage_full`”（`popup-overlay-redesign-design.md:1636`；§14.3 同步）；
- `p3-summary.md:34/89` 也已改为同一“安全前提”口径。旧“同步它不需要 mark_paint”的否证理由已删除。

### FQ-03 [P3] `PopupScale` Text 死分支 —— **已关闭**

- `crates/ui_core/src/paint/pass.rs:341` 原 `Primitive::Text(text) => Primitive::Text(text)` 删除，`PopupScale::primitive` 以 `_ => primitive` 兜底；`cargo test`/clippy/workspace check 均通过。P3K-06 不可达分支的 AGENTS 死代码问题关闭。

### FQ-04 [P3] §14.9/附录/DoD6 —— **请求项已关闭；文档 final 数字仍待迁移**

- §14.9 的“50”已改 55；附录 A“关键源码”已换为 `app/popup.rs`、`popup/*`、`kit/popup_panel|menu|menu_chain|...`、`a11y/*` 等新路径；DoD 6 已写真实窗口 `damage_full=unavailable`，由 headless `--pixel-check` 覆盖。
- 仍待回填：设计 §14.4 三端表/§14.5/p3-summary/kit-gallery 仍旧 55、3872、旧 source 0a9a（由 FQ-01 的 final 数据回填一并处理）；设计 §14.2 P3-6 行已先改为 Windows 106 done、mac/Linux 待复跑。

### FQ-05 [P3] 真实窗口交互全路径 —— **Windows 已关闭；mac/Linux 待复跑**

- 106 checks 覆盖：真实 pointer 点击打开（`interact-open-menu` 断言“指针点击打开，而非程序化 open_submenu”）、hover 级联、safe triangle 斜向、sibling 替换、Esc 逐层、outside click + Replay 视觉、Tab/方向键/Enter、scroll reanchor、resize reanchor、负向 self-check；report 全部 PASS。
- 交互驱动是 `gallery` 内 `App::dispatch_input_tracked_for_test` 注入 `InputEvent::pointer_*`/`key_*`（真实窗口内的引擎输入路径；非 OS 级 SendInput）。
- 限制（报告自述，属 P4/文档精度）：`resize` 阶段宿主不把内部逻辑尺寸同步到原生窗口，只断言 popup rect 重锚定，不截 resize 像素；`resize_note` 已明示。mac/Linux 尚未跑 106 checks。

### FQ-06 [P3] P2 遗留 P3 登记 —— **已关闭**

- 设计 §14.7 新增：R2-e（1×1 vs 0-size point anchor）、R2-f（子菜单句柄回调仅 root）、外部滚动语义（`window_scroll_epoch` 对任意非 popup 子树）、reconcile Closing churn，均为 P4 文档/行为/状态机口径，并注明无用户可见缺陷；§4.2/§4.7/§5.2 对应措辞已改（如 `WindowRect` 注释、外部滚动、root-only 回调）。

### FQ-07 [P3] `.docs` 入库/source-id —— **口径已写，执行未完成**

- 设计 §14.10 已写仓库约定：`.gitignore:3:.*` 忽略 `.docs/`；P3 提交由 lead `git add -f` 设计文档 + `implementation/`；`kit-gallery.md` 正常 add；source-id 建议忽略或提交前删除。
- 当前实际：`git ls-files .docs` 仍只有 `kit-gallery.md`/`kit-text.md`/`text-input-design.md`；popup 设计/`implementation/` 未跟踪；`git status` 为 `?? tools/source-id.txt`（未干净）。执行 lead 描述的 add -f + source-id 处理后关闭。

---

## 3. 新 Findings（r2）

### FQ-08 [P3] 新 backward-Tab 测试只做否定断言，未证明落点

- `crates/ui_core/tests/popup_behavior.rs` 新增 `a_non_modal_take_popup_shift_tab_escapes_backwards`（约 6215+）：
  - 建了 `first`/`second` 主树按钮、把焦点放 `first`，Take popup 内焦点为 `a`；`focus_prev` 后仅 `assert_ne!(focused, Some(a))` + `popup_state==Open`；
  - `second` 只被 `let _ = second;`，没有断言落点（可以是 `None`、错误背景节点或任意节点，测试仍通过）。
- 同一提交的 forward 测试 `a_non_modal_take_popup_tabs_out_to_the_main_tree_instead_of_wrapping` 对落点有精确断言（`Some(second)`）；backward 测试应同样断言 `Some(first)`（或明确目标），否则“Shift+Tab 离开 owner scope”只验证了“没有停在原节点”。
- 建议补 `assert_eq!(app.app().focused(window), Some(first))`（若语义是回到打开前焦点）并移除 `let _ = second;`。

### FQ-10 [P3] 审查期间未提交的 `gallery.rs` resize 断言改动 —— **已提交（`5374c4c8`），但触发 FQ-11**

- 复核中曾观察到 `M crates/ui_kit/examples/gallery.rs`（6 行 `resize-reanchor` 断言放宽：允许 x 或 y 移动 ≥40、原点非负，改错误文案）；该改动已提交 **`5374c4c8`**（`fix(gallery): real-pixel resize 断言平台无关`），不是产品运行时代码，属验收判据修复。
- 提交后 HEAD 变为 `5374c4c8`，随后 `00927ff9` docs；Windows 106 report 仍是 `5df859b8`，见 FQ-01/FQ-11。

### FQ-11 [P3] 最终提交历史/source/证据三处不一致（`5df859b8` vs `5374c4c8` vs `00927ff9`）

- 实际 HEAD：`00927ff9`（docs）← `5374c4c8`（gallery resize 断言）← `5df859b8`（最终审查修复）。
- 但文档仍写：
  - `p3-summary.md:3`：“P3 已分两次提交（`fa41987c`、`5df859b8`，最终 HEAD clean）”；`:10` diff 口径 `f24a3441..5df859b8`；
  - 设计 §14.1/§14.4/§14.5、kit-gallery 仍写 Windows 证据 HEAD `5df859b8`；
  - Windows report 实际 `source_revision=5df859b8`，source-id 文件已是 `00927ff9`。
- 影响：最终交付的“source identity / commit 历史 / 三端证据”不是同一棵树；`5374c4c8` 只改验收断言，功能风险低，但按本项目的 source-id 严格口径必须重跑 Windows（或明确注记该 commit 不改变被测行为），并将 docs/p3-summary 的 final HEAD/diff 改为 `00927ff9`（或最新提交）。
- 建议：在最终 HEAD 上重新生成 `tools/source-id.txt`、重跑 `--real-pixel-check`（至少 Windows）与 `--pixel-check`/`--debug-stack`，更新 report 的 `source_revision` 与 docs 的 HEAD/diff/三端表；`p3-summary` 的“两次提交/最终 HEAD”改为实际提交链。

### FQ-09 [P3] workspace 测试数与 commit 声称不一致

- 实测 `cargo test --workspace`：**103 suites / 9334 passed / 0 failed / 30 ignored**，exit 0；其中 `ui_core` lib 自 spawn 的 `1 passed; 2808 filtered out` 是重复子进程结果，去重后 **9333 passed**。
- 提交说明写 “workspace `cargo test` = 9326 passed”；9326 未能由当前 HEAD 复现（差 7–8，且 103 binaries 与实测 suites 数一致）。`cargo test -p ui_core -p ui_kit` canonical 为 3881 raw/3880 unique（ui_core lib 2802、popup_behavior 124、ui_kit 889、demo 39），与提交说明的 3880 一致。
- 影响：仅数字/记录不一致，代码测试全绿；建议在最终文档/提交说明改为实测 9333（或注明 9334 raw 含 1 个自 spawn 重复），并同步 docs 3872→3880、55→106（Windows）。

---

## 4. 门禁实测

| 命令 | 结果 |
|---|---|
| `cargo test -p ui_core -p ui_kit` | **3880 unique passed / 2 ignored**（raw 3881 含 1 个自 spawn；ui_core lib 2802+1 ignored、frame_record 8、gui_smoke 0+1 ignored、layout 4、popup_behavior 124、snapshot 14、demo 39、ui_kit 889）；exit 0 |
| `cargo test --workspace` | **9334 raw / 9333 unique passed / 30 ignored**，103 suites，exit 0 |
| `cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings` | exit 0 |
| `cargo +nightly fmt --all -- --check` | exit 0 |
| `cargo check --workspace --all-targets` | exit 0 |
| `gallery --pixel-check` / `--debug-stack` / `--real-pixel-check` | 证据日志：94 / PASS / **106 checks PASS**（Windows clean HEAD） |
| `gallery --real-pixel-check` 三端 | Windows 106/0 PASS；mac/Linux 待 final |

- 首次 canonical ui_core+ui_kit 在 21:43 因并发 acceptance 持有 `target/debug/examples/gallery.exe` 出现 **LNK1104**（非代码失败）；acceptance 结束后重跑 exit 0。
- `tools/source-id.txt` 当前存在且未跟踪；`git status` 不干净（见 FQ-07）。

## 5. AGENTS / 代码质量复核

- 新增 2,777 行扫描：注释 0（`//` 命中仅 `sed 's/^diff_sha=//p'` 的脚本模式，不是代码注释）；`#[allow]/#[expect]/#[path]` 0；`serde_json` 动态/`rename_all`/`deny_unknown_fields`/RawValue 0；`unsafe`/`todo!`/`unimplemented!`/`dbg!` 0；新增 `format!` 仅在 `gallery` 示例（临时路径）。
- FQ-03 后 `PopupScale::primitive` 仅穷尽可缩放图元 + `_ => primitive`，无 Text 专属死代码；clippy `-D warnings` 覆盖新增 25 文件。
- 新增测试无 `#[ignore]`/`#[allow]`；P3 修复测试锚点（shadow 向量、scrim identity、idle 0 工作、frame budget、pivot 缓存键）未放宽；新增 FI/FA 测试大多有精确断言（例外见 FQ-08）。
- 新的 `PopupState::Closing` 真实状态、`Ignore→Consume`、非模态 owner-scope Tab、API 变体删除均有对应测试；`settings` Alert ok_action 有宿主断言（ok 关闭后 popup_total=0/title 不残留）。
- 基线已存在（非本轮引入）的 `#[allow(clippy::too_many_arguments)]` 仍见 `gallery.rs:2934`、`widgets/volume_heatmap/src/view.rs:576`；如按 AGENTS §4 对 touched 文件严格执行需另行处理（P4/历史债）。

## 6. 文档一致性 / P4

- FQ-02/04/06 的文档改动已落；设计 §14.2 P3-6 已改为 Windows 106 done、mac/Linux 待复跑。
- 仍待 final 回填：设计 §14.4/14.5/14.9、p3-summary、kit-gallery 的 55/3872/0a9a 与三端 final 数字；当前设计/`p3-summary` mtime 21:56 已在更新中。
- P4 列表 §14.7 的 P3R2-02/03、FQ 项与 R2-a..f 已纳入/处置；新增 P4 观察：`resize` 阶段不截原生窗口、source-id clean-tree diff_sha 为空 blob、workspace 计数口径（9333 vs 9326）。

## 7. 结论与计数

- **P0 = 0，P1 = 0，P2 = 0**。
- **P3 = 5 pending/open**：
  1. **FQ-01**：mac/Linux final 106 checks、Windows report 仍为 `5df859b8`（final HEAD `00927ff9` 未重跑），三端 final 数据待回填。
  2. **FQ-07**：设计/implementation 尚未 `git add -f`，`tools/source-id.txt` 仍 `??`；设计 §14.10 已写执行口径但未执行。
  3. **FQ-08**：backward-Tab 新测试只做否定断言（应断言 `Some(first)`，并移除 `let _ = second;`）。
  4. **FQ-09**：workspace 测试数 commit 9326 与实测 9333 unique（9334 raw）不一致；测试全绿，仅数字/记录。
  5. **FQ-11**：final HEAD `00927ff9` / 提交链 / source-id / Windows 106 report（5df859b8）/ docs 的 HEAD 不一致；需在最终 HEAD 重跑或明确注记。
- FQ-02/03/04/05（Windows）/06 已关闭；FQ-10 已由 `5374c4c8` 提交（其引起的 FQ-11 单列）；FQ-05 的 mac/Linux 待跑并入 FQ-01。
- **是否可宣布完成（P0–P3=0）：暂否。** 代码/测试/Windows 证据已实质通过；剩余为三端 final + 最终 HEAD 证据重建 + 文档回填 + `.docs`/source-id 收口 + FQ-08/09 两条记录小修。完成上述并复跑后，P3 可清零。
