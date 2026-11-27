# P2 对抗性审查（完整性 / 测试有效性 / 规范 / 文档）

- 审查对象：worktree `E:\kytrix\.agents\worktrees\dsh-1d175414-1a41`，基线 `152616b1`（P1 提交），P2 未提交工作树 + 新增 `crates/ui_kit/src/kit/menu_chain.rs`（untracked）；`git diff 152616b1` 14 tracked 文件 +4339/−1530。
- 快照：代码 mtime ≤ **12:49:07**（`menu.rs`），`menu_chain.rs` 12:43:52、`popup.rs` 12:47:49；代码指纹 `git diff 152616b1 | git hash-object --stdin` = `ac7019cbb59ea9065905daa87a36bc8babbc2132`，`menu_chain.rs` = `95150ff45606e494cf4e557090da483eb5b63d17`。文档在审查期间被并发编辑：`p2-summary.md` 13:03:47、设计文档 13:04:01（本报告以该快照为准；提交前应重跑文档核对）。
- 方法：只读（未改任何受版本控制的文件）；静态读码 + `git show 152616b1:<file>` 逐测试/逐 API 对照 + 门禁复跑 + 既有测试反推。命令统一 `CARGO_TARGET_DIR=E:\kytrix\target`。日志：`E:\kytrix\target\p2q-gate-{test,pixel,debugstack,clippy,fmt,wscheck}.log`、`p2q-test2.log`、`p2q_extract.py`。
- 结论：**P0 = 0，P1 = 1，P2 = 2，P3 = 6 → 当前快照不可提交**。P2 主功能面（MenuChain/2–3 级/键盘/typeahead/checkable/链式 dismiss/a11y 链/gallery）大体真实交付，门禁全绿；但存在 1 个可导致 hover 链失效、极端路径可冻结 UI 的状态机缺陷，且文档仍含多处与代码不符的表述，另有一处“新桥接机制”实际退化而其测试走的是另一条路径。

---

## 1. 范围与方法

1. `git diff 152616b1`（ui_core a11y/active_descendant 小补 + ui_kit menu/menu_chain/context_menu/popup_panel/gallery + dropdown/select/mod）。
2. 必读文档：设计 §3.4/§4.7/§4.8/§4.10/§5.2/§6 P2/§7.3/§13（13:04 新写）、`implementation/p2-summary.md`、P1 §12.6/§12.7、`AGENTS.md`。
3. 测试保留性：自写脚本 `E:\kytrix\target\p2q_extract.py` 从 `git show 152616b1:<file>` 与工作树分别抽取 `mod tests` 内全部 `fn` 的完整函数体，逐字节归一化比较：`menu.rs` 旧 35 / 新 70 个函数，**common=35、changed=0、removed=0**；`context_menu.rs` 旧 20 / 新 24，**common=20、changed=0、removed=0**（即 P1 的 Menu 30 / ContextMenu 14 条测试逐条保留、断言未被放宽）。
4. 门禁（本机实测，exit code 全部 0）：
   - `cargo test -p ui_core -p ui_kit` → 3825 passed / 0 failed / 2 ignored。
   - `cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings` → PASS。
   - `cargo +nightly fmt --all -- --check` → exit 0。
   - `cargo check --workspace --all-targets` → exit 0。
   - `cargo run -p ui_kit --example gallery -- --pixel-check` → PASS，94 checks、`damage_full_frames=0`、exit 0。
   - `cargo run -p ui_kit --example gallery -- --debug-stack` → PASS，chain 阶段 `entries 3→0`、`z=[2,3,4]`、parent 链正确，exit 0。
   - 首次测试因**另一 agent 的 gallery.exe 占用 target** 报 `LNK1104`（与 P1 审查同样现象），其退出后完整重跑全绿；非仓库缺陷。

---

## 2. Findings（Q2-01…Q2-09）

严重度口径：P0 崩溃/数据丢失/主功能不可用；P1 重大正确性缺陷（含可冻结 UI 的边界路径）；P2 中等缺陷/交付证据不成立/文档与代码矛盾；P3 轻微、已登记或覆盖不足。

### Q2-01 [P1] MenuChain 状态与 Menu/引擎的“关闭被取消/失败重试”不同步：hover 链失效，失败重试后还有自父槽死循环（UI 冻结）风险

**证据（代码路径）**

1. 引擎允许在 Closing 阶段取消关闭：设计 D5/§4.7 “若按 opener → Toggle（关闭并消费；若在 Closing 中则同 id 取消关闭并回 Opening）”。P1 测试 `a_trigger_press_during_the_close_transaction_reopens_the_menu`（`menu.rs:3869-3880`）即该路径。
2. kit 侧：`Menu::sync_own` 在 `DismissPending|Closing` 时**立即** `chain().close(level)`（`menu.rs:1556-1577`），`MenuChain::close` 把该层置 `Closed`（`menu_chain.rs:322-343`）。随后引擎把同一 popup 取消关闭并回到 `Opening/Open`，`sync_own` 的 `Opening|Open` 分支**什么都不做**，不会把链层恢复为 `Open`。
3. 结果：菜单视觉上重新打开，但链层是 `Closed`。`sync_chain`（`menu.rs:1673-1708`）用 `chain.open_levels()` 找命中层，`Closed` 层被过滤；`chain.contains(pointer)` 对根面板也为 false → 走 `pointer_left`，`deepest_kept()` 为 None 直接返回。于是 **`update_hover` 永不执行：hover 打开子菜单、grace/safe-triangle、兄弟替换全部失效**（键盘 Right/点击仍可打开子菜单，因 `open_submenu_row` 直接用 `self.level()`）。只有把菜单完整关闭到 `on_level_closed`（`menu.rs:1579-1593`）才 reset 恢复。
4. 同一根因的错误路径更危险：`Menu::open_focused` 在 `panel.open` 失败时 `chain().reset()`，但**不清 `MenuInner.level`**（`menu.rs:1386-1414`）。重试 open 时 `level()` 仍为 `Some(0)` → 跳过 `open_root`，而链已空；`confirm_open(level)` 对空链返回 false（`menu_chain.rs:282-303`）。此时 hover 因 `open_levels()` 为空而不会触发（见上），但 **键盘 Right（`key_open_submenu`，`menu.rs:2660-2666`）或程序化 `open_submenu` 会直接走 `open_submenu_row`**：`begin_open(parent=LevelId(0))` 在空 `Vec` 上 push 得到 **index 0 且 `parent=Some(LevelId(0))` 的自引用槽**（`menu_chain.rs:254-280`）；子菜单打开后下一次 `sync_chain → refresh_geometry → open_levels()` → `Levels::depth` 沿 parent 指针无限循环（`menu_chain.rs:653-661`）→ 帧内 sync 卡死。
   - 触发条件：root open 曾因 `panel.open` 失败（如 `Anchor::Node` 指向已删除节点 → `AnchorNodeMissing`、`InvalidGeometry`、`ContentBuild` 等）→ 宿主重试 open → 键盘 Right/宿主 `open_submenu`。概率低但后果是 UI 冻结，且强类型错误路径本应可恢复。

**影响**：P2 核心交互（hover 级联）在常见“关闭中途重开”路径下静默失效；错误重试路径可冻结 UI。现有测试只断言 `is_open`，未覆盖重开后的链行为。

**建议修复**：二选一并在 `sync_own` 的 `Opening|Open` 分支做状态修复——(a) 把链层关闭推迟到 `panel.state == Closed/None`（`on_level_closed`）而不是 `DismissPending`，这样引擎取消关闭时链层仍是 `Open/Opening`；(b) 保留现有关闭时机，但在 `Opening|Open` 分支检测链层状态，不在 `Open/Opening` 时重新 `open_root`/`confirm_open` 恢复。另需在 root 错误路径 `chain.reset()` 后 `inner.level.set(None)`。测试：扩展 `a_trigger_press_during_the_close_transaction_reopens_the_menu`，重开后 hover 子菜单行 + 200ms 断言 popup 栈为 2；再补“open 失败→重试→Right/open_submenu 不挂死（并断言链层状态已恢复）”。

### Q2-02 [P2] 文档（§13/§4.8/§7.3 与 p2-summary 当前快照）仍多处与代码/测试不符

`p2-summary.md`（13:03:47）与设计 §13（13:04:01）已修掉一部分旧错误（如 “PopupError 13→15”“2 个强类型错误”在 §13.1/§13.3/§3.3 已改为 13 变体），但快照仍有下列矛盾/不实表述：

| # | 文档位置 | 文档表述 | 代码/测试实际 |
|---|---|---|---|
| 1 | `p2-summary.md:5`、`:13` | “2 个强类型错误” / “`popup.rs` +1 字段 +2 `PopupError` 变体” | `PopupError` 前后均 **13 变体**（`popup.rs:473-533`），无 `A11yTargetMissing/OutOfScope`；§3.3（`:50`）已写“仍 13 变体”，同文件自相矛盾 |
| 2 | `p2-summary.md:13` | `a11y/popup_tests.rs +145（2 测试）` | 实际 **+1 测试**（`a_declared_active_descendant_is_projected_updated_and_cleared`，popup_tests.rs:1722）；同文件 §6 的 22 条计数与 +1 一致 |
| 3 | `p2-summary.md:37`、`:44`；设计 §4.8:574、§13.2:1455、§13.3:1464/1465 | `Menu/ContextMenu::bind_trigger` “左键 action + …” | 代码只绑定键盘：Menu 仅 `Alt+F10`（`menu.rs:1498-1522`），ContextMenu 仅 `Shift+F10/ContextMenu/F10`（`context_menu.rs:196-230`）。**没有左键绑定**；该 Action 也不返回/不单独暴露，宿主只能按内部命名 `{key}.trigger` 自行猜着绑（`menu.rs:1504`、`context_menu.rs:202`） |
| 4 | `p2-summary.md:44` | `open_on` “节点锚：取节点底边中点” | 取**底边左端点**：`LogicalPoint::new(bounds.origin.x, bounds.max().y)`（`context_menu.rs:186-192`） |
| 5 | `p2-summary.md:22`、`:62`；设计 §4.8:576、§13.2:1452、§13.4 D-P2-3 | “父行滚出视口**只关子级**” | 测试 `scrolling_the_parent_row_out_of_view_closes_the_submenu`（`menu.rs:4990-5022`）断言的是 **整链关闭**：`!menu.is_open && stack.is_empty()`。实现是子菜单 anchor Lost → `close_chain`（`menu.rs:2753-2769`）。设计 §4.8 原文允许“关该子级**或整链**”，但当前文档与测试名都在宣称“只关子级” |
| 6 | `p2-summary.md:21`、`:61`；设计 §4.8:575、§13.2:1451、§13.4 D-P2-2 | “父面板-子菜单 6 点桥接多边形” | 见 Q2-03：实现是“父行 rect ↔ 子面板”，且 `SUBMENU_GAP=ZERO` 时退化为子面板本身；不是“父面板” |
| 7 | `p2-summary.md:56`；设计 §13.3:1467 | “ContextMenu 把左键 action 挂到 panel surface … Menu 的行点击路径不变” | `surface_action` 是 **Menu 自己传的**（`menu.rs:1405`、`item_panel.open` 455-459）；ContextMenu 没有额外挂载；Dropdown/Select 传 None。描述的张冠李戴 |

**影响**：P2 的文档是提交/验收依据，错误表述会让后续阶段按错误 API/行为做集成。文档当前仍在被并发编辑，建议**提交前以代码为准再回写一轮**并重新走一遍本表。

### Q2-03 [P2] “6 点桥接多边形”在菜单里实际退化；其集成测试实际走 grace 三角，未覆盖新代码（交付证据不成立）

**证据**

- `bridge_polygon(anchor, child)`（`menu_chain.rs:127-149`）在 `child` 与 `anchor` 水平重叠（含相接）时返回 None；否则返回 `anchor_edge→child_near/far` 的六点。菜单子项锚点是**父行 rect**（`menu.rs:2028-2033` 先 `set_anchor(row_rect)`，`PlacementSpec::submenu()` + `gap = SUBMENU_GAP = 0`，`menu.rs:47`、`:2031-2033`），所以 `child.origin.x == anchor.max().x`（右侧）或 `child.max().x == anchor.origin.x`（左侧翻面）。此时六点多边形退化为“子面板矩形 + 零宽边”，不会比子面板 rect 多出任何面积；而 `dismiss_outcome` 只在“按下不命中任何 popup rect”时才被调用（引擎规则 1.4），所以 `bridge_contains` 的桥接部分**在菜单实际几何下不可能改变结果**。
- `a_press_inside_the_bridge_area_keeps_the_chain_open`（`menu.rs:5052-5073`）的点击点 `child.hit_rect.origin.x - 2.0, panel.max().y + 40` 位于子菜单左上外侧、父面板下方，落在 **从退出点（父行中心）到子面板的 grace 三角** 内；该测试在 `open_tall_chain` 后没有让指针离开/重入父行，因此 grace 仍存在（`Menu::update_hover` 的 `over_trigger` 分支在指针仍在父行时不会计算/清除；测试直接 click）。它验证的是 grace → `Ignore`，不是 `bridge_polygon`。
- 该单元测试用 20px 间隙的合成几何（`menu_chain.rs:866-887`），与生产 `gap=0` 不同；`a_bridge_is_rejected…` 还明确覆盖“水平重叠返回 None”。

**影响**：功能上无用户可见回归（grace 已提供同类保护），但这是 ~50 行生产代码 + 2 组测试形成的“交付证据”，实际未在生产几何下生效；文档/设计（Q2-02 #6）据此宣称 P2-2/P2-5 的桥接机制。按 AGENTS §1.2/§4/§6，应二选一：删除 `bridge_polygon/bridge_contains` 并同步文档；或保留则补一条能真正触发的集成测试（例如把 `SUBMENU_GAP` 设为非 0 的测试专用场景，或 grace 过期后指针位于间隙再按下），并明确止损点。

### Q2-04 [P3] 强类型/错误处理缺口

- `ContextMenu::open_on` 在节点不存在/未布局时静默回退到 `(0,0)` 并 `Ok`（`context_menu.rs:180-194`），与设计 §4.13“变更型公开 API 返回 `Result`、禁止静默 clamp”不一致；也未使用现成的 `PopupError::AnchorNodeMissing` 语义。
- `Menu::open_submenu` 对不存在/未挂载的行返回 `Ok(None)`（`menu.rs:1348-1353`、`open_submenu_row` 2000-2014 的 `row_node` None），调用方无法区分“无此子菜单/行被虚拟化/参数越界”。
- `Menu::install_content` 仍 `let _ = panel.set_content(...)`（`menu.rs:2540-2548`），引擎错误被吞（P1 遗留，非 P2 回归）。
- 引擎 `set_popup_a11y` 对非法目标每次调用都走“清空 + 诊断 + `mark_a11y` + `request_frame` 且返回 `Ok(true)`”（`app/popup.rs:851-889`）：若宿主每帧传同一非法 declaration，会持续触发脏标记/帧请求（诊断有去重与容量上限，不无限增长）；kit 当前只传合法目标，实际风险低。

### Q2-05 [P3] 测试有效性的弱项与覆盖缺口（无空断言，但若干断言不能证伪其命名行为）

1. `typeahead_appends_characters_and_expires_after_the_timeout`（`menu.rs:4626-4652`）：A→Alpha 后按 L 断言仍为 `Some(0)`。若实现**不追加**（只按 `l` 匹配），没有标签以 `l` 开头，高亮同样停在 0 → 该断言无法区分“追加匹配”和“无匹配”。追加语义只在 `menu_chain` 单测的 buffer 层覆盖，未在菜单匹配层覆盖。建议用 “Alpha/Arm/Alarm” 这类前缀可区分的条目。
2. `the_keyboard_table_pages_and_jumps_inside_a_long_menu`（`menu.rs:4450-4482`）：`page` 由实现的 `page_step()` 算出再断言 PageDown 等于它，属自证；应断言独立期望（或至少断言“PageDown 后末行仍在可视区/位置单调”）。
3. `a_hover_opened_child_keeps_its_own_active_item_while_the_pointer_moves`（`menu.rs:4859-4878`）：打开时 active 已是 0，hover 的又是 0；未构造“指针移动会改变/不应改变”的对照，接近恒真。
4. `a_press_inside_the_bridge_area…`（Q2-03）与 `scrolling_the_parent_row_out_of_view_closes_the_submenu`（Q2-02 #5）命名与断言不符。
5. 覆盖缺口（均无测试）：子菜单**翻面**后的镜像 grace（`side_of`+`SafeTriangle::new` 的集成）、混合来源（pointer 打开层 + 更深 keyboard 打开层）在指针离开链时的关闭边界（`deepest_kept` 只按“非 Pointer”取最深，pointer 层若比 keyboard 层浅则不会被关，设计 B3 的“关闭所有 hover-opened 层”未覆盖）、release 后重开、`declared active_descendant` 与焦点推导同时存在时的优先级（新测试没有同时制造焦点）。
6. 负向自检：P2 未新增变异/故障注入 harness（P1 §12.6-6 E-12 仍开放）；本次审查为只读，无法临时改代码做 mutation，未执行变异验证。

### Q2-06 [P3] 公共 API 面/生命周期细节

- `PopupPanel::on_dismiss` 做成 `pub`（`popup_panel.rs:279-284`），实际只有同 crate 的 `ItemPanel` 使用；`pub(crate)` 即可，避免把 `PopupDismiss/DismissOutcome` 契约扩成公开面（AGENTS §1.2）。
- `Menu::open_submenu` 返回子层 `Menu` 句柄，但 `on_activate/on_close` 写在链共享的 `MenuSink` 上（`menu.rs:1056-1093`、`:1488-1496`）：对子层调用 `on_activate` 会覆盖根回调；对子层调用 `on_close` 会覆盖根回调且 `fire_close` 只在 Root 触发（`menu.rs:2715-2722`）→ 根关闭回调永久丢失。建议子层句柄不暴露回调 setter，或回调只允许根层设置。
- `Menu::bind_trigger` 重复调用会把旧 action handler 留在 `App`（只覆盖 `trigger_action`，release 只 off 最新一个），`ContextMenu` 同理。
- `ContextMenu::bind_trigger` 额外绑定 **plain F10**（`context_menu.rs:214-217`），设计只要求 Shift+F10/ContextMenu；可能和宿主 F10 菜单栏语义冲突（summary/设计已把“F10”写入，属设计外扩张，建议显式登记）。
- `ContextMenu` 接受 `MenuEntry`（可含 checkable），但 `on_item_click` 只回传 `id`，丢失 `MenuActivation.check`（`context_menu.rs:262-279`）；P2 新增的 checkable 语义在 ContextMenu 公开面上不可用。

### Q2-07 [P3] R2-02 未闭环：kit 主动关闭仍固定上报 `Programmatic`，P1 §12.6-10 的 P2 映射要求未做且未回写

- `PopupPanel::close` 固定 `DismissReason::Programmatic` 调 `begin_close_popup`（`popup_panel.rs:457-472`）；`Menu::sync_anchor` 的 `Lost/Resized/Moved`、窗口 resize、`close_chain` 都走它（`menu.rs:2275-2278`、`:2753-2769`）。P2 新增的 `on_close(reason)` 只有测试覆盖 OutsidePress/Escape（`menu.rs:4903-4928`），anchor-lost/resize 会错误上报 Programmatic。
- P1 §12.6-10 原文是“P2 MenuChain 前需补 `close_with_reason` 或映射”；P2 已新增 `on_close(reason)` 消费者，但映射未做，设计 §12.6-10 未更新、只在 §13.7 把 R2-02 标成 P3 deferred（`design:1236`、`:1529`）。建议实现映射或在 §12.6-10/§13.7 明确“不在 P2 做”的理由并补 reason 测试。

### Q2-08 [P3] 性能/内存观察（无量化证据，P3-5 未做）

- 每帧成本（菜单打开时）：`sync_chain` 调 `refresh_geometry` → `chain.open_levels()` 分配一个 `Vec`（`menu_chain.rs:434-444`），随后 `sync_chain` 又调一次 `open_levels()`；`sync_tree` 每帧 `collect()` 子菜单 `Vec<Menu>`（`menu.rs:1542-1548`）；`sync_active_row` 每帧构建一次 `PopupA11y` 并 `set_popup_a11y`（相等短路，不标脏）。层级 ≤ 32，成本很小但属可消除的每帧分配，建议 `SmallVec`/复用缓冲，或至少登记为 P3 热点。
- `MenuChain.levels` 与 `MenuSink.owners` 只增不减（`begin_open` 对新的 `(parent,row)` push 新槽，`close` 只清状态不截断；`owners.resize` 同理），直到 root reopen 才 `open_root` 清 `levels`（`owners` 也不清）。长时间存活的 ContextMenu 被逐个 hover 大量不同行时会按“曾打开过的 (parent,row) 数”累积（每槽 ~百字节量级）；无泄漏级风险，但属无界增长形态。
- `typeahead_row` 每次按键 `buffer.to_lowercase()` + 每个候选 `label.to_lowercase()`（`menu.rs:2344-2372`），短菜单可忽略。
- 未做 popup open/close 帧预算或空闲帧基线（P3-5），P2 无性能断言；本次审查也未做微基准（只读约束，避免改仓库）。结论：未发现性能回归证据，也未发现实测支撑。

### Q2-09 [P3] gallery 与文档交接

- `chain_menu` 只作为 `--debug-stack` 的 `OverlayCheck` fixture 创建/使用（gallery.rs:1930-1963、424-500），**从未接入正常 UI 的 trigger，也未加入 `Gallery::sync` 的组件同步列表**（gallery.rs:2526-2534 无 `chain_menu.sync`）。因此它无法作为交互式 3 级演示；若后续给它接 trigger，hover 级联不会工作（MenuChain 的 sync 从未运行）。当前交互式 3 级演示实际由主 `menu` 的“更多→视图”嵌套子菜单承担（主 menu 已 sync）。
- overlay-check 把原 phase 2 的 `record_stack_empty(..., "menu-close")` 替换为 chain 阶段；菜单关闭后空栈改由 `chain-stack-size == 3` 间接覆盖（若旧菜单未关闭则 4 条），语义仍覆盖但标签丢失，建议在 chain 阶段加一条显式 `menu-stack-empty` 断言。
- `.docs/gui-migration/spec-deltas/kit-gallery.md` 未同步 P2 新增的 chain overlay-check 阶段（summary §8 声明 out-of-scope；设计 P3-7 的 kit-gallery 同步只覆盖 P1）。属文档债，建议登记到 P3。

---

## 3. P2-1…P2-7 与 P1 §12.6 遗留核对

| 项 | 结论 | 核对依据 |
|---|---|---|
| P2-1 MenuChain | **done** | `menu_chain.rs` 942 行/14 测试；per-level 状态、parent/child、close 子树、槽复用、timer 按 level 绑定（`Menu` 持 pending/grace/close/typeahead timer + 对应 `LevelId`）；单测覆盖状态迁移/复用/children-first |
| P2-2 safe triangle/grace | **done（桥接部分证据不成立，Q2-03）** | 5 点多边形 + ±5px 镜像（`the_grace_triangle_spans…`/`the_bleed_mirrors…`）；300ms `close_grace`；open/leave/flip 重算（`refresh_geometry`）；兄弟替换/离开/重入 4 条集成测试均真实（推进虚拟时间 200/300/400ms，非 sleep） |
| P2-3 面板行为 | **done（文档“只关子级”不实，Q2-02 #5）** | 高亮/ensure_visible（List scroll_to_item）、CaretRight 指示、Group/Separator/disabled、Checkbox/Radio 渲染与 a11y role、长菜单虚拟化/滚动均有测试 |
| P2-4 键盘表 + typeahead + checkable | **done** | ↑↓/Home/End/PgUp/PgDn/←→/Enter/Space/Esc/Tab/Shift+Tab/Alt+F10 绑定齐全（`bind_extra_keys` + 引擎 Enter/Space 合成 click + Esc 引擎），checkable/typeahead 各测试真实；IME 限制按 D-P2-5 登记 |
| P2-5 链式 dismiss | **done（bridge 证据不成立，Q2-03）** | 祖先层按下只关后代、Esc 逐层、replay/Consume、Esc 后 focus 归还均有测试；`on_dismiss` 引擎回调接通（`popup_panel.rs:421-434`） |
| P2-6 ContextMenu | **partially done（已登记）** | WindowRect/节点锚/Shift+F10/Menu/plain F10 + 2–3 级共享链已交付；**右键自动绑定未交付**（引擎 `secondary_button_neither_clicks_nor_takes_focus` 证明无右键 action 抽象；D-P2-4/P3 候选）。设计 P2-6 原文含“右键”，属于未完成项而非 bug |
| P2-7 3 级测试 + a11y 链 + gallery | **done** | `a_three_level_hover_cascade…`（真实 hover 3 级、z/parent 断言）、`a_three_level_a11y_chain_nests_and_projects_expanded`（父=父 menuitem、expanded=true）、gallery `--debug-stack` 实测 entries 3→0/z=[2,3,4]/parent 链全 PASS（本机 log） |
| P1 §12.6 A-25 `active_descendant` kit 未声明 | **closed** | 引擎 `PopupA11y.active_descendant` 投影到 content 根 + 声明值优先于焦点推导（`a11y/tree.rs:436-439`、`a11y/popup.rs:153-158`）；Menu 每帧 `set_a11y`；2 条引擎测试 + Menu a11y 测试验证；非法目标“清空 + 诊断”与 summary §3.3 一致（但 summary 顶部仍残留“2 错误变体”，Q2-02 #1） |
| P1 §12.6 MenuChain 未落地/仅 1 级 | **closed** | 任意级链（受引擎 MAX_POPUP_DEPTH=32） |
| P1 §12.6 键盘缺 typeahead/PgUp/PgDn/Alt+F10 | **closed** | 见 P2-4 |
| P1 §12.6 R2-02 关闭 reason 映射 | **未闭环** | 见 Q2-07（P3 deferred，映射未实现） |
| P1 §12.6 E-12 测试真实性残余 | **仍开放（P3）** | 菜单命名/变异自检/pixel 覆盖仍在；P2 新增测试亦有弱项（Q2-05） |
| P1 §12.6 kit-gallery §13 同步 | **P2 新增内容未同步（P3 文档债）** | 见 Q2-09；summary §8 声明 out-of-scope |

---

## 4. 测试统计与有效性

### 4.1 统计

| 指标 | 实测 |
|---|---|
| `#[test]`：Menu / MenuChain / ContextMenu | 56（P1 30 + 26）/ 14 / 17（P1 14 + 3） |
| P2 新增 kit 测试 | **43**（26+14+3），与任务口径“新增 43 条”一致 |
| P2 新增引擎测试 | **2**（`a11y/popup_tests.rs` +1、`tests/popup_behavior.rs` +1） |
| P1 测试保留 | Menu 30/30、ContextMenu 14/14，函数体**逐字节未改**（脚本 common=35/20、changed=0） |
| 全量包测试 | 3825 passed / 0 failed / 2 ignored（ui_core lib 2805+1、frame_record 8、gui_smoke 0+1、layout 4、popup_behavior 111、snapshot 14、demo 39、ui_kit 844） |
| 逐文件计数核对 | 文档 §6 所列 popup/tests 43、placement 34、popup_behavior 111、a11y 22、menu 56、menu_chain 14、context 17、popup_panel 18、dropdown 27、select 32、dialog 54、tooltip 28、alert 11、color_picker 80 **全部与实测一致** |

### 4.2 有效性结论

- 无空断言/无重复计数/无真实 `sleep`（时间推进用 `app.advance` 虚拟时钟）；关键负向断言存在（禁用项不激活、越界不环绕、`outside_press=Consume` 不 replay、Esc 逐层、关闭后 entries=0）。
- 新增测试总体真实覆盖 P2-1..P2-7；P1 测试零放宽是强证据。
- 弱断言与缺口见 Q2-05；最需要补的两条：Q2-01 的“重开后 hover 仍工作”、Q2-03 的“桥接路径真实触发”。
- 门禁绿、像素门禁 94 checks、debug-stack 真实窗口断言 PASS（本机复跑，非引用 summary 数字）。

---

## 5. 规范扫描（AGENTS.md）

| 规则 | 结果 |
|---|---|
| 不写注释 | 新增 4339 行 + `menu_chain.rs` 全文：注释 **0** |
| 禁止 `#[allow]/#[expect]/#[path]` | 新增代码 **0** |
| 新依赖/workspace 依赖 | `git diff` 无 `Cargo.toml/Cargo.lock` 改动；`menu_chain.rs` 仅用既有 `smol_str`/`ui_core` |
| 生产代码 `unwrap/expect/panic/unreachable/unsafe/todo/dbg` | Menu/MenuChain/ContextMenu 生产段扫描 **0**（expect 仅在 tests） |
| `serde_json::Value/RawValue/json!` | 新增代码 0 |
| SmolStr | 类型/回调/`format_smolstr!` 使用正确；`String` 只出现在迭代临时（`typeahead_row` 的 `to_lowercase`）；无 `format!`→SmolStr 中转 |
| 强类型错误 | 大体符合（`PopupError` 贯通、`A11yDiagnostic` 强类型）；缺口见 Q2-04/Q2-07 |
| 模块边界 | `menu_chain` 新模块只公开 `HoverTimings`，其余 `pub(crate)`；`menu.rs`/`context_menu.rs` 分工合理；`PopupPanel::on_dismiss` 公开面偏大（Q2-06） |
| 死代码 | `bridge_polygon` 的实际可达性存疑（Q2-03）；`MenuChain::deepest_open/state/source/anchor/active` 等为 `#[cfg(test)]` 属正常测试接口；`ItemPanelOptions.roles.row` 在 Menu 路径未读（Dropdown/Select 使用，非死） |
| 公开 API 合理性 | `MenuOptions/MenuAction/MenuCheck/MenuCheckState/MenuActivation/outside_press/timings` 合理；`open_submenu` 为程序化建链引入、仅供测试/gallery，且返回共享 sink 的子层句柄有回调覆盖风险（Q2-06）；`bind_trigger` 名不副实（Q2-02 #3） |

---

## 6. 性能/内存观察

- 无实测基线（P3-5）；静态热点：每帧 2×`open_levels()` Vec、子菜单 `collect()`、每层 `set_popup_a11y`（相等即短路）；`refresh_geometry` 每帧重算 popup/行 rect（深度 ≤32，量级小）。
- 无 timer 泄漏：`Menu` 四个 timer 在 `cancel_timers`/`release`/`on_level_closed`/`close_submenus` 路径均 Cancel；引擎移除子树时会清 scoped bindings（`app/window_set.rs:136-150`）与 timer scope。
- `MenuChain.levels`/`MenuSink.owners` 按“历史上打开过的 (parent,row)”增长、root reopen 才清；无界但量级小（Q2-08）。
- `typeahead` 有 `to_lowercase` 分配；可忽略。

---

## 7. 文档核对（快照 13:03:47 / 13:04:01）

- **已写**：设计新增 **§13**（13.1 提交内容/13.2 P2 结果/13.3 API/13.4 D-P2-1…9/13.5 P1 遗留/13.6 门禁/13.7 P3/13.8 对齐）；§4.8 增“P2 落地口径（rev.9）”；§5.2/§7.3 已更新 API/测试落点；`p2-summary.md` 已补 §6 门禁基线与 §7 P3 清单。§3.4 已把 200ms/300ms 标注为 `HoverTimings` 承载。
- **数字核对**：diff 规模、文件行数、测试数、门禁数字（3825/94 checks/exit 0）、`PopupError 13`、`a11y+1/popup_behavior+1`、`+45` 增量均与实测一致（除 Q2-02 #1/#2 的残留矛盾行）。
- **不实/不一致**：Q2-02 表格 7 项；另 `design §12.6-10` 未回写 P2 已新增 `on_close(reason)` 消费者（Q2-07）；`kit-gallery.md` 未同步 P2 chain 阶段（Q2-09）。
- **过程提示**：文档在审查期间被另一 agent 修改（两个 mtime 13:03/13:04），且 `p2-summary` 内部仍自相矛盾——说明文档回写尚未收口。**提交前必须重新跑一遍本表**；若文档继续变化，以提交时快照为准。

---

## 8. 残余风险

1. **无变异/故障注入**（E-12 遗留）：新增 43 条测试是否“真能失败”只靠读码判断；Q2-03 已发现一条“测试通过但机制未触发”的实例。
2. **无真实窗口交互验收**：3 级 hover/斜向/sibling/Esc/外部点击/键盘/滚动在真实窗口的全路径（设计 P2-7 验收）未执行；`--debug-stack` 只覆盖程序化 3 级链 + 栈/rect；真实窗口像素/点击留 P3-6。
3. **性能未测**（P3-5）：无 open/close 帧预算与空闲帧基线。
4. **右键入口缺失**（D-P2-4）依赖引擎 pointer-button action 抽象，当前宿主只能自行拦截原生事件调 `open(point)`。
5. **Q2-01 的状态机脆弱性**提示后续任何“引擎可能取消/改写 popup 状态”的路径（a11y dismiss、focus lost、window state）都需要核对链状态同步，建议加一个“链状态不变量”单测/调试断言。
6. **workspace 测试未跑**（沿用 P0/P1 限制，仅 `cargo check --workspace --all-targets` 绿）；widgets/业务宿主行为未验证。

---

## 9. 结论

- 严重度计数：**P0 = 0，P1 = 1，P2 = 2，P3 = 6**。
- **当前快照不可提交**：Q2-01 需要修代码并补测试（重开后 hover 链 + 失败重试不挂死）；Q2-02 需要按代码回写文档（尤其 §4.8/§13/p2-summary 的 `bind_trigger` 左键/`open_on` 中点/“只关子级”/6 点桥接/PopupError 残留矛盾）；Q2-03 需删除或补真实测试。P3 项可在提交前登记（R2-02/E-12/perf/API 面/kit-gallery 文档债）或修复。
- 若把 Q2-01 修复、Q2-02 回写、Q2-03 决断完成，且 P3 项全部登记/接受，则 P2 的代码主体（MenuChain/2–3 级/键盘/typeahead/checkable/链式 dismiss/a11y 链/gallery 断言）与门禁状态满足提交条件；**P2-6 右键未交付必须作为已登记偏差随提交说明保留（D-P2-4/P3）**。
- 正向确认：P1 的 Menu 30 / ContextMenu 14 条测试逐条保留未放宽；P2-1..P2-5、P2-7 均有真实代码与测试覆盖；`active_descendant` 引擎语义与 kit 接线一致；无注释/allow/新依赖/生产 unwrap；clippy/fmt/workspace check/测试/pixel-check/debug-stack 全绿。

---

## 附录 A：本报告引用的门禁日志

| 命令 | 日志 | 结果 |
|---|---|---|
| `cargo test -p ui_core -p ui_kit` | `E:\kytrix\target\p2q-gate-test.log`（补充 `p2q-test2.log`） | 3825 passed / 0 failed / 2 ignored，exit 0 |
| `gallery --pixel-check` | `E:\kytrix\target\p2q-gate-pixel.log` | 94 checks PASS，exit 0 |
| `gallery --debug-stack` | `E:\kytrix\target\p2q-gate-debugstack.log` | PASS，chain entries 3→0，menu_frames=416/picker_frames=350（浮动），exit 0 |
| `cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings` | `E:\kytrix\target\p2q-gate-clippy.log` | exit 0 |
| `cargo +nightly fmt --all -- --check` | `E:\kytrix\target\p2q-gate-fmt.log` | exit 0 |
| `cargo check --workspace --all-targets` | `E:\kytrix\target\p2q-gate-wscheck.log` | exit 0 |
| 首次测试（并发占用） | `E:\kytrix\target\p2q-test.log` | `LNK1104` 环境冲突（另一 agent 的 gallery.exe），重跑绿 |

## 附录 B：P1 测试保留核对

- 脚本：`E:\kytrix\target\p2q_extract.py`（`git show 152616b1:<path>` vs 工作树，抽取 tests 内函数体、归一化空白后比较）。
- 结果：`menu.rs` old=35/new=70，removed=0，common=35，changed=**0**；`context_menu.rs` old=20/new=24，removed=0，common=20，changed=**0**（额外 26/3 个函数为 P2 新增测试/helper）。
- 因此 P1 的 30 条 Menu 与 14 条 ContextMenu 测试**逐条保留且断言未放宽**。
