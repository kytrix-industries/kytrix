# P1 终审对抗性审查：kit 组件 / 迁移 parity / 测试有效性

- 审查角色：P1 终审对抗性审查员（kit 组件/迁移 parity/测试有效性），只读、不改仓库
- 工作目录/worktree：`E:\kytrix\.agents\worktrees\dsh-1d175414-1a41`
- 基线：`d9949aca`（P0 提交）；审查对象：P1 未提交工作树（111 个文件变更 + 7 个新文件，`git diff --stat d9949aca` ≈ 6479 insertions / 9216 deletions）
- 结论速览：**P0 = 0，P1 = 0，P2 = 5，P3 = 8**。架构切换（旧 `kit::overlay`/`kit::layer`/引擎 overlay → 引擎 popup + `PopupPanel`）是干净的；实测门禁全绿；但存在 4 个应在提交前处置的 P2（2 个未裁决的 parity/设计偏差、1 个空转的首帧测试、1 个旧门禁命令静默 no-op）以及 1 个需产品确认的焦点语义偏差。**不建议在 P2 未处置（修复或明确裁决并登记）前提交 P1**；处置后无阻塞项。

---

## 1. 范围与方法

### 1.1 审查范围

- 设计：`.docs/popup-overlay-redesign-design.md` §4.2/§4.4/§4.6/§4.7/§4.9/§4.12、§5.2、§6 P1-9、§7.3、§8、§10、§11、D5/D6/D7/D12–D16。
- 迁移清点：`implementation/p1-migration-inventory.md`（组件映射/必保 parity/测试清单/§7 未决点）、`implementation/p1-cleanup-w7.md`（删除与测试迁移映射）。
- 实现：`crates/ui_kit/src/kit/{popup_panel,menu,dropdown,select,tooltip,context_menu,dialog,alert,color_picker}.rs`、`kit/test_support.rs`、`examples/gallery.rs`；`crates/ui_core/src/popup*`（仅对照 kit 契约，引擎细节由引擎审查负责）；宿主 `instrument_selector_ui`/`market_participants_ui`/`profile_ui`/`settings_ui`/`updater_ui`/`kytrix`/`widget`/`widgets`。
- 规则：`AGENTS.md`（无注释、无 `#[allow]`、无死代码、SmolStr、强类型错误、无新依赖）。

### 1.2 执行的方法与门禁（本机 Windows 11，`CARGO_TARGET_DIR=E:\kytrix\target`）

| 门禁 | 结果 |
|---|---|
| `cargo +nightly fmt --all -- --check` | PASS（exit 0） |
| `cargo test -p ui_core -p ui_kit` | **3734 passed / 0 failed / 2 ignored**（ui_core lib 2804+1 ignored、popup_behavior 92、frame_record 8、layout 4、snapshot 14、demo 39；ui_kit 773），exit 0 |
| `cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings` | PASS |
| `cargo check --workspace --all-targets` | PASS |
| `cargo run -p ui_kit --example gallery -- --pixel-check` | **PASS**，checks=93、sampled_frames=32、open_animation_frames=8、close_animation_frames=12、damage_full_frames=0、first_visible==stable=(496,91,300×375)、moved=(496,4,300×375)，exit 0 |
| `cargo run -p ui_kit --example gallery -- --debug-stack --auto-close=9` | **PASS**（menu metrics `(6,180.0,127.5)`、tier/栈/首帧/关闭后无残留/最终 entries=0），exit 0 |
| 旧 API `rg` 组合模式（inventory §5.3） | 0 命中（`kit::overlay|AnimationRouter|OverlayOptions|OverlaySurface|create_overlay|LayerSlot|Band::|.overlays(`） |
| 静态检查 | kit 生产代码 0 注释、0 `#[allow]`/`#[expect]`、0 `unsafe`/`Box::leak`；无新增依赖（仅 `ui_core/Cargo.toml` 增加 `popup_behavior` test target）；`format!`→0（SmolStr 路径正常） |

未复跑：`cargo test -p widget` 与各业务/ widgets crate 测试（W7 自证 182 passed；本轮按用户限制只跑 ui_core/ui_kit，另只做 workspace check）。

---

## 2. Findings

> 严重度：P0 = 架构/数据错误或用户可见严重回归；P1 = 必须本轮修；P2 = 提交前需修复或明确裁决并登记；P3 = 可留 P2/P3 阶段，但需登记。所有 `file:line` 以当前工作树为准。

### K1-01（P2）菜单/右键菜单的“祖先滚动 / 窗口 resize 关闭整链”矩阵未实现，测试把相反行为固化

**证据**

- 设计 §4.7「组件级 anchor/scroll/resize 矩阵」（`popup-overlay-redesign-design.md:494`）明确要求：**菜单/右键菜单在祖先滚动或窗口 resize 时关闭整链**；Dropdown/Select/ColorPicker 才同帧重新 settle。
- 实现：`MenuInner`/`ContextMenu` 没有任何“祖先滚动/resize → close chain”的检测与关闭；滚动会置 popup geometry dirty（`app/scroll.rs:142,179`）→ 同帧 re-settle：Menu（`Anchor::Node`，`popup/anchor.rs:11` 走 `node_screen_rect`）跟随锚点行，ContextMenu（`WindowRect`）钉在旧窗口点，二者都不关链。`MENU_DISMISS` 只有 `anchor_lost: true`，而引擎只在 `resolve_anchor` 返回 `AnchorNodeMissing` 时触发 `anchor_lost`（`popup/settle.rs:258-263,394-396`），**锚点滚出视口/祖先滚动/窗口 resize 都不会关链**；resize 走 engine `viewport_changed` 重新 settle（`popup/settle.rs:121-125`），不是关链。同理 Dropdown/Select/Tooltip 在“anchor 滚出视口/不可见”时也不会关闭/隐藏（设计 §4.7 要求关闭或隐藏），因为 `anchor.rs` 只判节点缺失。
- 相反行为被写成断言：`kit/context_menu.rs:763-777`（`the_page_scrolls_under_an_open_context_menu`）滚动后断言 `menu.is_open(...)` 仍为 true，测试消息自称“WindowRect 锚点”；`kit/menu.rs` 无祖先滚动用例。
- `Anchor::WindowRect` 的右键菜单在页面滚动后仍钉在旧窗口点；`Anchor::Node` 的 Menu 会被 re-settle 跟随锚点行（engine 在 layout 后重解 anchor），与“关整链”不同但仍未按矩阵关闭。

**影响**：设计验收矩阵缺失；滚动页面时右键菜单/菜单会悬停在不再对应内容的位置（或跟随行），与桌面惯例/设计不符；后续 P2 MenuChain 若以此为基础会把偏离固化。

**修法**：(a) kit 在 `sync` 中检测 anchor 节点的“祖先滚动 offset / 窗口尺寸”快照变化，按 §4.7 对 Menu/ContextMenu 调 `close_chain`（Dropdown/Select/ColorPicker 保持不动）；或 (b) 引擎把 §4.7 矩阵落为 `anchor_lost` 的可配置维度；同时把 `the_page_scrolls_under_an_open_context_menu` 改为“滚动后关链”或按 (b) 明确登记为设计变更。**阻塞否：需在提交前修或由 lead 明确裁决并写入偏差登记。**

### K1-02（P2）菜单/右键菜单/Dropdown/Select 从“模态 + 外部点击 Consume”改为“非模态 + Replay”，未经裁决/未登记，且与 P1 parity 必保清单冲突

**证据**

- 旧行为：Menu/ContextMenu/Dropdown/Select 均 `modal: true`（焦点陷阱 + 背景 hover/scroll/点击阻断），外部按下被吞（inventory §2.4 P1 parity 必保 #13/#14、§2.5；旧测试 `a_press_outside_closes_the_menu_and_does_not_reach_the_page`、`an_open_dropdown_consumes_background_scroll`、`the_page_does_not_scroll_under_an_open_context_menu`）。
- 新行为：`kit/menu.rs:53-58` `MENU_DISMISS.outside = OutsidePress::Replay`；root menu 用 `FocusPolicy::Take`（非模态，`menu.rs:1120-1126`）；`market_participants_ui` 直接 consumer 反而用 `PopupTier::Modal + Trap`（保持旧模态）。新测试：`menu::a_menu_is_non_modal_and_keeps_background_hover`、`dropdown::an_open_dropdown_does_not_swallow_background_scroll`、`context_menu::the_page_scrolls_under_an_open_context_menu`、三者的 `..._replays_the_press_to_the_page`；旧断言测试被删除/改名。
- inventory §7.1 D6/D7 明确把“root 菜单 modal 语义”与“menu/context 用 Consume 还是 Replay”列为**需 lead/用户决策（阻塞实现）**；`p1-cleanup-w7.md` 与设计 §11 无任何裁决/偏差记录（`rg` 全 docs 无 D6/D7 结论）。

**影响**：用户可见行为改变：(1) 菜单打开时背景可 hover/滚动；(2) 外部点击关闭菜单后该次 press+release 会激活下层控件（可能误触按钮/破坏性操作）；(3) Dropdown/Select 打开时页面可滚动。设计 D14 默认 Replay、D6 Auto 非模态支持该实现，但 P1 交付声明“行为 parity”，且 inventory 把旧行为列为“必保”。两者必须有一份明确裁决。

**修法**：由 lead/用户裁决并登记：(a) 保持设计（D14/D6）→ 更新 inventory/设计 §11 偏差、明确 menu/context 例外或接受全局 Replay，并在 gallery/真机做误触验证；或 (b) 恢复旧 parity → menu/context 用 `Trap + OutsidePress::Consume`，dropdown/select 视产品决定 Consume/Replay。**阻塞否：是（裁决/登记可在本次提交内完成）。**

### K1-03（P2）Dropdown / Select 的“首可见帧 == 稳定帧”测试是空断言：先 settle 再采样

**证据**

- `kit/dropdown.rs:1205-1228`：`harness.click(&dropdown, center)` → `popup_entry(&dropdown)` 记为“首帧” → `harness.settle()` → 再取 rect 比较。
- `Harness::click`（`dropdown.rs:947-958`）内部是 `inject(down)+inject(up)+self.settle(dropdown)`；`Harness::settle`（`dropdown.rs:914-924`）为 6×(advance 40ms+drain+sync) + advance 200ms + 2×drain，即**约 440ms 后才采样“首帧”**。因此断言比较的是两次稳定后的 rect，恒等；首帧错位/宽度晚一帧完全不可能被这条测试发现。
- `kit/select.rs:2166-2187` 与 `Harness::click/settle`（`select.rs:1356-1366,1389-1401`）同构，问题相同。
- 对照：`popup_panel::the_first_visible_frame_already_has_the_settled_rect`（open → 单帧 drain → 读 rect → 再 drain 后比较）、`menu::the_first_visible_frame_already_has_the_settled_rect`（`Harness::open` = open+sync+单帧 drain）、`dialog::the_first_visible_frame_has_the_settled_rect`（单帧 drain）是真实的；`color_picker` 在 2 帧后才采样（`color_picker.rs:3336-3355`，稍弱但非空转）。

**影响**：P1-12 要求“现有 kit 测试迁移为首可见帧断言”，Dropdown/Select 是生产使用最广的两个组件（Select 13 个生产构造点），其首帧错误（如 AtLeast 宽度测量晚一帧、trigger 宽度未在打开帧生效）不会被单测发现；设计 C1/C2 的核心保证在这两个组件上没有有效门禁。

**修法**：改为 `open()` 后只 drain 一帧（不 advance / 不 settle）→ 断言 `PopupState::Open` 且 `popup_rect().hit ==` 之后 settle 的 rect（必要时再补 `FrameRecord.rendered` 与首帧面板宽度）；ColorPicker 同样把采样提前到第一帧。**阻塞否：是（低成本，建议本轮修）。**

### K1-04（P2）旧门禁命令 `--dump-overlay` 被移除后静默 no-op，设计文档中的验收命令会假绿

**证据**

- `gallery.rs:139-165` 的 `parse_flags` 不再接受 `--dump-overlay`，未知参数只打印 `gallery: ignoring unknown argument`，退出码仍 0；`--dump-overlay --auto-close=9` 现在只会跑 auto-close 演示并正常退出，不执行任何 overlay-check/像素断言。
- 设计 §7.5（`design.md:826`）、§11.5（`:968`、`:981`）仍把 `--dump-overlay --auto-close=9` 写成验收命令；`p1-cleanup-w7.md` §4 记录了改名为 `--debug-stack`，但设计文档未回写（P1 收口时应回写 §11）。
- 新命令 `--debug-stack --auto-close=9` 实测 PASS 且断言实质（menu-open/metrics/stack size/tier/first-visible/close/residue/final empty/frames>0）。

**影响**：照抄设计/旧脚本的 CI 或人工验收会得到“exit 0 = PASS”的**假绿**；P1-12 的“保留命令名/退出码”只对 `--pixel-check` 落实，`--dump-overlay` 的迁移没有任何失败信号。

**修法**：提交前把设计 §7.5/§11.5 的验收命令改为 `--debug-stack`（若最终保留旧命令名则需保留别名并更新语义）；建议 `parse_flags` 对未知参数返回非 0 退出码（当前静默忽略是既有行为，但正是它让这次改名变成假绿）。**阻塞否：文档回写即可解除。**

### K1-05（P2，需产品/引擎确认）D16“默认按钮优先”退化为“树中第一个 Button”，自定义 footer 的对话框初始焦点会落在 Cancel

**证据**

- 引擎 `ui_core/src/popup/focus.rs:20-35`：`InitialFocus::DefaultButton` 的实现是“子树 DFS 中第一个 a11y role == Button 的可聚焦节点”，引擎没有“默认动作”显式标记/API（`InitialFocus` 只有 DefaultButton/FirstFocusable/Container）。
- kit Dialog 构建顺序（`kit/dialog.rs:550-625`）：root → title → content body（调用方内容）→ alert → confirm/cancel 按钮行。凡是**内容自身含 Button** 的对话框，内容按钮先于 Dialog 的 confirm/cancel，成为初始焦点。
- 生产受影响样例：`settings_ui/src/pages/proxy/editor.rs:502-507`（DialogBuilder content 内自建 footer）、`:897`/`:925` 依次构建 cancel、confirm Button——初始焦点是 **Cancel**；`instrument_selector_ui/src/prediction_selector.rs:1118/1191/1219` 在内容中构建 translate/previous/next Button（translate_visible 时可见）——初始焦点是内容按钮而非主操作。
- 现有 dialog 测试只用不含 Button 的内容验证默认按钮（`dialog::the_initial_focus_is_the_default_button`），因此未暴露。

**影响**：D16 的意图（键盘用户从默认动作开始）在自定义内容对话框上不成立；对危险/确认对话框初始焦点落在 Cancel 或内容按钮属于可感知的 a11y/交互偏差。

**修法**：给引擎 `InitialFocus` 增加显式节点目标（如 `InitialFocus::Node` + 内容构建后二次解析），或 Dialog 记录“默认按钮节点”并在 open 后显式取焦（引擎当前不提供 set_focus_on_open）；至少补“内容含 Button 时默认动作仍优先”的测试并登记该启发式。**阻塞否：需 lead/引擎 owner 裁决；若接受“第一个 Button 即默认”的近似，需在偏差表登记并补测试。**
（注：若旧 P0 dialog 的懒修复也取第一个可聚焦节点，则本条是“设计 vs 实现”的偏差而非回归，故列为 P2 待裁决而非 P1。）

### K1-06（P3）`sampled-animation-floor` 只保证累计下界，单次关闭动画消失仍可能绿灯；open 下限允许动画减半

**证据**（`gallery.rs:3607-3622`）

- `open_animation_frames = 打开到 settle 之间的非 Idle 帧数`，实测 8；断言 `>= 4`。若引擎开动画 120ms→60ms（≈4 帧）仍 PASS；只有降到 <4 帧（<~64ms）才红。
- `close_animation_frames` 是**两次 close transaction 的累计**（12），断言合计 `>= 3`；若其中一次关闭完全不播动画（只剩 1–2 个卸载帧），另一次提供 6 帧，合计仍 ≥3 而 PASS。W7 §7.1 已如实说明“是下界、不声称每帧恰为动画帧”，但断言消息写的是“动画常量变化不得让采样静默缩水”，口径偏强。

**影响**：P1-12 想用该下限防“动画常量变小/删除导致采样静默缩水”，对**单次关闭**的回归保护不足；属于门禁强度问题，不是功能缺陷。

**修法**：分别记录 open 与两个 close 的采样下界（open ≥4、每次 close ≥3），或改为读引擎动画状态/`FrameStats`（如 Open/Closing 动画帧计数）断言。**阻塞否：否。**

### K1-07（P3）宿主测试辅助统一过滤 `PopupTier::Scrim`，会掩盖宿主释放路径的 scrim 残留

**证据**：`updater_ui/src/test_support.rs:104-114`、`instrument_selector_ui/src/test_support.rs:146-156`、`market_participants_ui/src/test_support.rs:467-477`、`profile_ui/src/test_support.rs:789-799` 的 `overlay_count()`/`popup_count()` 都 `filter(|entry| entry.tier != PopupTier::Scrim)`；`updater_ui/dialog.rs:1436-1444`、`indicator.rs:345/373` 等以 `popup_count()==0` 断言“全部释放”。若 Dialog `release`/面板 `release` 后 orphan scrim 存活，所有宿主断言仍为 0。引擎有 `reap_orphan_scrims` 单测，但**宿主释放路径**没有覆盖。

**影响**：宿主级 scrim 生命周期回归不被现有测试发现（P1 风险清单中 scrim/modal 是重点）。**修法**：至少每个宿主保留一处断言 `popup_stack(window).entries.is_empty()`（不过滤），或断言无 `Scrim` 条目。**阻塞否：否。**

### K1-08（P3）未登记的 parity / 覆盖缺口清单

| # | 项 | 证据 | 说明 |
|---|---|---|---|
| a | Alert 由“独占/替换”变为“叠放” | 旧测试 `an_alert_dialog_replaces_the_previous_alert_slot` 删除；新 `dialog::a_second_alert_stacks_above_the_first`（`dialog.rs:2191`）；inventory §2.6 parity 提到 Alert 独占（`LayerSlot::Exclusive`） | 设计未明确 Alert 互斥；行为改变（两个 Alert 可同时存在，仅顶层关闭），需登记或补“叠放时焦点/scrim”的确认 |
| b | Tooltip 显式 TOP 定位覆盖丢失 | 旧测试 `the_tooltip_is_placed_above_the_trigger_by_the_overlay_placement`（显式 `Placement::TOP`）删除；新只测默认 Bottom（`tooltip.rs:862`） | `TooltipOptions::placement` 自定义路径无测试；生产 profile/market 已删除 `placement: BOTTOM` 覆盖（默认即 Bottom，行为一致） |
| c | typeahead / Select 搜索未实现，与设计文案不一致 | 设计 §5.2「Dropdown/Select 键盘/typeahead」、§7.3 测试清单；inventory §7.1-11 明确当前代码无 typeahead/搜索并需用户裁决；实现无相关键绑定/字段 | 属设计文案 vs P1 parity 范围缺口，需裁决后登记（建议 P2 与 MenuChain 键盘表一起做） |
| d | ColorPicker a11y role=Dialog 但非模态（`FocusPolicy::Take`） | `color_picker.rs:1355-1366`；inventory D12 要求 a11y owner 确认 | 引擎只按 Trap 派生 modal，非模态 Dialog role 是否语义合法未记录 |
| e | instrument_selector Dialog 显式 `width(720)` 导致面板宽度 ~400 → 736 | `instrument_selector.rs:456` `.width(DIALOG_WIDTH)`；`dialog.rs:317` panel = width+16；content root `dialog.rs:563`；旧 dialog 面板约 384+16，内容内 720 宽面板节点本会溢出 | 这看起来修复了旧的内容溢出，但属**未登记视觉/布局变更**；窄窗口（<752）会被 engine viewport max-size 夹住、内容根 720 溢出并被 `overflow_hidden` 裁剪，需要真实窗口/最小尺寸确认。无宽度断言测试 |
| f | Dialog resize 回中（recenter parity）无测试 | `dialog.rs:700-741` 有 recenter/apply_center；测试仅有初始居中 `the_panel_is_centered_in_the_viewport`；旧基线也无 resize 测试 | 设计 §4.7/§5.2 要求 resize 回中；代码存在但无回归保护（P3） |
| g | `MenuOptions.submenu_open_delay` 默认 150→200ms 未写偏差 | `menu.rs:48,880`；inventory §2.4 要求“迁移时显式记录变更”（设计 §3.4 默认 200ms） | 设计已定 200ms，但迁移记录缺失；行为类断言用 0 延迟，未验证默认值 |

**影响**：这些项单条都不影响门禁，但“P1 完成=全组件 parity 且偏差登记”的收口口径下需一次性登记/裁决，避免 P2 阶段误以为已保真。**修法**：在 P1 提交说明/设计 §11 偏差表逐条登记（或补实现/测试）。**阻塞否：否（但需成文）。**

### K1-09（P3）`ItemPanel` 的 `PanelState.height` 是死字段

**证据**：`kit/menu.rs:153-157` 定义 `height`；仅在 `:369`、`:459`、`:551` 写入，全文件无读取（`state.height` 无读取路径；`content_height()` 每次独立计算）。违反 AGENTS §4「不再使用的字段应直接删除」。

**修法**：删除 `PanelState.height` 及三处赋值。**阻塞否：否。**

### K1-10（P3）生产代码 2 处 `expect`（本地不变量可证明，但属 panic 路径）

**证据**：`kit/color_picker.rs:1014` `let mut panel = inner.panel.take().expect("上面刚检查过面板存在");`；`:1935` `let array: [TrackView; 4] = tracks.try_into().expect("四条 HSL 通道各建一条滑条");`。审查要求“无 unwrap/panic 生产路径”；两处均为局部不变量，但可无 panic 表达。

**修法**：前者改为 `if let Some(mut panel) = inner.panel.borrow_mut().take()`；后者用 `std::array::from_fn`/固定长度构造避免 `try_into`。**阻塞否：否。**

### K1-11（P3）`PopupAnimation::FadeSlide` 未接入任何 kit 组件，D7/§4.12“默认 Fade+Slide”未落地且未登记

**证据**：`menu.rs:408`（ItemPanel 打开时 `PopupAnimation::Fade`；Dropdown/Select 经 ItemPanel 共用）；`tooltip.rs:78-84` 默认 `PopupAnimation::Fade`；`color_picker.rs:1361` Fade；Dialog Fade（fade-only 符合设计）。`FadeSlide` 仅在引擎测试出现。P1 parity 旧 kit 只有 fade，故可能是刻意 parity；但设计 D7/§4.12 写的是默认 Fade（opacity）+ Slide（4px）。

**修法**：为 Menu/Dropdown/Select/ColorPicker/Tooltip 选择 `FadeSlide`（Dialog 保持 Fade），或按“P1 parity=fade only”登记偏差。**阻塞否：否。**

### K1-12（P3）宿主测试装置的 `advance/settle` 调整已逐个核对，无掩盖性改动；仅记录两处放宽

- `profile_ui/test_support.rs` 的 `click_alert_ok/cancel` 在 settle(2) 后 `advance(200ms)`、`market_participants_ui/test_support.rs:372-377` 新增 `settle_fade()`（3×advance 200ms+drain）、`market_participants_ui/menu.rs:1417/1460` 用它等动画结束：均为适配“关闭动画需时间推进”的合理改动；`an_open_panel_should_be_steady_when_nothing_moves` 在 settle_fade 后仍断言后续 2 帧无 frame request，强度保留。
- `instrument_selector_ui/instrument_selector/tests.rs:772-780` `settle_dialog_close` 由 `closed && find().is_none()` 改为 `closed || find().is_none()`（`:739` 的 `settle_close` 同构）：`closed` 本身已是 `settle_until(find none)` 的结果，等价于“再多推进 8×40ms 后节点已消失”，未断言 `on_close` 是否触发；属轻微放宽，建议改为“advance 轮询直到节点消失，再断言 closed 信号/回调恰好一次”。**阻塞否：否。**

### K1-13（P3）`PopupPanel::release` 会经 `close_now → finish_close` 触发 `on_close`，各消费者靠“自己记得先置 noop”规避，Menu/ContextMenu 未做

**证据**：`PopupPanel::release`（`kit/popup_panel.rs:455-458`）→ `close_now` → `finish_close` → `on_close(reason=Programmatic)`。已正确规避的消费者：Dialog（`dialog.rs:538-548` 先 `panel.on_close(|_,_| {})` 再 release）、ColorPicker（`color_picker.rs:1030`）。**未规避**：`ItemPanel::release`（`menu.rs:627-641`，`panel.release` 仍带 ItemPanel 注册的回调 `menu.rs:271-279,1403-1406`）→ `Menu::release`/`ContextMenu::release` 会调用调用方公开的 `on_close(Programmatic)`。旧 `Menu::release` 只在 `sync` 探测到卸载时回调，release 本身不回调；现有测试没有“release 后 on_close 次数”的断言（kit Menu/ContextMenu 生产消费者为 0，故未暴露）。

**影响**：宿主在销毁/重建菜单时可能收到一次伪“关闭”（Programmatic）回调；P2 MenuChain 若把 on_close 用于业务收尾会出现重复/错误清理。API 语义应统一，而不是每个消费者自己记得置 noop。

**修法**：把“release 不触发 on_close”收敛进 `PopupPanel::release`（例如 release 内部先清掉回调再 close_now），或明确提供 `release_silent`；Menu/ContextMenu 补“release 不触发 on_close”的测试。**阻塞否：否。**

---

## 3. 测试有效性评估

### 3.1 断言数量与删除映射

- kit 测试数（baseline → now）：dropdown 18→23、select 23→29、tooltip 20→26、menu 16→21、context_menu 12→14、dialog 38→48、color_picker 73→78、alert 11→11；新增 `popup_panel` 16；旧 `overlay.rs` 37 + `layer.rs` 14 随删除。ui_kit lib 773（P0 基线 769，含已删除的 51 条）——净增不是简单相减，逐条映射见 `p1-cleanup-w7.md` §3，抽查的删除项（ui_core 12 条覆盖去向、kit 首帧 4 条、外部点击/模态/焦点）均能在 `popup_behavior.rs`/`popup/tests.rs`/`a11y/popup_tests.rs` 找到对应用例（已用 `fn <name>` 逐一验证存在）。
- 未发现静默删除：被删测试均有替代或设计变更说明；**但** K1-03 的“首帧”测试名不副实，属“断言被放宽”的实质问题（数量增加但有效性不达标）。
- 首帧断言真实性：`popup_panel`、`menu`（单帧 drain）、`dialog`（单帧 drain）真实；`color_picker`（2 帧后采样）稍弱；`dropdown`/`select` 空转（K1-03）。
- 负向/自证：`popup_panel::opening_and_closing_a_popup_never_forces_a_full_repaint`、dropdown/select/menu/context_menu/color_picker 的 `..._never_forces_a_full_repaint` 均为逐帧 `!damage_full`；`popup/tests.rs`/`a11y/popup_tests.rs` 对栈/焦点/a11y 不变量有反向用例（引擎审查另评）。

### 3.2 `gallery --pixel-check`（93 checks）有效性

- **未静默移除**：与 P0 基线（`git show d9949aca:gallery.rs`）对比 record key 集合一致，新增 `sampled-animation-floor`；P0 文档 108 checks → 93 的差额可由“采样帧 45→32（引擎动画 120/90ms，旧 kit fade 150ms）”解释，非断言删除。
- 保留的强项：每采样帧 `!damage_full`、变化像素 ⊆ damage、popup/trigger 外逐像素背景基线、移动后旧浮层区域全区域扫描 + 覆盖量下限（30‰ fixture 设备像素）+ ≥2 分散像素注入负向自检、首可见帧 rect==稳定帧、关闭/重开恢复基线、baseline 不透明、idle 无动画源 + 20 帧 idle 断言。
- 局限：K1-06 的动画下限口径偏弱；背景残留断言只在 `compare_background=true` 的 open/reopen 帧上记录（idle 帧因 `PixelFrame::Idle` 提前 return 不再做 background-residue，但因无绘制帧/像素不变，不构成覆盖缺口——已核实）。
- 首帧真实性：`observe` 在 drain 后采样，“首个非 Idle 帧”即首个实际绘制帧（TestApp 只在 drain 内绘制），`first_visible==stable` 为真实断言。

### 3.3 `gallery --debug-stack` 有效性

- 断言为实质：`menu-open`（Ok(true)）、`menu-metrics`（viewport children>0/content>0/thumb>0）、`menu-stack-size==1`、`menu-stack-tier==Auto`、`menu-first-visible-rect`（on_frame 采样 `first==stable`）、`menu-close-call`、`menu-close` 后栈空、picker 同构（open/metrics/first-visible/stack/close）、结束 `entries.is_empty()` 与 `menu_frames>0 && picker_frames>0`；实测 `menu_frames=445`、`picker_frames=401`、最终 entries=0。
- 局限：只断言 rect/栈，不含像素（符合设计 P0/P1 分工，像素由 `--pixel-check` 承担）；`first`/`stable` 依赖 `on_frame` 采样，若 `on_frame` 被移除或只在个别轮次运行时断言退化（当前 W7 已注明仅在 `--debug-stack` 流程启用）——作为残余风险记录。

---

## 4. 迁移 parity 核对表（kit 组件）

| 组件 | 关键 parity 项 | 结论 |
|---|---|---|
| PopupPanel | 首帧==稳定帧、content 闭包内置面板/body、宽度 Auto/Exact/AtLeast、`close` 进入 Closing、`close_now` 立即、`on_close` 恰好一次（含 Esc/Programmatic 原因）、open 幂等、无 damage_full、`set_anchor`/`set_width` 同帧生效、a11y role/label 动态更新 | **OK**（单测覆盖强；AtLeast 与旧 ItemPanel 宽度公式（text+extra+16）核对一致；`close()` 先置 reason 再 begin_close，在 begin_close 失败时 reason 会残留，但 Engine 状态机下不可达，仅记录） |
| Dropdown | trigger 开合/幂等、单选回填+先关后回调、禁用行不选不关、↑↓/Home/End/Enter/Space/Esc、虚拟列表 overscan、trigger 宽度跟随、AtLeast/Exact、外部关闭、稳定无帧、Closing 中 opener 取消关闭、debug id | **OK 除 K1-02/K1-03**；typeahead 未实现（K1-08c） |
| Select | 单选/多选、chips/max_tags、clearable、a11y 摘要、`:hit:`/list 命名、release_scope、窗口 resize 重 settle、外部 Replay、多选不关面板 | **OK 除 K1-02/K1-03**；无搜索/typeahead（K1-08c） |
| Tooltip | 500/100ms 延迟、即时、hover/focus 触发、离开/失焦关闭、hover 区含 tooltip rect、穿透、不取焦、Esc 只关自身、不关菜单且在其上、内容切换不可复活、modal 打开隐藏并按设计不再悬于其上、max_width+padding 宽度、`on_close` 不涉及 | **OK**；显式 TOP 覆盖缺失（K1-08b）、FadeSlide 未用（K1-11） |
| Menu | Auto tier、root Take、子菜单 parent/z（engine 保证）、Right→Left 翻面、parent panel avoid（engine 由 parent solved rect 生成）、200ms hover、hover 不夺焦、键盘 → 夺焦/← 回父行、Esc 只关一层、release 激活、空白不激活、`set_entries` 打开中更新、max_height 内部滚动、a11y Menu/行角色/分组/分隔线、debug id、关闭链 | **OK 除 K1-01/K1-02**；release 触发 on_close（K1-13） |
| ContextMenu | WindowRect + TopLeft/BottomLeft、4px clamp、先关后回调、Esc 幂等、第二个实例替换第一个、同实例重开保留根/关旧子菜单、子菜单在父上、label 透传、外部 Replay（行为变更，K1-02）、页面可滚动（K1-01） | **OK 除 K1-01/K1-02**；release 触发 on_close（K1-13）；gallery 调用点已用 `node_screen_rect`（K9 坐标混用已消除） |
| Dialog | Modal+Trap、scrim 独立栈项/a11y hidden/与 modal 生命周期绑定、outside/escape 策略、初始焦点（见 K1-05）、关闭恢复一次、recenter（初始/每帧）、居中、标题动态 a11y+节点、confirm/cancel/close 动作、fade-only、关闭中不跑 action、`close_now` reason、幂等/重开、Alert 内容面板、多 Dialog 叠放仅顶层关、ContextMenu 被 modal 驱逐、release 摘 action/scope | **OK 除 K1-05/K1-08a/K1-08e/f** |
| Alert | 内容面板、校验/错误、closing_guard（改为 `Fn(&App)` 以读 popup closing）、11 测试保留 | **OK**；独占→叠放见 K1-08a |
| ColorPicker | 单 content 闭包（K3 修复）、Canvas 局部坐标、tab 切换 rebuild+request_measure、hex 提交/失焦归一、hover 预览/跨间隙、确认回调一次、非确认不回调、外部/Esc 关闭、`on_close` 一次、无残影/无 damage_full、重开同 rect、`open_state`/`on_open` | **OK**；2 处 `expect`（K1-10）、非模态 Dialog role（K1-08d） |

---

## 5. 已核实正确点（抽查/实测）

1. **旧机制真正消失**：inventory §5.3 组合 `rg` 零命中；`kit/mod.rs` 删除 `layer`/`overlay`，新增 `popup_panel`；`ItemPanel` 为 `pub(crate)` 且只持有 `PopupPanel + List`，无 Layer/Overlay/animation router 字段；无私有 z 序/定位/删除逻辑（kit 只调 `open_popup/raise_popup/set_popup_anchor/close_popup/begin_close_popup` 与手工 `tree.remove` 仅限内容节点/measure 节点）。
2. **无新依赖/无注释/无 allow/无死 API 大规模残留**：Cargo 仅加 test target；kit 生产代码无注释、`#[allow]`、`unsafe`；旧死 API（`Menu::key/surface_node`、`MenuEntry::is_actionable`、`ContextMenu::key`、`ContextMenuInner.window`、`MenuMode::ListBox`）均已删除（仅 K1-09 的 `PanelState.height` 遗留）。
3. **PopupPanel 关闭事务**：`close` 进入 Closing→`is_open=false`/open signal 同帧 false/不可命中；`sync` 在 state None/Closed 时 `finish_close` 恰好一次回调（`the_close_callback_reports_the_dismiss_reason_exactly_once`/`closing_programmatically_...`/`close_now_...`）；reason 来自 on_dismiss（Esc 为 Escape、程序化 Programmatic）。
4. **首帧在引擎层成立**：`popup_behavior::opening_a_popup_settles_geometry_in_the_first_frame`、`text_dominated_height_is_stable_on_the_first_frame`、`popup_panel`/`menu`/`dialog` 首帧单测均绿；`--pixel-check` first_visible==stable、`--debug-stack` menu/picker first==stable。
5. **damage 局部性**：`--pixel-check` damage_full_frames=0；Six kit 组件的 open/close `!damage_full` 断言；`popup_panel::opening_and_closing_a_popup_never_forces_a_full_repaint`。
6. **宿主迁移**：`WidgetServices/Cx/binding/test_support/Composition` 的 layer/animation 字段/参数/断言全部删除且无残留引用；业务 test_support 的 `overlays()` 改 `popup_stack` 并排除 Scrim（K1-07 为局限）；`kytrix` title_bar 测试改为 `[content..., popup_root, hud]` 语义；`instrument_selector` dialog 走 services 单例/路由/scope 回收测试仍绿。
7. **错误类型**：组件 open 返回 `Result<bool, PopupError>`；`DialogError::Popup(PopupError)`/`MenuError::Popup` 映射保留 source；无字符串错误；`PopupPanel` 的 bool 便捷方法内部吞错（`unwrap_or(false)`）与设计 draft 一致（仅记录）。
8. **状态与信号**：所有组件每次 sync 都调 `PopupPanel::sync` 探测引擎侧关闭（host 卸载/窗口销毁不再静默）；`release` 释放 scope/action（Select 有回收测试，Dropdown 无 scope 无需释放）；trigger a11y `expanded` 由 `open_state` 驱动、关闭同帧清除。

---

## 6. 残余风险

1. **真实窗口/三端未验证**：P1 验收仍以 headless 持久像素 + 真实窗口栈/rect 为主（设计已登记 P3-6）；context menu 在滚动/多窗口下的位置与菜单滚动关闭行为只有在真机验收才能确认（K1-01）。
2. **P2 MenuChain/P2-1..7 的接口基础**：当前 Menu 仍是“每层一个 Menu + parent popup id”，无 grace/safe-triangle/键盘目标状态；K1-01/K1-02 的处置会影响 P2-5 链式 dismiss 的实现口径，越早裁决越好。
3. **`--debug-stack` 首帧采样依赖 on_frame**：若后续重构帧 hook，first/stable 断言会退化为“末帧==末帧”（与 K1-03 同类风险），建议在 P2 把首帧断言下沉到组件单测（已有部分）。
4. **instrument_selector Dialog 736 宽**：小窗口/高 DPI 下的裁剪与缩放需真机确认（K1-08e）；旧实现是 400 宽面板 + 720 内容溢出，本次是修正但属视觉变更。
5. **`PopupPanel::sync` 每帧探测**：空闲时组件仍每帧调用（gallery idle 测试仅断言无绘制帧），未做 popup 工作量的性能基线（P3-5）。
6. **`menu/context` 的 Auto tier + opener toggle 与组件自身 toggle 双路径**：依赖引擎“opener 优先/Toggle 消费”规则；`Dropdown::open` 自身还保留了 is_open 判断（engine toggle 路径下可能走不到），当前测试覆盖两种路径，但 P2 链式 dismiss 改造时需复核双触发。

---

## 7. 结论

- 计数：**P0 = 0、P1 = 0、P2 = 5（K1-01..K1-05）、P3 = 8（K1-06..K1-13）**。
- 最关键 3 条：**K1-02**（非模态/Replay parity 变更无裁决，用户可见误触风险）、**K1-01**（设计 §4.7 菜单滚动/resize 关链矩阵未实现且测试固化相反行为）、**K1-03**（Dropdown/Select 首帧测试空转，P1-12 门禁无效）。
- **是否可提交 P1：暂不建议**。架构切换与门禁本身干净（无 P0/P1），但按“零 finding 才可提交”口径，需先完成：① lead/用户对 K1-01/K1-02 的裁决与登记（修实现或写偏差）；② 修复 K1-03（改首帧采样，约 1 小时）；③ 更新设计 §7.5/§11.5 的 `--dump-overlay` 为 `--debug-stack`（K1-04）；④ K1-05 的默认焦点语义确认。完成后 P3 项可留 P2/P3 阶段并逐条登记，不阻塞本次提交。
