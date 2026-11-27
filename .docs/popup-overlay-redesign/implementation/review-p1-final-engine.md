# P1 终审对抗性审查（引擎 + kit 整体行为，review-p1-final-engine）

- 审查对象：worktree `E:\kytrix\.agents\worktrees\dsh-1d175414-1a41`，基线 `d9949aca`（P0），P1 全部未提交工作树（当前 `git status`：`crates/ui_core`/`crates/ui_kit`/`crates/widget`/`crates/kytrix`/业务 ui/widgets 共 111 文件 + 7 个新增路径；`.docs` 被 `.gitignore` 忽略）。
- 审查范围：P1 引擎 popup 子系统（settle/dismiss/focus/animation/placement/stack/a11y/damage）与 kit 全组件迁移（PopupPanel/Dropdown/Select/Tooltip/Menu/ContextMenu/Dialog/Alert/ColorPicker）、宿主迁移、旧 API 删除、gallery 门禁；并逐条复核 `review-p1-engine-behavior.md`（B-01…B-17）与 `review-p1-engine-api.md`（A-01…A-34）。
- 只读约束：未修改任何仓库文件。所有实验代码在仓库外 `E:\kytrix\target\p1-final\exp`（独立 crate，path 依赖指向本 worktree；不改仓库）。审查结束 `git status --short` 不新增本报告之外的文件（`.docs` 被忽略）。
- 审查快照时间：2026-11-27 09:xx（CST）。审查期间设计文档 §12（P1 回写，1045-1304 行）已由另一 agent 并发写入；本报告对 §12 的声称做了独立复核（见 E-05/E-13）。

## 1. 范围、基线与门禁

### 1.1 读过的设计/证据/早审

- `.docs/popup-overlay-redesign-design.md`：§4.1–§4.14、§5.1/§5.2、§6（P1/P2）、§7.1/§7.3/§7.5、§8、§10、§11（P0 回写）与并发写入的 §12（P1 回写）。
- `.docs/popup-overlay-redesign/implementation/review-p1-engine-behavior.md`、`review-p1-engine-api.md`、`p1-cleanup-w7.md`、`p1-migration-inventory.md`（D1/D3/D6/R1–R11 决策）。
- 实现：`git diff d9949aca -- crates/ui_core crates/ui_kit crates/widget crates/kytrix`；重点通读 `popup.rs`、`popup/{anchor,animation,dismiss,focus,placement,settle,stack,tests}.rs`、`app/{popup,frame,input,focus,window_set}.rs`、`layout/hit_test.rs`、`paint/pass.rs`、`node/tree{,/*}`、`window{,/*}`、`a11y/{popup,popup_tests,action,adapter,mapping,tree}.rs`、`kit/popup_panel.rs` 与 8 个组件、`examples/gallery.rs`、`tests/popup_behavior.rs`（4068 行/92 条）。

### 1.2 门禁结果（本机，`$env:CARGO_TARGET_DIR='E:\kytrix\target'`）

| 命令 | 结果 |
|---|---|
| `cargo test -p ui_core -p ui_kit` | **PASS**：3734 passed / 0 failed / 2 ignored（ui_core lib 2804 + popup_behavior 92 + frame_record 8 + layout 4 + snapshot 14 + demo 39 + ui_kit 773）。首次执行因并发 agent 正运行 `gallery.exe` 导致 LNK1104（gallery.exe 被占用），重跑 PASS；非代码问题。 |
| `cargo test -p ui_core --features test-support --test popup_behavior` | **PASS**：92 passed |
| `cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings` | **PASS** |
| `cargo +nightly fmt --all -- --check` | **PASS** |
| `cargo check --workspace --all-targets` | **PASS** |
| `cargo run -p ui_kit --example gallery -- --pixel-check` | **PASS**：checks=93、sampled_frames=32、open=8/close=12、damage_full_frames=0、first_visible==stable=(496,91,300×375)、moved=(496,4,300×375)，exit 0 |
| `cargo run -p ui_kit --example gallery -- --debug-stack --auto-close=9` | **PASS**：overlay-check menu 度量 `(6,180.0,127.5125)`、picker children=4、menu_frames=485、picker_frames=475、最终 entries=0，exit 0 |
| `rg` 旧 API 零引用（7 组组合模式，等价语义） | **0 命中**（`OverlayOptions/OverlaySlot/AnimationRouter/kit::layer/LayerSlot/create_overlay/.overlays()/dismiss_overlay/place_overlay/MenuMode::ListBox/overlay_wrapper`） |

门禁绿，但绿门禁不能覆盖本报告 E-01/E-02/E-03/E-04 四类问题（现有测试/像素门禁均未覆盖：模态关闭期的背景输入、遮罩实际颜色、面板阴影像素、小视口下的内容可达性）。

### 1.3 仓库外探针（实验设计）

- 位置：`E:\kytrix\target\p1-final\exp`（`cargo run --manifest-path ...`；`[workspace]` 独立；`ui_core` 开 `test-support` feature）。
- 方法：`TestApp` + 真实帧泵 `drain/advance` + `popup_stack/popup_rect/popup_state/app.focused/probe.read_pixels`；所有时序用 `advance` 推进，不用 sleep。
- 实验：
  - EXP1 打开 kit Dialog（fade=false）读 scrim 的 Quad 颜色；
  - EXP2 打开 kit Dialog → `close()` → DismissPending 期间点击背景按钮，读 action 计数；
  - EXP3/EXP6 260×200 视口打开长 Menu，检查 popup 高度/内容底边/键盘 End 后末行位置；
  - EXP4 直接 `open_popup` 设 `match_anchor_width=true`（锚点 200、内容 50）；
  - EXP5 菜单 a11y 快照（trigger/Menu 记录的 has_popup/controls/active_descendant）；
  - EXP7 hover 打开子菜单后点击父行（D5/§4.8 不 toggle）；
  - EXP8 菜单 close_now 后重开；
  - EXP9 通过 `PopupPanel`（menu surface）与普通带阴影面板对比面板外 1–20px 像素；EXP13 进一步区分 self-clip / ancestor-clip；
  - EXP10 外部 `remove_node` 删除 popup content 后焦点与 Tab；
  - EXP11 opener press 消费后窗口失焦，再点背景按钮；
  - EXP12 a11y 路径的动作（MenuItem 行节点上的 action）；
  - EXP14 320×220 视口打开高 Dialog，检查 confirm 按钮可达性。
- 关键原始输出（cargo run 本次）：
  ```
  EXP1 scrim premul rgba=[0.0, 0.0, 0.0, 1.0] alpha=1 (old theme scrim alpha=0.42)
  EXP2 state after close+frame: is_closing=true popup_state=Some(DismissPending)
  EXP2 during-modal-fade background clicks=1 down_hit=Some(背景按钮) (expected 0 under modality)
  EXP3 viewport=260x200 popup_hit=200x192 popup content max bottom=338 clipped=true
  EXP6 highlighted=Some(11) last_row_bounds y=338 popup_hit=200x192 last_row_fully_visible=false
  EXP9 panel_rect=(100,105,200x108) pixels +1..+20=255 (control plain shadow=245/250/253)
  EXP10 before removal focused=Some(n3); after removal focused=None; after Tab focused=None
  EXP11 opener press consumed -> focus lost -> first background click=0, second=1
  EXP12 AT-like Click on MenuItem: handled=true activated=0 menu_open=true highlighted=Some(0)
  EXP13 below self-clip panel=245 (shadow visible); below ancestor-clip child=255 (shadow clipped)
  EXP14 viewport=320x220 popup=312x212 confirm bounds y=337..367 confirm_reachable=false
  ```

## 2. Findings（E-01…E-16）

严重度口径沿用早审：P0=正确性缺陷/硬要求违反/回归/门禁虚假通过；P1=重大健壮性或用户可见回归；P2=次要问题（边界/可用性/API）；P3=风格/文档/测试缺口。**P0 阻塞提交。**

### E-01（P0，阻塞）模态关闭事务（DismissPending/Closing）不再阻断背景输入：淡出 90ms 内点击会落到背景控件并执行 action

- 位置/证据链：
  - `crates/ui_core/src/popup/stack.rs:559-565`：`topmost_modal()` 只接受 `Opening|Open`；DismissPending/Closing 的 modal 不再算模态。
  - `crates/ui_core/src/popup/dismiss.rs:319-342`：`on_pointer_down` 的 modal scope 分支依赖 `topmost_modal()`；无 modal 时按普通 press 派发。
  - `crates/ui_core/src/popup/dismiss.rs:78-108`、`crates/ui_core/src/layout/hit_test.rs:360-373`：`block_entry` 给 modal/scrim 节点写 `blocked:true`，hit_test 直接返回 `None`；scrim 又因 `popup_at`/`topmost_modal` 过滤不再是“外部点击”路由目标 → 按下落到背景节点。
  - 设计 §4.7 事件模型（design:494）：“kit `PopupPanel` 统一处理：置 `DismissPending`（立刻不可命中/不可聚焦/**吞掉落在 `paint_rect` 内的按下**）”；§6 P1-5（design:767）把“**Closing/DismissPending 输入阻断**”列为 P1 交付项；D6/G6“模态浮层阻断下层输入”；§4.12“关闭开始即不可命中（DismissPending）”并不等于“把输入放行给背景”。当前实现把“不可命中”做成了“hit 穿透”，没有“吞掉按下”。
  - 设计并发 §12.2 P1-5 的完成项里没有“Closing/DismissPending 输入阻断”，§12 未登记此行为变化（漏项，见 E-13）。
- 独立复现（EXP2，kit Dialog 真实组件）：
  - `dialog.open` → drain → `dialog.close` 后同帧 `popup_state=Some(DismissPending)`；
  - 在淡出期间点击 (100,550) 背景按钮：`InputOutcome.hit=Some(背景按钮)`，背景 `on_click` 计数 = **1**（期望 0）。
- 影响：按 Esc/确认关闭 Dialog 后的 90ms 内，用户第二次点击（或双击确认）会命中背景按钮/列表/菜单，执行误操作；同时背景 hover/滚动/新 Tooltip 也在淡出期恢复（`topmost_modal`=None、`hovered/scroll` 的 scope 亦为 false）。这是安全/正确性回归，且与“DismissPending 吞掉 paint_rect 内按下”的硬要求冲突。
- 修法建议：引擎把“存在 DismissPending/Closing 的 Trap modal（含其 scrim）”视为输入阻塞期：`on_pointer_down` 在 modal 消失前若命中任何 closing modal/scrim 的 `paint_rect`（scrim 即全视口）→ Consume（并 `suppress_release`）；scroll/hover 同样走 scope 判定但允许“关闭期不许新 hover/tooltip”。或让 scrim 保持可命中但仅消费输入、不作为 dismiss 目标，直到移除。补测试 `closing_modal_swallows_background_presses_until_removed`。
- 是否阻塞：**是（P0）**。

### E-02（P1）Dialog 遮罩由“主题色 42%/55% 黑”变成 100% 不透明黑：`ScrimSpec.density=1.0` + 引擎黑色硬编码，主题 `Role::Scrim`/`ComponentColors.backdrop` 生产死代码

- 位置/证据：
  - `crates/ui_kit/src/kit/dialog.rs:49` `const SCRIM_DENSITY: f32 = 1.0;`，`:392-394` 传给 `ScrimSpec`。
  - `crates/ui_core/src/app/popup.rs:38-43` `scrim_paint(density)` = `Color::srgb(0,0,0,density)`；`:145-163` scrim 节点只用该 PaintSpec。
  - 旧实现（基线 `d9949aca` 的 `crates/ui_kit/src/kit/overlay.rs:486-489,1004-1025`）：`Scrim::default().density=1.0` 作用于 `Role::Scrim` 颜色（`bind_style{background: theme.role(Role::Scrim), opacity: density}`），主题色为 `black_with_alpha(0.42/0.55)`（`theme/palette.rs:101,152`、`theme/roles.rs:232`）；即旧 Dialog 背景是 42%（Light）/55%（Dark）黑。
  - EXP1：Dialog 打开后 scrim Quad 的 premultiplied RGBA = `[0,0,0,1.0]`（alpha=1.0）。
  - `Role::Scrim` 当前只被 `theme/components.rs:364-372` 的 `backdrop` 消费，而 `backdrop` 全仓无生产读取点 → 主题遮罩色成为死字段；AGENTS §2.1。
- 影响：所有 Dialog/Alert 背后被纯黑完全盖住（应用界面不可见），与旧视觉/主题语义明显不一致；Dark/Light 无差异；写回 §12.3-D-P1-? / §12.7-B-02 只登记了“density 已用”，没有登记该项视觉变化。
- 修法建议：在 kit 侧按主题取 `scrim_alpha`（或给 `ScrimSpec` 增加 `color`/`opacity`）后传 density=0.42/0.55；若坚持 engine 黑色，删除 `Role::Scrim`/`ComponentColors.backdrop` 死代码并在 §12 登记“遮罩不再随主题”。补像素或 primitive-alpha 断言（如 scrim quad alpha == 主题 alpha），并在 `--debug-stack`/`--theme-check` 覆盖 Dialog。
- 是否阻塞：建议修（视觉回归 + 死主题字段）；若用户明确接受“全黑遮罩”可降级，但必须登记偏差并删除死字段。

### E-03（P1）所有 `PopupSurface` 阴影被引擎 popup 内容根 `overflow_hidden` 裁剪：Menu/Dialog/ColorPicker 的投影全部不可见

- 位置/证据：
  - `crates/ui_core/src/app/popup.rs:20-27`：每个 popup 的内容根样式固定 `absolute + overflow_hidden()`。
  - `crates/ui_kit/src/kit/popup_panel.rs:645-646`：panel 节点 `paint.shadows = surface.shadows`；`PopupSurface::menu()/dialog()` 定义多层阴影（`:43-94`）；panel/body 也各带 `overflow_hidden`（`:622-634,660-667`）。
  - 引擎 clip 语义（`paint/pass.rs:466-509`）：节点自身的 overflow hidden 不裁自己的绘制（`clip.push` 在 `rebuild_lane` 之后，只作用于子节点），但**祖先**的 clip 会裁子节点绘制。EXP13 对照：带 shadow 的节点自身 `overflow_hidden` 时面板外 1px = 245（阴影可见）；把它放进同为 200×100 的 `overflow_hidden` 容器后，面板外 1px = 255（阴影被裁）。
  - EXP9（kit `PopupPanel` + `PopupSurface::menu()`，白色背景，面板 rect=(100,105,200×108)）：面板下边缘 +1/+2/+4/+8/+20px 像素全部 `[255,255,255]`；普通带同款阴影的面板（EXP9C）=245/250/253/255（阴影可见）。EXP9 的 primitive 中 Shadow 节点的 clip 上限正好是 popup 内容根/面板边界。
  - 旧基线 `kit/overlay.rs` 的 panel 无 `overflow_hidden` 祖先（`build_panel` 只有 flex/padding/paint），旧 `OverlaySurface` 阴影可见。
- 影响：`PopupSurface.shadows` 作为 §5.2 的公开外观能力在 P1 全量失效——菜单/对话框/取色器丢失投影，视觉与旧版不一致；`painted_extent`/damage 还包含这些不可见阴影（无害但语义不符）。现有测试只断言 `Shadows` 数据（`color_picker` 的 surface 断言）与生命周期，没有面板外像素断言，故门禁全绿。
- 修法建议：为 popup 内容根预留“阴影 pad”（例如 kit 在 `popup-content` 与 panel 之间加一层透明 wrapper，padding = 最大 shadow extent；或引擎在 `PopupContentStyle` 上开放 `paint_pad`，并把 hit_rect 与面板 rect 分离）；或者把 `overflow_hidden` 下移到真正需要裁切的滚动/Canvas 内容层，不动 popup 内容根。补“面板外 2px 存在阴影像素/Shadow primitive 未被祖先 clip 完全裁掉”的回归测试。
- 是否阻塞：建议修（用户可见回归）；若接受无阴影需登记并删除 `shadows` 数据/文档，避免“声明存在但不可见”。

### E-04（P1）`constrained_size` 没有 kit 消费者：窗口小于 popup 的 max_height/max_width 时，内容被引擎钳制并直接裁掉，Menu 行/ Dialog 按钮不可达；设计 §4.5/P1-4 的“确定高度 viewport + 内部滚动”未实现（§12.2 P1-4 的声称不成立）

- 位置/证据：
  - 引擎会算并暴露约束：`crates/ui_core/src/popup/settle.rs:371-391` 设置 `PopupEntry.constrained_size`；`crates/ui_core/src/app/popup.rs:696-723` 在 snapshot 暴露；`popup_behavior::a_constrained_popup_exposes_its_applied_size_without_unsettled_churn` 只验证引擎数值。
  - kit 全仓无 `constrained_size`/`PopupDiagnostic::Resized` 读取点（`rg` 仅 gallery 打印与测试）。
  - `crates/ui_kit/src/kit/menu.rs:504-516` `ItemPanel::content_height()` 只用 kit 自己的 `MenuOptions.max_height`（默认 320），`write_height` 把 surface 高度写为该值；`popup_panel.rs` 的 panel/body 也不读实际 popup 高度。
  - EXP3/EXP6（260×200 视口，12 行菜单）：popup `hit_rect` 高度 192（引擎 max_size），但 popup 子树内容底边 = 338；键盘 End 高亮末行后末行 bounds y=338（在 popup 外），List 的滚动视口仍按 surface 高度（312）计算 → **末行无法滚入可见区**。
  - EXP14（320×220 视口，10 行内容 + confirm 的高 Dialog）：popup/panel 高度 212，但 confirm 按钮 bounds y=337..367（面板外），水平方向 confirm 宽 384 > 面板宽 312 → **确认按钮不可见/不可点**。
  - 设计 §4.5“PopupPanel 在 resized=true 时把内容放进一个确定高度 viewport（height=max_height，overflow_hidden + 内部 List/ScrollBar），而不是仅靠 max-height 裁剪”；§12.2 P1-4 声称已实现 → 与实际不符（文档错误，见 E-13）。
- 影响：窗口高度 < Menu/Dropdown/Picker 的默认 320 或 Dialog 内容较高时，用户看不到也点不到底部条目/按钮；不是纯视觉问题。正常 800×600 窗口不触发，属“窗口可缩放到小尺寸”的真实场景。
- 修法建议：kit 在 `PopupPanel::sync`/组件 sync 中读取 `app.popup_rect(id).hit`（或新增 `constrained_size` 查询）后，把实际可用高度传给 surface/body：菜单 `height = min(content_height, popup_rect.height - padding*2)` + List 内部滚动；Dialog 把内容区设固定高度并可滚动、footer 固定（§4.5）。需要首帧前生效（engine 在 settle 后才给出），可接受一帧测量或在 settle 后下一帧回写并 request measure。补小视口菜单/Dialog 可达性测试（键盘 End + 点击）；同时修正 §12.2 P1-4 表述。
- 是否阻塞：**建议修复或由 lead 明确降级为 P2/P3 并登记**；这是 P1-4 交付项与设计 §4.5 的直接缺口。

### E-05（P2）外部删除 popup content 节点后 `PopupStack::remove` 不清 `focus.owner`，`validate_owner` 又在 entry 不存在时直接返回：焦点永久失效（Tab 无法恢复）；§12.3-D-P1-21“validate_owner 会修复失效 owner”不成立

- 位置/证据：
  - `crates/ui_core/src/popup/stack.rs:305-330`：`remove` 清理 handlers/order/animations/focus.records/max_sizes/parent/child/scrim，**未清 `focus.owner`**。
  - `crates/ui_core/src/popup/focus.rs:145-166`：`validate_owner` 在 `stack.entry(owner)` 为 Err 时 `let Some(content) = ... else { return; }` 直接返回，不会修复 owner。
  - 外部删除路径 `crates/ui_core/src/app/window_set.rs::remove` 只调用 `popup::reap_orphan_scrims`（修剪 entry/scrim），不恢复焦点。
  - EXP10：打开 `FocusPolicy::Take` popup（content 内可聚焦节点）→ `focused=Some(n3)`；`App::remove_node(content)` → entry 被 prune、栈空，但 `focused=None`，再按 Tab 仍 `None`（主树焦点被 stale focus owner 屏蔽）。
  - 写回 D-P1-21 声称该路径由 `validate_owner` 修复；与代码/实验不符。
- 影响：公开 API（`App::remove_node`，widgets/kit 重建内容时也会删节点）删除 popup content 后，键盘焦点/IME/焦点环全部失效，直到另一个 popup 正常开关才可能恢复；与 AGENTS 的强类型/无悬挂状态精神不符。正常 `close_popup`/`PopupPanel` 路径不受影响（会先 `release_focus_on_close`）。
- 修法：`PopupStack::remove` 时若 `focus.owner == Some(id)` 则清 owner 并（在 app 层）把焦点恢复到主树/fallback；或让 `validate_owner` 在 entry 缺失时清 owner 并 `set_owner None` + 主树 fallback。补“外部删除 popup content 后焦点恢复、Tab 可用”的回归测试；修正 D-P1-21 文档。
- 是否阻塞：否（不受支持的删除路径，但公开 API + 文档错误）。

### E-06（P2）`suppress_release` 在窗口失焦/取消指针会话时不重置：窗口失焦后的第一次点击被静默吞掉（B-15 遗留，§12.7 已注“留待终审”）

- 位置/证据：
  - `crates/ui_core/src/app/input.rs:218-227`：`PressRouting::Consume` 时 `pointer.suppress_release = true`。
  - `:427-439`：下一次 `on_pointer_up` 取走 `suppress_release` 并吞掉 release。
  - `cancel_pointer_session`（`:916-950`）与 `on_window_focus_lost`（`:901-914`）清 press/pending_drag/hover/capture，但**不清 `suppress_release`**。
  - EXP11：popup 打开时按 opener（Toggle 消费 press，suppress=true，不释放）→ `window_focus_lost` → 点击背景按钮：背景 action 计数 **0**；再点一次才变 1。
- 影响：press 被消费（opener Toggle/模态消费/Replay 等）后窗口失焦（Alt-Tab/最小化）再回来，第一次点击被吞；用户感知为“点一下没反应”。B-15 的其余项（`popup_at/subtree_hit_contains` 不看 transparent/blocked）当前无可观测错误路由，可接受；此条是可观测输入缺陷。
- 修法：`cancel_pointer_session`/`on_window_focus_lost` 中清 `suppress_release=false`（以及 pointer.press/pending_drag 一并清）；补失焦后首次点击回归测试。
- 是否阻塞：否。

### E-07（P2）Menu/ContextMenu 的“祖先滚动/窗口 resize → 关闭整链”矩阵未实现（当前所有 popup 都只重新 settle）

- 位置/证据：
  - 设计 §4.7 组件级矩阵：菜单/右键菜单在祖先滚动或窗口 resize 时**关闭整链**；Dropdown/Select/ColorPicker 在 anchor 仍可见时**同帧重新 settle**，anchor 消失/不可见/滚出视口才关闭。
  - 引擎只提供 `anchor_lost`（节点缺失）：`popup/settle.rs:258-269` + `popup/dismiss.rs:521-555`；节点仍存在（滚动/隐藏/移出视口）时不触发。
  - kit `Menu::sync`（`kit/menu.rs:1186-1205`）只做 panel.sync/sync_hover/submenus，没有任何滚动/resize 监听；ContextMenu 直接委托 Menu。
  - 现有测试 `popup_behavior::a_scrolled_anchor_carries_the_popup_in_the_same_frame` 固化的是“滚动后跟随”（Dropdown 语义），没有 Menu 关闭用例；`kit` 的 `select::an_open_select_follows_a_window_resize` 只有 Select 的 re-settle 测试，Menu 没有。
- 影响：菜单/右键菜单的触发条放在滚动容器或窗口 resize 后，菜单会跟着锚点移动/被压缩而不是关闭整链，与桌面菜单惯例和设计矩阵不符（P2 MenuChain 阶段要修，但 P1 的“1 级 parity”已引入该行为差异）。
- 修法：Menu/ContextMenu 在 `sync` 中记录 anchor 祖先滚动偏移/窗口尺寸快照，变化时 `close()` 整链（根）；Dropdown/Select/ColorPicker 保持 re-settle。补滚动/resize 菜单关闭测试与 Dropdown 跟随测试的成对用例。
- 是否阻塞：否。

### E-08（P2）a11y 无法“激活” Menu/Dropdown/Select 选项：MenuItem 行不声明 Click action，List 的 select action 只改高亮，`ItemPanel` 未接 `List::on_activate`

- 位置/证据：
  - `crates/ui_kit/src/kit/list.rs:590-599`：行容器无 action；行节点的 Select 动作挂在 List viewport 上（`:594`）。
  - `crates/ui_kit/src/kit/list.rs:1036-1048`：AT（无 hit）点击走 `activate` 回调；但 `menu.rs` 的 `ItemPanel::new`（`:177-283`）从未调用 `list.on_activate(...)`；行点击的激活走内层 `{list}:row.{index}:hit` 节点的 `row_action`（`menu.rs:220-237`），该 action 不在 a11y 目标节点（ListItem 行节点）上。
  - `crates/ui_core/src/a11y/mapping.rs:188-191`：`MenuItem/ListBoxOption` 默认动作集为空，只有节点自身声明 Click 才暴露；因此 AT 记录不会给 MenuItem 提供 Click（或 Click 被 `UnadvertisedAction` 拒绝）。
  - EXP12：菜单 a11y 记录中 MenuItem 的 engine 节点上分发“该行实际注册的 select action” → `handled=true`、`highlighted=Some(0)`，但 `on_activate` 回调计数 **0**、菜单仍打开（预期应激活并关链）。
- 影响：屏幕阅读器用户可用方向键（焦点在 panel surface 上的键盘路径）但无法用 AT 的 Click/Activate 激活菜单/下拉/选择项；Dialog 内 Select 的 AT Click 只改变选择高亮、不落业务回调。属 a11y 功能缺口（P1-7 只声称角色/AT dismiss，未声称激活）。
- 修法：`ItemPanel` 接 `List::on_activate`（→ 同 `row_action` 的 activate 流程），并在行 a11y patch 声明 Click/映射到该 action；或把 row_action 挂到 List 行节点上（pointer 双触发要避免）。补 AT Click 激活 Menu/Dropdown/Select 的测试。
- 是否阻塞：否（既有实现同样未支持；但 P1 声称 a11y 面完成时应补）。

### E-09（P3）公开 API 死字段：`Anchor::Node.edge` / `AnchorEdge` 无读取者；`PlacementSpec.match_anchor_width` 引擎无生产消费者（均已在 §12.6 待复核登记，但未裁决/未删）

- 位置/证据：`popup/anchor.rs:5-12` 对 `Anchor::Node { node, .. }` 只用 node；`AnchorEdge` 仅构造（20+ 处）；`placement.rs:34-46` 只改纯函数结果，`settle.rs:319-340` 只写 origin，不写宽；kit 从不设 `match_anchor_width`（宽度由 `PanelWidth::AtLeast` 测量实现）。AGENTS §2.1。
- 影响：调用方传 `edge:Top` 或 `match_anchor_width:true` 得到静默 no-op，API 语义误导；死字段违反仓库规范。
- 修法：删除 `AnchorEdge`/`edge` 与 `match_anchor_width`（同步 kit 调用点），或真正接入设计语义；若保留必须写入 §12 最终 API 与限制。建议 P2 阶段前删除。
- 是否阻塞：否。

### E-10（P3）`A11yAction::ShowTooltip` 被投影广告但引擎返回未处理（测试还把该拒绝行为锁定）

- 位置/证据：`a11y/popup.rs:189-195` 给 `described_by` 的 trigger 推 `ShowTooltip`；`a11y/action.rs:95` `A11yAction::ShowTooltip => false` → `UnhandledAction` 诊断；`a11y/popup_tests.rs:519-536` 明确断言“没有 handler 的 ShowTooltip 必须留未处理诊断”。
- 影响：AT 看到可执行的 ShowTooltip 但调用无效；设计 §4.10 列了该动作，未规定“仅声明不实现”。应实现（显示对应 tooltip）或停止广告并在设计登记。
- 是否阻塞：否。

### E-11（P3）IME caret/焦点派生几何未含 `paint_offset`（A-22 剩余面；§12.6 待复核 5）

- 位置/证据：`window/focus.rs:25-42` `text_caret_rect` 走 `interaction::scroll::to_screen`（只含祖先滚动），不含 `popup_transform.offset`；`node_screen_rect`（`node/tree.rs:586-604`）已含 transform，焦点环 damage 用它。P1 kit 未启用 FadeSlide（D-P1-19），生产暂不可观测；一旦 P3 启用 slide 或自定义 popup 使用 FadeSlide，IME 候选窗/焦点环会滞后 4px。
- 修法：统一派生几何入口（把 popup transform 偏移并入 `to_screen` 或 caret 计算）；补 FadeSlide+editable 用例。是否阻塞：否。

### E-12（P3）测试真实性/门禁覆盖缺口（A-33/B-17 的剩余面）

- 位置/证据：
  - `tests/popup_behavior.rs:3598` 残留 `eprintln!("[dbg] ...")` 调试输出。
  - `popup_behavior.rs:2729 a_dpi_change_keeps_the_popup_logical_geometry_stable` 全程只在 DPR=2 下打开一次，没有“DPI 变化”，名不副实（§7.1-3 要求 DPI 跟随）。
  - `popup_behavior.rs:845 slide_animation_offsets_paint_and_hit_without_moving_the_settle_rect` 仍只断言 hit/settle rect 与动画期间 hit，不断言 FrameRecord 中 Quad/Glyph 的绘制位置（A-33 明确保留）。
  - `popup/tests.rs:1087 close_popup_closes_children_before_the_parent` 只断言最终空栈，不验证子先于父（I8 的真正顺序仍不可观测；动画路径可用 on_dismiss 顺序观察）。
  - `kit/popup_panel.rs:1036 an_at_least_panel_follows_its_anchor_width` 实际只手动 `set_width`，没有锚点宽度来源。
  - `kit/menu.rs a_pointer_press_activates_the_entry_it_lands_on` 实际是一次完整 click（release 激活），命名与 D15 易混。
  - P1 popup 套件没有负向/变异自检记录（P0 有注入故障自检）；gallery `--pixel-check` 只覆盖 ColorPicker，不覆盖 Dialog/Alert/Tooltip/Menu 的像素。
- 影响：上述语义（DPI 变化、绘制偏移、关闭顺序、锚点宽度、Dialog 遮罩/阴影）在测试中是弱断言或空覆盖，是 E-02/E-03 漏网的直接原因。
- 修法：按条目补断言/改名/删除调试输出；把 Dialog scrim 颜色、面板阴影像素纳入 `--pixel-check`/组件测试；给 2–3 条关键 popup 测试记录一次“故意破坏实现→测试变红”的负向自检。
- 是否阻塞：否。

### E-13（P3）P1 回写 §12 与实际工作树不一致（行数/交付声称）

- 位置/证据：
  - §12.1/§12.5 的文件行数与当前工作树不符：`popup.rs` 551→当前 633、`app/popup.rs` 775→809、`popup/tests.rs` 1429→1540、`tests/popup_behavior.rs` 3947→4068、`ui_kit/kit/popup_panel.rs` 1201→1283（实现方在写回后继续改动；审查快照已更新）。
  - §12.2 P1-4 声称 PopupPanel 已把 `constrained_size` 用于内部滚动 → 与 E-04 不符。
  - §12.2 P1-5 的完成列表漏了设计 §6 P1-5 明文要求的“Closing/DismissPending 输入阻断” → E-01。
  - §12.3 D-P1-21 声称 `validate_owner` 会修复外部删除的 focus owner → 与 E-05 不符。
  - §12.6 的“待复核 1/3/5/7/8/9”多数仍是开放 finding（本报告 E-09/E-11/E-14 等）。
- 影响：回写是提交说明/后续阶段的唯一事实来源；当前 3 处声称与代码相反，会把缺陷带入 P2/P3。
- 修法：提交前按最终代码更新 §12.1 数字、修正 P1-4/D-P1-21 表述、把本报告 E-02/E-03/E-04/E-05/E-06/E-07/E-08 纳入 §12.6/§12.7，或注明“终审发现，见 review-p1-final-engine.md”。
- 是否阻塞：否（但必须随提交修正）。

### E-14（P3）生产 `expect` 兜底（B-11 剩余）

- `crates/ui_core/src/app/popup.rs:135,154,169,189,251,267,389,396` 等 `expect("window was just resolved")`，以及 `stack.rs:252,256` 索引不变量。均在已解析 window 后使用，实际不可达；但设计 §4.13/AGENTS 倾向强类型错误。可改为 `debug_assert!` + 早返回 `PopupError`，或保留并登记 §12.6-8。是否阻塞：否。

### E-15（P3）立即关闭路径的 modal owner_modal 后代关闭顺序是祖先先于子（I8）

- `crates/ui_core/src/app/popup.rs:556-564` `close_entry(modal)` 对 `owner_modal_descendants` 按栈 z 升序逐个 `close_entry`；若后代之间存在 parent 链，父先被移除、子后移除。动画路径 `dismiss.rs:133-167` 已用 `subtree.iter().rev()` 保证子先父。仅 `App::close_popup`（立即）/重开收尾路径可达。修法：后代按 depth 降序关闭；补顺序可观测测试。是否阻塞：否。

### E-16（P3）`raise_popup(modal)` 不重排 scrim z，“scrim=modal z−1”仅在创建时成立

- `crates/ui_core/src/popup/stack.rs:248-264` `raise` 只改子树（modal 的 subtree 不含 scrim），scrim 留在原 z；若公开 `raise_popup` 用于 modal，scrim/modal 不再相邻，I7/§4.2 的“连续 z 对”只在创建时保证。kit 不 raise modal；补不变量测试或让 raise 同步移动 `scrim_for`/`scrim_of`。是否阻塞：否。

## 3. 早审 finding 关闭复核

### 3.1 已独立复核关闭（代码 + 测试/探针）

| Finding | 复核证据 |
|---|---|
| B-01/A-01 侧表 stale transform/pointer_transparent | `node/tree/arena.rs:575-601` remove 清两张表；`node/tree/tests.rs:108-178` 同时覆盖 transform+transparent+槽位复用；`closing_an_opening_popup_keeps_later_nodes_clean` 可命中 |
| A-02 模态 a11y 挂 hidden trigger | `a11y/popup.rs:255-257` modal 无父挂 window root；`a11y/popup_tests.rs:600-675` 断言 `assert_no_hidden_ancestors` + 背景 hidden + dialog 可见 |
| A-03 ZPolicy 回滚 panic | `app/popup.rs:209-408` reserve_z 前置于建节点 + 统一 cleanup；`invalid_z_policy_open_leaves_no_residue_and_next_open_works` |
| A-04 默认 feature 编译 | `ui_core/Cargo.toml` `[[test]] popup_behavior required-features=["test-support"]`；`cargo test -p ui_core --no-run` 通过（本次全量测试） |
| A-05 Scrim 全视口 | `settle.rs:191-195,325-334` scrim 跳过 max/placement 直接 viewport；`a_modal_scrim_covers_the_full_viewport_and_damages_it` |
| A-06 modal+scrim 关闭事务 | scrim 复制 modal animation，同帧 DismissPending→Closing；`a_modal_scrim_closes_with_the_modal`/`programmatic_begin_close_animates...` |
| A-07 Hint 叠 modal | `app/popup.rs:240-246 HintBlockedByModal`（owner_modal 例外）；`a_tooltip_opened_from_dialog_content_is_allowed_above_the_modal`、`a_background_tooltip_is_rejected...` |
| A-08 AT scope 旧物理链 | `a11y/action.rs:462-472` 改 `node_in_popup_scope`；`a_modal_dialog_and_its_inner_popup_accept_at_actions` |
| A-10 子菜单方向无强制 | `app/popup.rs:53-79 effective_placement` 强制 Right/Left + fallback 过滤；`a_submenu_ignores_vertical_fallbacks...`、`submenu_placement_flips_right_to_left...` |
| A-11/A-12 a11y skip-container 父/多父 | `a11y/popup.rs:269-286` 沿 Container 上溯；`a11y/tree.rs:1122-1177` 统一 effective parent + cycle；`assert_unique_parents`/22 条 popup a11y 测试 |
| A-13 Scrim 伪造/density | `open_popup` 拒绝 tier=Scrim（`app/popup.rs:217-228`）；`a_user_supplied_scrim_tier_is_rejected`；density 已消费（E-02 是语义回归，非未用） |
| A-14 CloseChain | `dismiss.rs:448-460`；`escape_close_chain_policy_closes_the_whole_chain` |
| A-15 anchor_lost 关链 | `dismiss.rs:521-555`；`an_anchor_lost_chain_closes_the_whole_chain` |
| A-16 Closing 中 Toggle | `dismiss.rs:309-313,498-519` cancel_close；`pressing_the_opener_while_closing_cancels_the_close`（回 Opening，不触发 opener action） |
| A-17 owner_modal 传递 | `app/popup.rs:100-115` opener 路径 `nearest_modal`；`a_popup_opened_inside_an_owner_modal_popup_keeps_the_modal_scope` |
| A-18 dismiss_topmost 事务 | `app/popup.rs:793-808` → `request_close`；`dismiss_topmost_runs_the_closing_transaction` |
| A-19 limitShift vs margin | `placement.rs:303-366` 最终双轴 clamp；`limit_shift_clamps_into_the_viewport_margin` |
| A-21 reorder damage 后代外延 | `node/tree/arena.rs:429-482` 遍历子树 painted_extent；`raise_damages_descendant_painted_extent` |
| A-23 sync_order 静默失败 | `PopupError::StackSync`（`app/popup.rs:179-207`、`stack.rs:342-366`）；orphan scrim 用例；`invalid_z_policy...` |
| A-24 旧 overlay 层序 | 旧文件已删除；`rg` 零引用；HUD/popup root 固定层序测试 |
| A-28 placement NaN | `placement.rs:94-131` 强类型校验 + finite 断言；`extreme_finite_inputs_never_produce_a_non_finite_rect` |
| A-30 死字段/诊断 | `solved_origin/settled_size/PopupEntry::rect/PopupDirty.measure` 全仓不存在；`an_anchor_lost_dismissal_records_a_diagnostic` |
| B-02 Scrim 机制 | 全视口/绘制/damage/生命周期（A-05/A-06/A-13）已闭；视觉 opacity 回归见 E-02 |
| B-03 事件模型 | FocusLost/ParentClosed/EscapePolicy 均派发（`window_deactivation_emits_focus_lost`、`parent_close_emits_parent_closed_for_children`、`escape_close_chain_policy...`）；替代式关闭路径用 Programmatic notify |
| B-04 窗口失焦误关 Modal | `dismiss.rs:557-582` 仅筛 Auto/Hint 根（Scrim 未命中）；`window_deactivation_keeps_a_modal_with_scrim` |
| B-05 Closing 重开 | 同 A-16 |
| B-06 §7.1 测试 | 文本首帧/虚拟列表/DPI（弱，见 E-12）/max-size clamp/modal hover+scroll/owner_modal/popup stack invariant/invalid z 均有测试 |
| B-07 required-features | 同 A-04 |
| B-08 死字段 | 同 A-30 |
| B-10 orphan scrim | `reap_orphan_scrims` + `an_orphaned_scrim_is_reaped_when_the_modal_node_disappears` |
| B-12 Opening 语义、B-13 DefaultButton 启发式 | 已设计登记（D-P1-18、B-13 表）；kit `is_dismissable/is_closing` 收口、Dialog confirm 先构建 + 测试锁定 |
| B-14 AT dismiss 目标 | `a11y/action.rs:488-519` 按 enclosing popup 定位；`at_dismiss_closes_the_popup_that_owns_the_target`、`hide_tooltip_closes_the_tooltip_even_below_another_popup` |

### 3.2 部分关闭 / 文档化保留 / 转入本报告

| Finding | 状态 | 剩余问题 |
|---|---|---|
| A-09 match_anchor_width | kit 已用 `PanelWidth::AtLeast` 实现宽度保真；引擎字段无消费者 | E-09（死字段，需裁决） |
| A-20 resized 信号 | 引擎 `constrained_size`/诊断已暴露 | **E-04**（kit 未消费，功能缺口） |
| A-22 paint_offset 派生几何 | 锚点/焦点环已修 | **E-11**（IME caret 未含 offset） |
| A-25 has_popup/controls 瞬态 | 引擎支持 static patch（`static_haspopup_controls_and_expanded_survive...`） | kit 未声明静态 has_popup（D-P1-13，P2 计划）；E-08 相关 |
| A-26 混合 a11y 父链环 | patch 链有 cycle 检测 | 待复核（§12.6-6）；无新证据，保留 |
| A-27 两个 MaxSize | Window::set_max_size 失效缓存已修 | 命名未收敛（P3） |
| A-29 子菜单 a11y 父 | 设计 D-P1-12 登记 P2 推迟 | 确认仍为父 popup 内容根，非 menuitem；P2 |
| A-31 调试面 | popup_stack/diagnostics/settle_rounds 有 | `FrameRecord.popup_damage/z_changes` 未实现（P3） |
| A-32 a11y O(n) | 未改 | P3 性能（D-P1-15） |
| A-33 测试真实性 | 有改善 | **E-12**（绘制位置/DPI/顺序/负向自检） |
| A-34 规范核查 | clippy/无注释/无 allow/wrap 通过 | 残余 expect → **E-14** |
| B-11 生产 expect | 部分（sync_order 已改） | **E-14** |
| B-15 指针/穿透卫生 | transparent 已消费；popup_at 过滤 benign | **E-06**（suppress_release 未重置） |
| B-16 settle layout_ran | 保留 | P3 性能（D-P1-15） |
| B-17 测试真实性 | 部分 | **E-12** |

### 3.3 回归 / 新发现（不在早审清单内）

- **E-01 模态关闭期背景输入**（P0，新回归）
- **E-02 遮罩 100% 黑 + 主题遮罩色死代码**（P1，视觉回归；早审只查“是否有遮罩/是否全视口”）
- **E-03 PopupSurface 阴影被内容根 clip**（P1，视觉回归）
- **E-04 constrained_size 无 kit 消费者**（P1，P1-4 交付项缺失）
- **E-05 外部删除 popup content 焦点悬挂**（P2，D-P1-21 声称不实）
- **E-07 Menu/ContextMenu 滚动/resize 矩阵**（P2）
- **E-08 a11y 选项激活缺口**（P2）
- **E-13 §12 回写与代码不一致**（P3）

## 4. 已核实正确点（正面证据）

1. **settle 帧序/≤2 轮/子级跟随**：插入点在 canvas-text eviction 后、ensure_visible 前（`app/frame.rs:196-209`）；max-size→inset 两轮；3 级子菜单同帧跟随；`popup_settle_rounds_never_exceed_two`、`three_menu_levels_follow_the_parent_without_a_frame_lag`、`text_dominated_height_is_stable_on_the_first_frame`、`a_virtual_list_inside_a_popup_settles_after_item_changes` 通过。
2. **首可见帧 == 稳定帧**：引擎 `opening_a_popup_settles_geometry_in_the_first_frame` 断言 `rendered`+rounds=2+ rect==稳定帧；PopupPanel/Dialog/Menu/Dropdown/Select/ContextMenu 均各自有首帧测试。
3. **dismiss 矩阵主路径**：modal scope 优先、Auto 链 outside 优先、opener 同 id Toggle 消费、不同 opener 先关后不消费、命中祖先只关后代、默认 Replay、Consume 显式 opt-in、Esc 只关一层/跳过 Closing、tooltip Esc 自关、Dialog 内 Select 外部关闭、modal 关闭回收 owner_modal 后代与 scrim、窗口失焦只关 Auto/Hint 保留 Modal/Scrim。
4. **事件模型**：`ParentClosed`/`FocusLost` 真实派发；替代式关闭走 Programmatic notify；handler Ignore 会恢复注册；`begin_close` 幂等/未知 id 返回强类型错误。
5. **焦点单一真值**：`App::focused`/paint/a11y/cursor/IME/text_edit 统一经 `popup::focus::effective_focus`；Take/Trap 快照/恢复一次/不抢新 popup 焦点/opener fallback/nearest focusable/root fallback；Trap Tab 闭环；IME composition 开关两侧。
6. **Trap 范围**：`node_in_popup_scope` 覆盖 modal 子树 + owner_modal 后代；`owner_modal` 嵌套弹层可交互；a11y `in_modal_scope` 物理链一致；`modal_scope` 公共 API 语义已改为 content node。
7. **a11y 链表**：逻辑父唯一（`effective_a11y_parent` + `resolve_parent` 跳过 Container）、modal 无父挂 root、背景 hidden、scrim hidden、expanded 清空、separator hidden/focusless、`assert_no_hidden_ancestors`/`assert_unique_parents`/`assert_relations_resolve` 契约测试；accesskit 字段接线。
8. **z/order/层序**：z 单调、order==z、子菜单在父上、Auto 根替换、modal 关闭无关链、HUD 最后且 pointer-transparent、popup root 固定 tail；`reorder_children` 不置 structure epoch、damage 子树 painted_extent；`!damage_full` 的非模态开关/动画/raise 用例通过。
9. **动画**：Fade/FadeSlide 的 `paint_offset` 不改 layout；paint/hit 同偏移；damage 覆盖旧∪新偏移；Closing 立即不可命中；关闭 90ms 后移除；modal fade-only；子 popup 锚点跟随父 paint_offset（`a_child_popup_anchor_follows_the_parent_paint_offset`）。
10. **Scrim 机制**：全视口 rect/damage、与 modal 同帧关闭、窗口失焦保留、伪造 tier 拒绝、orphan 回收；仅视觉 alpha 有回归（E-02）。
11. **PopupPanel 行为**：首帧 settle/宽度 Exact/AtLeast 测量回写/`set_width` 同帧生效/幂等 open/close_now/`on_close(reason)` 恰好一次/Closing 不命中/稳态零帧；Dropdown/Select 宽度跟随 trigger、长条目撑宽、显式宽度保持；Menu 1 级子菜单 200ms hover、hover 不夺焦、键盘 → 夺焦、release 激活、Esc 逐层、Auto 根替换、虚拟列表长菜单；Dialog 居中/初始焦点/Tab 闭环/遮罩 hidden/Alert 叠放/重开/关闭回调；ColorPicker 内容闭包/局部坐标 Canvas/tab rebuild+request_measure/open-close 回调。
12. **宿主迁移**：`crates/widget` 去 layer/animation_router 后 workspace check 通过；14 widgets/5 业务 ui 的组件 sync 路径保留；`rg` 旧 API 零引用。
13. **代码质量（新增生产文件）**：无注释、无 `#[allow]/#[expect]`、无 `unwrap/panic/todo`、无新增依赖；`PopupError` 13 变体强类型；`sync_order` 失败映射 `StackSync`；`#![deny]`/clippy `-D warnings` 绿。
14. **用户裁决对齐**：Menu = Auto + Take（根 Take、hover 子菜单 None、键盘/点击 Take）；outside 默认 Replay（Consume opt-in）；Alert 可叠放；Hint-in-modal 仅背景阻塞（owner_modal 内允许）；Tooltip 不取焦；ColorPicker 非模态（role=Dialog）；Dialog/Alert 模态 scrim；D16 默认按钮。

## 5. 残余风险

1. **同窗口多模态/嵌套 modal**：现有测试覆盖两个 Dialog 叠放与 owner_modal 后代，但未覆盖“关闭下层 modal / 同时关闭两个 / modal 内再开 modal 后 Esc 顺序”的组合；E-01 的修复会影响该矩阵，需补。
2. **多窗口**：PopupStack per-window 与 `PopupId` 防跨窗口已单测；无真实双窗口交互测试（P3 三端/多窗口）。
3. **真实窗口像素/三端**：`--debug-stack` 只 dump rect/stack，真实窗口 readback/截图仍是 P3-6；本报告 E-02/E-03 只能靠 headless pixel/primitive 断言覆盖，建议纳入 `--pixel-check` 或组件像素测试。
4. **性能**：动画帧 lane 重建、settle `layout_ran` 触发、a11y 每帧全树 project/DFS 未做基线（P3-5）；PopupPanel `apply_measure` 在 `open` 内同步 `refresh_text+compute_layout` 两次（out-of-band layout），未测帧预算。
5. **外部删除/节点重建路径**：E-05 只修焦点；`prune_missing` 仍静默移除 entry（无 ParentClosed/on_dismiss），kit 只能在下一次 `sync` 以 Programmatic 收尾；建议在 `remove_node` 路径补事件或登记为不支持。
6. **文档漂移**：§12 行数/交付声称与当前工作树不一致（E-13）；另一 agent 可能继续写 §12，提交前需以最终代码复核一次。
7. **测试真实性**：见 E-12；E-02/E-03 是“门禁全绿但缺陷存在”的直接证据，说明像素/颜色类断言仍是空档。
8. **P2 菜单链语义**：safe-triangle/grace/typeahead/2–3 级/PgUp/PgDn/Alt+F10 未实现（D-P1-24 已登记）；E-07 的滚动/resize 矩阵与 E-08 的 AT 激活应在 P2 一并收口。

## 6. 结论（给 lead）

- **严重度计数：P0 × 1、P1 × 3、P2 × 4、P3 × 8（共 16 条）**。
- **最关键 3 条**：
  1. **E-01（P0）** 模态 DismissPending/Closing 期间背景输入未阻断：关闭 Dialog 后 90ms 内点击会执行背景按钮（EXP2 实测计数 1）；违反 §4.7“DismissPending 吞掉 paint_rect 内按下”与 D6 模态阻断。
  2. **E-04（P1）** `constrained_size` 无 kit 消费者：小视口下 Menu 末行（EXP6）与 Dialog confirm（EXP14）被裁到 popup 外、不可滚动/不可点；§12.2 P1-4 声称已实现，实际未实现。
  3. **E-02 + E-03（P1）** 视觉回归：Dialog 遮罩从主题 42%/55% 黑变成 100% 不透明黑（EXP1 alpha=1.0），且所有 PopupSurface 阴影被 popup 内容根 `overflow_hidden` 裁掉（EXP9/EXP13 面板外像素 255 vs 对照 245）。
- **是否可提交 P1：否（当前不建议直接提交）。** 需先修 E-01（P0，模态关闭期输入吞噬）并补回归测试；E-04 必须修复或由 lead 明确降级为 P2/P3 且修正 §12.2 声称；E-02/E-03 建议在提交前修（遮罩 alpha 取主题、阴影 pad/裁剪层次），若用户明确接受视觉变化，则至少删除 `Role::Scrim`/`backdrop` 死字段并在 §12 登记偏差。E-05/E-06 建议同批修（都是小改 + 补测试）；E-07/E-08 可随 P2 菜单链/AT 激活收口，但需登记。其余 P3 可在 P2/P3 清账。
- 正面结论：引擎骨架（settle/dismiss/focus/animation/stack/a11y）、kit PopupPanel 与 8 个组件的迁移、旧 API 删除、宿主迁移、门禁与 gallery 自动化整体质量高；3734 测试 + clippy/fmt/workspace check + `--pixel-check`/`--debug-stack` 全绿，未发现数据竞争/内存错误/panic 类 P0；本报告的 P0/P1 集中在“模态关闭期输入、约束内容适配、遮罩/阴影视觉与主题”四个收口面。

---

### 附：审查期间仓库外实验路径

- `E:\kytrix\target\p1-final\exp\Cargo.toml`、`src/main.rs`（EXP1–EXP14）
- 日志：`E:\kytrix\target\p1-final\{gates.log,gates2.log,gates3.log,gates4.log,gallery.log,workspace.log}`
- 仓库未修改：`git status --short` 与审查开始时一致（仅 P1 实现方改动；`.docs` 被 `.gitignore` 忽略，本报告不进入 git）。
