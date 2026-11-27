# P3 对抗性审查（kit lens：Dialog / ColorPicker / 动画 / close reason / a11y）

- **审查对象**：worktree `E:\kytrix\.agents\worktrees\dsh-1d175414-1a41`，基线 `f24a3441`（P2 提交），P3 未提交工作树。
- **代码快照**：`git diff f24a3441 -- . ':(exclude).docs' | git hash-object --stdin` = **`3ed96435519e67b4ae290424973749d954254781`**；22 tracked 文件 **+3393/−125**；最新代码 mtime `menu.rs 16:08:12`、`context_menu.rs 16:07:36`、`color_picker.rs 16:06:27`（审查开始前已冻结，审查期间代码未变）。未跟踪文件仅 `tools/`（2 个三端验收脚本）。
- **文档快照**：设计 §14 / `p3-summary.md` / `kit-gallery.md §14` 在审查期间由其他 agent 并发写入（设计 16:52:37、p3-summary 16:52:37、kit-gallery 16:51:53）。本报告对文档的引用以该快照为准；lead 提交前需在文档定稿后再核对一次。
- **用户裁决基线**：P3-1 删除 `Dialog::recenter`；P3-2 `Scale`（FadeScale 0.98→1，kit 采用）；P3-3 ColorPicker 细化；P3-6 真实窗口像素由 acceptance 负责。
- **方法**：只读静态审查（未改任何受版本控制文件）+ 全量门禁复跑 + **仓库外探针**（`E:\kytrix\target\p3-kit-probe`，path 依赖本工作树，仅写入 `E:\kytrix\target`）实测 scrim 缩放与 Dropdown 动画。命令统一 `CARGO_TARGET_DIR=E:\kytrix\target`。
- **结论**：**P0 = 0，P1 = 0，P2 = 1，P3 = 7 → 当前快照不可提交**。P3 主功能面（recenter 删除、FadeScale/FadeSlide 引擎实现、close reason、ColorPicker、a11y 基准、三端真实像素）基本真实交付，门禁全绿、新增测试真实且有渲染级断言；但 **Dialog 的 `FadeScale` 把 scrim 一起缩放**（关闭动画期间窗口四周出现未压暗亮边，且与 §14.6 D-P3-2“scrim 不缩放”的声明相反），另有 1 条文档与代码相反（D-P3-5 说 Dropdown/Select 仍 Fade，实际已 FadeSlide）及 1 条 a11y 偏差登记理由不成立（accesskit 并不缺 Splitter/Group 角色）。

**提交结论**：建议先修 P3K-01（引擎或 kit 任一处 ~1 行 + 1 条断言）并订正 P3K-02/P3K-03 文档，其余 P3 可随修复轮登记；修完/回写后 P3 可提交。

---

## 1. 范围与门禁实测

### 1.1 审查范围

| 区域 | 文件 | 关注点 |
|---|---|---|
| kit 主体 | `kit/{dialog,color_picker,menu,context_menu,dropdown,select,popup_panel}.rs` | recenter 删除、FadeScale/FadeSlide 采用、`close_with_reason`/AnchorLost、a11y role、测试真实性 |
| 引擎支撑 | `popup.rs`、`popup/{animation,dismiss,settle}.rs`、`paint/pass.rs`、`node/tree{,.rs,/arena.rs,/content.rs}`、`app/{frame,popup}.rs`、`test_support/record.rs` | lane scale 的 pivot/damage/hit/a11y/侧表、modal fade-only、FrameStats |
| 宿主/验收 | `examples/gallery.rs`、`tools/real-pixel-check-{macos,linux}.sh` | `--real-pixel-check` 语义、`--pixel-check`/`--debug-stack` 无回归 |
| 文档 | 设计 §4.10/§4.11/§4.12/§4.13/§4.14/§5.2/§6 P3/§7.4/§13/§14；`p2-summary.md`；`p3-summary.md`；`kit-gallery.md §14`；`AGENTS.md` | 与代码一致性、偏差登记真实性 |

### 1.2 门禁（本机 Windows 11 / D3D12 WARP（headless）/ RTX 5080（真实窗口）/ 125% DPI，2026-11-27）

| 命令 | 结果 |
|---|---|
| `cargo test -p ui_core -p ui_kit` | **PASS**：ui_core lib **2805 passed / 1 ignored**、frame_record 8、gui_smoke 1 ignored、layout 4、**popup_behavior 120**、snapshot 14、demo 39、**ui_kit lib 875**；合计 **3865 passed / 0 failed / 2 ignored**（exit 0） |
| `cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings` | **PASS**（exit 0） |
| `cargo +nightly fmt --all -- --check` | **PASS**（exit 0） |
| `cargo check --workspace --all-targets` | **PASS**（exit 0） |
| `gallery --pixel-check` | **PASS** 94 checks、`damage_full_frames=0`、`open_animation_frames=8`、`close 6+6`、首可见帧 rect == 稳定帧；exit 0 |
| `gallery --debug-stack` | **PASS**：3 级链 entries 3→0、`z=[2,3,4]`、parent 链正确、picker 栈 1→0；`menu_frames=350/picker_frames=205/retries=0`；exit 0 |
| `gallery --real-pixel-check` | **PASS** 50 checks、15.694 s、`failures=0`；Windows 证据 `target/real-pixels/windows/`（screen 4800×3072、content 1200×900 @dpr 1.25）；exit 0 |
| macOS/Linux 证据（acceptance 产出，只读核对） | `target/real-pixels/macos-remote/report.txt`、`linux-remote/report.txt` 均 `checks=50 failures=0 result=PASS`（dpr 2.0 / 1.0）；PNG 证据齐全 |
| `git status --short` | 代码仅 P3 工作树改动；`?? tools/`（未跟踪三端脚本）；`.docs` 改动来自并发文档回写；**无审查者改动** |

### 1.3 外部探针（仓库外，`E:\kytrix\target\p3-kit-probe`）

用公开 API（`TestApp` + kit `DialogBuilder`/`Dropdown` + 引擎 `open_popup`）实测两件事：Dialog 关闭期 scrim 的 `paint_scale` 与实际绘制 quad；Dropdown 打开帧的 `paint_offset`。探针仅 path 依赖工作树，不写仓库；输出作为 P3K-01/P3K-02 的直接证据。

---

## 2. Findings（P3K-01…P3K-08）

> 严重度口径沿用前轮：P0 崩溃/数据丢失/主功能不可用；P1 重大正确性缺陷；P2 用户可见行为缺陷/设计契约违反（含交付声明与行为相反）；P3 轻微/边界/测试质量/登记准确性与覆盖缺口。

### P3K-01 [P2] Dialog 的 `FadeScale` 让 **scrim 在关闭动画中一起缩放**：窗口四周出现未压暗亮边，且与 §14.6 D-P3-2“scrim 不缩放”的声明相反

**证据链**

1. kit：`dialog.rs:414-418` 在 `fade=true` 时把 spec 设为 `PopupAnimation::FadeScale`，同一 spec 携带 scrim（`dialog.rs:427-430`）。
2. 引擎关闭路径：`popup/dismiss.rs:148-164` 把 modal 的 scrim 收进 candidates（scrim 条目自身 `tier=Scrim`、`focus=None`）；`dismiss.rs:206-214` 的“modal”判定是 `entry.is_modal() || entry.tier == PopupTier::Modal`，**`PopupTier::Scrim` 两者皆否**，于是 `scale_to = POPUP_SCALE_FROM(0.98)`；`dismiss.rs:217-225` 为 scrim 推入 Closing 动画。`popup/animation.rs:148` 每帧插值 scale，`:176-183` 将 `{offset,opacity,scale,pivot,blocked}` 写入 `PopupTransform`，`paint/pass.rs` 对其 quad 施加 lane scale。
3. **kit 级探针实测**（`DialogBuilder::new("Scrim probe")`，视口 800×600，打开→关闭；直接读 `popup_stack` 与 `rendered().quads`）：

   | 时刻 | 条目 | `paint_scale` | 实际绘制 quad |
   |---|---|---|---|
   | 打开（settled） | Scrim | 1.0 | `(0,0 800×600)` ✅ |
   | 关闭第 1 帧（16/90 ms） | Scrim | **0.99112** | **`(3.5531,2.6648) 792.8938×594.67035`** |
   | 关闭第 2 帧（46/90 ms） | Scrim | **0.982337** | **`(7.0652,5.2989) 785.8696×589.4022`** |
   | 同上两帧 | Modal（Dialog 面板） | 1.0 | —（fade-only 正常） |

   引擎直开 modal+scrim 的对照组同样得到 `(7,5.25 786×589.5)`。
4. 影响：关闭 90 ms 内 scrim 只覆盖 98%，四周 **3–7 logical px（125% DPI 下 4.4–8.8 物理 px）保持未压暗**；早期帧 scrim 的不透明度约为 `(1-eased)×backdrop`（≈0.55×0.42），亮边与压暗区的像素差可达数十级，暗色主题/亮背景更明显。关闭事务的“scrim 全视口”契约（§4.6/§4.12“对话框 fade-only”）被破坏。
5. 这是 **P3 新引入**：P2 的 Dialog 为 `PopupAnimation::Fade`（`git show f24a3441:crates/ui_kit/src/kit/dialog.rs`），引擎当时没有 scale，scrim 不缩放。
6. 覆盖缺口：`the_modal_dialog_degrades_fade_scale_to_fade_only`（`dialog.rs:1511-1540`）只断言 **Dialog 条目**；引擎 `modal_fade_scale_is_fade_only`（`popup_behavior.rs:5674-5693`）**不带 scrim**；`--real-pixel-check` 只在稳定态采样（§14.4 的 baseline/open/close 完成帧），不采样关闭动画帧 ⇒ 现有全部门禁都不会报红。

**建议修复（二选一，均很小）**

- (a) 引擎：`PopupTier::Scrim` 视作 no-scale（`animation.rs` 的 modal 判定与 `dismiss.rs:206` 一并加 `|| tier == PopupTier::Scrim`），并在 `modal_fade_scale_is_fade_only` 或新测试中断言“关闭动画期间 scrim `paint_scale == 1.0` 且绘制 quad == 视口”；
- (b) kit：Dialog 回 `PopupAnimation::Fade`（P2 原文 §5.2 即 `Fade/None`；FadeScale 的 modal 退化仍由引擎 `modal_fade_scale_is_fade_only` 守护）。

同时订正 §14.6 D-P3-2 与 `p3-summary.md`（现写“modal/scrim 只淡入淡出，不缩放”，与实测相反）。

### P3K-02 [P3] 文档 D-P3-5 / §5.2 / §14.2 / p3-summary 对 Dropdown/Select 动画的描述与代码相反（实际已是 `FadeSlide`）

**证据**

- `dropdown.rs:26,178,501`：`panel: ItemPanel`、`self.inner.panel.open(app, window, PanelOpen{…})`；`select.rs:41,224,761-764` 同。
- `menu.rs:431-474` 的 `ItemPanel::open` **无条件**设置 `spec.animation = PopupAnimation::FadeSlide`（`menu.rs:468`），不区分 Menu / ContextMenu / Dropdown / Select。
- 探针实测（Dropdown 打开后读栈）：打开帧 `paint_offset=(0, 4)`，16 ms 后 `(0, 2.604)`、`state=Open` ⇒ Dropdown 实际在跑 FadeSlide（Select 共用同一 `ItemPanel`）。
- 文档却在 4 处写成“Dropdown/Select 仍为默认 Fade”：`§14.6 D-P3-5`（并据此登记 P4 候选）、`§14.2 P3-2`、`§5.2:719`、`p3-summary.md:4/87/95`。§14.1 自述“`ItemPanel` 打开动画改 `FadeSlide`”，与 D-P3-5 自相矛盾。

**评估**：行为本身**符合** P3-2 计划与本次裁决（四组件都 FadeSlide），因此**不需要改代码**；问题在文档/登记。请订正上述 4 处（并把 D-P3-5 从“P4 候选”改为“已实现”），建议顺带补一条 Dropdown/Select 的 slide 断言（打开帧 `paint_offset.y > 0`、`settle` 后归零且 settle rect 不变），否则这 2 个组件的动画切换完全无测试。

### P3K-03 [P3] D-P2-10/D-P3-9 的“accesskit 限制”理由不成立：Separator 已映射 `Splitter`，Group 角色也存在

**证据**

- `ui_core/src/a11y/mapping.rs:57`：`A11yRole::Separator => accesskit::Role::Splitter`；同文件 `:760-762` 的测试直接断言“ARIA separator → AccessKit 0.25 Splitter（UIA Separator/AT-SPI separator）”。kit Separator 行使用 `A11yRole::Separator`+`hidden`（`menu.rs:2647-2650`）⇒ **设计 §4.10 要求的 “Splitter + 不可聚焦/hidden” 实际已达成**，D-P2-10 的“Separator vs Splitter”半边是伪偏差。
- `A11yRole::Group` 存在（`a11y.rs:18`）且映射 `accesskit::Role::Group`（`mapping.rs:22`）；kit Group 行用 `A11yRole::Label`（`menu.rs:2642-2646`）是**真实偏差**，但真实理由是“菜单组头是独立 `Label` 行而非容器（`Group` 作为无子项容器语义不合适）”，不是“accesskit 缺角色”。
- P3 新测试 `group_and_separator_rows_expose_their_a11y_roles`（`menu.rs:3770-3805`）把 `A11yRole::Separator`/`A11yRole::Label` 固化为期望并注明“D-P2-10 登记”；若 P4 改 Group 语义需同步改此测试。

**建议**：§13.4 D-P2-10 / §14.6 D-P3-9 改写偏差理由（Separator 可宣告“已满足设计”；Group 写真实产品/结构理由并保留 P4 评估）。代码行为本身符合登记，**不阻塞**。

### P3K-04 [P3] `PopupScale::compose`/`inherited_scale` 是不可达代码，且组合数学在 pivot 不同/外层≠1 时错误；§14.3 却宣称支持“外层×内层 compose”

**证据**

- 可达性：popup content 全部由 `app/popup.rs:129-141 create_content_node` 建在 `popups.root()` 下（平铺）；唯一写入 `PopupTransform` 的路径是 popup entry node（`animation.rs`/`dismiss.rs`/`settle.rs`），不会嵌套。因此 `paint_node` 沿节点继承时 `inherited_scale` 恒为 `None`，`PopupScale::compose`（`paint/pass.rs:243-251`）**永远不可达**。
- 数学：`outer∘inner` 的等价“绕 pivot 缩放”固定点为 `[(1−F_o)P_o + F_o(1−F_i)P_i] / (1−F_oF_i)`；实现取 `P_o + F_o(P_i−P_o)`，仅当 `F_o=1` 或 `P_o=P_i` 时相等。当前不可达，但一旦将来出现嵌套（或有人复用该函数）会以错误 pivot 缩放。
- 文档 `§14.3` 与 `p3-summary.md:33` 均写“`PopupScale` 沿节点继承并 `compose`（外层×内层、pivot 也经外层变换）”，把不可达路径描述成能力。

**建议**：按 `AGENTS.md §1.2/§6` 删除 `compose` 与 `inherited_scale` 线程（当前 popup 树平铺，单层 transform 足够），并同步文档；若确认要支持嵌套 transform，则补嵌套用例并修正为固定点公式。

### P3K-05 [P3] `paint_scales` 侧表在 `NodeTree::remove` 未清理（其他侧表都清理），删除测试未覆盖

**证据**

- `node/tree/arena.rs:323-325`：插入节点时只对 `popup_transforms` 做 resize；`arena.rs:578-615` 的移除循环清理 text/editable/debug/clip/baked/painted/paint_marks/scroll_slot/**popup_transforms**/popup_pointer_transparent/a11y，**没有 `paint_scales`**（该表在 `arena.rs:76` 初始化、`content.rs:813-826` 读写）。
- `node/tree/tests.rs:113-166` 的 `remove_clears_the_extent_and_baked_screen_bounds_side_tables` 仅在 `PopupTransform` 字面量里补了 `scale/pivot` 字段，**没有**断言 `paint_scales` 被清。
- 影响：槽位复用后 `paint_scale_of` 可能读到旧的非 1.0 值 ⇒ `paint_state` 判不可复用，多一次 lane 重建（当前无错误渲染，随后自愈）。但违反“侧表生命周期与节点一致”（设计 C3）与 `AGENTS.md §1.1/§4`，且现有测试已为同类侧表建立断言先例。

**建议**：remove 时把 `paint_scales[index]` 置回 `1.0`（或 `resize_with`/清槽），并在现有删除测试里加一条“复用槽位不得继承 paint_scale”。

### P3K-06 [P3] lane scale 的逐 primitive 分支只有 Quad/Canvas 有渲染断言；`Primitive::Text`（CanvasText）只缩放 origin

**证据**

- 新增 `PopupScale::primitive`（`paint/pass.rs:293-356`）覆盖 Quad/Shadow/Image/GlyphRun/Path/Geometry/Clip/Canvas/Text；`scale_canvas`（`:358-366`）递归重建 Canvas。
- P3 的 9 条引擎测试里，几何断言都来自 `painted_quad`（`popup_behavior.rs:5458-5466`，只查 `record.quads`）与 `fade_scale_scales_canvas_content_about_the_popup_center`（`:5857-5912`，只查 Canvas 里的 Quad）。**GlyphRun（文本位置+字号）、Image（图标）、Path/Geometry transform 组合、Shadow blur、Clip corner radius、border width、Geometry transform 均无任何断言**。
- `Primitive::Text`（CanvasText）只缩放 origin、字号不变（`paint/pass.rs:350-353`）。当前两个 FadeScale 组件（Dialog 内容 / ColorPicker）不含 canvas text，故无即时用户影响；engine lens（P3E §3）认为 canvas 文本在 push 前已 resolve 成 GlyphRun、该分支基本不可达 —— 若确认不可达，应删除该分支或注明，避免被误读为“字号缩放已覆盖”；这是实现不对称与文档表述问题，不构成用户影响。

**评估/建议**：ColorPicker 面板里的文本是 GlyphRun、图标是 Image —— 恰是未覆盖的两类。建议补 1–2 条渲染断言（缩放 popup 内一个文本节点/一个 Image 节点，比较 `rendered()` 的 glyph 位置/尺寸或 Image bounds），或明确登记“lane transform 的 primitive 覆盖仅有 Quad/Canvas 证据”。

### P3K-07 [P3] `scale` 不作用于 hit/a11y/IME 是未登记契约

**证据**：`layout/hit_test.rs:360-367` 只把 popup `offset` 叠加到命中点；`popup/stack.rs:645-665` 的 `popup_at/subtree_hit_contains` 用 `hit_rect.translate(paint_offset)`；IME caret 只加 offset（`window/focus.rs:38-46`）；引擎测试把“命中不跟随绘制缩放”作为期望（`popup_behavior.rs:5548-5556`：`assert_ne!(hit, root_node, "命中不得跟随绘制缩放")`）。即 0.98 缩放在 120 ms 内**只是渲染效果**，点击区/`a11y_rect`/IME 仍按稳定几何。

**评估**：与 D10“lane transform（渲染）”一致，2% 缩放下无害；但 §4.12 只对 slide 写了“hit 跟随 paint_offset”，scale 的这一契约未写进 §4.12/§14.3。建议明写并把该断言作为门禁（避免将来有人把 hit 也按 scale 处理导致 2% 边缘命中抖动）。

### P3K-08 [P3] P3-2 的可观测/门禁缺口：`--debug-stack` dump 不含 `paint_scale`，像素门禁不采样动画帧

**证据**

- `gallery.rs:3148` 的 debug-stack 行输出 hit/paint/a11y/offset/constrained，**未输出** P3 新增的 `PopupSnapshotEntry.paint_scale`（`popup.rs:341`）；真实窗口栈断言无法观察 scale。
- `--pixel-check`（94 checks）只在 ColorPicker 场景做残影/覆盖判定（P2 已知覆盖，§14.7 已登记 P4 扩展）；`--real-pixel-check` 只在稳定态截图（baseline → open → close 完成）。因此 P3K-01（scrim 缩放）与 P3K-06（GlyphRun/Image 缩放）**不会被任何像素门禁捕获**，§14.4/§14.7 未登记“动画帧不采样”。

**建议**：debug-stack 行加 `scale=`（`{:.3}`）；在 §14.4/§14.7 明确“像素门禁只验证稳定态；动画正确性由引擎实例断言（quad/canvas/glyph）保证”，并优先补 P3K-06 的文本/图片断言。

---

## 3. P3-1 / P3-2 / P3-3 核对表

### 3.1 P3-1 Dialog/Alert（recenter 删除 + 双份 closing）

| 检查项 | 结论 | 证据 |
|---|---|---|
| `Dialog::recenter` 删除彻底 | ✅ | `git diff` 删除 `recenter` 函数与 `sync()` 中的调用；`rg recenter` 全仓仅剩测试名/说明与设计文档；`centered_origin/center_anchor/apply_center` 仍被 **open 时**调用（`dialog.rs:451,901-913`），符合“居中只由 open anchor 决定” |
| resize/重新居中测试移除、替换合理 | ✅ | 旧 `resizing_the_window_recenters_the_dialog` 被 `resizing_the_window_keeps_the_anchor_opened_with`（`dialog.rs:1429-1468`）替换：断言 resize 后 origin 不变、且**不等于**新视口中心、popup rect == 面板布局盒、`is_open`/`state==Open` |
| 宿主/测试影响 | ⚠️ 未验证 | `cargo check --workspace --all-targets` 绿，但 workspace 测试按用户限制未跑；未发现下游测试显式依赖 resize 后居中（`rg set_size` 无 dialog 断言命中） |
| 居中/边界行为 | ✅（记录） | 探针：800×600 → resize 240×160 后 modal 被引擎 clamp 到 `(36,4)`（左右 36/4、上下 4/36），仍在视口内；窄视口下 `centered_origin().max(4)` 与 clamp 到 `viewport−8` 恰好对称（`a_narrow_viewport_clamps_...` 通过）。resize 后不对称是 D-P3-1 的既定裁决，建议 §14.6 补一句“缩小窗口后引擎按 viewport margin 钳制，可能不对称居中” |
| `is_open/is_closing/state` 语义 | ✅ | `Dialog::is_open`（id 或引擎 state）、`is_closing`（DismissPending/Closing）、`state` 透传未变；resize 不关闭（测试断言）；`DialogState.window` 仍被 `sync_scroll` 使用（`dialog.rs:802,832`），无死字段 |
| 双份 closing/reason | ✅ 已是 P1 完成态 | `Dialog` 只剩 `intent: DialogClose` 供 kit 回调语义，无自有 closing/reason 状态 |
| Alert | ✅ 继承 Dialog（D9/D11），无独立 popup | — |
| 回归 | ❗ P3K-01 | Dialog 的 FadeScale 引入 scrim 缩放（本次唯一 P2） |

### 3.2 P3-2 Scale（D10）

| 检查项 | 结论 | 证据 |
|---|---|---|
| 引擎 `FadeScale` + lane transform | ✅ | `popup.rs`（枚举/常量/`paint_center`/`paint_scale` 快照）、`animation.rs`（scale 插值 + tick 报告）、`dismiss.rs`（关闭反向 1→0.98）、`settle.rs`（pivot 跟随）、`paint/pass.rs`（`PopupScale` 施加于 quad/shadow/image/glyph/path/geometry/clip/canvas/text） |
| 首帧 rect == 稳定帧 / 不动布局 | ✅ | 引擎 `fade_scale_open_paints_the_panel_scaled_around_its_center`（hit/a11y rect == settled，绘制 quad 按 scale 围绕中心）；kit `the_panel_fades_and_scales_in_without_moving_the_settled_rect`、`the_menu_slides_in_without_moving_the_settled_rect` |
| 关闭帧无残影 / damage | ✅ | 引擎 `fade_scale_close_reverses_the_scale_and_keeps_damage_local`（damage 覆盖稳定 rect、移除帧仍局部）；kit 关闭帧 damage 覆盖旧 rect 断言；`--pixel-check`/`--real-pixel-check` 关闭后逐像素恢复（Windows differing=0） |
| `damage_full` | ✅ | 非模态 picker/menu 打开/动画/关闭帧断言 `!damage_full`；`--pixel-check damage_full_frames=0`；modal 仍整帧（P1 契约） |
| 动画期间 hit/a11y | ⚠️ 未登记（P3K-07） | slide：hit 随 offset（引擎测试）；scale：hit/a11y/IME 不随 scale（引擎测试作为期望），设计未写 |
| pivot 正确性 | ✅ | 首帧 pivot 由 settle 同步到 paint center 后才 paint（引擎测试断言首可见帧中心不变）；pivot 用 paint rect 中心，阴影造成 ≤2% 级偏移（可忽略） |
| FadeScale 子菜单/anchor drift 防护 | ✅（间接覆盖） | kit `Menu::ancestors_animating`+`watch_animation` 在父层动画期忽略 `Moved` 并在动画结束帧 `was_animating` 兜住 snap-back；P2 既有测试 `submenus_opened_during_the_root_opening_animation_survive`（`menu.rs:5963-5991`）在 FadeSlide 启用后**实际经过该守卫**（settle 会把 state 置 Open，root 动画期间 sync_anchor 不再被 `Opening` 跳过）——缺一条“关闭守卫即失败”的定向用例，但覆盖面成立 |
| modal fade-only | ⚠️ 部分（P3K-01） | Dialog 面板 scale=1（测试+探针）；**scrim 被缩放**（探针） |
| 旧测试放宽/删除 | ✅ | 引擎/kit 均无被弱化的旧测试（diff 删除行只有 recenter 测试与实现替换）；新增 9+9 条 |
| kit 采用范围 | ✅（文档错，见 P3K-02） | Menu/ContextMenu/Dropdown/Select = FadeSlide（经 `ItemPanel`）；ColorPicker = FadeScale；Tooltip = Fade；Dialog = FadeScale→modal 退化为 fade |

### 3.3 P3-3 ColorPicker

| 检查项 | 结论 | 证据 |
|---|---|---|
| 内容闭包 / 局部坐标 Canvas / tab `request_measure` / open-close 回调 | ✅ 沿用 P1 完成态 | `color_picker.rs:1367-1401`（内容闭包 + `on_open`）、`:2101`（tab measure）、`:2223`（close callback）；相关 P1 测试全保留 |
| `FadeScale` 采用 | ✅ | `color_picker.rs:1383`；新测试断言首帧 `0.98 ≤ scale < 1`、settle 后 1.0、hit/布局盒不变、8 个动画帧全部局部 damage、关闭帧覆盖旧 rect |
| 锚点丢失 reason | ✅ | `color_picker.rs:1033-1052` 改 `close_with_reason(AnchorLost)`；**但没有专门的 P3 测试覆盖 ColorPicker 的 AnchorLost 回调**（Dropdown/Select/ContextMenu/Menu 都有；ColorPicker 只有 `the_on_close_callback_fires_once_per_close` 之类程序化路径）→ 建议补 1 条（低成本，属 P3K-06 同类测试缺口，不单列 finding） |
| 剩余 P3-3 项 | ✅ | 设计 §12.6/§14.2 所列项均已在 P1 完成，本次只增加动画与 reason |
| 动画期间输入 | ✅ 记录 | scale 仅渲染（P3K-07）；120 ms 内 hit 用稳定 rect，无“点不到”风险（缩放≤1 只会在稳定 rect 内收缩） |

---

## 4. 测试真实性 / 负向自检

### 4.1 新增测试（引擎 +9、kit +9）与真实性

- **引擎 9 条**：`fade_scale_open_paints_the_panel_scaled_around_its_center`、`fade_scale_close_reverses_the_scale_and_keeps_damage_local`、`fade_scale_paints_the_same_logical_geometry_at_dpr_one_and_two`、`modal_fade_scale_is_fade_only`、`fade_scale_coexists_with_fade_and_fade_slide`、`fade_scale_scales_canvas_content_about_the_popup_center`、`begin_close_popup_forwards_an_explicit_reason_to_the_dismiss_handler`、`idle_frames_do_no_popup_settle_or_damage_work`、`popup_open_and_close_frames_stay_within_the_frame_budget`。
  - 真实性：`painted_quad`/`rendered().primitives` 读的是**实际渲染实例**（不是布局/快照字段），断言“绘制矩形 = settled rect 按 scale 绕中心缩放”+“两帧中心不变”+“damage 覆盖稳定 rect”+“DPR 1/2 逻辑几何一致”，能证伪 pivot/缩放/damage 错误；`fade_scale_scales_canvas_content...` 还覆盖 Canvas 内 quad。
  - 负向强度：`fade_scale_paints_the_same_logical_geometry_at_dpr_one_and_two` 校验 `frame_dump` 物理实例；`idle_frames_...` 断言 0 settle/0 damage/0 popup 工作；`popup_open_and_close_frames...` 有 p95 预算（阈值口径见 D-P3-8，已登记）。
- **kit 9 条**：recenter 反证、slide/scale 首帧与 damage、modal fade-only、锚点矩阵 reason、AnchorLost（Menu/ContextMenu/Dropdown/Select）、`close_with_reason` 单次回调、Group/Separator a11y。
  - 真实性：`the_menu_slides_in_without_moving_the_settled_rect` 断言打开帧 `paint_offset.y ∈ (0,4]`（没断言**恰好 4**，略松但可证伪“无动画”）；`the_anchor_matrix...` 用真实窗口 resize 触发，断言 reason 序列精确等于 `[AnchorLost]` 再 `[AnchorLost, Programmatic]`；`closing_with_an_explicit_reason...` 断言 `on_close` 恰好一次且 reason 透传；`group_and_separator...` 断言 role/label/hidden/无 Focus action。
  - 未覆盖：Dropdown/Select 的 slide 本身（见 P3K-02 建议）、ColorPicker 的 AnchorLost 回调、scrim 不缩放（P3K-01）。
- **测试保留性**：`git diff f24a3441` 的删除行仅包含 recenter 测试与实现替换（无断言被放宽/删除）；gallery 只新增 `--real-pixel-check`（2023 行）与 flag/usage，**`--pixel-check`（94 checks）/`--debug-stack` 语义未动**；逐文件 `#[test]` 计数与 §14.5 完全一致（menu 79 / menu_chain 13 / context_menu 21 / popup_panel 19 / dropdown 28 / select 33 / dialog 55 / tooltip 28 / alert 11 / color_picker 81；popup_behavior 120）。

### 4.2 负向自检

- `--pixel-check`：保留既有“2 点注入故障”自检（输出 `residue = … + 2-point injected-fault self-check`）。
- `--real-pixel-check`：实测输出 3 组负向自检均判红 —— `self-check-offset`（基线平移 (4,3)，differing=27236）、`self-check-recolor`（改色块，differing=2304）、`self-check-identical`（未改动必须 0 差异）；另有 `trigger-exclusion`（悬停/焦点敏感触发器环排除）。检测器可信度合格。
- 缺一条：**scrim/动画帧的像素负向用例**（P3K-01/P3K-08）。

---

## 5. 残余风险与跨 lens 观察（不单列 finding）

1. **workspace 测试未跑**（沿用用户限制）：`crates/widget`/业务 crate 的 dialog/select 宿主测试可能依赖旧 recenter 或旧动画时长；`cargo check --workspace --all-targets` 绿，但运行期行为未验证。建议提交后 CI 或 lead 补跑一次 workspace test。
2. **文档仍在并发写入**：本报告引用的 §14/D-P3-x/p3-summary 为 16:52 快照；lead 需在文档定稿后复核 P3K-01/02/03/04/08 涉及的表述。
3. **三端真实像素证据在仓库外**：macOS/Linux 原始证据在远端 `/Users/...`、`/home/...`，本工作树只有 `target/real-pixels/{macos-remote,linux-remote}/` 副本（已核对 `checks=50 failures=0`）；`target/` 不入版本库，提交后不可复现，需 acceptance/文档记录可复现命令（已有脚本）。
4. **FadeSlide 方向**：引擎 `slide_origin(Side::Bottom) = +4 → 0`（自下方 4 px 上移入位），P1 引擎测试即锁定该方向；kit 首次在生产启用，若产品期望“自触发器向下展开”需另行裁决（不作为本轮缺陷）。
5. **scale pivot 取 paint rect 中心**（含阴影外延）：阴影不对称时 pivot 偏离面板中心，2% 缩放下位移 ≤0.2 px，可忽略；若未来调大 scale 幅度需改为 hit rect 中心。
6. **性能守卫阈值**：设计目标为 `idle p95 + 2 ms`，测试守卫为 `+16.7 ms`（D-P3-8 已登记，并把实测 idle≈1 µs / open≈1.0 ms / close≈63–65 µs 写入 §14.2），可接受；但 p95 采样仅 8 帧、单机单场景，属基线而非回归门禁。
7. **P3-5/P3-6 的验收口径**：`--real-pixel-check` 已 Windows/macOS/Ubuntu 50 checks PASS（验收 agent 证据）；Linux 内容尺寸受 compositor 影响（1276×670 vs 请求 960×720），断言数量一致而非像素一致——§14.4/§14.9 已写明，属已登记差异。
8. **P3-4/§12.6-6 pixel 扩展**：`--pixel-check` 仍只覆盖 ColorPicker（§14.7 已登记 P4）；Tooltip/Alert/Dropdown/Select 无像素门禁。
9. **提交前核对（非 finding）**：`tools/real-pixel-check-{macos,linux}.sh` 目前 untracked，提交必须 `git add tools/`；`--real-pixel-check` 的 Windows 命令与脚本在 `CARGO_TARGET_DIR` 非默认时行为不同（脚本假定 `./target/debug/...`），验收复跑需默认 target。

### 5.1 跨 lens 关联（与本轮并发的 `review-p3-engine.md` 独立复现/互补）

| 本报告 | engine lens | 关系与说明 |
|---|---|---|
| P3K-01（scrim 关闭缩放，kit 探针） | **P3E-02 [P2]** | 同一根因，两 lens 独立实测（engine：`paint_scale=0.9825`；kit：Dialog 关闭第 1/2 帧实际绘制 quad `(3.55,2.66) 792.9×594.7` / `(7.07,5.30) 785.9×589.4`）。lead 去重后按 1 个 P2 处理，修法同一处 |
| P3K-02（D-P3-5 文档与代码相反） | **P3E-07a** | 独立确认：Dropdown/Select 经 `ItemPanel` 已是 FadeSlide；两 lens 都给出“改文档、不改代码”的结论 |
| P3K-04（compose 不可达+公式错） | **P3E-06** | 同一问题，独立复现；建议合并为一条 |
| P3K-05（`paint_scales` 未清理） | **P3E-05** | 同一问题；合并 |
| P3K-03/07/08（a11y 登记理由、scale 不参与 hit/a11y、门禁观测缺口） | — | kit lens 独有 |
| — | **P3E-01 [P2]（`Shadow.offset` 当点变换）** | **kit 可见影响**：ColorPicker 的 surface 用 `PopupSurface::menu()` 阴影（`color_picker.rs:92-95` → `popup_panel.rs:45-68`），因此 ColorPicker 的 FadeScale 打开 120 ms 内阴影会按 `(1−f)×pivot` 漂移约 10–15 logical px 再回弹；`the_panel_fades_and_scales_in_without_moving_the_settled_rect` 只断言 scale/damage，`--real-pixel-check` 只采样稳定态 ⇒ kit 侧门禁同样漏检。修复在引擎（本报告不重复计数），但 kit 验收应以修复后 ColorPicker 动画帧做一次目视/像素复核 |
| — | **P3E-03/04/08/09**（性能口径、空闲帧恒真、`close_with_reason` stale reason、settle pivot 时序） | 引擎 lens 覆盖；其中 P3E-08 与 kit `PopupPanel::close_with_reason` 先写 reason 再 `begin_close_popup` 的实现有关（`popup_panel.rs:461-474`），建议修复轮一并处理 |

> **跨 lens 合计**：本轮**可见缺陷的根因共 2 个 P2**（① 关闭动画给 `PopupTier::Scrim` 施加 0.98 缩放；② `PopupScale` 把 `Shadow.offset` 当绝对点变换），kit 侧用户可见面分别是 Dialog 关闭亮边与 ColorPicker 打开阴影漂移；其余为文档/测试/卫生类 P3。

---

## 6. 结论

| 严重度 | 数量 | 编号 |
|---|---|---|
| P0 | 0 | — |
| P1 | 0 | — |
| P2 | **1** | P3K-01（Dialog FadeScale 使 scrim 关闭期缩放） |
| P3 | **7** | P3K-02（D-P3-5 文档与代码相反）、P3K-03（D-P2-10/D-P3-9 理由不成立）、P3K-04（compose 死代码+公式错）、P3K-05（paint_scales 未清理）、P3K-06（primitive 覆盖/CanvasText 不对称）、P3K-07（scale 不参与 hit/a11y/IME 未登记）、P3K-08（debug-stack/像素门禁不覆盖 scale/动画帧） |

**最关键 3 条**

1. **P3K-01（P2）**：Dialog 的 `FadeScale` 经引擎关闭路径把 `PopupTier::Scrim` 一起缩到 0.98；kit 探针实测关闭第 1/2 帧 scrim 绘制 quad = `(3.55,2.66) 792.9×594.7` / `(7.07,5.30) 785.9×589.4`（视口 800×600），窗口四周 3–7 px 未压暗，与 §14.6 D-P3-2“scrim 不缩放”的声明相反，且无任何测试/像素门禁覆盖。修法：引擎把 Scrim 视作 no-scale，或 Dialog 回 `Fade`；两者都 ~1 行 + 1 断言。
2. **P3K-02（P3）**：D-P3-5 及 §5.2/§14.2/p3-summary 四处称“Dropdown/Select 仍 Fade”，但两者走 `ItemPanel`，实测打开帧 `paint_offset=(0,4)` ⇒ 已是 FadeSlide。行为符合裁决，**改文档**（否则 acceptance/后续阶段按错误动画假设验收，且 D-P3-5 的“P4 候选”是伪待办）。
3. **P3K-04（P3）**：`PopupScale::compose`/`inherited_scale` 不可达（popup 树平铺），且组合 pivot 公式在 `F_o≠1 且 P_o≠P_i` 时错误；§14.3 却宣称支持“外层×内层 compose”。建议删除并同步文档，或补嵌套用例+修正公式。

**是否可提交 P3**：**当前不可提交**（1 条 P2 + 2 条与行为/证据相反的文档登记）。完成下列最小修复后 P3 可提交：(1) 修 P3K-01（引擎或 kit）+ 断言 scrim `paint_scale==1`；(2) 订正 P3K-02 的 4 处文档（并把 Dropdown/Select 动画断言补上为佳）；(3) 订正 P3K-03 的偏差理由；(4) P3K-04/05 建议同轮删除/清理（低风险纯收益），P3K-06/07/08 可在 §14.6/§14.7 登记为 P4 后提交。其余门禁与三端像素证据均通过，P3-1/P3-2/P3-3 主体交付真实性成立。

**跨 lens 提醒**：按本轮并发的 engine lens（`review-p3-engine.md`），还有一个 P2 根因 **P3E-01（`PopupScale` 把 `Shadow.offset` 当绝对点变换）**；其 kit 可见面是 **ColorPicker 打开动画期间阴影漂移 ≈10–15 px**（ColorPicker surface 用 `PopupSurface::menu()` 阴影）。该问题不在 kit 代码内、本报告不重复计数，但与 P3K-01 一样不会被现有 kit 测试与 `--real-pixel-check`（仅稳定态）捕获，建议同一修复轮处理并在修复后对 ColorPicker 动画帧做一次复核（见 §5.1）。
