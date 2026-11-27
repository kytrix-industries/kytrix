# 最终对抗性审查（架构 / 行为 / 跨阶段）— review-final-arch

- 审查员：子 agent `final-review-arch`（最终对抗性审查员 A；只读审查 + 仓库外探针）
- 审查对象：worktree `E:\kytrix\.agents\worktrees\dsh-1d175414-1a41`，HEAD `fa41987c`（P3 提交），对比基线 `71a5a5cc`（P0 前）
  - `git diff --stat 71a5a5cc..HEAD -- . ':(exclude).docs'`：**174 文件，+40366 / −10598**
  - 提交：`d9949aca` P0 → `152616b1` P1 → `f24a3441` P2 → `fa41987c` P3；审查开始/结束时 `git status --short` 均为空
- 结论摘要：**P0 = 0，P1 = 0，P2 = 2（FA-01、FA-02），P3 = 2（FA-03、FA-04）→ 按用户门槛（P0–P3 全 0）当前不可宣布完成。**
  - 两条 P2 都是**此前各轮审查未覆盖**的问题：一条是 P1 引擎 dismiss 语义与 P2 kit 安全三角契约的跨阶段矛盾（既有测试为空断言，现行门禁全部漏检）；另一条是**首次执行 workspace `cargo test`** 暴露的宿主迁移测试失败（此前各轮按用户限制只跑 `ui_core`/`ui_kit`）。
  - 两条 P3 属于 AGENTS §1.1/§4 的死状态/死 API 与文档-实现不一致（`PopupState::Closing` 永不赋值；若干新增公开变体在生产路径上从未被构造）。
  - 其余 P0/P1 面（渲染不变量、像素门禁、旧 API 删除、模态/焦点/IME/a11y 主路径、三端脚本与 Windows 真实像素）经复核未发现新阻断项。

---

## 1. 范围与方法

### 1.1 审查范围（全部只读）

| 层 | 覆盖 |
|---|---|
| 引擎 popup | `ui_core/src/popup.rs`、`popup/{stack,dismiss,settle,focus,animation,anchor,placement,tests}.rs`、`app/{frame,popup,input,focus}.rs`、`a11y/popup.rs`、`a11y/tree.rs`（popup 路径）、`node/tree/{layout,arena,content,tree}.rs`、`paint/pass.rs`（popup transform/scale/damage）、`window/paint.rs`、`render/compose.rs`、`window_set.rs` |
| kit | `kit/{popup_panel,menu,menu_chain,context_menu,dialog,alert,dropdown,select,tooltip,color_picker,list}.rs` |
| 宿主迁移 | `crates/widget/{services,cx,binding,error}`、`crates/kytrix/{composition,shell/dialogs}`、14 个 widgets crate + 5 个业务 ui crate 的 diff；重点 `market_participants_ui/menu.rs`、`profile_ui/*`、`instrument_selector_ui/*`、`settings_ui/*` |
| 文档/证据 | 设计 §0/§4/§5/§6/§7/§10–§14；`implementation/` 全部 summary 与 r1/r2/r3 审查报告；`AGENTS.md` |
| 前轮结论抽样复核 | P1 final engine/kit/quality（r2/r3）、P2 menu/quality、P3 engine/kit/acceptance 的关键“fixed/deferred”结论逐条落到代码 |

### 1.2 执行的命令与结果（本机 Windows 11 / D3D12 WARP / 125% DPI，`CARGO_TARGET_DIR=E:\kytrix\target`）

| 命令 | 结果 |
|---|---|
| `cargo test -p ui_core -p ui_kit` | **PASS，3872 passed / 0 failed / 2 ignored**（ui_core lib 2806+1 ignored、frame_record 8、gui_smoke 0+1 ignored、layout 4、popup_behavior 123、snapshot 14、demo 39、ui_kit 878） |
| `cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings` | PASS（exit 0） |
| `cargo +nightly fmt --all -- --check` | PASS（exit 0） |
| `cargo check --workspace --all-targets` | PASS（exit 0） |
| `cargo test --workspace` / `cargo test --workspace --no-fail-fast` | **FAIL（exit 1）**：唯一失败 `settings_ui::settings_tests::credentials::a_failed_write_should_keep_the_dialog_and_what_was_typed`（settings_ui lib 235 passed / 1 failed；其余 crate 全绿）。详见 FA-02 |
| `gallery --pixel-check` | **PASS 94 checks**：32 sampled / open 8 / close 6+6 / `damage_full_frames=0` / first_visible == stable `(496,91,300×375)` / moved `(496,4,300×375)`，exit 0 |
| `gallery --debug-stack` | **PASS**：3 级链 z=[2,3,4]、parent 链正确、链关闭 entries=0；picker 打开 1 条 → 关闭 0 条；`overlay-check PASS (menu=(9,270.0,180.075) picker_children=4 entries=0)`，exit 0 |
| `gallery --real-pixel-check` | **PASS 55 checks / 0 failures / 15872 ms / frame 94**（Windows 本机；`target/real-pixels/windows/report.txt`）。含 dialog scrim theme/composite/alpha（derived alpha 0.5492，theme 0.55）、scrim/panel 负向自检 |
| 三端证据核对（只读） | `E:\kytrix\target\real-pixels\macos-remote\report.txt`、`linux-remote\report.txt` 均 `checks=55 result=PASS`，`source_revision=f24a3441, source_status=dirty(30 files), diff_sha=0a9a218b…`（与设计 §14.4 一致；本次未复跑远端） |
| 旧 API `rg` 复核（独立） | `create_overlay/close_overlay/place_overlay/bring_overlay_to_front/OverlaySlot/OverlayDismiss/OverlayPlacement/overlay_style/note_overlay_container/overlay_containers/animation_router/AnimationRouter/AnchorEdge/match_anchor_width/kit::layer/LayerSlot/OverlayOptions/OverlaySurface/OverlayA11y/OverlayError/overlay_wrapper/MenuMode::ListBox/.layer()/.animation()/services.layer` **全零命中**；`dump-overlay` 仅 gallery 改名提示 3 处（设计 §12.9 结论成立） |

### 1.3 仓库外探针（不改仓库）

- 位置：`E:\kytrix\target\final-review-arch\probe`（独立 `[workspace]`，path 依赖本工作树 `ui_core`(test-support)/`ui_kit`，`CARGO_TARGET_DIR=E:\kytrix\target\fr-probe`）
- 目的：对“安全三角/grace 内外部按下 `DismissOutcome::Ignore` 应 **不关链、不 replay**（设计 D-P2-2）”做端到端验证：全窗背景可点击节点 + 根菜单 + 20 行长子菜单，`Menu::sync`/`Submenu::sync` 按宿主帧序推进 30 帧、悬停到安全三角内一点后按下/抬起，统计背景点击。
- 原始输出：`E:\kytrix\target\final-review-arch\probe-safe-triangle.txt`（要点见 FA-01）。
- 另：探针仅写 `E:\kytrix\target`；仓库工作树无任何改动（`git status --short` 为空）。

---

## 2. Findings（最终轮新增，编号 FA-xx；已登记 P4 项不重复计数）

### FA-01 [P2] 安全三角/grace 内的外部按下被 **replay 到背景控件**：`DismissOutcome::Ignore` 的“不 replay”契约未实现

**位置**

- 引擎 `crates/ui_core/src/popup/dismiss.rs:274-299`（`resolve_auto_outside`）：
  - `:284-286` `OutsidePress::Ignore` 策略分支直接 `return PressRouting::Dispatch;`
  - `:289` dismiss handler 返回 `DismissOutcome::Ignore` 时同样 `=> PressRouting::Dispatch`
- `crates/ui_core/src/app/input.rs:218-228`：只有 `PressRouting::Consume` 会阻断后续派发并置 `suppress_release = true`；`Dispatch` 表示“继续走普通命中派发”。
  - 即 `Ignore` 在当前实现里等价于“**不关链但把这次按下/抬起正常派发给背景**”，正是 D14 的 replay 行为。
- kit 侧 `crates/ui_kit/src/kit/menu.rs:1731-1743`（`dismiss_outcome`）：`OutsidePress` 且 `chain().safe_area_contains(pointer)` → `DismissOutcome::Ignore`。

**契约依据（不可辩驳）**

- 设计 §3.4:240“P2：safe triangle/grace 内按下 Ignore 不关链（D-P2-2…）”；
- 设计 §13.4 D-P2-2:1481“**不关链、不 replay**”；
- `implementation/p2-summary.md:62`“`DismissOutcome::Ignore`——不关链、不 replay；落在链外才按 D14 默认 Replay”；
- 设计 §13.2 P2-5:1460“safe triangle/grace 内 Ignore 不关链（无桥接几何）”。

**探针证据（当前 HEAD 干净树，仓库外）**

```
root opened = true
entry slot0 tier=Auto state=Open rect=(10,38 200x68)      ← 根菜单
entry slot1 tier=Auto state=Open rect=(210,42 200x320)    ← 20 行子菜单
probe point = (208.0,126.0)   ← 根面板下沿(106)之下、子菜单左缘(210)之左 2px：在 derived safe triangle 内
subtree hit at point = Some(NodeId(w1.n3.1))              ← 命中全窗背景节点（可点击）
after hover: menu open = true; sub open = true
menu still open = true; submenu still open = true; background clicks delta = 1
```

- 链确实保持打开（`Ignore` 分支生效），但 **背景可点击节点收到了一次按下+抬起并触发 click（delta=1）**；若该处是图表/画布，还会开始拖拽/平移或改变焦点。
- grace（300ms）多边形由更远的 exit 点构成时区域更大，背景被误激活的机会更高。

**为什么现有门禁全绿**

- `crates/ui_kit/src/kit/menu.rs:5418-5449` `a_press_inside_the_safe_triangle_keeps_the_chain_open` 的“安全三角按下没有落到背景控件”断言是**空断言**：该 harness 的背景可点击节点只有 `(600,500 80×40)`（`menu.rs:3290`），而三角落点由 `child.hit_rect.origin.x - 2, panel.max().y + 40` 给出（`open_tall_chain`），二者不重叠——断言与“是否 replay”无关。
- `--pixel-check`/`--real-pixel-check` 均不采样“按下语义”，`popup_behavior` 也没有 `Ignore` → 背景不派发的引擎用例（`dismiss_topmost_honours_ignore_and_keeps_the_handler` 只验证 handler 保留）。

**影响**：违反用户已裁决的 D-P2-2/桌面惯例（Qt/Chromium 的 grace 只影响 hover 关闭，不把点击“送进”背景控件）；菜单保持打开同时触发背景动作/焦点/拖拽，属于非预期输入穿透。

**修法（建议）**

1. `resolve_auto_outside`：`DismissOutcome::Ignore` 与 `OutsidePress::Ignore` 都返回 `PressRouting::Consume`（Consume 路径已负责 `suppress_release`，抬起也会被吞掉）；语义变为“消费本次按下+抬起，不关链”。
2. 若认为还有“Ignore 但需要继续派发”的合法场景，则新增显式路由（如 `PressRouting::ConsumePreservingChain`），不要复用默认 Replay。
3. 测试：
   - 引擎：构造 Auto 链 + 全窗背景可点击节点，模拟 `on_dismiss` 返回 `Ignore`，断言背景 action 未触发、`pointer.suppress_release == true`、抬起后不补发 click；
   - kit：把 `a_press_inside_the_safe_triangle...` 的背景可点击节点扩到覆盖三角落点（或换成全窗），使现有断言具备判别力。

**阻塞否**：**是**（P2，契约违反 + 输入穿透；修法约 2 行 + 2 条测试）。若不修，`p2-summary`/设计 D-P2-2 的“不 replay”属于不实交付声明。

---

### FA-02 [P2] workspace `cargo test` 红：`settings_ui` 凭证测试未随“模态关闭动画 + 关闭期输入阻断”迁移

**位置/证据**

- 失败命令与输出（当前 HEAD 干净树）：

```
cargo test --workspace --no-fail-fast
…
running 236 tests
test settings_tests::credentials::a_failed_write_should_keep_the_dialog_and_what_was_typed ... FAILED
panicked at crates\settings_ui\src\settings_tests\credentials.rs:336:5:
the retry should take the dialog away
test result: FAILED. 235 passed; 1 failed; …
```

- 失败用例 `crates/settings_ui/src/settings_tests/credentials.rs:303-353`；关键时序 `:332-334`：

```rust
harness.press(KeyCode::Escape);   // 关闭“写入失败”的模态 Alert
harness.settle(3);                // 只 drain 3 帧，不推进虚拟时钟
confirm_dialog(&mut harness);     // 立刻点击凭证对话框的 Confirm，期望写回并关闭
```

**机制（代码级）**

1. 写入失败后的提示是独立 Modal Dialog（scrim + `FadeScale`，P3 起 `DialogBuilder` 默认 `fade=true`）；Escape 关的是最上层可关闭层 = 该 Alert，engine 进入 `DismissPending` 并启动 90ms 关闭动画（`popup/dismiss.rs:121-230`）。
2. `crates/settings_ui/src/settings_tests/test_support.rs:359-362` 的 `settle(frames)` 只 `drain()`（`TestApp::advance` 才是推进时钟），因此 3 帧后 Alert 仍是 `DismissPending`。
3. P1 终审 E-01 的**有意语义**：`DismissPending/Closing` 的 Modal 继续阻断背景输入（`popup/stack.rs::topmost_modal` 用 `is_visible()`、`popup/dismiss.rs::on_pointer_down` 消费 scope 外按下、`popup_behavior::a_closing_modal_swallows_background_presses_until_it_is_removed`）。于是紧随其后的 Confirm 按下/抬起被 **Consume + suppress_release** 吞掉。
4. `confirm_dialog` 之后的 `settle_animation(20×16ms)` 会让 Alert 完成关闭，但那次点击已经丢失 → 凭证对话框保持打开、未写回 → 断言 `:336` 失败。

**为什么是迁移漏项**：同一提交对**姊妹用例** `a_failed_clear_should_keep_the_set_until_the_retry_writes_it` 恰好在同样的 `press(Escape); settle(3)` 之后补了 `harness.advance(200)`（diff：`credentials.rs:379-381`，`+ harness.advance(200);`），但漏掉了本用例。这与此前各轮“workspace 测试未跑（用户限制只跑 ui_core/ui_kit）”的登记（设计 §12.6-1、§14.5）一致：**宿主测试从未在全量门禁下跑过**。

**影响**：AGENTS §8.1 的 `cargo test` 门禁在 HEAD 上为红；最终交付不能声称“测试全绿”。产品行为本身（关闭中的模态仍阻断输入）是 E-01 的既定裁决，因此这是**测试/交付门禁缺陷**，不是新的产品正确性缺陷；但它会掩盖未来真实回归，且与“P3 已完成”的收口声明冲突。

**修法**：`settings_ui/.../credentials.rs` 在 `press(Escape)` 后改为 `settle_animation(&mut harness)`（或 `harness.advance(200)` 再 `settle(3)`），与姊妹用例一致；随后 **`cargo test --workspace --no-fail-fast` 全量复跑**。另建议：把“Escape 关闭模态后 90ms 内点击会被吞”的时序写成 kit/宿主测试注释或公共 helper（`close_and_settle_animation`），避免每个宿主各自踩。

**阻塞否**：**是**（P2，交付门禁红；修复 1 行 + 复跑）。

---

### FA-03 [P3] `PopupState::Closing` 是**永不赋值**的公开状态：状态机文档、kit 分支、a11y 测试三处与实际不符

**证据**

- 定义：`crates/ui_core/src/popup.rs:75-85`（`Opening, Open, DismissPending, Closing, Closed`）。
- 全部 `set_state(...)` 调用点只有 `Opening/Open/DismissPending/Closed`：
  - `popup/animation.rs:63,77,193`（Open/Opening）
  - `popup/dismiss.rs:100`（DismissPending）
  - `popup/settle.rs:385`（Open）
  - `app/popup.rs:596,605`（Closed）
  - 全仓没有任何 `set_state(_, PopupState::Closing)`。
- 实际关闭事务是 `Open → DismissPending →（动画结束）removal`：`begin_close_inner` 只置 `DismissPending` 并 push `PopupAnimationKind::Closing`；`animation::tick` 完成后 `finalize_close` 直接删节点（`popup/animation.rs:158-222`、`app/popup.rs:232-248`）。
- 因此以下代码是**死分支**：`popup/dismiss.rs:184`（`if matches!(state, PopupState::Closing) { continue; }`）、`popup/animation.rs:182` 的 `Closing`、`popup/dismiss.rs:517`、`popup/stack.rs:720`、`kit/popup_panel.rs:306`（`is_closing` 匹配 `Closing`/`Closed`）、`kit/popup_panel.rs:547`。`PopupState::Closed` 同样在 `App::popup_state` 上不可观测（entry 在 remove 前已被移出，remove 后 API 返回 `None`），kit 的 `Some(PopupState::Closed)` 分支（`popup_panel.rs:375,544`）也不可达。
- 文档冲突：设计 §4.2:301/§4.7:495 写“关闭事务：`DismissPending→Closing→动画后移除`”；`p1-migration-inventory.md:697` 写“`close()` 后 … `PopupState::Closing`”；`a11y/popup_tests.rs:1301` 把 `Some(DismissPending | Closing)` 当作可能状态断言。而 §12.7 B-09/B-12 只登记了 `is_open/is_closing` 的 kit 口径，未登记 `Closing` 未被实现。

**影响**：公开 API 暴露一个永不出现的状态（宿主若按文档在 `state() == Some(Closing)` 时做“关闭中”分支，将永远不触发；kit 内部匹配为死代码）；设计/回写文档与实现不一致。AGENTS §1.1/§4（不保留过时代码/文档）。

**修法（二选一，并同步文档/测试）**

- A（补实现）：`begin_close_inner` 在完成 `DismissPending` 阻塞后把条目置 `Closing`（`animation::tick` 首帧或紧接 `push_animation`），`finalize_close` 前保持 `Closing`；补 `popup_behavior` 断言“动画中 state == Closing，结束后 entry 消失”，并让 kit `is_closing` 分支真正可达。
- B（删状态，推荐按 AGENTS §1.2）：删除 `PopupState::Closing`，关闭期唯一真值 = `DismissPending`；删 kit/引擎死匹配，改设计 §4.2/§4.7 与 `p1-migration-inventory` 措辞，a11y 测试同步收窄。
- 无论选哪个，`Closed` 的对外可观测性也要写明（只能 `popup_state` 返回 `None`），或删掉 kit 的 `Some(Closed)` 分支。

**阻塞否**：不阻塞（P3，卫生/文档/API 一致性）；但按用户“P3=0”门槛需处理。

---

### FA-04 [P3] 新增 popup 公开 API 存在**生产路径从未构造**的死变体（AGENTS §1.1/§4）

**证据（全仓 grep，区分生产/测试）**

| 变体 | 定义 | 生产构造 | 测试构造/使用 | 结论 |
|---|---|---|---|---|
| `popup::MaxSize::Viewport` | `popup.rs:396-413` | `PlacementSpec::default()`（`popup.rs:454`） | 多处 | 唯一生产在用 |
| `MaxSize::Width` / `Height` | 同上 | **无** | 仅 `popup/placement.rs:710,729` 单测 | 生产死变体 |
| `MaxSize::WidthAndHeight` | 同上 | **无** | **全仓 0 次构造** | 完全死变体 |
| `OutsidePress::Ignore` | `popup.rs:117` 附近 | **无** | 仅 `popup/dismiss.rs:267` 匹配自身 | 死变体（且见 FA-01：该策略当前也会把按下派发走） |
| `Anchor::NodeRect` | `popup.rs:419` | **无**（`kit/menu.rs:1391` 仅对其做 match 取 opener） | 约 22 处测试 | 生产死 API |
| `ZPolicy::Above(PopupId)` | `popup.rs` | **无**（kit/hosts 全用默认 `Newest`） | 5 处 popup_behavior 测试 | 生产未使用 |
| `EscapePolicy::CloseChain` | `popup.rs` | **无**（实现分支 `dismiss.rs:458` 之外无构造） | 1 条测试 | 生产未使用 |
| `RestoreFocus::RootFallback` | `popup.rs` | **无**（实现分支 `focus.rs:92` 之外无构造） | 1 条测试 | 生产未使用 |

**对照标准**：P1 终审以“无生产消费者”为由**删除**了公开字段 `PlacementSpec.match_anchor_width` 与 `AnchorEdge`（设计 §12.3-23/27、§12.9-8，且 `AGENTS.md` §1.1/§1.2/§4 要求删除不再使用的 API/代码）。本轮 P2/P3 又新增/保留了上表变体，且设计 §12.7 的 A-27/B-08 只处理了“同名 `MaxSize`”命名问题，未处理“变体本身无消费者”。

**影响**：公开 API 面积与文档承诺大于实际能力；读者/宿主会误以为 `MaxSize::Width/Height`、`NodeRect`、`ZPolicy::Above`、`EscapePolicy::CloseChain` 等已被验证可用（实际只有单测或完全无构造）。与 AGENTS 验收口径冲突。

**修法（择一）**

- 删除无生产消费者且设计不强制保留的变体（至少 `MaxSize::WidthAndHeight`、`OutsidePress::Ignore` 可直接删；其余按设计取舍）；
- 或明确保留为“对外扩展 API”：在设计 §14.7/P4 列表登记逐项，并补至少一条生产式使用/集成测试，避免再次以“死字段已清零”口径收口。

**阻塞否**：不阻塞（P3 卫生），但按用户“P3=0”门槛需处理或由 lead 明确裁决登记。

---

## 3. 设计 DoD 对照（§10，逐条）

| DoD | 结论 | 依据/缺口 |
|---|---|---|
| 1 缺陷清零（残影/首可见帧/像素门禁） | **PASS** | `--pixel-check` 94 checks、`damage_full_frames=0`、first_visible==stable；`--debug-stack` 首可见帧==稳定帧；Windows `--real-pixel-check` 55/55；macOS/Linux 证据 `checks=55 PASS`（只读核对） |
| 2 架构（旧 API 全删、零引用、单栈） | **PASS** | 独立 `rg` 上面 1.2 的 25 个旧 API 模式全零；`App` 只剩 `open_popup/close_popup/...` 一套；`dump-overlay` 仅 gallery 改名提示 |
| 3 组件（D9 全部 + 2–3 级真实路径） | **PASS（有保留）** | 8 组件迁移并有行为测试；`--debug-stack` 3 级链 z/parent/关闭 entries=0 真实窗口 PASS；MenuChain 79+13+21 条测试；保留见 FA-04（API 死变体）与 §6 |
| 4 渲染不变量 I1–I9 | **PASS（I7 有一处未验证）** | I1/I2/I3/I4/I5/I6/I8/I9 均有测试/像素门禁支撑（P0 render 三轮 + 本轮 pixel/real-pixel）；I7“modal 之下不得有后代 popup”只覆盖 Hint，**程序化在 modal 期间开 Auto 根未有测试**（见 §6 残余） |
| 5 门禁（cargo test/clippy/fmt/check/性能） | **FAIL** | ui_core/ui_kit test、clippy、fmt、workspace check、pixel 均 PASS；**workspace `cargo test` 红（FA-02）**；性能守卫/实测与 §14 一致（本轮未重跑性能用例） |
| 6 三端真实窗口验收 | **PASS（本机仅 Windows 复跑）** | Windows 55/55 本机复跑 PASS；macOS/Linux 仅核对 `E:\kytrix\target\real-pixels\{macos-remote,linux-remote}/report.txt`（55/PASS，source 快照一致）；本轮未在远端复跑（环境限制） |
| 7 文档回写/kit-gallery 同步 | **PASS（有 2 处失真）** | 设计 §11–§14、p1/p2/p3 summary、kit-gallery §13/§14 均已回写；失真：设计 §4.7/§12.7 的 `Closing` 状态（FA-03）与 D-P2-2“不 replay”（FA-01） |

---

## 4. 跨阶段风险（P0↔P1↔P2↔P3 交互，逐条）

1. **P1 dismiss 语义 ↔ P2 kit 安全三角（FA-01，最高优先）**：kit 在 `on_dismiss` 里返回 `Ignore` 期待“只取消关闭”，但引擎把 `Ignore` 映射成默认 Replay 路由；P2 的 300ms grace/5 点多边形把可按键区域扩大后，影响面从“9px 缝隙”扩大到“exit 点→子菜单的整块三角”。P2 测试用“背景控件不在落点”伪装成覆盖，属于跨阶段契约未打通。
2. **P0 damage/局部重绘 ↔ P1/P3 动画**：`paint_offset`（P1）与 lane scale（P3）只作用于绘制，damage 用 layout rect 超集 + 每帧旧/新 offset 并集；本轮复核 `animation::tick`、`settle` pivot 同步、`paint_state` 缓存键（含 pivot）与 `raise`/`reorder` 的 `order_rebuilds+1` 测试均一致，未发现漏 damage；`--pixel-check` 的 `changed-outside-damage` 采样与 `damage_full_frames=0` 也支持该结论。
3. **P1 关闭事务（E-01 阻断输入）↔ 宿主测试/产品时序（FA-02）**：宿主（settings/profile/kytrix dialogs）普遍在 `Escape` 后立刻继续点击；关闭动画 90ms + 关闭期仍阻断输入是**有意行为**，但宿主测试按旧“立即卸载”编写。除已发现的 1 条外，其余宿主用例恰好都调用了 `settle_animation`/`advance`，所以只有这一条红；这类时序脆弱性应在 kit 提供统一 helper/文档。
4. **P1 模态作用域 ↔ a11y/输入/I7**：`topmost_modal` 以 `is_visible` 覆盖 `DismissPending`（E-01）与 `owner_modal` 后代 scope 均已测试；但**在 modal 打开期间通过公开 `open_popup` 开一个 Auto 根不会关闭/拒绝**（`open_popup` 只对 `Hint` 做 `HintBlockedByModal`），该 Auto 会叠到 modal/scrim 之上且不可交互（输入走 modal consume），modal 关闭后也不会被级联关闭（`begin_close_inner` 只级联 `owner_modal` 后代）。I7 的“modal 之下不得有后代 popup”未被程序化路径覆盖。这是 API 契约空白（P4 候选，见 §6）。
5. **P0 结构增量绘制 ↔ P1 popup 节点增删/重排**：`reorder_children` 要求全排列、`sync_order` 先 `prune_missing`，`reorder` 只 mark order/paint 并 damage 子树；`raising_a_popup_reorders_paint_and_hit_without_a_full_repaint`（`order_rebuilds+1`、`!damage_full`、命中顺序跟随）与 `sync_order_mirrors_the_stack_order_in_the_popup_root` 均在；未发现 P0 路径回归。`paint_scales` 清理（P3E-05）也已复核闭合。
6. **P3 性能统计/门禁 ↔ 真实输入**：`--pixel-check`/`--real-pixel-check` 只采样稳定态与固定场景（P3K-08/P3A-04 已登记），FA-01 的“按下语义”不被任何像素/栈门禁覆盖；跨阶段结论是**门禁绿 ≠ 输入契约正确**，P4 列表应把“engine dismiss 语义的引擎级用例”提为一等公民，而不是只依赖 kit 行为测试。
7. **多窗口**：每窗口独立 `PopupStack`、`focus_owner`、`scroll_epoch`；`window_deactivated` 只关 Auto/Hint 根；窗口销毁 `close_window` 直接丢弃窗口（popup dismiss 回调不派发，kit 依赖 `sync` 的 `popup_state==None` 走 `finish_close(Programmatic)`）。本轮未发现新问题，但“窗口销毁时渲染 on_close reason”仍是隐含契约（P4 观察，不计数）。

---

## 5. 已核实正确点（对既有 review 结论抽样复核，均有代码/实测支撑）

1. **P3E-01 阴影向量缩放已修**：`paint/pass.rs:302-303` `shadow.offset = (self.length(offset.x), self.length(offset.y))`；`fade_scale_scales_the_shadow_offset_as_a_vector` 用 `frame_dump().instances` 与旧公式 >2px 守卫（r2 结论成立）。
2. **P3E-02 scrim fade-only 已修**：`popup.rs:571-574` `is_fade_only = is_modal() || tier ∈ {Modal, Scrim}`，`animation.rs:42/50/57` 与 `dismiss.rs:206/210` 统一使用；`fade_scale_modal_scrim_is_fade_only` 同时断言 modal+scrim。
3. **P3E-09 pivot 缓存/侧表**：`paint_state` 键含 `(factor, pivot)`（`node/tree/content.rs`），`arena.rs:610` 移除时重置 `(1.0, ZERO)`；`the_paint_reuse_key_includes_the_scale_pivot` 同 factor 不同 pivot 必失配。
4. **P3E-03 性能守卫**：`tests/popup_behavior.rs:5872` `budget = idle_p95 + 2ms`，中位帧断言（`:5867-5880`），p95 仅记录；与 §14.6 D-P3-7 一致。
5. **M2-02R 父层 hover 复位**：`menu.rs:2678-2701` 在子层 `on_close` 里仅当 `!over_row && last_hovered == Some(row)` 清 `last_hovered/trigger_over` 并把 active 指回父行；`task-58` 修复可复核。
6. **K1-07 `popup_total`**：`instrument_selector_ui`、`market_participants_ui`、`profile_ui`、`updater_ui` 的 `test_support.rs` 均有 `popup_total()`，且在释放/关闭路径断言 `== 0`（market_participants menu 3 处、profile 5 处、instrument 6 处等）。
7. **关闭事务实现健壮性**：`close_entry` 在 `remove_node` 前先 `set_state(Closed)`，因此 `prune_missing` 不会提前吞掉条目、`App::close_popup(...).expect(...)` 在全部测试中仍为 `Ok`；scrim 由 `reap_orphan_scrims` 兜底、modal 级联 child-first（`depth` 降序）均有测试。
8. **焦点单一真值**：`app/focus.rs::set_focus` 在 popup owner 存在时转 `popup_focus::set_owner_focus`，`focused/validate_focus` 同源；`opening_a_take_popup_moves_focus_and_closing_restores_it_once`、`dialog_tab_cycles_inside_the_modal_scope`、`a_stale_focus_owner_is_repaired_when_the_next_tab_runs` 全绿。
9. **旧 API 删除完整性**：§1.2 的 25 模式零命中（独立 grep），旧 overlay/layer/AnimationRouter/ItemPanel 旧形态均无残留；`PopupHost`/`cx.popup_host()` 确未引入（设计 §8 已改口径）。
10. **像素门禁真实性**：`--pixel-check` 的 `checks=94` 组成（32 sampled + 16 background-residue + 46 static，含 3 条分段 animation floor）与 §12.3-9/§14.5 一致；`--real-pixel-check` 含 offset/recolor/identical/scrim-fault/panel-fault 负向自检（本轮 Windows 输出逐项 PASS）。
11. **a11y 链**：`a11y/popup.rs::project` 的 modal scope/adopted/logical parent/expanded/has_popup/active_descendant 投影与 `a11y/tree.rs::effective_a11y_parent`（物理 vs 逻辑单父、cycle 回退）一致；`--debug-stack`/`a11y/popup_tests` 22 条全绿。
12. **三端脚本与证据快照**：`tools/real-pixel-check-{macos,linux}.sh`、`macos-gates-runner.sh` 存在；远端 report 的 `source_revision=f24a3441 / dirty(30) / diff_sha=0a9a218b…` 三端一致（与设计 §14.4 相符）；本轮只复跑 Windows。

---

## 6. 残余风险与 P4 列表符合性

**P4 列表（§14.7 + §12.6）总体符合“非本轮阻塞”的定位**（R2-01 首帧内部滚动、R2-02 非锚点 reason、R2-04 初始焦点回退、R2-05 观察、A-26/27/31/32、E-12 测试真实性、性能基线、P3E-03、P3K-07/08、P3A-03/04、source-id、D-P2-4/D-P2-5、macOS TCC、pixel 扩展），但本轮新增三类应登记/裁决项：

1. **FA-01/FA-02 不在 P4 列表内**（本轮新增 P2；按用户“P0–P3=0”不能转 P4）。
2. **I7 的“程序化 modal 期间开 Auto 根”未覆盖**（API 契约空白，P4 候选）：`open_popup` 只拦 `Hint`；建议要么在 `open_popup` 拒绝/关闭无关 Auto（与 Modal 打开路径对称），要么在设计 §4.6/I7 明写“程序化 Auto 可在 modal 之上（不接收输入）”，并补测试。当前行为是“可见但不可交互、modal 关闭后仍残留”，不建议保留为未登记状态。
3. **“workspace 测试未跑”这一流程限制应写入 P4/CI 项**：本轮已证明它会藏住真实红（FA-02）。建议把 `cargo test --workspace --no-fail-fast` 纳入提交前门禁（至少 settings_ui/widget/kytrix 三 crate），并在文档中撤销“workspace 测试未跑”的长期豁免。
4. 既有 P4 中值得升级复查的两项（仍可保留 P4，但建议与 FA-01 一起处理）：P3K-08（像素门禁不采样动画帧/按下语义）与 P3A-04（real-pixel 只验证稳定态/不验证内容正确性）——它们是本轮 FA-01 类问题能穿过的直接原因；P3K-07（scale 不参与 hit/a11y/IME）应与 §4.12 契约文字对齐。
5. `PopupState::Closing` 的文档/测试口径（FA-03）与 `p1-migration-inventory.md:697`、`a11y/popup_tests.rs:1301` 一并在修法 A/B 中同步，避免修完代码留下“测试允许一个不存在的状态”。

---

## 7. 计数、结论与建议修复顺序

| 严重度 | 数量 | 编号 |
|---|---|---|
| P0 | **0** | — |
| P1 | **0** | — |
| P2 | **2** | FA-01（safe triangle/grace 按下被 replay 到背景）、FA-02（workspace `cargo test` 红：settings_ui 用例未迁移） |
| P3 | **2** | FA-03（`PopupState::Closing` 永不赋值/文档-实现不一致）、FA-04（新增公开 API 死变体：`MaxSize::Width/Height/WidthAndHeight`、`OutsidePress::Ignore`、`Anchor::NodeRect`、`ZPolicy::Above`、`EscapePolicy::CloseChain`、`RestoreFocus::RootFallback`） |

**最关键 3 条**

1. **FA-01（P2）**：D-P2-2 的“不 replay”没有实现——引擎把 `DismissOutcome::Ignore` 映射为 `PressRouting::Dispatch`（`popup/dismiss.rs:284-297`），探针证明菜单保持打开的同时背景控件被点击一次（`background clicks delta = 1`）；既有 kit 测试的“没有落到背景控件”是背景节点不覆盖落点的空断言。修法：`Ignore → Consume`（+ suppress_release）+ 引擎/kit 各一条具判别力测试。
2. **FA-02（P2）**：HEAD 上 `cargo test --workspace` 唯一失败为 `settings_ui` 凭证用例；根因是 Escape 关闭模态后的 90ms 关闭动画期间输入被阻断（E-01 有意语义），测试未推进虚拟时钟；同一提交的姊妹用例已加 `advance(200)`，本用例漏了。修法：补 `advance/settle_animation` 并全量复跑 workspace 测试。
3. **FA-03（P3）**：`PopupState::Closing` 从未被赋值，但设计 §4.2/§4.7、`p1-migration-inventory` 与 a11y 测试仍把它当作状态机一环；kit 3 处匹配为死分支。补实现或删除并同步文档/测试。

**是否可宣布完成：否。** 按用户“P0/P1/P2/P3 全 0 才可结束”的门槛，当前 **P2=2、P3=2**，仍不可收口。建议最小修复顺序：

1. FA-01 引擎 2 行 + 2 测试（含把 kit 安全三角用例改成具判别力）；
2. FA-02 宿主测试 1 行 + `cargo test --workspace --no-fail-fast` 全绿；
3. FA-03 状态机二选一 + 文档/测试同步；
4. FA-04 删除或登记死变体（最低限度：`MaxSize::WidthAndHeight`、`OutsidePress::Ignore`）；
5. 复跑：`cargo test -p ui_core -p ui_kit`、`cargo test --workspace --no-fail-fast`、clippy/fmt/workspace check、`gallery --pixel-check`、`--debug-stack`、Windows `--real-pixel-check`（三端其余平台按 §14.4 证据/脚本复跑），并保持 `git status --short` 为空。

---

## 8. 与并发 final review 的重叠/去重（供 lead 汇总；截至 19:30 快照）

本轮同期还有 `review-final-interaction.md`、`review-final-quality.md` 两个最终审查报告；仅就我独立复核过的交叉项说明：

1. **`settings_ui` 红（本报告 FA-02）与 `review-final-interaction.md` 的 P3 宿主测试红是同一根因**（Escape 关闭模态后 90ms 关闭动画/输入阻断 + 测试不推进时钟）。两报告分别计 P2/P3，lead 去重时应合并为一条；我建议按 **P2**（`cargo test` 门禁红，且与“P3 已完成/门禁全绿”的声明冲突），修法一行。
2. **`review-final-interaction.md` 的 P2“非模态 popup 的 Tab 被锁在 popup focus scope”与我的代码核对一致**：`app/focus.rs::focus_next/prev` 只要存在 `popup.focus_owner` 就走 `popup_focus::next_in_popup_scope`（不区分 Take/Trap），而 D6 已把 Dropdown/Select/ColorPicker 从旧 modal 改成非模态 `Take`；旧实现虽是 `trap_focus: modal=true`（旧 Dropdown/Select 本就有意 trap），但在新语义下“非模态仍锁 Tab”值得修（否则键盘用户只能 Esc 离开）。本报告未重复计数，建议 lead 采纳其 P2 并在 `focus_next` 对 `FocusPolicy::Take` 走主树 `next_focusable`（或先关闭 popup 再移动焦点）。
3. **本报告 FA-01/FA-03/FA-04 未被另两份 final report 覆盖**（其摘要的 Menu 矩阵未验证“Ignore 按下是否 replay”；`Closing` 与死变体项不在其 P3 列表）；`review-final-quality.md` 提到的“reconcile Closing churn”是 **kit `MenuChain::LevelState::Closing`** 的另一问题，与本报告 FA-03 的 **引擎 `PopupState::Closing`** 不同，可分别登记/合并为“状态机清理”一条 P4/P3。
4. **`review-final-interaction.md` 的“宿主级模态死路（设置保存失败 Alert 的确定按钮无动作）”我未独立复现**，不纳入本报告计数；若成立，与 FA-02 同属宿主迁移路径，建议 lead 优先复核（它可能是产品缺陷而非测试问题）。

---

## 附录：本轮审查产生的证据

- 探针源码：`E:\kytrix\target\final-review-arch\probe\{Cargo.toml,src/main.rs}`（独立 workspace，path 依赖本工作树；不写仓库）
- 探针输出：`E:\kytrix\target\final-review-arch\probe-safe-triangle.txt`（FA-01 原始输出）
- 真实窗口证据（本机 Windows 复跑）：`E:\kytrix\.agents\worktrees\dsh-1d175414-1a41\target\real-pixels\windows\{report.txt,00..06*.png}`（`checks=55 failures=0`，`revision=fa41987c status=clean`）
- 三端历史证据（只读核对）：`E:\kytrix\target\real-pixels\{macos-remote,linux-remote}\report.txt`（55/PASS，`f24a3441 + dirty(30)`）
- 门禁命令输出摘要：`--pixel-check` 94 checks/0 full；`--debug-stack` 3 级链 entries 3→0；workspace 测试仅 1 failed（见 FA-02）
