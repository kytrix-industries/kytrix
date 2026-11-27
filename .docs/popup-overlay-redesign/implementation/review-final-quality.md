# 最终对抗性审查 B：质量 / 完整性 / 测试 / 文档 / 规范（final-review-quality）

- 审查人：`final-review-quality`（独立子 agent，只读）
- 时间：2026-11-27 18:55–19:35（Asia/Shanghai）
- 对象：worktree `E:\kytrix\.agents\worktrees\dsh-1d175414-1a41`，HEAD `fa41987c`；提交 `d9949aca`(P0)、`152616b1`(P1)、`f24a3441`(P2)、`fa41987c`(P3)；基线 `71a5a5cc`
- 只读约束：未修改任何仓库源码/提交；仅写入本报告与 `E:\kytrix\target\final-review-quality\` 下的日志。结束时 `git status --porcelain` 为空。
- 本报告的 P4 项按用户要求不计入 finding 计数；P4 真实性单列。

---

## 1. 范围与方法

1. 逐提交审 `git diff`（175 文件、+40445/−10598；P3 代码 diff 24 文件 +4405/−131、tools 5 脚本 +353，均与文档一致）。
2. AGENTS 全条扫描（新增行与整文件）：注释、`#[allow]/#[expect]/#[path]`、死代码、依赖、serde、SmolStr、集合、错误类型、模块布局。
3. 测试真实性：逐文件 `#[test]` 计数、跨提交计数差、删除测试及其覆盖去向、空/恒真断言扫描、P3 修复测试负向判别力抽查。
4. 文档核对：设计 §10 DoD 逐条、§11–§14、`p2/p3-summary`、`kit-gallery.md` §14、P4 列表。
5. 门禁实测：`cargo test -p ui_core -p ui_kit`、`cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings`、`cargo +nightly fmt --all -- --check`、`cargo check --workspace --all-targets`、`gallery --pixel-check`；性能单测 `--nocapture`。
6. 证据抽查：`target/real-pixels/{windows,macos-remote,linux-remote}` 的 report/run.log/gate 日志/PNG 时间戳；源码/证据哈希；三端 source identity 与提交树的可复现性。
7. 本机 `git status`、tools 文件模式、提交文件清单、`.docs` gitignore 行为。

日志：`E:\kytrix\target\final-review-quality\{cargo-test,pixel-check,perf-test,fmt,clippy,workspace-check}.log`。

---

## 2. Findings

> 计数：**P0 = 0，P1 = 0，P2 = 0，P3 = 7**。P4 已登记项不计（见 §8）。
> P2 为 0 的理由：P3 acceptance r2 的 R2-01（P2）在本轮复核中已实质降级——代码树身份可证为最终代码（见 FQ-01），且本轮观察到一次 clean-tree 独立复跑 55/0 PASS；剩余为文档/证据元数据 P3，不是功能或验收失败。

### FQ-01 [P3] 三端证据的文档数字/source identity 与提交树不可对账，且证据目录可被重跑覆盖（R2-01 残余）

**证据**

- 设计/总结/kit-gallery 写 Windows“report `elapsed_ms=16004`、frame 95”，并写“P3 最终快照 HEAD `f24a3441` + dirty(30 files)，`diff_sha=0a9a218b…`，以 `report.txt` 的 source 字段为准”（`popup-overlay-redesign-design.md:1601`、`:1615`、`:1627`，`p3-summary.md:55`、`:63`、`:76`，`kit-gallery.md:762`、`:767`）。
- 提交时的 Windows `report.txt`（mtime **18:54:17**，commit **18:54:41**）实际为 `last_frame=94 elapsed_ms=15622`；文档数字来自同期 `run.log` 的脚本 run（18:38:54，`elapsed=16005 frame=95`），不是 report。之后本审查期间（**19:10:46**）report/PNG 又被另一并发复跑覆盖为 `source_revision=fa41987c, source_status=clean, source_identity=git, last_frame=94 elapsed_ms=15871, 55/0 PASS`——说明 `target/real-pixels/windows` 不是不可变归档。
- `report.txt` 的 `source_diff_sha=0a9a218b…` 无法由提交树复现：按 `tools/record-source-id.ps1` 同一算法 `git diff HEAD | git hash-object --stdin`，对 `git diff f24a3441 fa41987c` 计算为 **`c8143cfd3e87c7a14914d3264b1e5a5ad53f3e1d`**（代码+tools 子集为 `fe9ef417…`），均不等于 `0a9a218b…`。
- 原因已定位：source-id 生成时点约 18:38:35；所有 `crates/`+`tools/` tracked 文件 mtime ≤ 18:38:19（最后是 `gallery.rs` 18:38:19）；**唯一在 source-id 之后修改的 tracked 文件是 `.docs/gui-migration/spec-deltas/kit-gallery.md`（18:50:20）**。因此 0a9a 对的是“最终代码 + 旧版 kit-gallery 文档”的树，不是提交树；代码身份实际是最终的，但文档声称“最终树/以 report 为准”不可对账。
- mac/linux 本地 report 仍为 `f24a3441 dirty(30)`/`0a9a…`；Windows 已被本审查期间的复跑改成 clean HEAD。三端证据不再处于同一快照。

**影响**：代码/门禁/像素结果本身成立（三端 55/0，且本轮 clean-tree 复跑 55/0），但审计严格性不成立：文档引用的耗时/帧号不是提交时 report 的值，`diff_sha` 不可复现，归档目录可被任意复跑覆盖（P3A-03/P4 已登记“acceptance 目录归档未落地”，但本轮显式观察到覆盖发生）。

**建议（不改代码）**：§14.4/§14.9/kit-gallery §14 改为“report 数字随 run 浮动；P3 提交时 report=15622ms/frame94，脚本 run.log=16005ms/frame95；后续独立复跑见证据目录时间戳”，并说明 source-id 0a9a 对应“代码最终 + 旧 kit-gallery 文档”（或按代码/tools 子集重新生成 source-id 并注记）；把 final report/PNG 复制到不可变 acceptance 目录（P4 P3A-03）。

### FQ-02 [P3] P3R2-01 未关闭：`D-P3-4` 仍用已被否证的旧 rationale（设计 + p3-summary 两处）

**证据**

- 设计 §14.3 与 D-P3-4 仍写：“`settle` 同步 pivot 不 `mark_paint`：**pivot 是纯绘制派生值，同步它不需要 mark_paint**；缓存键含 pivot 保证不误用旧 pivot”（`popup-overlay-redesign-design.md:1590`、`:1638`）；`p3-summary.md:34`、`:89` 同义。
- 引擎 r2 已明确该理由错误：pivot 变化**会**改变 scale≠1 时的绘制位置；不 mark_paint 之所以安全，是因为动画期间 `animation::tick` 每帧已 `mark_paint`，且 `paint_state` 复用键已加入 pivot（P3E-09）。`review-p3-engine-r2.md:47-53`（P3R2-01）要求改这一句；设计 §14.6 应写成“安全前提 = 同帧 tick 已 mark_paint + 缓存键含 pivot”，而最终文档仍是旧句。

**影响**：同一决策在交付文档中保留了已被审查否证的理由；后续复核会重复发现，且掩盖“依赖时序前提”的真实契约。

**建议**：把设计 §14.3/§14.6 D-P3-4 与 p3-summary §7 改为引擎 r2 给出的正确口径（1–2 句，无代码改动）。

### FQ-03 [P3] P3R2-02 未关闭：`PopupScale::primitive` 的 `Primitive::Text` 分支是新增死代码

**证据**

- `crates/ui_core/src/paint/pass.rs:341`：`Primitive::Text(text) => Primitive::Text(text)`（P3 新增；P2 基线无此分支）。
- `resolved_canvas`（`pass.rs:440-469`）在 `emit_content` 把 `Primitive::Canvas` push 进 scene 之前递归把 `Text` 解析成 `GlyphRun`（`pass.rs:2003-2019`）；解析失败时 Text 被丢弃（`skipped`），不会进入 lane。`PopupScale::primitive` 只作用于已解析的 lane/Canvas，因此该分支不可达。
- 引擎 r2 已将此列为 P3R2-02（`review-p3-engine-r2.md:55-60`）并建议删除；最终设计 §14.3 却写“`Primitive::Text` 现为恒等直通（P3K-06 closed）”，但代码保留不可达分支，违反 AGENTS §1.2/§6“删除无实际需求代码”。

**影响**：不造成功能错误，但 P3K-06 的“closed”缺少证据；死分支也是 AGENTS 规范明确禁止的。

**建议**：删除该 match 分支，并在 §14.3 写“Text 在进入 lane 前已解析，P3K-06 不可达/已结案”；或补一条证明分支可达的测试（当前不存在）。

### FQ-04 [P3] 最终设计文档仍有残留不一致：§14.9 的 50 vs 55、附录 A 的已删源码、DoD 6 的 real-window `damage_full`

**证据**

1. `popup-overlay-redesign-design.md:1678` 写“三端断言数量相同（**50**）”，而三端 report/§14.4/§14.9 其它处均为 **55**（`55 = 50 + 5 个 P3A-04 scrim 检查` 量级；实际三端 report 均 `checks=55`）。
2. 附录 A“关键源码”仍列 **已删除文件**：`app/overlay.rs`、`interaction/overlay.rs`、`kit/{overlay,layer}.rs`（P1-11 已删；`Test-Path` 均为 false），与 DoD 2/§12.9“旧 API 零引用”矛盾（`popup-overlay-redesign-design.md:1693`）。
3. DoD 6 / §10 写“真实窗口验收（含截图/`damage_full`/像素判定）”，但真实窗口 report 明确 `damage_full=unavailable(...)`（仅 headless `--pixel-check` 断言 `damage_full_frames=0`）；§14.4 已说明，DoD 6 未同步措辞。

**影响**：交付文档内部/与代码不一致；不改变功能验收，但属于“文档 DoD 全部达成”声明不严谨。

**建议**：50→55；附录 A 改为新源码（`app/popup.rs`、`popup/*`、`kit/{popup_panel,menu,menu_chain,...}.rs`）；DoD 6 改为“真实窗口截图/像素；`damage_full` 由 headless gate 覆盖”。

### FQ-05 [P3] DoD 3 / §6 P2 验收的“真实窗口 hover/斜向/sibling/Esc/外部点击/键盘/滚动全路径”没有证据

**证据**

- 设计 §6 P2 验收（`popup-overlay-redesign-design.md:807`）要求“3 级在真实窗口完成 hover/斜向/sibling/Esc/外部点击/键盘/滚动全路径”；DoD 3（`:956`）写“P2：2–3 级链与真实窗口路径已达成”。
- 证据面：`--real-pixel-check` 的 3 级菜单是**程序化**打开——`gallery.rs` 用 `MenuChain::open(app, window, anchor)` + `open_submenu(...)`（约 435–452），不注入真实 pointer/key 事件；`--debug-stack` 的 chain 阶段同样是程序化 open/close（gallery.rs §overlay-check）。真实窗口只验证了 stack/rect/像素，不验证 hover 级联、safe triangle、sibling 替换、Esc/外部点击/键盘路径。
- `review-p2-quality.md` §8.2（第 194 行）已明确记录该残余“真实窗口交互验收未执行”；最终 P3 未补该路径（P3K-08/P4 也明确像素门禁只验稳定态）。

**影响**：DoD 3 的“真实窗口可用”缺少验收证据；headless 测试覆盖很强，但“真实窗口全路径”这一条仍是声明大于证据。

**建议**：要么补一份真实窗口手动/自动交互验收记录（hover/斜向/Esc/sibling/键盘），要么把 §6/§10/§13.2 的措辞降为“headless 全路径测试 + 真实窗口程序化 3 级链/像素”，并登记为 P4 验收项。

### FQ-06 [P3] P2 遗留的 P3 观察仍有未登记/未修正项（R2-e、R2-f、外部滚动语义、reconcile Closing）

**证据**

- **R2-e**：设计 §4.2 注释写 `WindowRect = 指针点扩展的 1×1 rect`（`popup-overlay-redesign-design.md:330`），实现 `ContextMenu::open_at` 用 `LogicalRect::from_origin_size(point, LogicalSize::ZERO)`（`context_menu.rs:173`）；`review-p2-quality-r2.md` §3.2-6 已标为 P3 未登记。
- **R2-f**：`open_submenu` 句柄上 `on_activate/on_close` 仅 root 生效、submenu 上静默无效，设计 §5.2/§13.3 与新 API 表未注明（`review-p2-quality-r2.md` §3.2-5）。宿主误用会静默无效。
- **外部滚动语义**：P1 §12.6-13/P2 §7 已登记 P3“`window_scroll_epoch` 对任意非 popup 子树滚动 +1，宽于设计 §4.7 的‘祖先滚动’字面”；最终设计 §14.7 未列入 P4，§4.7 措辞也未改。若接受“外部滚动（含无关滚动容器）关闭菜单”，应改措辞并登记；否则是行为偏差。
- **reconcile Closing churn**：`review-p2-quality-r2.md` §3.2-4 记录 pointer-close 标 `Closing` 后下一帧在 panel 仍 Open 时立即 `reopen`，`Closing` 几乎只存活一帧；无用户可见但状态机冗余，未登记。

**影响**：P2 审查明确说“§3.2 的 P3 观察建议登记”；最终 `p2-summary §7`/设计 §13.7 只登记了部分，R2-e/R2-f/外部滚动/Closing churn 仍无 P3/P4 去向，违反“无未登记 P0–P3”。

**建议**：在 §13.7/§14.7 登记为 P4 或修正文案；R2-e 二选一（把实现改 1×1 或把设计注释改 point anchor），R2-f 在 §5.2/§13.3 注明 root-only 回调，外部滚动措辞改“外部（含无关容器）滚动”，Closing churn 登记 P4 状态机清理。

### FQ-07 [P3] 设计/实施/审查记录全在 `.docs`（被 `.*` 忽略），未 force-add，也没有“为何不入库”的明确说明

**证据**

- `git check-ignore -v .docs/popup-overlay-redesign-design.md` → `.gitignore:3:.*`；`git ls-files .docs` 只有 `kit-gallery.md`、`kit-text.md`、`text-input-redesign-design.md`。
- 4 个提交只带 `.docs/gui-migration/spec-deltas/kit-gallery.md`（每个提交 1 个 docs 文件）；设计文档（268 KB）、`implementation/`（p0/p1/p2/p3-summary + 全部 review 报告）均只在工作树。提交说明反复引用 `implementation/review-p0-disposition.md`、`implementation/p1-summary.md` 等，clone 后不可见。
- 对照：text-input 设计文档在 `b1e01c79`（2026-11-25）被 tracked，`71a5a5cc` 仍在更新它；说明本仓有“设计文档入库”的先例。设计 §12.1 只解释“kit-gallery 是跟踪文件、不受 .docs 忽略规则约束”，没有解释设计文档/实施记录为何不跟踪；没有 `git add -f` 的说明。
- `tools/source-id.txt` 同理是生成物（见 §8 P4 观察），未被 `.gitignore` 忽略（运行 `record-source-id.ps1` 后 `git status` 会出现未跟踪文件）。

**影响**：交付审计链在仓库内不完整；“4 个提交内容完整/无遗漏未跟踪”无法对 clone 成立。若 `.docs` 是用户约定的工作区目录，则至少需要一句明确说明。

**建议**：`git add -f` 设计文档 + `implementation/`（或至少 summary/review），或在该提交说明/§14 写“`.docs` 为工作区文档，不入库；kit-gallery 为 tracked 例外”，并给 `tools/source-id.txt` 加 ignore。

---

## 3. AGENTS 对照（新增行 + 整文件）

| 项 | 结果 | 证据 |
|---|---|---|
| 生产代码注释（行/块/文档注释、注释掉的代码） | **0 新增** | 38,233 新增行中 `//`、`/*`、`*/` 均 0（含测试；整文件复核未见新注释） |
| `#[allow]/#[expect]/#[path]` | **0 新增** | 新增行 0；整文件仅 2 处**基线已存在**的 `#[allow(clippy::too_many_arguments)]`：`ui_kit/examples/gallery.rs:2934`、`widgets/volume_heatmap/src/view.rs:576`（`git show 71a5a5cc:...` 同样存在，非本轮引入；见 §9 残余） |
| 新依赖 | **无** | `Cargo.toml` 仅 `ui_core` 新增 `[[test]] popup_behavior`（required-features=test-support）；`Cargo.lock` 无 diff |
| 死代码 | **FQ-03 1 处**（`PopupScale` Text 分支）；其余 clippy `-D warnings` PASS | `paint/pass.rs:341`；`cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings` exit 0 |
| 强类型错误 | 合规 | `PopupError` 13 变体；`EngineError::PixelHarness` 强类型；新增公共 API 变更型返回 `Result/Option`；无新增字符串错误 |
| `unwrap/expect`（生产新代码） | 新增 `expect(` 均在测试/测试支持/示例（1733 处含大量既有测试）；抽查新增生产文件无新增 `unwrap/expect` | 新增行 `unwrap(`=0；`expect(` 位于 `popup_behavior`/`popup/tests`/`test_support`/组件测试模块 |
| `panic!`（生产新代码） | 15 处新增均在测试/测试支持/示例（`paint/pass.rs` 测试模块、`render/compose.rs` 测试、`test_support/*`、`popup_behavior`、`kit/{menu,color_picker}` 测试） | 逐文件定位；无生产路径新增 panic |
| serde 动态 JSON / `rename_all` / `deny_unknown_fields` / RawValue | 0 新增 | 扫描新增行 |
| SmolStr | 新 popup 类型（`PopupA11y.label`、`MenuChain` typeahead、`PopupPanel` key/labels）均 `SmolStr`/`format_smolstr!`；`popup_panel.rs:789` 的 measure 文本用 `String::with_capacity` 拼接后 `SmolStr::new`，与 §2.2“拼接优先 format_smolstr”有轻微偏差（P1 已存在，非本轮；观察项） | 新增 src 文件字符串扫描 |
| 集合/预分配 | 无新增 `HashMap/HashSet`；新增 `Vec/SmallVec` 多处带 capacity；无新 ahash 需求 | 新增行扫描 |
| 模块布局 | 合规：`ui_core/src/popup.rs` + `popup/{anchor,animation,dismiss,focus,placement,settle,stack,tests}.rs`、`a11y/popup.rs`、`app/popup.rs`、`kit/popup_panel.rs`/`menu_chain.rs`；无 `#[path]` | `git ls-files`/编译 |
| 测试代码特殊规则 | 新增测试无 `#[allow]/#[expect]/#[ignore]`；测试内注释 0；无 `#[should_panic]` 新增 | 新增行扫描；`#[ignore]` 全仓仍仅 2 处既有（`gui_smoke.rs`、`dark_mode.rs`） |

clippy/fmt/workspace check 实测：`FMT_EXIT=0`、`CLIPPY_EXIT=0`、`CHECK_EXIT=0`（`target/final-review-quality/gates.log`）。

---

## 4. 测试统计与真实性

### 4.1 计数（本机复跑，HEAD `fa41987c`）

| 套件 | 结果 |
|---|---|
| ui_core lib | 2806 passed / 1 ignored |
| frame_record | 8 passed |
| gui_smoke | 0 passed / 1 ignored |
| layout | 4 passed |
| popup_behavior | 123 passed |
| snapshot | 14 passed |
| demo example | 39 passed |
| ui_kit lib | 878 passed |
| **合计** | **3872 passed / 0 failed / 2 ignored**；与设计 §14.5/p3-summary 一致 |

- 日志中另有一条 `1 passed; 2806 filtered out` 是测试进程自 spawn 的子测试二进制输出（非重复计数）；按套件去重后为 3872。
- 逐文件 `#[test]` 计数与文档逐项一致：`popup/tests 43`、`popup/placement 34`、`popup_behavior 123`、`a11y/popup_tests 22`、`menu 79`、`menu_chain 13`、`context_menu 21`、`popup_panel 20`、`dropdown 29`、`select 34`、`dialog 55`、`tooltip 28`、`alert 11`、`color_picker 81`。
- 跨提交计数：P1 新增 popup 套件 43/110/21，P2 后 111/22、menu 76/menu_chain 13/context 20；P3 后 123/22、menu 79/context 21/popup_panel 20/dropdown 29/select 34/dialog 55/color_picker 81，与 `+67`/`+25` 的文档增量一致。

### 4.2 真实性抽查

- 新增行无 `assert!(true)`/`assert_eq!(true,true)`/`|| true`/`if false`/空测试；无新增 `#[ignore]`。
- P3 修复测试判别力（逐条读码 + 引擎 r2 复核）：
  - `fade_scale_scales_the_shadow_offset_as_a_vector`：primitive rect + GPU instance bounds 双层断言，含“旧 buggy 公式 >2px”回归守卫（`popup_behavior.rs:5947-6048`）。
  - `fade_scale_modal_scrim_is_fade_only`：open/close 两阶段、modal+scrim 两对象均断言 `paint_scale==1.0`，结束后 scrim 移除（`:6051-6111`）。
  - `fade_scale_scales_glyph_runs_and_images_in_animation_frames`：与 `PopupAnimation::None` 对照取未缩放 bounds，再比较 scaled rect（`:6155+`）。
  - `idle_frames_do_no_popup_settle_or_damage_work`：8 帧断言 `!rendered`/damage 空/`popup_animation_records==0`/open/close false/settle 0/unsettled false；`popup_open_and_close_frames_stay_within_the_frame_budget`：open/close 中位帧 ≤ idle p95+2ms，open 帧断言动画记录 >0（`:5780-5887`）。
  - `a_refused_close_does_not_leak_its_reason_into_a_later_close`（popup_panel）：dismiss Ignore 后 close_now 必须收到 Programmatic，旧实现会先写 reason ⇒ 必红（kit r2 复核）。
  - `the_paint_reuse_key_includes_the_scale_pivot`：同 factor 不同 pivot 必须 `!reusable`；旧键只因子 ⇒ 必红。
- P2 task-62 的恒真断言已删：当前 `menu.rs` 无 `assert_ne!(level2.id, root_entry.id)`（原 `menu.rs:4484`）；`derived_safe_triangle` 隔离、failed-open `panel.open` 分支、ContextMenu `open_on` 错误/重复绑定/`on_item_click` check 均有新测试（context_menu.rs:1055/1103/1142）。
- P1 删除测试有映射：`p1-cleanup-w7.md` §3 给出 `interaction_tests` 6 改写 + 9 删除逐条覆盖去向、a11y/focus/hud 迁移；本审查按名字复核未见“无去向”的静默删除。
- 负向自检：`--pixel-check` 94 checks PASS（`damage_full_frames=0`、首可见帧==稳定帧）；`--real-pixel-check` report 含 `self-check-offset`/`self-check-recolor`/scrim/panel fault，均 PASS。
- 残余：E-12 的变异自检 harness、pixel 仅 ColorPicker 等仍为 P4（已登记）；P3 median 守卫（p95 仅打印）是 P3E-03 已接受口径，非新 finding。

---

## 5. 文档核对（§10–§14、summary、kit-gallery）

- 规模/文件数/测试数/P3 新增数/`gallery +2490/−4`/三端 55/`pixel-check 94`：与实测一致（P3 numstat 24+4405/−131、docs 27/−1、tools 353；P2 15+6270/−1475、P1 129+24005/−9410 均与 summary 拆分口径一致）。
- §11–§14 的 P0/P1/P2/P3 收口、D-P1/D-P2/D-P3 偏差登记、P4 列表总体齐全；kit-gallery §13/§14 已同步新命令/94/55/fresh sway/trap/独占桌面，R2-02 的旧 P3A 文案已修。
- 不一致见 FQ-01（证据数字/身份）、FQ-04（50、附录 A、DoD6）、FQ-05（DoD3 真实窗口交互）、FQ-06（未登记 P3 观察）、FQ-07（`.docs` 未入库）。
- DoD §10 逐条：1 基本达成（headless + 真实窗口稳定态；动画帧不采样为已登记 P4 P3K-08）；2 达成（旧 API 代码零引用，`git grep` 仅在历史设计文档/AGENTS/改名提示出现）；3 见 FQ-05（声明大于证据）；4 达成；5 达成（3872、clippy/fmt/check、性能实测）；6 达成（三端 55/0，但证据元数据见 FQ-01）；7 见 FQ-07（设计/实施记录未入库）。

---

## 6. 提交清洁

- 4 提交线性；`git status --porcelain` 为空；`git diff --check` 无 whitespace error。
- 文件清单：P0 59 文件、P1 130、P2 16、P3 30；无 `.log/.png/.tmp/target` 临时文件。`tools/` 5 脚本模式：3×100755（mac/linux/macos-runner）+2×100644（windows.ps1/record-source-id.ps1），与 p3-summary 一致；无意外可执行位。
- 关键新文件全部已跟踪：`ui_core/src/popup*`、`a11y/popup*`、`app/popup.rs`、`ui_kit/src/kit/{popup_panel,menu_chain}.rs`、`ui_core/tests/popup_behavior.rs`；P0 `paint/extent.rs` 已跟踪。
- `.docs` 被 `.gitignore:3:.*` 忽略：设计文档、`implementation/*` 未跟踪；只有 `kit-gallery.md`（及历史 `kit-text.md`、`text-input-design.md`）tracked。见 FQ-07。
- `tools/source-id.txt` 当前不存在（故 status 干净），但 `.gitignore` 未忽略它；按文档“提交前重生成”执行会重新弄脏 status（P4 观察，见 §8）。

---

## 7. 性能 / 内存观察

- 实测（本机 Windows 11 / D3D12 WARP，HEAD `fa41987c`，`--nocapture`）：`idle_p95=800ns；open_median=32.1µs，open_p95=1.0925ms；close_median=25.5µs，close_p95=134.8µs`；与 p3-summary §4 的 29.1/26/137.8µs、1.1283ms 同量级，属 run 间浮动。
- 守卫：median ≤ idle p95+2ms（测试通过；余量约 60×）；p95 仅记录——P3E-03/obs3 已接受；没有当前回归证据。设计 §6 P3-5 的“帧 ≤ idle p95+2ms”措辞与 median 守卫的歧义属既有观察，非新 P3。
- 空闲：`idle_frames...` 断言 8 帧 0 rendered/0 damage/0 popup work；`--idle-check` 三端 0 帧；`--pixel-check damage_full_frames=0`。
- `FrameStats.frame_cpu` 覆盖 `App::frame` 全程（含 popup tick/paint），idle 帧也写入 `Some`；`popup_opening/closing/animation_records` 由 tick 真实回填（不是恒 false/default），idle 断言有效。
- 内存：P3 新增 `paint_scales: Vec<(f32, LogicalPoint)>`（约 12 B/节点，与既有 `painted_extents`/`baked_screen_bounds`/`popup_transforms` 侧表同型）；`arena::remove` 重置、槽位复用不继承（`node/tree/tests.rs:128-186` 覆盖）。未在设计中给 P3 新侧表的内存估算（P0 §11.3-4 只算 44 B/节点），属 P4 观察。
- 已知 P4 口径：真实窗口 `damage_full=unavailable`（headless 覆盖）；像素门禁不采样动画帧（P3K-08）；`--debug-stack` 不含 `paint_scale`。

---

## 8. P4 登记真实性与登记缺口

P4 §14.7 共 18 行（含 1 行已 fixed 的 Separator/Group 留档），逐项核对均为真实残余，未发现把 P0–P2 代码缺陷伪装成 P4 的情况：R2-01/R2-04/R2-05、A-26/27/31/32、E-12、性能扩展、P3E-03、P3K-07/08、P3A-03/04、source-id、ContextMenu 右键、typeahead IME、TCC、Separator/Group（fixed）、P3-4 扩展。

但 P4 列表不完整/有未登记项：

1. `review-p3-kit-r2.md` 观察 1：`settle.rs:418` 的 `set_popup_settle_rounds` 累加在常规帧会被 `frame.rs` 的 `FrameStats{popup_settle_rounds: settlement.rounds}` 覆盖，实际无效；建议删 API 或改累加路径——未在 §14.7 登记。
2. `review-p3-kit.md` §3.3：ColorPicker 的 `AnchorLost` 回调无专门测试（Dropdown/Select/ContextMenu/Menu 有）；最终仍只有 `the_on_close_callback_fires_once_per_close` 程序化路径——属测试覆盖缺口，未登记。
3. FQ-06 的 R2-e/R2-f/外部滚动/Closing churn 属 P3 而非 P4，不能靠 P4 列表吸收。
4. `tools/source-id.txt` 未加入 `.gitignore`（P4 行只说“提交前重生成”）。
5. P3R2-01/P3R2-02 是 P3，不在 P4；不能按“P4 已登记”豁免。

---

## 9. 残余风险与不可执行项

- **只读/环境**：本轮按用户限制只跑 `ui_core`/`ui_kit` 测试与 workspace check；workspace `cargo test` 未跑（用户在 P0/P1/P2/P3 均限制；AGENTS §8 的全文 workspace test 仍是交付风险）。
- **真实窗口 command 并发**：审查期间 19:10:46 Windows `real-pixels/windows` 被另一进程复跑覆盖（clean HEAD 55/0 PASS）；`--real-pixel-check`/`--debug-stack` 需独占桌面，脚本有 3 次 retry；证据目录非不可变。
- **P4 已登记**：动画帧/内容正确性/acceptance 归档/TCC/source-id 等按 P4 不计。
- **基线既有规则偏差**：`gallery.rs:2934`、`volume_heatmap/view.rs:576` 的 `#[allow(clippy::too_many_arguments)]` 在基线已存在（非本轮引入）；若按 AGENTS §4 对 touched 文件严格执行，需重构参数或另行登记 P4。
- **SmolStr**：`popup_panel.rs:789` measure 文本用 `String` 拼接后转 `SmolStr`（P1 代码）；与 §2.2 精神有偏差，非本轮引入；如严格执行需改 `format_smolstr!`/`SmolStr` 构建方式。
- **性能 median 守卫**：设计“帧 ≤ idle p95+2ms”与测试用 median 的措辞差异属 P3E-03/obs3 既有观察；p95 实测 1.09ms 也远低于 2ms，无当前回归。
- **不可宣布 P0–P3 全 0**：因 FQ-01…FQ-07 均为 P3（无 P0/P1/P2）；P3 仍需处理/登记后才能按用户标准宣布完成。

---

## 10. 结论

- **P0 = 0、P1 = 0、P2 = 0、P3 = 7**（FQ-01…FQ-07；P4 不计）。
- 代码/门禁/测试/性能主体真实通过：3872 passed/2 ignored、clippy/fmt/workspace check PASS、gallery `--pixel-check` 94 checks PASS、三端真实像素 55/0 PASS；AGENTS 新增行 0 注释/0 allow/0 新依赖/0 动态 JSON；无静默删除测试。
- 最关键 3 条：
  1. **FQ-01**：三端证据的文档数字/source identity 与提交树不可对账（0a9a vs c8143c；Windows 16004/95 vs 报告 15622/94，且审查期间被复跑覆盖为 clean 15871/94）；代码身份实为最终，但证据元数据/归档需回写（R2-01 残余）。
  2. **FQ-02/FQ-03**：引擎 r2 的两条 P3（D-P3-4 旧 rationale、`PopupScale` Text 死分支）在最终设计/代码中仍未关闭，且设计声称 P3K-06 closed 与代码不符。
  3. **FQ-05/FQ-06/FQ-07**：DoD 3“真实窗口全路径”仅有程序化证据；P2 遗留 P3 观察（R2-e/R2-f/外部滚动/Closing churn）未登记；设计/实施/审查文档未入库（`.docs` ignored，只有 kit-gallery tracked），缺少明确口径。
- **是否可宣布完成（P0–P3=0）：否。** 无需回滚或改功能代码；最小收口为：文档回写（FQ-01/02/04/05/06）、删除死分支或补证据（FQ-03）、明确 `.docs`/source-id ignore 策略（FQ-07）并复跑受影响门禁（删除分支后至少 `cargo test -p ui_core -p ui_kit`/clippy）。
