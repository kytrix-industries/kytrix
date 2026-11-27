# 浮层（Popup / Overlay）机制重构设计与实施计划

- 状态：**实施中（rev.7）**；rev.5 已通过三轮独立评审（无 Blocker/Major）；P0-0 判别实验已完成（`evidence/p0-diagnosis.md`），按结论收窄 P0-2/P0-3（见 §2.3、§6）；**P0 已提交（`d9949aca`）；P1 实施完成、待提交**（旧 overlay/layer 已删除，引擎 popup 栈 + kit `PopupPanel` + 全组件 + 宿主迁移落地，见 §12）
- 范围：`crates/ui_core`（引擎：popup 子系统 + 渲染/损毁不变量）与 `crates/ui_kit`（组件：Dropdown / Select / Tooltip / Menu / ContextMenu / Dialog / Alert / ColorPicker），以及 `crates/widget` 宿主与全部下游调用点
- 依据：`popup-overlay-redesign/evidence/` 下 5 份调查/复现报告与截图；`popup-overlay-redesign/review/` 下 3 份独立评审；P1 迁移清点 `implementation/p1-migration-inventory.md`、收口记录 `implementation/p1-cleanup-w7.md`、P1 早审 `implementation/review-p1-engine-{behavior,api}.md`
- 源码基准：worktree `dsh-1d175414-1a41`，P0 基线 HEAD `71a5a5cc`（P0 提交 `d9949aca`）；文中 `file:line` 为定稿时位置，实施时以符号名为准

---

## 0. 决策记录（用户已裁决）

| # | 决策点 | 结论 |
|---|---|---|
| D1 | 架构归属 | **A：引擎级一等公民**。`ui_core` 拥有 popup 栈、定位求解、尺寸约束、关闭/焦点/模态/重绘契约与 a11y 关系；`ui_kit` 只保留外观、内容构建、业务回调与菜单链交互策略 |
| D2 | 承载模型 | **A：单窗口 in-tree overlay root**。不引入原生弹出窗口；所有浮层是窗口内专用 overlay 根下的节点，绘制/命中/a11y 顺序由引擎统一管理 |
| D3 | 定位时序 | **A：布局后 popup settle pass**。`layout(+text) → popup measure/place → 必要时二次 layout → paint`，总计最多 2 轮；首个可见帧位置必须正确 |
| D4 | 重构与修复范围 | **A：全量**。引擎 popup 子系统 + 渲染/损毁不变量（残影根因所在）+ kit 全部相关组件重写 + 跨 crate 迁移；删除旧 API，不做兼容层、不做过渡实现 |
| D5 | 菜单交互基准 | **A：桌面惯例（Qt/Chromium 型）**。hover 延迟打开子菜单 + safe-triangle/指针桥；Esc 只关最上层；外部点击关整链（replay 见 D14）；键盘打开时焦点进入菜单；点击父项打开子菜单 |
| D6 | 层级与模态 | **A：单一全局栈**。z 由单调序号决定，后开在上；绘制/命中/a11y 同一顺序；模态浮层阻断下层输入/焦点/a11y，并在打开时关闭无关 popup 链；tooltip 为可穿透的 hint 层 |
| D7 | 动画 | **A：淡入淡出 + 轻微位移/缩放**。位置在首帧即正确，动画只作用于 opacity 与渲染偏移，不参与布局定位；scale 按 D10 在 P3 追加 |
| D8 | 验收与门禁 | **A：沿用 text-input 重构模式**。不变量单测 + 像素级残影回归（真实窗口/持久增量通路）+ gallery 自动化 + Windows/macOS/Linux 三端验收 + 实施后偏差回写 + 独立审查 |
| D9 | 组件范围 | Dropdown、Select、Tooltip、Menu（含 2–3 级子菜单）、ContextMenu、Dialog、Alert、ColorPicker 迁移。**Toast/Notification 不做；菜单栏（menubar）本轮不做；MessageBox 不新建（项目当前无此组件，Alert 继续作为 Dialog 内容面板）** |
| D10 | scale 动画 | **B：P3 追加**。实现 0.98→1 的 lane transform（0.98→1，120ms），P1 仅做 fade+slide |
| D11 | MessageBox | **A：不新建、从计划移除**（项目当前没有 MessageBox 组件） |
| D12 | Tooltip 交互 | **A：纯提示**（Hint tier、穿透、不接收鼠标/键盘；Esc 只关自身；hover 区域含 tooltip rect） |
| D13 | 菜单 hover 焦点 | **A：hover 不夺焦**；键盘事件才把 DOM focus 移入最深打开层 |
| D14 | 外部点击 replay | **B：replay**。关闭整链后把该次 press+release 重新派发给下层命中控件；opener 上的按下仍按 Toggle 消费（避免二次打开） |
| D15 | 菜单项激活 | **A：release 激活**（press 与 release 同一 item 才激活） |
| D16 | Dialog 初始焦点 | **A：默认按钮优先**，无默认按钮回退第一可聚焦，再回退容器 |

以下技术约束不是偏好选择，而是调查与评审确认的硬约束：

| # | 硬约束 | 依据 |
|---|---|---|
| C1 | 定位不能依赖“上一帧几何”；必须在本帧 layout/text 之后求定位，定位结果可触发二次布局，且首可见帧位置正确 | 引擎审计 F1/F5；kit 审计缺陷 2；复现“首帧面板在最终位置、色板在旧位置” |
| C2 | 内容尺寸可能晚于内容创建（文本排版、Input 行盒、列表行高、tab 切换），定位必须使用本帧测量后的尺寸 | 引擎审计 §3.3；kit 审计 §1.4、缺陷 11 |
| C3 | 保留绘制数据（lane / instance ops / scroll slot）的生命周期必须与节点生命周期一致；旧内容不得在局部重绘中被再次绘制 | paint/damage 报告；评审 02 的 R1 反向核查结论：该缺陷需先判别再落修法 |
| C4 | 移动/隐藏/删除必须 damage “上一帧实际绘制范围 ∪ 本帧绘制范围”，含阴影、焦点环、Canvas/Geometry 外溢 | paint/damage 报告 R2；引擎审计 F4 |
| C5 | 浮层必须位于不受滚动/裁剪容器影响的独立根层；普通根子节点不得插到浮层之上（popup root 之后只允许 Debug/HUD 层） | 引擎审计 F2；kit 审计 §2；评审 01/02 |
| C6 | 关闭/外部点击/Esc 必须由“引擎产生 dismiss 事件 → owner 决策 → owner 执行关闭（含动画/回调）”，不得引擎直接删节点 | 引擎审计 F3；kit 审计缺陷 6 |
| C7 | 锚点必须显式指定且坐标系可区分（窗口坐标 / 内容坐标 / 节点）；现有 `Anchor::Fixed` 与 ContextMenu 已因坐标混用产生 bug | kit 审计 §5.5、缺陷 10 |
| C8 | 引擎不得依赖“全量重绘逃生口”保证浮层正确性；非模态 popup 开关/移动不得 `damage_full` | 引擎审计 F4；paint/damage §6；评审 01 M11 |
| C9 | 像素级残影门禁必须使用“持久增量重绘通路”（真实窗口或新增 harness）；现有离屏 `render_offscreen` 固定走 `FramePlan::Full`，不能复现残影 | 评审 02 Blocker 6 |

---

## 1. 摘要

当前浮层机制：

- **引擎**：浮层 = 直接挂在 root 下的绝对定位节点 + `OverlaySlot`；定位只有“点/锚点四方向”，只在**布局之前**解析一次（`app/frame.rs:59`，layout 在 `:128`）；z 序 = root 子节点顺序；关闭由引擎在 pointer-down 中直接 `remove_node`。
- **kit**：`kit::overlay::Overlay`（2692 行）在应用层重复实现定位/flip/clamp/淡入淡出/首帧隐藏（`frame_at_open`+`revealed`）；`Layer`（Band+LayerSlot）用重排 root 子节点模拟 z 序；`ItemPanel` 只对 menu/dropdown/select 私有，ColorPicker 自己 `open 空面板 → build_panel`。

结论：该机制只够支撑“单个、无层级、静态锚点”的 popup 原型；无法满足下拉/选择/提示/菜单/右键菜单/对话框/2–3 级级联菜单。用户报告的 palette 残影是“定位晚一帧 + Canvas 绝对坐标/无裁剪 + damage 覆盖缺口”叠加的结果；其中**定位晚一帧与 Canvas 坐标问题是代码+复现双重证实**，而“保留实例/op 泄漏”这一候选机制与代码存在矛盾，必须在 P0 用 E1–E3 判别后再落修法（见 §2.3）。

本设计把浮层做成**引擎级子系统**：独立 popup root、单调 z 序、布局后 settle、纯函数 placement、tier/chain-aware dismiss/focus/modal/a11y、以及“移动/隐藏/删除 = 上一帧绘制外延 ∪ 本帧外延”的 damage 契约；kit 退化为 `PopupPanel` + 组件外观 + 菜单链状态机。实施分 **P0（渲染不变量 + Canvas 坐标契约，含全量调用点迁移）→ P1（引擎 popup 内核 + kit 全部组件 + widget 宿主迁移，原子切换并删除旧 API）→ P2（菜单链/2–3 级子菜单）→ P3（Dialog/Alert/ColorPicker 细化、scale 动画、三端验收、性能、文档回写）**。

---

## 2. 现状调查（证据链）

### 2.1 分层现状

```
kit 组件（Menu / Dropdown / Select / Tooltip / Dialog / Alert / ColorPicker / ContextMenu）
  └─ kit::overlay::Overlay（自算定位、flip/clamp、淡入淡出、frame_at_open/revealed）
       ├─ kit::layer::Layer（Band 10/20/30/40/50 + LayerSlot，靠重排 root 子节点表达 z 序）
       └─ ui_core::App::create_overlay / place_overlay / bring_overlay_to_front / close_overlay
            └─ root 的直接子节点 + OverlaySlot{dismiss, placement, trap_focus, opener, restore_focus}
```

七个 kit 组件确实共用同一个 `Overlay`，但每个组件各自拼 `OverlayOptions` + 自建开关/焦点状态机；`Layer`/`AnimationRouter` 还通过 `crates/widget` 的 `WidgetServices`/`WidgetContext` 暴露给 14 个 business widget crate（见 §5.3）。

### 2.2 帧序现状（问题的根）

```
平台泵：
  app.run_frame_hook()            platform/windows/event_loop/pump.rs:646-654   ← kit 所有 sync()/place() 在这里
  frame_all() → App::frame()      app/frame.rs:25-368
    timers / animation completions
    normalize_scroll_offsets      :58
    resolve_overlay_placements    :59    ← 引擎唯一 popup 定位入口，早于 layout，用上一帧 bounds
    tick_animations               :61
    refresh_styles                :88
    refresh_text                  :115
    layout_if_dirty (taffy)       :128
    layout_text (parley)          :140
    clamp_scroll_offsets          :152
    resolve_canvas_text           :156
    canvas-text eviction loop     :167-192
    ensure_visible / blink / IME  :196-198
    paint_frame                   :211
```

结论：**定位不是布局的一部分**，而是布局前的一次命令式解析；内容尺寸变化、嵌套子菜单、滚动/resize 全部至少滞后一帧（引擎审计 §3.3、§4；kit 审计 §1.4；评审 02 Major 7 进一步指出插入点必须落在 :196 之前并重跑 virtual list/scroll）。

### 2.3 残影 BUG 根因（证据分级）

复现环境：Windows 11、DPI 1.25、客户区 1200×800、预编译 gallery（HEAD `71a5a5cc`）；`--dump-overlay` + GUI 自动化双路径；完整证据 `evidence/reproduction.md`、`evidence/paint-damage-rootcause.md`。

**观测事实（可复跑）**

| # | 事实 | 证据 |
|---|---|---|
| O1 | 打开 ⑭ ColorPicker 后节点几何正确：`overlay:group (36,79 300×375)`、`panel.content (44,87 284×359)`、`palette (44,115 284×284)`、footer `(44,416 284×30)`；两轮 `--dump-overlay` 一致，`overlay-check` PASS | reproduction.md §2/§3 |
| O2 | 同时画面里存在第二份 12×12 色板，整体平移 **+413 逻辑 px** 绘制在 `(44,529)` 起；只从其它分区/日志区空隙露出 8–12 px 窄带；灰阶行=第 0 行、底部两行=第 10/11 行，颜色逐像素一致 | reproduction.md §3/§4 |
| O3 | `413 = 375（面板高）+ 2×4（gap）+ 30（trigger 高）`，精确等于 `Overlay::open` 用 `LogicalSize::ZERO` 的初始 origin `(36,492)` 与 `place()` 修正后 origin `(36,79)` 之差 | reproduction.md §3 |
| O4 | 交互路径下残影**静态保持约 4.8–5.4 s、0 像素变化**；窗口 resize（全量重绘）后第一帧清零；关闭浮层时淡出期间仍在、卸载帧清零；重新打开再次出现 | reproduction.md §4/§5 |
| O5 | 转折帧：同一帧“面板 chrome 在最终位置、整幅色板在修正前位置”，下一帧色板才归位 | `evidence/shots/18-transient-misplaced-body-frame.png` |
| O6 | `overlay-check` 只检查 menu 行数/content/thumb 与 picker open/children 数量，**不检查 rect 也不检查像素**，因此对残影完全免疫 | gallery.rs `pass()` 逻辑（~173-190、2811-2824）；评审 02 Minor 19 |
| O7 | 程序化打开路径（`--dump-overlay`）在打开帧后 1–2 个普通帧内清除残影；交互路径则永久保留 | reproduction.md §4（programmatic 对照） |

**候选机制与证据强度（不得混同为“已证实”）**

| 机制 | 内容 | 状态 | 依据 |
|---|---|---|---|
| M-A 定位晚一帧 | `Overlay::open` 用 `LogicalSize::ZERO` 求初始 origin（`kit/overlay.rs:734-740`），`place()` 用上一帧尺寸修正且 reveal 先于 move（`:842-862`）；ColorPicker 先 open 空面板再 build（`color_picker.rs:1313-1342`） | **已证实**（代码 + O3 + O5） | 复现 §3；kit 审计缺陷 3/5 |
| M-B Canvas 绝对坐标 + 无裁剪 | `sync_body_canvases` 用 `node_bounds` 的窗口绝对坐标生成画布（`color_picker.rs:2077-2119`；`palette_canvas` `:328-363`）；Canvas 默认不裁剪（`paint/pass.rs:294-301,1577-1590`），overlay 面板/group 无 `overflow_hidden`（`overlay.rs:959-1032`） | **已证实**（代码 + O2/O5） | kit 审计 §5.3 |
| M-C damage 覆盖缺口 | 移动/移除只并 `layout.bounds`，不含 shadow/焦点环/Canvas 外溢（`node/tree/arena.rs:375-388`；`node/tree.rs:301-303`）；Canvas damage 用未 bake 坐标（`paint/pass.rs:1594-1611` vs `:666`）；旧位置未进入 damage 后留下静态像素 | **已证实（P0-0 收窄）**：本残影的驻留机制是「打开路径在错误位置画了一次、旧位置从未进入任何 damage」，不是「移动/移除时忘记旧绘制外延」；E2/E9 覆盖式 damage 一帧清零 | `evidence/p0-diagnosis.md` §0/§4/§5；paint/damage §2/§3/§9 R2 |
| M-D 保留实例/op 泄漏 | 旧 lane/ops 在释放/顺序变化时未重建，局部重绘反复画旧内容 | **refuted（P0-0 E1/E2/E3/E9）**：局部新 damage 不显影；1px resize/滚动全量重绘不出现完整 12 行色板；3 轮开关不叠加；与代码核查一致 | `evidence/p0-diagnosis.md` §3 E1–E3/§4 |
| M-E scroll slot/烘焙不匹配 | 槽位索引只在全量 paint 重置（`paint/pass.rs:108-117,493-524`），增量帧沿用缓存，delta 按索引取偏移（`render/delta.rs:247-257`） | **refuted（P0-0 E9）**：滚 1px 后残影一帧清零，任何位置都不出现被平移的色板；+413 是定位差不是滚动差 | `evidence/p0-diagnosis.md` §3 E9/§4 |

**P0-0 判别实验（已完成，证据 `evidence/p0-diagnosis.md`）**

- **E1**：残影出现后不 resize，只在残影附近制造新的小 damage（HEX caret/焦点环/色板格点击）。旧 rect 内新 damage 不显影、残影逐像素不变 ⇒ M-D 排除。
- **E2**：1px resize 后第一帧残影清零，280 帧内从未出现完整 12 行色板 ⇒ M-D 排除，damage 类成立。
- **E3**：关闭/重开 picker 3 轮（含关闭后空转 ≥3 帧）残影位置与像素完全相同、不叠加、关闭后 0 残留 ⇒ M-D 排除。
- **E9**：滚轮 1 tick（≈1.2 逻辑 px）后一帧内残影清零，任何位置都不出现被平移色板 ⇒ M-E 排除，+413 是定位差不是滚动差。
- **E4（可选）**：菜单开关后无阴影环残留（被根级浮层关闭的整窗 damage 掩盖）⇒ R2 的“移除外溢”子项 inconclusive，留 harness 锁定。
- **判定**：R1/M-D refuted；R4/M-E refuted；R2/M-C confirmed（驻留机制，收窄为“打开路径旧位置未进 damage”）；R3/M-A confirmed（错位来源）。
- **结论**：最小修复集 = kit 定位时序（P1 的 settle/内容闭包覆盖）+ Canvas 局部坐标契约（P0-4）+ 面板 `overflow_hidden`（P0-6）+ 持久增量像素门禁（P0-5）；P0-2 降级为纯防御性不变量测试，P0-3 从本轮移除。

**结论**：用户“定位晚一帧导致残留没清”的判断方向正确（M-A），但必须同时修 M-B/M-C；M-D/M-E 已被 P0-0 实验否证，禁止无证据地重写 ops 生命周期（评审 02 Blocker 1/2）。相关证据源编号保留 paint/damage 报告的 R1–R5（见 `evidence/paint-damage-rootcause.md` §9），本文引用时写“evidence-Rx”。

### 2.4 引擎缺陷清单（按严重度）

完整推导见 `evidence/engine-overlay-audit.md` §11。

| # | 严重度 | 缺陷 | 关键证据 |
|---|---|---|---|
| F1 | P0 | 定位只在 layout 前解析一次、用上一帧尺寸；无布局后二次解析；首帧 Above/Left 必错 | `app/frame.rs:59` vs `:128`；`app/overlay.rs:40,86-90`；`interaction.rs:631-655`；测试靠多跑 2–3 帧掩盖（`interaction_tests.rs:1733-1734,1794-1796`） |
| F2 | P0 | z 序 = root children 顺序；无浮层根层/显式排序键；`bring_to_front` = `append_child` + 整窗重绘 | `interaction/overlay.rs:77-83`；`app/overlay.rs:132-159`；`node/tree/arena.rs:318-349` |
| F3 | P0 | outside-click/Esc 只认最上层一个并直接删节点；无 owner 链、逐层策略、hover 桥接、级联关闭；点父项被判“子菜单外部点击”只关子层且吞点击 | `interaction/overlay.rs:56-74`；`app/input.rs:227-238`；`interaction_tests.rs:2077-2124` |
| F4 | P0 | 无重绘契约：根级浮层开关靠 root damage 整窗重绘兜底；非 root 卸载丢绘制外溢；保留实例生命周期存在缺口 | `node/tree/dirty.rs:27-38,114-146`；`arena.rs:375-388`；`paint/instance.rs` 见 §2.3 M-D |
| F5 | P0 | 嵌套定位无拓扑：子浮层锚点用父的上一帧 bounds，逐层滞后；`overlay_containers` 创建序 ≠ 栈序 | `app/overlay.rs:76-78`；`interaction.rs:638-642`；`node/tree/traverse.rs:93-97` |
| F6 | P1 | 焦点双真值：打开模态不迁移焦点（懒修复），paint 用 raw focus、输入/a11y/cursor 用 effective focus；非模态 popup 不取焦点 | `app/overlay.rs:46-64`；`app/focus.rs:11-20,41-70`；`window/paint.rs:155`；`paint/pass.rs:673`；`text_edit.rs:3189-3242` |
| F7 | P1 | opener 启发式且不可指定/更新；opener/锚点死亡无级联关闭，锚点失效浮层跳到 (0,0) | `app/overlay.rs:19-28`；`interaction/overlay.rs:44-49`；`interaction.rs:639-642` |
| F8 | P1 | 无 viewport clamp/flip/margin/size 约束（kit 自行实现两份 placement）；引擎 Anchor 无校验 | `interaction.rs:330-346,631-655`；`app.rs:297-303` |
| F9 | P2 | a11y：wrapper 默认 GenericContainer，无 expanded/haspopup/owns/describedby 自动 wiring；modal 唯一真值 = trap_focus；scope 变化全树 O(n) | `a11y/mapping.rs:219-221`；`a11y/tree.rs:221-227,946-962` |
| F10 | P2 | placement 无 NaN/Inf 校验；`set_absolute_inset` 直写 taffy style；无运行时 `set_max_size` API | `interaction.rs:631-655`；`node/tree/layout.rs:101-115`；评审 02 Major 20 |
| F11 | P3 | 每帧全量 resolve（含空闲帧）、topmost 扫描、Tab 全树 DFS、hover 不重新命中 | `app/frame.rs:59`；`interaction/overlay.rs:77-93` |
| F12 | P3 | 关闭浮层不 cancel IME composition；`restore_focus` 无 fallback | `app/window_set.rs:126,135`；`app/focus.rs:113-125` |

概念上成立、重构保留：树序统一导出 paint/hit/a11y 顺序（`paint/pass.rs:469-486`、`layout/hit_test.rs:392`、`a11y/tree.rs:865-913`）；opener 链 + 模态 scope 算法（`interaction/overlay.rs:32-53,86-93`）；节点删除清理（`app/window_set.rs:9-143`）；单层 outside-click 的确定性；focus trap 的输入过滤。

### 2.5 kit 组件层缺陷清单（摘要）

完整证据见 `evidence/kit-overlay-audit.md` §9（18 条）。

| # | 严重度 | 缺陷 | 关键证据 |
|---|---|---|---|
| K1 | P0 | 固定 Band 序与“最新在上”冲突：Dialog/Alert（10/20）被后开的 Popup/ContextMenu/Tooltip 压住，scrim 也在下面 | `layer.rs:11-30,99-108,152` |
| K2 | P0 | 所有 placement 基于上一帧几何；`frame_at_open/revealed` 等一帧才可见；fade 在不可见时开播 | `overlay.rs:728-740,781-804,810-865,842-853` |
| K3 | P0 | ColorPicker 先 `open` 空内容再 `build_panel`：首帧按 0 尺寸定位、必翻必跳 | `color_picker.rs:1313-1342` |
| K4 | P0 | Canvas 绝对坐标 + pre-layout sync + 面板无裁剪（§2.3 M-B） | `color_picker.rs:2077-2119,1691-1710`；`overlay.rs:959-1032` |
| K5 | P0 | 外部点击/Esc 由引擎 `remove_node` 直接卸载：绕过 fade/on_close；只关最上层 ⇒ 子菜单链只关叶子、父菜单残留且吞点击 | `app/input.rs:227-238`；`app/overlay.rs:175-184` |
| K6 | P1 | 关闭中 `is_open` 仍 true，toggle 吞点击（150ms 内点开关“没反应”）；鼠标路径实际由引擎强关 | `overlay.rs:890-909`；`color_picker.rs:1093-1096`；`dropdown.rs:612-616`；`select.rs:893-901` |
| K7 | P1 | `LayerSlot::Exclusive` 只对 Alert/ContextMenu 生效；dropdown/select/menu/取色器可同时打开 | `layer.rs:104`；`dropdown.rs:193-207`；`select.rs:242-256` |
| K8 | P1 | 子菜单缺 safe triangle/指针桥、链式外部关闭、a11y expanded/haspopup/父子关系；hover 打开不夺焦；2–3 级零测试 | `menu.rs:1710-1751,1618-1678,1530-1535` |
| K9 | P1 | 坐标系混用：`Anchor::Fixed/point` 是窗口坐标，ContextMenu 接收内容坐标 point，滚动后整体偏移 | `overlay.rs:359-403`；`context_menu.rs:160-187`；`gallery.rs:1523-1534` |
| K10 | P1 | 内容增长（`set_items`/AtLeast 宽度）导致 2 帧宽度收敛 + 1 帧落点滞后；贴底菜单一帧溢出 | `menu.rs:338-364,422-427,526-581` |
| K11 | P1 | 无测量回调/布局后钩子；`Layer::enforce` 只在 kit `open/sync` 里执行，宿主漏 sync 就丢 z 序保证 | `overlay.rs:806,885`；`layer.rs:191-217` |
| K12 | P1 | Canvas/面板默认不裁剪，裁剪靠各组件自己记得加（只有 menu surface 加了 `overflow_hidden`） | `menu.rs:410` vs `overlay.rs:959-1032` |
| K13 | P2 | Tooltip band 最高且可命中：挡住其它浮层的 outside-click 并可能拦截点击 | `tooltip.rs:140-160`；`interaction/overlay.rs:56-74` |
| K14 | P2 | 锚点滚出视口/被隐藏时浮层仍钉在屏幕边缘；只有节点删除才关闭 | `overlay.rs:820-833,245-256` |
| K15 | P2 | Dialog 自建 closing/reason 与 Overlay 双份状态；`open()` 被当 `set_anchor` 用于 recenter；ContextMenu 重复 close | `dialog.rs:148-180,690-756`；`context_menu.rs:219-232` |
| K16 | P3 | `Band::order()` 与派生 `Ord` 双份顺序；`MenuMode::ListBox` 仅测试使用（死 API） | `layer.rs:19-30,152`；`menu.rs:989-1009` |
| K17 | P1（流程） | 测试全部 settle 后断言，单帧错位/残影不可能被发现；取色器放置用例存在 `let _ = visual;` 空断言 | `overlay.rs:1534-1540`；`color_picker.rs:2964-2969,3315-3327` |
| K18 | P1 | “double trigger pointer-down”不是 kit 重复注册；是“第一次关闭 + 第二次落在 150ms fade 窗口被吞”的观感（真正原因 K6） | `color_picker.rs:934,1029-1033`；`app/input.rs:492-546` |

### 2.6 子菜单成熟度判定

- `MenuSubmenu` 存在，1 级可用：右侧定位 + flip、150ms hover 延迟、Esc 先关子菜单、Right/Left 有实现；同 band 内 newest-on-top 由 `sequence`+`enforce` 保证（`menu.rs:43-44,938-987,1618-1678,1710-1751`；`context_menu.rs:751-825`）。
- **仅 1 级原型**：无 safe triangle/指针桥、无 hover-out 关闭、外部点击不关链（只关叶子并吞点击）、a11y 父子关系缺失、hover 打开不进入焦点、Left/兄弟切换/2–3 级无测试、长菜单滚动直接关子菜单；每一层都重复“0 尺寸初始 origin → 首帧隐藏 → 第二帧 flip”的抖动。
- 判定：**不能满足 2–3 级级联菜单**，必须在引擎 popup 栈之上重写菜单链（§4.8）。

### 2.7 为什么现有测试没抓到

1. kit 单测一律“先 drain（帧、布局）再 sync”，真实窗口是“hook(sync) → frame(layout)”（`pump.rs:646-654`），时序相反 ⇒ 单测用最新几何、真机用上一帧几何（kit 审计 §1.4、§7.2；gallery 文档 §10.2 已记录同类问题）。
2. 所有断言都在 settle 后，没有“首个可见帧”的断言；`overlay-check` 只检查子节点数量（评审 02 Minor 19）。
3. 缺持久增量通路的像素回归：现成离屏 `render_offscreen` 固定 `FramePlan::Full`（`test_support/pixels.rs:73-76`），无法复现残影（C9）。
4. 已有 `damage_full==false` 断言只覆盖部分场景（`app/invalidate_tests.rs:199,287` 等），**没有 popup 路径**。

---

## 3. 外部成熟方案调研结论

完整调研与引用见 `evidence/external-research.md`（Qt 6.8、Flutter、Chromium views、Radix/Floating UI、Windows/macOS/Wayland/HTML top-layer，约 100 条引用）。

### 3.1 四种模型对比（摘要）

| 维度 | Chromium views | Qt 6 | Flutter | Web（popover/Floating UI） |
|---|---|---|---|---|
| 承载 | 子 Widget（in-window tier）+ 原生菜单窗口 | `Qt::Popup` 独立原生窗口 + QPA grab | `Overlay`/`OverlayEntry`/`OverlayPortal`（in-tree） | top-layer / portal（in-DOM） |
| 定位 | BubbleFrameView：锚点 rect → 首选方向 → 只在“严格更好”时翻面 → 平移进可用区 | 可用屏幕几何 → 翻到锚点上方 → clamp；太高则缩小并内部滚动 | `_MenuLayout`：翻到锚点另一侧再 clamp | Floating UI：offset → flip(bestFit) → shift(limitShift) → size → hide |
| 层级 | Widget z 序 + owner 关系 | 窗口栈 + transient parent/popup grab | Overlay 条目 z 序（后插在上） | popover 栈 + top layer |
| 子菜单 | MenuItemView 子窗口，上一级保持打开 | QMenu 级联 + `QMenuSloppyState` | RawMenuAnchor 每层一个 child，父请求关闭 | Radix 5 点 grace polygon（300ms/100ms） |
| 外部点击 | MenuController 统一处理，Esc 逐层 | popup grab + `replayMousePress`（默认 replay） | TapRegion `consumeOutsideTaps` | DismissableLayer |
| 焦点 | 菜单取焦、关闭还原 | QMenu 激活/还原 | MenuAnchor 焦点转移 | focus scopes / inert |
| 重绘 | 子 Widget 独立重绘 | 独立窗口，OS 合成 | Overlay 独立 layer/RepaintBoundary | 浏览器合成 |

### 3.2 可复制机制（本设计采用）

1. **Overlay root + 单调 z 序号**（Flutter）：最新打开恒在最上是一个整数的性质。
2. **先测量再定位**（Qt `sizeHint`、Chromium `GetFrameSizeForClientSize`、Flutter `getConstraintsForChild`）：绝不复用内容变化后的旧尺寸。
3. **纯函数 placement + 候选序列**（Floating UI）：`offset → flip（严格更好才翻）→ shift（limitShift 防脱锚）→ size（视口内缩小 + 内部滚动）→ clamp`。
4. **菜单链 containment + grace area**（Radix 5 点多边形 300ms；Qt `QMenuSloppyState` 斜率判定）：指针斜向移动不塌链，WCAG 1.4.13 Hoverable 的工程实现。
5. **每个父级只有一个打开的子级**（Flutter RawMenuAnchor）：打开新子级先关旧子级及后代；关闭子级先于父级。
6. **链式 dismiss**（Chromium MenuController/Qt hideUpToMenuBar）：外部按下若命中链中某一层，只关该层之上的后代；未命中则关整链。**本设计默认 replay（D14，与 Qt/Chromium 默认一致）；opener 上的按下按 Toggle 消费**。
7. **Esc 逐层关闭**：Qt 惯例 + D5（APG 只要求关闭含焦点的菜单，不强制逐层——评审 03 m6）。
8. **打开即迁移焦点、关闭还原 + fallback**：菜单/对话框取焦；tooltip 永不取焦。
9. **tier 语义**：Auto（菜单/下拉/选择/取色器：同族替换）、Hint（tooltip：不关菜单、可穿透）、Modal（对话框/Alert：阻断并关闭无关链）。
10. **hover 打开仅限“父链已打开或键盘/点击触发”**（APG + Qt）：避免 WCAG 3.2.1 On Focus。

### 3.3 明确不复制

- 原生 popup 窗口（Wayland xdg_popup serial/grab、Windows owned HWND + capture、macOS child window/level）：成本高、破坏单窗口 damage/输入模型（D2）。
- Chromium 箭头镜像复杂度：本产品不需要 arrow bubble。
- Material “Esc 关整链”：与 D5 冲突。
- Navigator/Route 机制用于菜单：modal 仅作为 `tier=Modal` 的栈节点。
- 菜单栏（menubar）本轮不做（D9）。

### 3.4 关键数值（可配置）

| 参数 | 默认值 | 来源/说明 |
|---|---|---|
| 子菜单 hover 打开延迟 | 200ms（可配；P2 由 `HoverTimings.open_delay` 承载） | Qt `SH_Menu_SubMenuPopupDelay` ≈ 225ms；Radix 100ms |
| 指针离开链后的关闭宽限 | 300ms（**P2 已落地：`HoverTimings.close_grace`**） | Radix grace 300ms；Qt sloppy timeout 同量级 |
| grace polygon bleed | -5px（按翻面方向镜像符号） | Radix |
| 外部点击关闭策略 | 关整链 + replay 按下+抬起（D14）；**P2：safe triangle/grace 内按下 Ignore 不关链（D-P2-2；Q2-03 后不保留桥接几何）** | 与 Qt/Chromium 默认一致 |
| Esc | 只关最上层（tooltip 优先关自身，不关菜单） | D5 + WCAG 1.4.13 |
| tooltip 显示/关闭延迟 | 500ms / 100ms | ARIA/MDN 1–5s 区间与桌面惯例，可配 |
| 视口 margin | 4px | 现有 kit `margin=4` |
| 翻面规则 | 仅当“严格减少溢出”时翻面；翻面后 shift 限制不脱锚 | Chromium/Floating UI bestFit |
| 动画 | 开 120ms EaseOutCubic（opacity 0→1 + 4px 渲染偏移）；关 90ms；对话框 fade-only | 现有 `animation_tick` 与桌面观感 |
| 淡出/关闭期间行为 | 立即不可命中、不可聚焦；节点动画结束后移除 | 本设计（修复 K6） |

---

## 4. 目标架构

### 4.1 分层与职责

```
┌──────────────────────────── ui_kit（外观与交互策略） ───────────────────────────┐
│ PopupPanel（surface/shadow/clip/内容闭包/测量回写）                               │
│ MenuChain（hover intent / safe triangle / 层级键盘 / 焦点转移）                    │
│ 组件外观与业务：Dropdown / Select / Tooltip / Menu / ContextMenu / Dialog /      │
│                 Alert / ColorPicker                                              │
└─────────────────────────────────┬───────────────────────────────────────────────┘
                                  │ PopupSpec + 内容构建闭包 + dismiss 回调
┌─────────────────────────────────▼───────────────────────────────────────────────┐
│ ui_core::popup（引擎不变量）                                                     │
│ PopupStack（per-window：tier / 单调 z / 父子链 / topmost / 状态机）               │
│ placement（纯函数：candidates/flip/shift/resize/clamp/limitShift）                │
│ settle pass（布局后 measure→place→必要时二次布局，同帧收敛）                       │
│ dismiss/focus/modal/a11y（tier/chain-aware，事件而非直接删节点）                  │
│ damage 契约（painted_extent + retained lane/scroll slot/Canvas 不变量）           │
│ 调试面（debug_stack / FrameRecord 断言）                                          │
└─────────────────────────────────────────────────────────────────────────────────┘
```

职责边界：

- 引擎不知道“菜单/下拉/提示”是什么，只认识 `tier`（Auto/Hint/Modal）、几何、栈、焦点、模态、dismiss 事件与重绘。
- kit 不自行计算 origin、不重排 root 子节点、不直接 `remove_node`；几何只读 `PopupRect`，关闭只走 `close()`。
- 菜单链的 hover/safe triangle/键盘属于 kit `MenuChain`；引擎只提供矩形包含、指针位置、dismiss 事件与关闭事务。

### 4.2 数据模型（`ui_core::popup`）

```rust
pub struct PopupId { window: WindowId, slot: u32, generation: u32 }   // 防 ABA、跨窗口唯一

pub struct PopupStack {                       // 每个 Window 一个，窗口创建时预建
    window: WindowId,
    root: NodeId,                             // 专用 overlay 根（0×0、绝对定位、不可命中、不可见 a11y 容器）
    next_z: u64,
    entries: Vec<PopupEntry>,                 // 稳定索引；关闭留墓碑，空闲槽复用（generation 自增）
    order: Vec<PopupId>,                      // 按 z 升序（绘制/命中/a11y 同一顺序）
    dirty: PopupDirty,                        // 脏标记，settle 只在脏时运行
}

pub struct PopupEntry {
    pub id: PopupId,
    pub node: NodeId,                         // 内容根（kit PopupPanel 挂在这里）
    pub owner_modal: Option<PopupId>,         // 从模态内部打开的 popup 的最近模态祖先（a11y/输入 scope）
    pub parent: Option<PopupId>,              // 菜单链父级（每层最多一个打开的子级）
    pub child: Option<PopupId>,
    pub tier: PopupTier,                      // Auto | Hint | Modal | Scrim
    pub scrim_for: Option<PopupId>,           // 仅 Scrim：对应的 Modal（不占用 parent/child 槽）
    pub state: PopupState,                    // Opening | Open | DismissPending | Closing | Closed
    pub anchor: Anchor,
    pub placement: PlacementSpec,
    pub dismiss: DismissPolicy,
    pub focus: FocusPolicy,                   // 模态唯一真值：Trap 即 modal
    pub opener: Option<NodeId>,               // trigger/父菜单项节点；Toggle 事件源
    pub pointer_transparent: bool,
    pub animation: PopupAnimation,
    pub a11y: PopupA11y,
    pub z: u64,
    pub hit_rect: LogicalRect,                // panel 的布局 border box（命中/a11y）
    pub paint_rect: LogicalRect,              // 由 painted_extent 派生的绘制外延（damage）
    pub paint_offset: LogicalPoint,           // 动画渲染偏移（不参与 layout；hit 跟随此偏移）
    pub current_origin: LogicalPoint,         // 本帧 layout 后的实际 origin（settle 前）
    pub solved_origin: LogicalPoint,          // settle 求解出的目标 origin（子 popup 锚点增量修正用）
}

pub struct PopupRect {
    pub hit: LogicalRect,                     // panel 布局 border box（命中/a11y）
    pub paint: LogicalRect,                   // 由 painted_extent 派生的绘制外延（damage）
    pub a11y: LogicalRect,                    // a11y bounds（布局盒，不含阴影）
}
```

关键类型：

```rust
pub enum Anchor {
    Node { node: NodeId },                                      // P1 最终：edge/align 已删（边由 PlacementSpec.side 决定，§12.3-23）
    WindowRect(LogicalRect),                                    // 显式窗口坐标（右键菜单 = 指针点；实现以 0-size point rect 打开，文档的 1×1 为 R2-e P4 口径）
    NodeRect { node: NodeId, rect: LogicalRect },               // 节点内容坐标 rect（引擎减祖先滚动）
}

pub struct PlacementSpec {
    pub side: Side,                      // Top|Bottom|Left|Right
    pub align: Alignment,                // Start|Center|End
    pub gap: LogicalPx,
    pub fallbacks: SmallVec<[Placement; 4]>,  // Auto/Dropdown: opposite + 垂直方向；Submenu 仅 Right→Left
    pub viewport_margin: LogicalPx,
    pub shift: ShiftPolicy,              // None | Limit
    // P1 最终：match_anchor_width 已删除（kit 用 PanelWidth::AtLeast，§4.2 落地口径 8）
}

pub enum PopupTier { Auto, Hint, Modal, Scrim }

pub enum DismissPolicy {
    Chain {
        outside: OutsidePress,      // Replay (默认，D14) | Consume
        escape: EscapePolicy,       // CloseTopmost (默认) | Ignore
        anchor_lost: bool,          // true = 关整链
        focus_lost: bool,
    },
    Explicit { outside: bool, escape: bool },   // 模态对话框默认 outside=false, escape=true
    Hint { escape: bool },                      // tooltip：Esc 只关自身
}

pub enum OutsidePress { Consume, Replay }

pub enum FocusPolicy {
    None,                                         // tooltip
    Take { restore: RestoreFocus },               // 菜单/下拉/选择/取色器：打开取焦、关闭还原
    Trap { restore: RestoreFocus, initial: InitialFocus }, // 模态：取焦 + 阻断 + Tab 循环
}

pub enum RestoreFocus { Opener, Previous }

pub struct PopupSpec {
    pub parent: Option<PopupId>,       // 菜单链/模态后代；None = 根 popup
    pub tier: PopupTier,
    pub anchor: Anchor,
    pub placement: PlacementSpec,
    pub dismiss: DismissPolicy,
    pub focus: FocusPolicy,
    pub opener: Option<NodeId>,
    pub pointer_transparent: bool,
    pub animation: PopupAnimation,     // None | Fade | FadeSlide | FadeScale（P3）
    pub a11y: PopupA11y,
    pub scrim: Option<ScrimSpec>,      // 仅 Modal：Some 时引擎合成 PopupTier::Scrim 栈项
}

pub struct ScrimSpec { pub density: f32, pub color: Color }   // P1 最终：color.with_opacity(density)
```

`PopupA11y`：`role`、`label`、`expanded_of: Option<NodeId>`、`owns: SmallVec<[NodeId;2]>`、`has_popup: Option<HasPopup>`、`described_by: Option<NodeId>`、`a11y_parent: Option<NodeId>`（a11y 树嵌套父；None 则挂 trigger/root）、**`active_descendant: Option<NodeId>`（P2 新增，§13.3：投影到 popup content 根；目标须在 content 子树或 `owns` 内，非法值清空 + 诊断）**。`modal` 不再单独存字段，唯一真值 = `matches!(focus, FocusPolicy::Trap{..})`（评审 03 m1）。dismiss 回调由 `PopupStack` 的注册表持有（不放进 `PopupEntry`）。Scrim 由引擎在 modal open 时合成：分配连续的两个 z（scrim = next_z，modal = next_z+1），`scrim_for = Some(modal)`，不占 parent/child 槽（评审 01 R2 / 评审 03 次条）。

**P1 落地口径（rev.7 / rev.8 终审修复，与上面代码块的差异以此为准，见 §12.3/§12.4/§12.10）**

1. **`Anchor::Node` 为 `{ node }`**（`align` 只来自 `PlacementSpec.align`；`AnchorEdge` 与 `edge` 字段在 P1 终审修复中**删除**——E-09/Q1-02 死字段关闭，边由 `PlacementSpec.side` 决定），且解析为**节点完整 border box**（`NodeTree::node_screen_rect`，含祖先滚动与 popup `paint_offset`）：Bottom 以 `rect.bottom` 为基准、Top 以 `rect.top`、Left/Right 以对应边，gap 从对应边起算；翻面到对侧后面板仍在节点矩形之外（`a_node_anchor_flip_never_overlaps_the_anchor_border_box`）。
2. **`PopupEntry` 最终字段**：`hit_rect/paint_rect/a11y_rect`、`paint_offset`、`constrained_size: Option<LogicalSize>`（settle 回传的已应用尺寸）；设计原稿的 `settled_size`/`solved_origin` 未落地（按死字段规则删除，A-30/B-08）。
3. **`PopupDirty { geometry, order }`**；`PopupState::{Opening,Open,DismissPending,Closing,Closed}` 保留，但 `Opening` 在首帧 settle 后即置 `Open`（B-12/D-P1-18），kit 以 `is_dismissable()` 判定“打开”。
4. **`ZPolicy`/`PopupSpec.z` 已删除（P3 最终）**：z 由 `PopupStack` 的 order/`raise_popup` 维护，语义固定为 Newest；不再暴露 ZPolicy/Above。
5. **Scrim 由引擎绘制（P1 终审修复 E-02）**：`ScrimSpec { density: f32, color: Color }`（默认 `density=0`/`BLACK`，即“无遮罩”）；`scrim_style(viewport)` 做全视口绝对尺寸，`scrim_paint(spec)` 用 **`spec.color.with_opacity(spec.density)`** 绘制（不再硬编码黑色）；kit Dialog 传主题 `ComponentColors.backdrop` 色（本身含 `scrim_alpha`：Light 0.42 / Dark 0.55）与 `density=1.0`（见 §5.2/§12.10-E-02），使 `ComponentColors.backdrop`/`Role::Scrim` 不再是死字段。`PopupTier::Scrim` 不可由调用方传入（`open_popup` 返回 `PopupError::InvalidScrim`）。
6. **`PopupSpec` 最终字段**：`{parent,tier,anchor,placement,dismiss,focus,opener,pointer_transparent,animation,a11y,scrim}`（P3 删除 `z`）；`PopupSpec::new(anchor, placement)` 默认 `tier=Auto`、`DismissPolicy::default() = Chain{outside: Replay, escape: CloseTopmost, anchor_lost: true, focus_lost: true}`、`focus=Take{restore: Opener}`、`animation=Fade`、无 scrim。**`PopupSpec` 不再有 `initial_focus` 字段（E-21：`open_popup` 的 content 闭包在 open 内才创建节点，公开字段不可达，已删除）**；Trap 初始焦点统一用打开后的 `App::set_popup_initial_focus(id, node)`（K1-05），`InitialFocus::{DefaultButton,FirstFocusable,Container}` 仅作 fallback。
7. **`MaxSize` 只剩 `node::tree::MaxSize`（数值 `{width,height}`）**：`popup::MaxSize` 策略枚举与 `PlacementSpec.max_size` 已在 P3 删除；运行时尺寸约束统一走 `NodeTree::set_max_size`（`Window::set_max_size` 同步失效 popup 缓存，P1-4）。
8. **`PlacementSpec.match_anchor_width` 删除（P1 终审修复 E-09）**：引擎字段无生产消费者；宽度匹配由 kit `PanelWidth::AtLeast{min}` + `PanelMeasure` 测量回写承担（§4.4/§5.2），`rg` 零引用。

### 4.3 帧管线与 settle loop

**插入点（修正评审 02 Major 7 / rev.2）**：必须在 canvas-text eviction 循环（`app/frame.rs:167-192`）之后、`ensure_visible`/`sync_blink`/`sync_ime_cursor_area`（`:196-198`）之前。settle 若改变布局，按真实帧序重跑：`layout_if_dirty` → `layout_text` → `rewindow_virtuals` → `clamp_scroll_offsets` → `resolve_canvas_text`（真实顺序为 `rewindow_virtuals` 在 `clamp_scroll_offsets` 之前，`app/frame.rs:151-152`）；并重新执行 `ensure_visible/blink/IME`。`Window::needs_frame` 必须纳入 `PopupDirty`；所有 `request_measure/open/close/set_anchor` 显式 `request_frame`。

```
frame(window):
  ... timers / styles / text / layout / clamp / resolve_canvas_text / eviction loop
  if stack.dirty (anchor/内容/viewport/order/动画脏):
      settle(最多 2 轮，含上面列出的重跑步骤)
  ensure_visible / sync_blink / sync_ime_cursor_area
  upload_atlas_pages
  paint_frame
```

`settle` 规则（避免中间位置触发 50% damage 升级）：

1. **第 1 轮只应用尺寸约束**：`apply_max_size`（新增运行时 `set_max_size` API）→ 需要时 `layout_if_dirty + layout_text + rewindow_virtuals + clamp_scroll_offsets + resolve_canvas_text`。
2. **第 2 轮计算最终 origin 并一次性 `apply_inset`**：使用最终测量尺寸跑 `solve_placement`；每个 popup 只写一次 inset（不做“先错后对”的中间位置），再 `layout_if_dirty` 一次。总布局轮数 ≤ 2。
   - **子 popup 锚点必须用父 popup 的“已求解 origin 增量”修正**：按深度从浅到深处理；`child_anchor = 当前 anchor_rect + (parent.solved_origin − parent.current_origin)`，因为 origin 平移不改变父内容内部行位置。这样无需每层单独 layout，避免 3 级菜单逐层滞后（评审 01 R3）。
   - 若内容尺寸在轮 2 后仍变化（文本/列表），记录 `Unsettled` 并在下一帧再 settle；不允许以“逐层 layout”突破 2 轮上限。
3. **dirty 闸门**：只有 `PopupDirty` 置位才运行 settle；空闲帧 0 popup 工作。`request_measure()`/anchor 变化/视口变化/字体变化/列表重排会置脏。
4. **报告聚合**：把两次 layout 的 `LayoutReport` 合并进本帧 `FrameStats`（评审 02 Major 7：现在只保留第一次 report）。
5. **收敛异常**：第 2 轮后尺寸仍变化 ⇒ 保留结果并记录 `PopupDiagnostic::Unsettled`，下一帧再 settle；不得无限循环、不得切换全量重绘逃生口。
6. **首可见帧**：`state=Open` 在 settle 完成同帧置位并绘制；删除 `frame_at_open/revealed`。
7. **锚点失效**：`resolve_anchor` 返回 `None` 时按 `DismissPolicy.anchor_lost` 关整链或保留并 clamp；不得跳到 (0,0)。
8. **不改变主树布局**：popup root 是独立的 0×0 绝对节点，popup 不影响普通内容布局（0×0 containing-block 的可行性先由 P1-0 原型验证；失败则改为 viewport-size + hit/focus skip-container 的专用 popup layer，且 damage 用子节点自身范围——评审 02 rev.2 Major）。

### 4.4 定位求解器（纯函数）

```rust
pub struct PlacementInput {
    pub anchor: LogicalRect,              // 锚点矩形（父项 action rect / trigger rect / 指针点）
    pub size: LogicalSize,                // 已测量内容尺寸
    pub viewport: LogicalSize,
    pub avoid: Option<LogicalRect>,       // 仅子菜单：父菜单 panel rect；候选与其严格相交视为放不下
    pub spec: PlacementSpec,
}

pub fn solve_placement(input: PlacementInput)
    -> Result<PlacementResult, PopupError>;   // PlacementResult{ rect, side, resized: bool }

// 1) 候选序列 = 首选 + fallbacks（去重）；有 avoid 时 Right/Left 候选横向基准取 avoid 外缘（Qt 子菜单规则），纵向仍按 anchor 对齐
// 2) 候选与 avoid 严格相交 ⇒ 视为放不下；第一个完全放得下的候选胜出；否则取 overflow 最小（bestFit；相等时优先不覆盖 avoid）
// 3) 跨轴 shift 进视口，limitShift 保证不脱锚
// 4) 仍放不下：max-size = 视口 - 2*margin（返回 resized=true，调用方设 max 并让内容滚动）
// 5) clamp origin 到 margin
// 6) 非法几何（含 avoid NaN/负尺寸）返回 PopupError::InvalidGeometry，禁止静默归零
```

规则：

- 翻面仅在“严格减少溢出”时发生；翻面后仍 shift；shift 有 limitShift；尺寸不够时缩小 + 内部滚动。
- **子菜单候选固定 `Right-Start → Left-Start`**，禁止 Top/Bottom（评审 03 M6：Qt/Flutter 均不跨锚点翻）；仍放不下则 shift/clamp + 内部滚动；子菜单与父项 action rect 相交时翻到父菜单左侧（Qt 规则）。
- **宽度匹配（原 `match_anchor_width`）**：Select/Dropdown 宽度 = max(anchor 宽度, 内容最小宽度)，不超过视口可用宽度。P1 最终由 kit `PanelWidth::AtLeast` 实现，引擎字段已删除（见下）。
- 纯函数、可单测、无副作用；输入输出全部强类型。

**P1 实现口径（rev.7）**

- **`avoid` 的语义**：= 父 popup 在 settle 中**已求解**的 rect（按深度浅→深传递，`settle.rs`）；`Right/Left` 候选的横向基准取 `avoid` 外缘（不是 anchor 边），纵向仍按 anchor 对齐；候选与 `avoid` 严格相交视为放不下（`fitted=false`）。
- **子菜单方向强制在引擎**：`open_popup` 对带 `parent` 的 spec 调 `effective_placement`：`side` 非 Left/Right 时改写为 `Right`，`fallbacks` 只保留 Left/Right（为空则补 `Left-Start`）。kit 侧 `PlacementSpec::submenu()` 亦默认 `Right-Start → Left-Start`。
- **`Anchor::Node` 用完整 border box**（§4.2 P1 落地口径 1）：`Bottom` 基准 = rect.bottom + gap、`Top` = rect.top − gap − height、`Right/Left` = 对应边 ± gap；`align` 在边跨度内对齐；翻面到对侧后面板仍在节点矩形之外（`a_node_anchor_flip_never_overlaps_the_anchor_border_box`）。
- **`viewport_margin` 是硬约束（A-19 裁决）**：fit 候选最终把 origin 双轴 clamp 到 margin 内（可撤销 limitShift 的部分位移，“margin 优先于不脱锚”）；仅当 popup 被 max-size 收缩后仍大于可用视口时才允许越出。
- **非法/极端输入**：`validate` 拒绝非有限/负尺寸与非法 gap/margin（`InvalidGeometry`/`InvalidPlacement`）；`gap≈f32::MAX` 等极端有限输入在返回前校验 rect 有限，否则 `InvalidGeometry`，不产生 NaN（A-28）。
- **`match_anchor_width` 生产由 kit 落地（A-09 处置，P1 终审修复后引擎字段删除）**：设计语义「宽度 = max(内容宽, anchor 宽)，并夹到视口可用宽」由 kit `PanelWidth::AtLeast{min}` + `PanelMeasure` 测量回写实现（Dropdown/Select/ItemPanel）；引擎 `PlacementSpec.match_anchor_width` 因无生产消费者，已在 P1 终审修复中**删除**（E-09/Q1-02 死字段关闭，`rg` 零引用）。

### 4.5 尺寸与滚动

- 引擎为每个 popup 提供**运行时 max-size**（`MaterializedStyle`/`NodeTree::set_max_size`，新增 API；taffy 0.15 的 oof 布局会对绝对定位盒应用 clamp，已有 `min_max_size_clamp_measured_leaf` 测试可参照 `layout/taffy_tree.rs:489-507`）。
- **max-height 只夹节点盒子，不产生内部滚动**（评审 02 Major 20）：`PopupPanel` 在引擎约束态（`constrained_size`/`hit_rect` 被夹）下把内容放进一个**确定高度**的 viewport（`height = available_body_height`，`overflow_hidden` + 内部 List/ScrollBar），而不是仅靠 max-height 溢出被裁；P1 终审修复 E-04/Q1-01 已由 Menu/Dialog 实际消费。
- 菜单/选择：内容超长时内部滚动，键盘导航 `ensure_visible`；tooltip 优先整体移动不缩小；对话框超出视口时内容区滚动、footer 固定。
- **popup 内容根不裁剪**（P1 终审修复 E-03/D-P1-30）：内容根移除 `overflow_hidden`，由 kit `panel`/`body` 各自裁剪内容，使 `PopupSurface` 阴影可见；Canvas 在引擎契约上兜底（§4.11 I3）。

### 4.6 堆叠与层级

- **popup root**：窗口创建时预建，root 的固定层级节点（0×0、绝对定位；**不得使用 `interactive=false`**——现有 `hit_test::pick` 会把非 hittable 作为 disabled 向全部后代传播（`layout/hit_test.rs:367,409-425`），`is_effectively_interactive` 也沿祖先链返回 false（`node/tree/content.rs:922-937`）。改用新增的 `HitFocusSkip::Container` 语义：容器自身不参与命中/焦点，子节点正常；a11y 构建器同样采用 skip-container（不生成节点、不隐藏子节点）。popup 节点是其子节点、绝对定位。root 子节点顺序固定为 `[...普通内容, popup_root, debug_hud]`；`debug_hud` 是 root 的固定子节点（不是 popup 栈成员），pointer-transparent，永远在 popup 之上（评审 01 M13 / 评审 02 Blocker 3 / 评审 02 rev.2 Blocker）。
- **z 序**：`next_z` 单调；`order` 按 z 升序；popup root 的子节点顺序与 `order` 一致。**重排不得触发 `structure_epoch` 全树重建**：新增批量 `reorder_children` API，只更新子节点顺序并标 damage（评审 02 Blocker 3）。
- **最新在上**：新开/raise 获得最大 z；子菜单打开 z 更大 ⇒ 在父之上；关闭不改变其它 z。
- **tier 规则**：
  - `Auto`（Dropdown/Select/Menu/ContextMenu/ColorPicker）：打开新的 Auto 根时关闭其它 Auto 根链（不同 trigger 之间的 dropdown/menu 切换一键完成）；同一 opener 的 toggle 由 opener 规则处理。
  - `Hint`（Tooltip）：不关任何 popup，pointer-transparent，不参与 dismiss 竞争；不取焦点；**modal 打开时一律隐藏/关闭**（用户打开对话框时旧 tooltip 不得悬在模态之上；modal 关闭后由 hover 重新触发）。
  - `Modal`（Dialog/Alert）：阻断下层输入/焦点/a11y；打开时关闭所有无关 Auto 链（其 `owner_modal` 内后代除外）并隐藏/关闭所有 Hint。
  - `Scrim`（模态遮罩）：**引擎合成的独立栈项**（`PopupSpec.scrim = Some(...)` 时创建），分配与 modal 连续的两个 z（scrim = next_z，modal = next_z+1），`scrim_for = modal`，不占 parent/child 槽；全视口、pointer-consuming、a11y-hidden、不可聚焦；点击 scrim 等价一次 OutsidePress（按 modal 的 `Explicit{outside}` 处理）；damage = 全视口；不是模态内容的子节点，因此不受 `overflow_hidden`/popup 尺寸约束（评审 01 M05/R2 / 评审 03 B1）。**P1 落地**：遮罩由引擎绘制（`scrim_style(viewport)` + `scrim_paint(ScrimSpec{color,density})`，颜色 = `color.with_opacity(density)`，默认黑/0；P1 终审修复 E-02 后 kit Dialog 传主题 `ComponentColors.backdrop` 色（含 `scrim_alpha`：Light 0.42 / Dark 0.55）与 `density=1.0`，遮罩重新随主题），调用方不得直接传 `PopupTier::Scrim`（返回 `InvalidScrim`）；modal/Scrim 打开与关闭走整帧重发（§12.3-17）。Scrim 不参与普通 popup 命中派发：pointer-down 先跳过 Scrim，把它作为“外部点击”路由给最顶层非 Scrim 的 Auto 链/modal 策略，避免挡住 Dialog 内 Select 的外部点击（评审 03 rev.2 Major 2）。**Scrim 生命周期与 modal 严格绑定**：modal 进入 `DismissPending`/`Closing` 时 scrim 同帧进入 `Closing`，modal 移除时 scrim 一并移除（`scrim_for` 指向的 modal 不存在时 scrim 必须被回收，`reap_orphan_scrims`），并有对应测试。
  - HUD 不是 popup 栈成员：`debug_hud` 是 root 的固定子节点，排在 `popup_root` 之后、pointer-transparent，永远在最上。
- **模态 scope**：`owner_modal` 记录“从模态内部打开的 popup”的最近模态祖先；这些 popup 属于该 modal 的 a11y/输入 scope（Select inside Dialog、ColorPicker inside Dialog 可用）；scope 外一律阻断（评审 01 M04 / 评审 03 M10）。**modal 关闭事务必须递归关闭其 `owner_modal` 后代**，避免子 popup 悬在已关闭对话框之上（评审 03 rev.2 Major 3）。
- **性能**：z 变化只 damage 受影响 popup 的 `paint_rect`；打开/关闭不 `append_child(root)`、不整窗重绘。

### 4.7 关闭与驳回

**事件模型**：引擎永不直接删除 popup 节点。所有驳回产生事件：

```rust
pub struct PopupDismiss { pub id: PopupId, pub reason: DismissReason, pub pointer: Option<LogicalPoint> }
pub enum DismissReason { OutsidePress, Escape, Toggle, AnchorLost, FocusLost, ParentClosed, A11yDismiss, Programmatic }
pub enum DismissOutcome { Close, CloseChain, Ignore }
```

`open()` 时注册 `on_dismiss: Fn(&mut App, PopupDismiss) -> DismissOutcome`；未注册默认 `Close`。kit `PopupPanel` 统一处理：置 `DismissPending`（立刻不可命中/不可聚焦/吞掉落在 `paint_rect` 内的按下）→ 启动关闭动画 → 动画结束 `stack.close(id)` → 子级先于父级 → 焦点事务 → `on_close(reason)`。

**完整 dismiss 决策矩阵**

| 顶层类型 | 外部按下 | Esc | 锚点消失/滚出视口 | 窗口失焦/最小化 |
|---|---|---|---|---|
| Auto 根链（菜单/下拉/选择/取色器） | 关闭整链并按 D14 replay 按下+抬起；若按下命中 opener → `Toggle`（关闭并消费；若在 Closing 中则同 id 取消关闭并回 Opening） | 关最上层（`EscapePolicy`） | 关整链 | 关整链 |
| Hint（tooltip） | 不处理（穿透） | 只关 tooltip 自身 | 隐藏 tooltip | 隐藏 tooltip；modal 打开时也隐藏 |
| Modal（Dialog/Alert） | 按 `Explicit{outside}`：默认忽略且消费；`outside=true` 时按 Chain 处理 | 按 `Explicit{escape}` | 忽略（保持） | 保持，窗口销毁时统一清理 |

- **指针命中规则**：pointer-down 按下面的固定顺序解析，**modal scope 先于一切 opener/hit 规则**（评审 03 rev.3 Major）：
  0. **scope 判定**：取最上层 `FocusPolicy::Trap` 的 modal `M`。命中 `M` 子树或 `M` 的 `owner_modal` 后代 → 进入“scope 内解析”；否则 → 消费该次 press+release（Scrim 命中同样走此路径），再按 `M.dismiss`（`Explicit{outside}` 默认忽略；`outside=true` 时关整链）处理，**不得**触发任何背景 opener 的 Toggle（违反 D6）。
  1. **scope 内 / 无 modal 时**，按 z 从高到低遍历（跳过 Scrim；Scrim 在更高层无 popup 时按 OutsidePress 处理）：
     1.1 **Auto 链的 outside 优先于 modal 面板**：若存在属于当前 scope 的 Auto 根链（含 `owner_modal = M` 的 Select/ColorPicker；多条时取 **scope 内 z 最高的 Auto 根链**），且命中既不在**任一** Auto 链的 `hit_rect`、也不在**任一** Auto 链的 `opener` → 先把 `OutsidePress` 交给该链（D14 replay），结束本次解析。这样 Dialog 内的 Select 可以正常外部关闭，同时“点另一个 opener 切换”不会被此步拦截（评审 03 rev.4 Minor ①②）。
     1.2 **opener 优先**：命中某个 Auto 根链的 `opener` 节点——同一 opener 的链已打开 → `Toggle`（关闭并消费 press+release）；不同 opener 的链已打开 → 先关闭旧 Auto 根链（保留新链祖先链），再**不消费**，把 press/up 正常派发给该 opener（一次点击完成切换，评审 01 R1）。
     1.3 命中某个 popup 的 `hit_rect`：若该 popup 不是其链的最深层 → 先关闭它之上的后代，再把点击派发给该层（评审 03 rev.2 Major 4）；若是最深层 → 正常派发。
     1.4 都不命中 → 交给最顶层 Auto 根的 `on_dismiss(OutsidePress)`；kit 可用自己的 safe-triangle/链几何返回 `Ignore`（引擎不存多边形，评审 01 M03/评审 02 Major 16）。默认 `OutsidePress::Replay`（D14）：关闭整链后把该次 press+release 重新派发给下层命中控件；`Consume`/`Ignore` 为显式 opt-in。
- **Esc**：跳过 `Closing`/`DismissPending` 层；优先关闭 Hint（tooltip），否则关闭最上层 Auto/Modal 的当前层；返回 `consumed: bool` 给宿主（宿主不得在 consumed 时关窗口；修复 gallery G8 类问题）。
- **关闭顺序**：标记 `DismissPending`（立刻不可命中）→ 子级先关 → 父级；打开新 popup 时先关旧同 tier 链再打开新链（一个事件循环内）。
- **组件级 anchor/scroll/resize 矩阵**（区别于“锚点消失”）：菜单/右键菜单在**外部滚动（含祖先与无关滚动容器；`window_scroll_epoch` 对任意非 popup 子树滚动 +1，见 P4 R2 外部滚动语义）**或窗口 resize 时 **关闭整链**；Dropdown/Select/ColorPicker 在祖先滚动/resize 且 anchor 仍可见时 **同帧重新 settle**，仅 anchor 节点消失/不可见/滚出视口时关闭；Tooltip 重定位，anchor 消失时隐藏（评审 01 R4 / 评审 03 M7）。
- **窗口生命周期**：窗口隐藏/最小化关闭 Auto/Hint（保留 Modal）；窗口销毁清理整栈与动画；多窗口各自独立栈、不允许跨窗口 popup。

### 4.8 菜单链状态机（kit `MenuChain`）

```text
每层状态：Closed | PendingOpen(timer, level_id) | Open | PendingClose(timer, level_id)

on pointer_enter(item, level):
  if grace_intent.active(item)                          // 指针在 safe area 内朝子菜单移动
      -> ignore sibling replacement; keep current child/highlight
  if item.has_submenu && !item.disabled && chain_eligible(level):
      cancel pending close of this level; start PendingOpen(delay, level_id)
  else if item is normal/disabled/group/separator:
      close descendants of level; cancel timers

on pointer_leave(chain):
  start PendingClose(grace, level_id_of(deepest_hover_opened_level))

on PendingOpen fire (level still current):
  close_siblings(level); remove_all_descendants(level); open child(level)
  z = engine.next_z(); if opened_by_keyboard: focus first item

on PendingClose fire:
  if !chain_contains(pointer): close_subtree(level_id)   // 绑定具体层，不用全局 topmost
  // 若指针重新进入链任意 rect/连接多边形：取消

on Esc: engine.dismiss(topmost, Escape)                 // 引擎统一；kit 只收回调
on outside press: 引擎判定；kit 在 on_dismiss 中用 chain geometry 返回 Ignore/Close/CloseChain
on anchor_lost / ancestor scroll / window resize: 按组件矩阵（菜单/右键菜单 → close_chain；dropdown/select/colorpicker → 重新 settle；tooltip → 重定位/隐藏）
on parent close: 子级先关闭（close_subtree）
```

实现要求（吸收评审 03）：

- **safe triangle / 指针桥**：5 点多边形（退出点内缩 5px + 子菜单近端上下角），按翻面方向镜像；**每次子菜单 open/flip/scroll 后重算**，300ms 过期；`grace_intent` 期间禁止兄弟替换（B2）。
- **兄弟切换**：hover 到另一个 submenu 行时，若指针不在 grace 区 → 立即 close 旧子级 + 起新 PendingOpen；hover 普通行 → 关子级并取消 timer。
- **PendingClose 绑定 level id**：离开整条链时关闭所有 hover-opened 层、保留 click-opened root（B3）；不得使用全局 `close_topmost` 处理 pointer 路径。
- **键盘表（唯一权威，P2-4/P2-6 引用）**：
  - **键盘归属**：`keyboard_target = 当前 DOM focus 所在层`；hover 只更新该层 `active_item`，不移动 DOM focus；**第一次键盘事件若发生在 hover-opened 的更深层**，先把 DOM focus 移入该层（同样只移一次），`active_descendant` 只引用本层 item（评审 03 rev.2 M1）。
  - Up/Down：跳过 Group/Separator/disabled；越界滚动 + ensure_visible；每层维护唯一 `active_item`。
  - Home/End、PageUp/PageDown。
  - Right/Enter：打开子菜单并聚焦首项（点击父项同义：保持打开、不 toggle）；disabled 子菜单父项不得 hover/open。
  - Left：关闭当前子菜单并回到父项高亮。
  - Esc：引擎逐层关闭；焦点逐层归还父项/opener。
  - Tab/Shift+Tab：关闭整链，焦点移到 opener 之后/之前的 tab stop（APG，不 trap）。
  - typeahead：2s buffer，按层独立，换层清空；跳过 disabled/separator；IME composing 时不处理字符。
  - Alt/F10/Shift+F10：打开/聚焦菜单或 ContextMenu（P2-6）。
- **checkable 项**：Space 切换不关菜单；Enter 选择并关链；radio 组内互斥；`checked` 状态更新；分隔线用 `A11yRole::Separator` + `hidden`（accesskit 映射 `Splitter`，满足 §4.10；P3 修正见 D-P2-10）。
- **长菜单**：内部滚动；父项滚出视口 → **整链关闭**（P2 实现，M2-05 统一；设计原稿允许“该子级或整链”）；子菜单 flip 只 Right→Left。
- **hover 打开不夺焦**（D13）：只更新 active_item；键盘路径才把焦点移入；`keyboard_target` 按上面的“键盘归属”定义（DOM focus 所在层，首次键盘事件时移入最深打开层）；`active_descendant` 只引用本层 item。
- **2–3 级**：递归 MenuChain；每层独立 PopupPanel；测试覆盖 3 级打开/关闭/兄弟替换/外部/Esc/滚动/翻面。

**P1 状态（P2 已落地，见下）**：`MenuChain` 未落地（P2-1）；P1 的 `Menu`/`ContextMenu` 只有 **1 级子菜单 parity**（`PopupSpec.parent` 链 + 200ms hover timer + Esc 只关一层 + outside 关整链 + 父关闭级联，D-P1-24），safe triangle/grace polygon、兄弟替换、typeahead、checkable 键盘表与 2–3 级测试属 P2。

**P2 落地口径（rev.9，2026-11-27；实现与设计正文的差异见 §13.4）**

- **实现**：`menu_chain.rs` 的 `MenuChain`（`pub(crate)`）+ 每层一个 `PopupPanel`（`PopupSpec.parent` 串联）；状态 `LevelState{Closed,Opening,Open,Closing}`（=设计 `PendingOpen/PendingClose`），timer 存放在 `Menu` 的 `pending_timer/grace_timer/close_timer` 并绑定具体 `LevelId`；`close_subtree` 子先父；打开新子级先关旧子级/后代（兄弟替换），祖先层按下只关后代（§13.2）。
- **safe triangle / grace**：5 点多边形（退出点 ±5px bleed 按翻面镜像 + 子菜单近/远端上下角），子菜单 open/flip/scroll 后重算；300ms `close_grace` **自 open/几何变化起算并过期清多边形**；grace 期间禁止兄弟替换；外部按下落在 **grace 三角 ∪ 即时 safe triangle** 内返回 `DismissOutcome::Ignore`（D-P2-2；无桥接几何）。
- **键盘表**：↑↓ / Home / End / PageUp / PageDown / ←→ / Enter / Space / Esc / Tab / Shift+Tab / Alt+F10（+ Shift+F10 / ContextMenu 键用于 ContextMenu）；typeahead 2s 单层 buffer、换层清空、跳过 disabled/separator、text/IME 事件不入 buffer（D-P2-5）；checkable：Space 切 Checkbox 不关菜单、Enter 选择关链、Radio 同层互斥（D-P2-8）。
- **ContextMenu**：`Anchor::WindowRect` 指针锚（`open/open_at`）+ 节点锚（`open_on`，底边左端点）+ `bind_trigger`（**仅键盘：Shift+F10/ContextMenu 键**；裸 F10 未绑定；trigger 需已可聚焦，左键由宿主绑定）；`on_item_click` 回调携带 `Option<MenuCheckState>`（§13.3）；2–3 级共享同一 `MenuChain`；**未内建 secondary-button 自动绑定**，真实右键由宿主调用 `open(point)`（D-P2-4）。
- **长菜单 / anchor**：anchor `Lost/Resized/Moved` 一律**关整链**（含内部 List 滚动导致的父行滚出视口，M2-05 统一；不再声称“内部滚动豁免/只关子级”）；ContextMenu 外部页面滚动同样关整链（D-P2-3）。
- **a11y**：`active_descendant` 落 popup content 根（role=Menu）而非 surface patch；2/3 级 a11y 父=父 menuitem，`expanded`/`has_popup` 投影（§13.3/D-P2-1）。

### 4.9 焦点、模态与 IME

- 引擎 `FocusManager` 单一真值：`effective_focus` 由 PopupStack 计算（最上层 Trap/Take 的焦点 owner，否则主树 focus）；paint、命中、a11y、cursor、IME 全部读同一真值（修复 F6；评审 02 Major 11 的 `text_edit::apply_focus`、caret blink、IME owner 一并更新）。
- 打开 `Take/Trap`：保存 `FocusSnapshot{node, scope}`；`InitialFocus` 规则：对话框默认按钮 → 第一个可聚焦 → 容器；菜单首项；关闭时按快照恢复（仅在“最外层关闭事务”恢复一次；若事务中已有新 popup 取焦则跳过，避免抢焦点——评审 03 M9）；opener 失效 fallback 最近可聚焦祖先 → root。
- 模态：`FocusPolicy::Trap` 为唯一 modal 真值；Tab/Shift+Tab 在 modal scope 内 closed-loop（现有 `next_focusable` 已 wrap，明确并测试）；scope = 顶层 modal 子树 ∪ `owner_modal` 后代 popup；a11y 下层隐藏/inert；scrim 为独立 `PopupTier::Scrim` 栈项（§4.6），pointer-consuming、a11y-hidden、全视口 damage，不参与焦点遍历。
- IME：每次 `effective_focus` 变化执行 cancel/commit；打开 popup 从 editable 抢焦取消 composition；Esc 在 composition 中先取消组合；popup 内 editable（ColorPicker hex）关闭时 commit/cancel 明确；同步 `sync_ime_cursor_area` 与平台 IME enable 状态（评审 03 M11）。

### 4.10 a11y

- **a11y 树按 popup 链嵌套**（评审 03 B5）：根 popup 的 a11y 节点挂到 trigger（或 window root），子菜单挂到父 menuitem，**每个节点唯一父**；物理树仍平铺在 popup root，a11y 构建器按 `a11y_parent` 关系重组；`owns/controls` 只作为适配器兜底，不重复暴露。
- 角色：Menu/MenuItem/MenuItemCheckBox/MenuItemRadio/Separator/Dialog/AlertDialog/Tooltip/Group；`haspopup`（opener 侧：Menu/Listbox/Dialog）、`expanded`（opener/父 menuitem，关闭开始同帧清除）、`described_by`（tooltip → trigger）、`active_descendant` 跟随 active_item（**P2：由 `PopupA11y.active_descendant` 投影到 popup content 根，目标须在 content 子树/owns 内，非法值清空并留诊断**，§13.3/D-P2-1）。
- accesskit 0.25 字段接线：`a11y/adapter.rs`、`a11y/mapping.rs`、`A11yNode` 增加 `set_has_popup/set_controls/set_owns/set_described_by/set_active_descendant`；`A11yPatch.expanded` 已存在（`a11y.rs:244`），补 `has_popup/owns/described_by/a11y_parent`。菜单分隔线用 `Role::Splitter` + 不可聚焦/hidden，并在设计记录该平台限制（评审 03 m2）。
- AT dismiss：新增 `A11yAction::{Dismiss, HideTooltip, ShowTooltip}`；`Action::HideTooltip → DismissReason::A11yDismiss`（只关 tooltip），menu/dialog 用 `CustomAction("dismiss")` 或合成 Esc，映射表写入文档（评审 03 m7）。
- 模态 a11y：`aria-modal` 由 `FocusPolicy::Trap` 派生；scrim `hidden`；下层仅隐藏不销毁；关闭后恢复 `expanded/haspopup` 状态。

### 4.11 绘制与 damage 契约

**对浮层生命周期的保证**

- 每个节点持久记录 `painted_extent`（实现：`paint/extent.rs`——绝对布局空间 L 的 `LogicalRect` + 固定 1 物理 px pad，**不是**设备空间 `DeviceRect`，见 §11.3-4）：写入分两条通道——quad/glyph/image/Canvas/shadow/ring 由 `PrimitiveEmitter::note_painted` 在 emit 时累计，独立 Geometry 节点的 chunk 外延由 `paint_geometry_node` 用 `geometry_chunk_damage` 并入；lane reuse 时保留，`hide_subtree`/空 lane/geometry 清空时先 damage 旧 extent，`remove` 对子树 layout.bounds ∪ painted_extent 取并集（并入父 damage 前按祖先滚动差换算到父节点发射空间）；从未绘制节点回退 `layout.bounds`（评审 02 Major 8）。
- 任何 popup 的 `hit_rect`/`paint_rect`/z/visibility 变化：damage = 旧 `painted_extent` ∪ 新 `painted_extent`；关闭/删除用最后 `painted_extent`。
- 模态打开/关闭与 scrim（`PopupTier::Scrim` 独立栈项）：damage = 全视口；非模态 popup 不触发全窗 damage（I6）。**P1 实现**：modal/Scrim 的 open 与 close 直接走 `force_full_republish()`（整帧重发，`damage_full`），非模态 popup 全程局部 damage（`opening_and_closing_a_non_modal_popup_never_forces_a_full_repaint`、`closing_animation_frames_never_force_a_full_repaint`）。
- z 变化：damage = 受影响 popup 的 `paint_rect` 并；批量 `reorder_children` 不触发 structure epoch。
- **Canvas 坐标契约（结构性修法）**：`Canvas` 内部 quad 坐标 = **节点内容盒坐标**（原点 = border box 左上 + border/padding，即 `content_offset`；当前全仓 Canvas 节点 `content_offset = 0`，实现为 `bounds.origin + content_offset`，见 §11.3-6）；引擎 emit 时统一加节点屏幕偏移（含滚动/烘焙），`painted_extent` 与 damage 用同一偏移。实现为 `CanvasDraw { canvas, offset }` 载荷与 encoder 递归平移（`paint/instance.rs`、`paint/pass.rs`），并迁移全部真实 `Canvas::new` 调用点 **132 处/41 文件**（口径见 §5.3 与 §11.3-7）。
- **Canvas 内容外延（painted_extent/damage）采用实际并集语义**：`Canvas` 节点的外延 = 递归并集其**实际图元**外延（quad/glyph/image/path、shadow 的物理 blur pad、Geometry 经 `draw.transform` 的 AABB、嵌套 canvas 的 `offset`）∪ 节点盒；不使用 `Canvas::bounds()` 作为“内容外延声明”，也不依赖越界 debug 断言（C-10）。成本是 canvas lane 重建时多一次 O(图元数) 矩形并集（lane 复用不执行），见 §11.3-17。
- **painted_extent 的 scroll 复用**：`paint_state` 的 reuse 判据保持 `lane_offset/clip_key/opacity`（滚动 lane 的 `bake = total - slot_offset` 故意不变）；仅对 `screen_baked` 的 Canvas/Geometry 增加“节点屏幕 rect 变化”判断（评审 02 Major 9）。

**渲染管线不变量（引擎测试门禁）**

| 不变量 | 内容 | 落点 |
|---|---|---|
| I1 | 移动/隐藏/删除 damage ⊇ 上一帧 `painted_extent`（含 shadow pad/ring/Canvas/Geometry） | `paint/pass.rs`、`node/tree/*`、`app/removal_tests.rs` |
| I2 | lane 释放/消失/顺序变化必须使 `buffer.ops` 重建或精确移除；`delta.removed` 进入重建信号；paint 内 `scene.release_lane` 必须上报 removed/order 变化 | `paint/instance.rs`、`render/compose.rs`、`render/delta.rs`（先用 E1–E3 判别，不做无证据的大改） |
| I3 | Canvas 内容坐标 = 节点局部坐标；`Canvas::bounds()` 与节点布局/滚动/烘焙一致；禁止绝对窗口坐标画布 | `paint/canvas.rs`、`paint/pass.rs`、kit/widget 全部 canvas 调用点 |
| I4 | 每帧维护显式 node→scroll slot 表；集合/顺序变化时重映射并发送完整 `ScrollTable`（截断 stale slot），无法重映射才 full paint；与 virtual list slot 同步 | `paint/pass.rs`、`render/delta.rs`、`app/virtual_list.rs` |
| I5 | `paint_state` reuse 判据保持 lane_offset/clip_key/opacity；仅 screen_baked Canvas/Geometry 额外比较屏幕 rect | `node/tree/content.rs`、`paint/pass.rs` |
| I6 | popup 移动/关闭/打开回归必须断言 `FrameRecord.damage_full == false`（模态/全视口动画除外），且不得用 resize/DPI 全量逃生口掩盖 | `test_support/record`、`app/overlay_tests.rs`、gallery 自动化 |
| I7 | 栈不变量：z 单调且 order==z；每父级 ≤1 打开子级；所有 rect 有限且满足 viewport margin；modal 之下不得有后代 popup（Hint 在 modal 打开时隐藏/关闭，不参与）；HUD root child 最上且 pointer-transparent | `popup/tests.rs` |
| I8 | 关闭事务：Closing/DismissPending 层不可命中/不可聚焦/不可作为 dismiss 目标；关闭子先于父；焦点只恢复一次 | `popup/tests.rs` |
| I9 | 打开/关闭 popup 不改变主树布局（popup root 0×0 独立） | `app/overlay_tests.rs` |

### 4.12 动画

- 默认 `Fade`（opacity 0→1，120ms EaseOutCubic）+ `Slide`（4px 渲染偏移，动画到最终位置）。
- **Slide 不用布局 inset**（评审 01 M06 / 评审 02 Major 14）：由 `PopupEntry.paint_offset` 在 paint/hit 阶段统一施加（paint 遍历时给 popup 子树 `total` 加偏移；hit 用 `hit_rect.translate(paint_offset)`），不改 layout、不改 settle 结果；damage 覆盖“带偏移的 painted_extent ∪ 最终位置”，首可见帧的 settle rect 不变；动画结束后 `paint_offset = 0`。绘制/命中的接入点：`paint/pass.rs` 的 popup 子树入口与 `layout/hit_test.rs` 的 popup 分支（P1-8 实现并测试）。
- 对话框/Alert：**fade-only**（大面积 Slide 会触发 damage 阈值）。
- **P1 实现**：`paint_offset` 由引擎 `popup_transform` 侧表（`PopupTransform{offset,opacity,blocked}`，节点级）+ `PopupEntry.paint_offset` 共同承载；paint/hit/damage 跟随偏移并有测试（`slide_animation_offsets_paint_and_hit_without_moving_the_settle_rect`、`slide_animation_damage_covers_the_offset_and_the_final_rect`、`a_child_popup_anchor_follows_the_parent_paint_offset`）；**P1 kit 未启用 FadeSlide**（全部 Fade/None，D-P1-19）。
- `Scale`（0.98→1）：D10 裁决为 P3 追加；通过引擎子树渲染变换（lane transform，120ms）实现，P1 仅 fade+slide。
- 关闭：90ms fade；关闭开始即不可命中（DismissPending）；动画结束移除节点；tooltip/menu 的延迟与动画解耦。

### 4.13 错误处理（强类型）

```rust
pub enum PopupError {
    UnknownWindow { window: WindowId },
    UnknownPopup { id: PopupId },
    StalePopup { id: PopupId },
    InvalidGeometry { anchor: LogicalRect, size: LogicalSize, viewport: LogicalSize, avoid: Option<LogicalRect> },
    InvalidScrim { tier: PopupTier, focus: FocusPolicy },  // scrim 必须 tier=Modal 且 FocusPolicy::Trap；调用方不得直接传 tier=Scrim
    InvalidPlacement { gap: LogicalPx, margin: LogicalPx },
    Cycle { id: PopupId },
    DepthExceeded { depth: usize },            // MAX_POPUP_DEPTH = 32
    AnchorNodeMissing { node: NodeId },
    HintBlockedByModal { window: WindowId },   // modal 打开期间背景 Hint 被拒（owner_modal 内的 Hint 允许）
    StackSync { source: EngineError },         // order/节点同步失败（原 expect 兜底改为强类型）
    ContentBuild { node: NodeId, source: EngineError },   // 保留引擎错误
    LimitReached { max: usize },               // 栈条目上限 MAX_POPUP_ENTRIES = 1024
}
```

P1 最终变体为上面 **13 个**（设计原稿 11 个）：新增 `HintBlockedByModal`（用户裁决 B 的 Hint-in-modal 收窄）与 `StackSync`（`sync_order` 失败不再 panic），`InvalidGeometry` 增加 `avoid` 字段（非法 avoid 几何同路径返回），`InvalidScrim` 同时覆盖“调用方伪造 `tier=Scrim`”和“scrim 与 Modal/Trap 不匹配”。

- 变更型公开 API 返回 `Result<_, PopupError>`；查询型可返回 `Option`（文档明确区分，消除评审 01 M09 的矛盾）。
- `open_popup` 内容闭包失败必须引擎侧回滚：删除部分子树、恢复 focus 快照、不留下 entry、不改变 open 状态。
- 禁止 `unwrap`/静默 clamp。

### 4.14 调试与可观测

- `App::popup_stack(window) -> Option<PopupStackSnapshot>`：`[PopupId, tier, parent, z, state, hit_rect, paint_rect, owner_modal, focus, paint_offset, constrained_size]`。
- `--dump-overlay` 改为 dump 快照（**P1 最终命令名：`--debug-stack`**；`--debug-stack` 的 `overlay-check` 为**真实窗口栈/rect 断言**：栈条目数/tier/菜单度量/首可见帧 rect==稳定帧/关闭后无残留/结束时栈空，不读像素）。像素类断言（`damage_full==false`、残影基线像素对比、覆盖下限、注入自检）由 headless **`--pixel-check`** 承担，两者不可互相替代。
- `FrameRecord` 增加 `popup_damage`/`z_changes`/`settle_rounds`：P1 只落地 `FrameStats.popup_settle_rounds`/`popup_unsettled`（`test_support/record.rs`、`app/frame.rs`），**`popup_damage`/`z_changes` 未加（P3，A-31）**；`--debug-stack` 以 `App::popup_stack` 为唯一 dump 面。

---

## 5. API 规格

### 5.1 `ui_core` 新增/修改

新增 `crates/ui_core/src/popup.rs`（模块根：类型/错误/常量）与 `crates/ui_core/src/popup/`：`anchor.rs`、`placement.rs`（含 `PlacementInput`）、`stack.rs`、`settle.rs`、`dismiss.rs`、`focus.rs`、`animation.rs`、`tests.rs`（workspace clippy 禁止 `mod.rs`，与 `app.rs/app/` 风格一致）；**P1 实际落点**：a11y 投影在 `crates/ui_core/src/a11y/popup.rs`（+`a11y/popup_tests.rs`），App/Window 集成在 `crates/ui_core/src/app/popup.rs`（无 `popup/a11y.rs`）。

```rust
impl App {
    pub fn open_popup(
        &mut self,
        window: WindowId,
        spec: PopupSpec,                    // 子 popup 通过 spec.parent 指定父级；无第二通道
        content: impl FnOnce(&mut App, WindowId, NodeId) -> Result<(), EngineError>,
        on_dismiss: Option<Box<dyn Fn(&mut App, PopupDismiss) -> DismissOutcome>>,
    ) -> Result<PopupId, PopupError>;
    pub fn close_popup(&mut self, id: PopupId) -> Result<(), PopupError>;          // 立即（含子树，子先于父）
    pub fn close_popup_chain(&mut self, id: PopupId) -> Result<(), PopupError>;    // 立即关整链（自链根）
    pub fn begin_close_popup(&mut self, id: PopupId, reason: DismissReason) -> Result<bool, PopupError>; // 事务：DismissPending→Closing→动画后移除；false=不可关闭
    pub fn popup_state(&self, id: PopupId) -> Option<PopupState>;
    pub fn set_popup_a11y(&mut self, id: PopupId, a11y: PopupA11y) -> Result<bool, PopupError>; // 变更型→bool（是否变化）
    pub fn set_popup_initial_focus(&mut self, id: PopupId, node: NodeId) -> Result<bool, PopupError>; // Trap 初始焦点（P1 终审补 K1-05）
    pub fn raise_popup(&mut self, id: PopupId) -> Result<(), PopupError>;
    pub fn set_popup_anchor(&mut self, id: PopupId, anchor: Anchor) -> Result<(), PopupError>;
    pub fn request_popup_measure(&mut self, id: PopupId) -> Result<(), PopupError>;
    pub fn popup_rect(&self, id: PopupId) -> Option<PopupRect>;          // hit/paint/a11y 三 rect
    pub fn popup_anchor_rect(&self, id: PopupId) -> Option<LogicalRect>; // 锚点当前窗口 rect（P1 终审补 E-07）
    pub fn popup_anchor_visible(&self, id: PopupId) -> Option<bool>;     // 锚点可见性（P1 终审补 E-07）
    pub fn window_scroll_epoch(&self, window: WindowId) -> Option<u64>;  // 祖先滚动纪元（P1 终审补 E-07）
    pub fn popup_stack(&self, window: WindowId) -> Option<PopupStackSnapshot>;
    pub fn popup_pointer(&self, window: WindowId) -> Option<LogicalPoint>; // 最近一次指针位置
    pub fn dismiss_topmost(&mut self, window: WindowId, reason: DismissReason) -> bool; // 走关闭事务；返回 consumed
    pub fn popup_diagnostics(&self, window: WindowId) -> Vec<PopupDiagnostic>;
    pub fn modal_scope(&self, window: WindowId) -> Option<NodeId>;      // 保留：最上层 modal popup 的 content 节点
}

impl Window {
    pub fn node_screen_rect(&self, node: NodeId) -> Option<LogicalRect>;  // 含祖先滚动与 popup paint_offset（补 G10）
    pub fn set_max_size(&mut self, node: NodeId, max: node::tree::MaxSize) -> Result<(), EngineError>; // 同步失效 popup max-size 缓存
    pub fn scroll_epoch(&self) -> u64;                                   // 祖先滚动纪元（P1 终审补 E-07）
}

impl NodeTree {
    pub fn set_max_size(&mut self, node: NodeId, max: MaxSize) -> Result<(), EngineError>;
    pub fn reorder_children(&mut self, parent: NodeId, order: &[NodeId]) -> Result<(), EngineError>; // 全排列，不触发 structure epoch
    pub fn hit_test(&self, point: LogicalPoint) -> Option<HitResult>;    // 既有 API，widgets 指针换算复用
}
```

P1 最终补充：`begin_close_popup`/`popup_state`/`set_popup_a11y`/`popup_diagnostics` 为 P1 补缝新增（kit 关闭事务与动态 a11y 需要）；**P1 终审修复新增** `set_popup_initial_focus`（K1-05 默认按钮显式指定）、`popup_anchor_rect`/`popup_anchor_visible`/`window_scroll_epoch`/`Window::scroll_epoch`（E-07 供 kit 做祖先滚动/resize 关链矩阵）。`close_popup` 保持“立即删除”语义（`close_now` 语义），程序化动画关闭统一走 `begin_close_popup`（B-09 契约化）。`App::modal_scope` 保留但语义改为 popup content 节点（D-P1-20）。**P3 新增**：`PopupAnimation::FadeScale`、`PopupSnapshotEntry.paint_scale`、`FrameStats.frame_cpu/popup_opening/popup_closing`（§14.3）。

引擎行为改动：`App::frame` 插入 settle（§4.3）；pointer-down/Esc 按 §4.7；渲染 I1–I9；a11y 按 §4.10；删除旧 `App::create_overlay/...`、`interaction::{Overlay,...}`、`app/overlay.rs`、`interaction/overlay.rs`、`overlay_style`、`note_overlay_container/overlay_containers`（P1-11 已完成）。

### 5.2 `ui_kit` 新增/修改

- `PopupPanel`（`crates/ui_kit/src/kit/popup_panel.rs`）：surface/shadow/clip/内容闭包/测量/动画/关闭事务；**P3**：`close_with_reason(app, DismissReason)`（与 `close` = Programmatic 并列）、动画 `None/Fade/FadeSlide/FadeScale` 由 `PopupSpec.animation` 决定；`PopupSurface { background: Role, corner_radius: LogicalPx, padding: LogicalPx, shadows: Shadows }`（`menu()/dialog()/tooltip()` 预设，替换 `OverlaySurface`）；`PanelWidth { Auto, Exact(LogicalPx), AtLeast { min } }`；`PanelMeasure { labels, extra, text_size }`；`PopupPanelOptions { surface, width, measure }`；`open(app, window, spec: PopupSpec, content: impl FnOnce(&mut App, WindowId, NodeId) -> Result<(), EngineError>) -> Result<PopupId, PopupError>`（完整 PopupSpec 含 tier/anchor/placement/dismiss/focus/parent/scrim；初始焦点由 `App::set_popup_initial_focus` 指定，E-21 后无 `spec.initial_focus` 字段）；`close/close_with_reason/close_now/set_anchor/request_measure/set_measure/set_width/set_a11y/sync/release/is_open/is_closing/state/id/panel_node/body_node/popup_node/open_state/on_close`。节点树：`popup-content`（引擎，**P1 终审修复 E-03 后不再 `overflow_hidden`**，panel 阴影可见）→ `{key}:panel`（surface/阴影/内容裁剪）→ `{key}:body` → 内容；`AtLeast` 时另有 `{key}:measure`（隐藏测量节点）。**内部滚动（P1 终审修复 E-04/Q1-01）**：`PopupPanel` 读取引擎 `constrained_size`（`available_body_height`），Menu 列表 / Dialog 内容区按实际可用高度收敛并内部滚动，Dialog footer 固定；**横向同理（round-3 E-20）**：`available_body_width` + Dialog 宽度 clamp 到 viewport−margin，320×220 下 confirm 不再横向被裁。
- `MenuChain`（`crates/ui_kit/src/kit/menu_chain.rs`，P2 已落地）：公开 `HoverTimings { open_delay=200ms, close_grace=300ms, typeahead_timeout=2s }`；内部 `LevelId`/`LevelState{Closed,Opening,Open,Closing}`/`OpenSource{Pointer,Keyboard,Click}`/`SafeTriangle`/`MenuChain`（`pub(crate)`）——per-level 状态/父子关系/rect/anchor/grace/active、`open_root/begin_open/confirm_open/cancel_open/close/mark_closing/reopen/...`、children-first、timer 绑定 level、兄弟替换；5 点 safe triangle + 300ms grace；外部按下落在 safe triangle/grace 内 Ignore（D-P2-2；Q2-03 后无桥接几何）。
- 组件公开面（P1 最终）：
  - `Dropdown::{new(app,theme,key,DropdownOptions), open(app)->Result<bool,PopupError>, close, close_now, sync, dispatch, release, set_items, set_selected, on_select, trigger_node, panel_node, popup_node, open_state, is_open, selected, highlighted}`；`Select::{new(...,SelectOptions), open, close, close_now, sync, dispatch, release, set_items, set_chosen, clear, on_change, chosen, chosen_indices, highlighted, placeholder_text, tooltip_text, is_open, is_disabled, set_disabled}`；宽度匹配用 `PanelWidth::AtLeast`（= 锚点宽），未显式宽度时 `Exact`/`Auto` 按配置；Dropdown/Select 也经 `ItemPanel` 使用 `FadeSlide`。
  - `Tooltip::{new(app,theme,key,TooltipOptions), attach, set_content, content, is_visible, panel_node, popup_node, sync, release}`；`TooltipOptions { show_delay=500ms, close_delay=100ms, placement, max_width }`；Hint tier + `pointer_transparent`；Esc 关自身；hover 区域含 tooltip rect（WCAG 1.4.13）。
  - `Menu::{new(app,theme,key,entries,MenuOptions), open(app,window,anchor)->Result<bool,PopupError>, open_at(...,highlighted), open_submenu(app,row)->Result<Option<Menu>,PopupError>, bind_trigger(app,window,trigger)->Result<(),EngineError>, close, close_now, sync, release, on_activate(MenuActivation), on_close(reason), set_entries, entries, row_count, highlighted, open_state, is_open, panel_node, popup_node}`；`MenuOptions { size, width, max_height, placement, label, timings: HoverTimings, outside_press: OutsidePress }`；Menu/ContextMenu 打开动画为 `FadeSlide`（4 px slide + fade，settle rect 不变）；锚点丢失/滚出视口关闭上报 `DismissReason::AnchorLost`；`MenuAction { id, label, icon, shortcut, disabled, danger, check: Option<MenuCheckState> }`（`checkbox(bool)/radio(bool)`，`MenuCheckState{kind: MenuCheck{Checkbox,Radio}, checked}`）；`MenuActivation { id, check: Option<MenuCheckState> }`；`MenuEntry::{Action(MenuAction), Group(SmolStr), Separator, Submenu(MenuSubmenu)}`。`ContextMenu::{new(app,theme,key,entries,ContextMenuOptions), open, open_at, open_on(app,window,node), bind_trigger(app,window,trigger), close, close_now, sync, release, on_item_click, on_close}` + `MenuAnchor`；`ContextMenuOptions { size,width,max_height,anchor,label,outside_press }`；ContextMenu 用 `Anchor::WindowRect(pointer_rect)`；`open_on` 取节点底边左端点；`bind_trigger` 仅键盘（Menu=Alt+F10；ContextMenu=Shift+F10/ContextMenu 键），左键/右键由宿主绑定（D-P2-4）。**R2-f**：`open_submenu` 返回的子菜单句柄上 `on_activate/on_close` 仅在 root 生效（子层静默无效），宿主应在 root 句柄注册回调。
  - `DialogBuilder::{new(title), alert, content, confirm, cancel, header, close_button, close_label, width, outside, escape, fade, on_close, build(app,theme,key)}`；`Dialog::{open(app,window)->Result<bool,DialogError>, close, close_now, sync, is_open, is_closing, state, title, set_title, popup_node, panel_node, body_node, confirm_button, cancel_button, close_button, with_alert, release}`；`DialogClose`/`DialogError`；spec 固定 `Modal + Trap{restore: Previous, initial: DefaultButton} + Explicit{outside,escape} + ScrimSpec{color: theme.dialog.backdrop, density: 1.0} + FadeScale/None`（P3：fade=true 用 `FadeScale`，modal fade-only）（P1 终审修复 E-02：遮罩 = 主题 backdrop 色 × 1.0，Light alpha 0.42 / Dark 0.55）；打开后 `App::set_popup_initial_focus(id, confirm)` 锁定默认按钮（K1-05，自定义 footer/内容按钮不再抢焦点）。**`recenter` 已于 P3 删除**（resize 不再重新居中，只保留打开时 anchor 居中，`resizing_the_window_keeps_the_anchor_opened_with`）；与旧 `closing/reason` 双状态已删除（改用引擎 `state` + `on_close(reason)`）。
  - `AlertBuilder::{ok, ok_cancel, input_ok, input_ok_cancel, ok_text, cancel_text, validate, ok_action, cancel_action, build}`；`Alert` 是 Dialog 的内容面板（**不新建 popup**，D9/D11），可多个 Dialog/Alert 按 z 叠放（无独占 band）。
  - `ColorPicker::{new(app,theme,key,initial_color,size), open(app,window)->Result<bool,PopupError>, close, sync, release, is_open, open_state, popup_state, is_closing, trigger_node, panel_node, popup_node, color, color_signal, draft, hovered_swatch, active_tab, hex_text, labels, set_labels, set_active_tab, set_color, on_change, on_open, on_close}`：内容闭包构建（已删除“空 open + build_panel 二次构建”）、palette/slider/well 局部坐标 Canvas、tab 切换 `request_measure`、Auto tier + `Take{Previous}` + `PopupA11y{role: Dialog}`（非模态）；**P3**：非模态 `FadeScale`（0.98→1/120ms，paint center pivot），锚点丢失上报 `DismissReason::AnchorLost`。
- 删除（P1-11 已完成）：`kit/overlay.rs` 的 `Overlay/OverlayOptions/OverlaySurface/Placement/PlacementConfig/Side/Alignment/Anchor/resolve_origin/Scrim/OverlayA11y/AnimationRouter`、`kit/layer.rs` 全文（`Layer/Band/LayerSlot/Entry`）、`kit::test_support::overlay_wrapper`、`MenuMode::ListBox`、`ItemPanel` 旧私有形态（保留为基于 `PopupPanel` 的 menu 视图）、`App::overlays`。`rg` 零引用证据见 §12.9。

### 5.3 调用点迁移清单（实施前以 `rg` 锁定）

**组件/框架层（必须迁移）**

| 区域 | 现状 | 数量级（评审 02 rg） |
|---|---|---|
| `crates/widget` | `WidgetServices.layer: Rc<Layer>`、`animation_router`、`Cx::layer()/animation()`、`binding.rs` 注入 | 4 个文件（services/cx/binding/test_support） |
| `crates/kytrix` | `composition.rs` 创建 `Layer/AnimationRouter` 并注入；`dialogs.rs` 使用 | 2 个文件 |
| ui_kit 组件 | dropdown/select/menu/context_menu/tooltip/dialog/alert/color_picker + overlay/layer | 10 个文件 |
| business ui crates | `instrument_selector_ui`、`market_participants_ui`、`profile_ui`、`settings_ui`、`updater_ui` 等直接 `Overlay::new`/`Band`/`Layer`/`AnimationRouter` | `Overlay::new` 直接 2 处（market_participants_ui/menu.rs:271、profile_ui/profile.rs:176）；`Select::new` 17 处/15 文件；`DialogBuilder::new` 12/11；`Tooltip::new` 6/6；`ColorPicker::new` 1/1 |
| 14 个 widgets crate | `cx.layer()/cx.animation()` 传给 `Overlay::new`（candlestick、order_book、volume_heatmap、cryptocurrency_etf、global_shipping_chokepoint、prediction_market、perpetual_statistics、financial_calendar、coin_market_cap…） | 约 15 个 view 文件 |
| 测试支持 | `App::overlays` 4 处（instrument_selector_ui:157、market_participants_ui:474、profile_ui:794、market_trade/tests:1564）；`ui_core/tests/hud.rs:346` `create_overlay` | 5 个文件 |
| Gallery | ⑫⑬⑭ + `--dump-overlay` + `--probe` + overlay-check | `examples/gallery.rs` |

**Canvas 迁移（P0）**：真实 `Canvas::new(` 构造点全仓 **132 处/41 文件**（统计口径：`crates/**/*.rs` 上匹配 `(?<![A-Za-z_])Canvas::new\(`，排除 `ChartCanvas::new`/`MapCanvas::new` 等子串；其中 P0 新增测试 +8：首轮 124/41 + 后续修复轮 5 个（其中 R2-01 再 +3 个））——`ui_core` 93/24（含 `test_support/snapshot`、`test_support/diagnostics`）、`widgets` 25/13（production：order_book/src/paint.rs、volume_heatmap/src/chart.rs、prediction_market/src/chart/paint.rs、candlestick/src/chart/paint.rs 等）、`ui_kit` 9/2（color_picker + spinner）、`widget` 5/2（test）。全部改为局部坐标（语义迁移 132/132，含 C-04 的 diagnostics），并更新 golden/像素测试。设计 rev.6 原写的 45 文件数是子串口径误计（把 `ChartCanvas::new` 等并非 `Canvas` 的构造点计入），已按实测修正；构造点数在 P0 各修复轮后为 132。

---

## 6. 实施计划

> 每个阶段可独立编译、测试、验收；P0 只改渲染不变量与 Canvas 语义（最终架构的一部分），另含两项用户裁决 A 的临时防御——P0-7（结构变化增量绘制，C8/I6 所必需）与 P0-8（旧 kit Overlay 首帧定位；`Overlay` 是 P1-11 会整体删除的过渡 API，P0-8 只做最小 measure/place/reveal，不改变 popup 架构）；P1 是唯一的“旧 popup API → 新 popup 栈”原子切换；P2/P3 只做行为与验收，不再有双机制。

### P0 — 渲染不变量 + Canvas 坐标契约 + 判别实验

**目标**：残影的可复现机制被判别并消除；Canvas 局部坐标契约与全量调用点迁移完成；建立持久增量像素门禁。

**任务**

| # | 内容 | 落点 |
|---|---|---|
| P0-0 | **已完成**：E1–E3/E9（含 E4/E8）判别实验，产出 `evidence/p0-diagnosis.md`；M-D/M-E refuted，M-C/M-A confirmed | gallery + 截图/像素分析 |
| P0-1 | `painted_extent` 持久记录（含 shadow pad/ring/Canvas/Geometry），移动/隐藏/删除 damage 用它；移除路径对子树取并集 | `paint/pass.rs`、`node/tree/*`、`render/damage.rs` |
| P0-2 | **按 P0-0 收窄**：不做生产代码改动，只补“lane 释放/顺序变化必须使 ops 一致”的防御性不变量测试（M-D refuted） | `paint/instance.rs`、`render/delta.rs`（测试） |
| P0-3 | **并入 P0-7**：结构增量必须保证 node→scroll slot 表在新增/删除/顺序变化时一致（M-E 本身已 refuted，此处只为结构增量服务） | `paint/pass.rs`、`render/delta.rs`、`app/virtual_list.rs` |
| P0-7 | **结构变化增量绘制（用户裁决 A，2026-11-26 追加）**：新增/删除节点不得强制全量绘制；收窄 `window/paint.rs` 的 `!removed.is_empty()`、structure epoch、无 lane/clip_key 升 full 三条路径；order 增量更新、scroll slot 同步、damage 用 painted_extent 局部覆盖；满足 C8/I6「非模态 popup 开关不 `damage_full`」 | `window/paint.rs`、`paint/*`、`render/*`、`node/tree/*` |
| P0-4 | Canvas 局部坐标契约：新增 transform/primitive 支持；迁移全部真实 `Canvas::new` 调用点 132 处/41 文件（ui_core/ui_kit/widgets/widget/examples/tests）；更新 canvas_text、damage、golden | `paint/*`、全仓调用点 |
| P0-5 | 持久增量像素 harness：真实窗口 gallery 截图 + `FrameRecord.damage/damage_full`（`FrameRecord` 需经 `#[cfg(feature="test-support")] App::frame_record(window)` 暴露；gallery 以 test-support feature 构建），或新增“持久 target + scissor 增量”测试 harness；基线对比排除 popup `paint_rect`（评审 01 B3 / 评审 02 rev.2 Major） | `test_support/*`、`examples/gallery.rs` |
| P0-6 | kit 最小防御（不改变 popup 架构）：ColorPicker 面板/内容根 `overflow_hidden`（Canvas 契约已修后仍保留）；gallery 像素断言 | `kit/color_picker.rs`、`gallery.rs` |
| P0-8 | **旧 kit `Overlay` 首帧定位最小修复（用户裁决 A，2026-11-26 追加）**：`Overlay::open` 在 open 内完成 measure（`App::refresh_text` + `Window::compute_layout`）→ place → reveal，首可见帧 rect == 稳定帧；`ColorPicker::open` 返回强类型错误并在 measure 失败时回滚。不改 popup 架构；`Overlay`/`Layer` 仍按 §8 在 P1-11 删除，P1-12 需重写依赖它的 `--pixel-check` 与首帧测试（§11.3-3、§11.3-9） | `kit/overlay.rs`、`kit/color_picker.rs`、`examples/gallery.rs` |

**验收**：① P0-0 结论明确（M-D/M-E 已排除）；② 残影消失——P0 以 **headless 持久增量像素门禁** `gallery --pixel-check` 证明（打开前后、移动后、关闭/重开全程对打开前基线逐像素对比 + 注入故障负向自检；用户已接受此替代真实窗口像素的路线），真实窗口 `--dump-overlay` 只断言 rect/children，真实窗口 readback/截图推迟到 P3-6；③ Canvas 全量迁移后 chart/spinner/取色器测试绿——本轮按用户要求只跑 `ui_core`/`ui_kit` 门禁，`widgets` 迁移由 P0-4 审查独立复跑 golden 与 P0-4b 的 9 crate 1639 passed 自证（§11.5、C-01 证据）；④ 非模态 popup/面板打开、关闭、重开用例 `damage_full==false`（P0-7）；⑤ 现有全量测试/bench 不退化。**不改变 popup 架构（旧 `Overlay` 仍用）；Canvas 语义是破坏性变更，已列出全部调用点；P0-8 是旧 Overlay 的最小临时防御，P1-11 删除。**

### P1 — 引擎 popup 内核 + kit 全量迁移 + 宿主迁移（原子切换）

**目标**：§4.2–§4.14 落地；`PopupPanel` 替换 `Overlay`；**所有**组件（含 Menu/ContextMenu）与 `crates/widget`/`crates/kytrix` 迁移；删除旧 API，只保留一套浮层机制。

**任务（内部按序提交，最终同一阶段收口）**

| # | 内容 |
|---|---|
| P1-0 | **popup root 原型验证**：0×0 绝对节点 + auto-size 子节点在 taffy 0.15 的 containing block/available-space 行为；若不可行改用 viewport-size popup layer + hit/focus skip-container + 子节点自身 damage。产出原型测试与结论（评审 02 rev.2 Major） |
| P1-1 | `PopupStack`/`PopupId`/`PopupEntry`/tier/z/父子链/状态机；popup root 预建 + `HitFocusSkip::Container` 语义（根自身不参与命中/焦点，子节点正常）；`reorder_children` 不触发 structure epoch；Debug HUD 固定层 |
| P1-2 | `settle` 接入（正确插入点、2 轮、重跑 layout_text/virtual/clamp/canvas text、报告聚合、dirty 闸门） |
| P1-3 | `solve_placement` 纯函数 + 单测（候选/flip/bestFit/shift/limitShift/resize/clamp/NaN/子菜单 Right→Left） |
| P1-4 | 运行时 `set_max_size` + `PopupPanel` 内部滚动（确定高度，不靠 max-height 裁） |
| P1-5 | dismiss 事件/矩阵（§4.7）、opener toggle、D14 replay（`Consume` 显式 opt-in 时才消费）、Closing/DismissPending 输入阻断、窗口生命周期 |
| P1-6 | FocusManager 单一真值（paint/input/a11y/cursor/IME/text_edit）与焦点快照/恢复/fallback |
| P1-7 | a11y 链嵌套（a11y_parent）+ expanded/haspopup/owns/described_by/active_descendant + adapter 接线 |
| P1-8 | Fade/Slide(paint_offset)/关闭事务动画 |
| P1-9 | `PopupPanel` + `MenuChain` 基础 + **全部组件迁移**：Dropdown/Select/Tooltip/Menu/ContextMenu/Dialog/Alert/ColorPicker（行为 parity，不含 P2 高级菜单语义） |
| P1-10 | 宿主迁移：`crates/widget` Services/Cx/binding/test_support、`crates/kytrix` composition/dialogs、14 个 widgets view、direct Overlay 消费方、测试支持 `App::overlays` 替换 |
| P1-11 | 删除旧 `Overlay/Layer/Band/LayerSlot/AnimationRouter/ItemPanel` API 与引擎旧 overlay API；`rg` 无引用 |
| P1-12 | 测试重写：现有 kit 测试迁移为“首可见帧断言”，`interaction_tests` 22 处 `create_overlay` 替换；gallery `--dump-overlay` 改 `debug_stack` |

**验收**：① 旧 API `rg` 零引用；② 全部组件 parity 测试 + 首可见帧/damage 断言；③ 非模态 popup 打开/关闭无 `damage_full`；④ workspace `cargo check --all-targets` 与测试全绿。

### P2 — 菜单链与 2–3 级级联子菜单

**目标**：D5 桌面菜单语义完整。

| # | 内容 |
|---|---|
| P2-1 | `MenuChain` 状态机（§4.8）：per-level 状态、timer 绑定 level、close siblings/descendants、children-first close |
| P2-2 | 5 点 safe triangle/grace（镜像 bleed、300ms、open/flip/scroll 重算）；grace 期禁止兄弟替换 |
| P2-3 | 菜单面板：行高亮/滚动/ensure_visible/子菜单指示/分组/分隔线/禁用/勾选 |
| P2-4 | 完整键盘表（§4.8）+ typeahead + checkable 语义 |
| P2-5 | 链式 dismiss：外部点击命中祖先只关后代、Esc 逐层、tooltip 优先、按 D14 replay（`Consume` 显式 opt-in） |
| P2-6 | ContextMenu：WindowRect、右键/Shift+F10/Menu、子菜单、与菜单共享 MenuChain |
| P2-7 | 3 级测试 + a11y 链嵌套测试 + gallery 3 级演示 |

**验收**：3 级在真实窗口完成 hover/斜向/sibling/Esc/外部点击/键盘/滚动全路径；最新层恒最上；`overlay-check` 菜单链用例 PASS；无 focus 丢失/背景输入。

### P3 — Dialog/Alert/ColorPicker 细化、scale 动画、三端验收与性能

| # | 内容 |
|---|---|
| P3-1 | Dialog/Alert 细化：FocusPolicy::Trap 的初始焦点/Tab 循环/outside/escape/restore；删除 recenter/双份 closing |
| P3-2 | `Scale` 动画（D10：lane transform，0.98→1，120ms；含 damage/像素回归） |
| P3-3 | ColorPicker 细化：内容闭包、局部坐标、tab 重新测量、open/close 回调 |
| P3-4 | gallery：⑫⑬⑭ 新 API、3 级菜单/残影像素采样、`--dump-overlay`/`--probe` |
| P3-5 | 性能：popup open/close 帧预算（P1 起点：非模态 popup 打开/关闭帧 CPU ≤ 16.7ms（60Hz 单帧预算）；空闲帧 0 popup 工作；P3 以基线复核，目标 popup-open 帧 ≤ 同窗口 idle p95 + 2ms）；`damage_full==false` 断言 |
| P3-6 | 三端真实窗口验收：每平台命令/截图 API/像素判定/已知差异/失败退出码（gallery `process::exit(3)`）；Wayland/macOS 特殊说明 |
| P3-7 | 文档回写：偏差登记、最终 API、测试/性能基线；同步 `.docs/gui-migration/spec-deltas/kit-gallery.md` |

**验收**：D8 全部门禁 + 三端 + 残影像素回归 + 性能基线。

---

## 7. 测试计划

### 7.1 引擎不变量测试（新增 `popup/tests.rs`、`app/overlay_tests.rs`）

> **P1 实际落点**：`crates/ui_core/src/popup/tests.rs`（43 条，栈/数据模型/层序/damage）+ `crates/ui_core/tests/popup_behavior.rs`（110 条，settle/dismiss/focus/动画/窗口生命周期，`required-features=["test-support"]`）+ `crates/ui_core/src/a11y/popup_tests.rs`（21 条）；设计原提的 `app/overlay_tests.rs` 未单独创建（内容并入前两者）。

1. 首帧放置：内容高度在 `layout_text` 后才确定；首个 `Open` 帧 `hit_rect` == 稳定帧（不含动画偏移）。
2. 内容增长：tab 切换/列表数据 → 同帧 settle，无第二帧位移。
3. 锚点跟随：滚动/移动锚点/窗口 resize/DPI；菜单/右键菜单在祖先滚动时关链、dropdown/select/colorpicker 重定位（§4.7 矩阵）。
4. 翻面/尺寸：四方向 + 子菜单 Right→Left；flip 只严格更好；shift 不脱锚；超长菜单缩小 + 内部滚动。
5. z/tier：连续打开 A/B/C；raise；子菜单在父上；第二个 Auto 根替换第一个；modal 关闭无关链；Debug HUD 最上且穿透。
6. 模态：输入/焦点/a11y/hover/scroll 阻断；modal 内打开 Select/ColorPicker 可用（owner_modal）；关闭恢复。
7. dismiss：外部按下按 §4.7 的 scope 优先顺序（modal scope 先判定；scope 内 Auto 链 outside 优先于 modal 面板；opener Toggle；replay press+release）；点祖先只关后代；Esc 跳过 Closing；tooltip Esc 只关自身；opener toggle 与 Closing 中重开；Dialog 内 Select 可被外部关闭；modal 关闭回收 scrim 与 owner_modal 后代；窗口失焦/最小化矩阵。
8. 焦点：打开取焦/关闭恢复一次/不抢新 popup 焦点/opener 失效 fallback；对话框 Tab 循环；IME composition 开/关两侧。
9. 动画：Slide 用 paint_offset，settle rect 不变；damage 覆盖偏移区；动画期间 hit 跟随 `paint_offset`（`hit_rect.translate(paint_offset)`）。
10. 错误：depth/cycle/stale/invalid geometry/content build 回滚/limit。
11. 栈不变量 I7/I8/I9；`damage_full==false` 覆盖 popup 移动/关闭/打开（模态除外）。
12. 评审 03 §7 列出的 popup/IME/窗口生命周期用例全部纳入（`popup_escape_ignores_closing_entries_and_closes_exactly_one_open_level` 等）。

### 7.2 渲染/损毁不变量测试（P0）

1. 移动/删除节点 damage ⊇ `painted_extent`（含 shadow/ring/canvas/geometry）。
2. Canvas 局部坐标：节点移动/滚动/父链移动时内容随动；damage 正确。
3. lane 释放/顺序变化：ops 与 scene 一致；旧 op 不被重画（按 P0-0 结论）。**实际落点**：order-only 不变量测试在 `render/compose.rs`（`releasing_a_lane_rebuilds_the_ops_without_the_freed_range` / `reordering_lanes_without_removal_rebuilds_the_ops`），而非 I2 原提的 `paint/instance.rs`/`render/delta.rs`——接受该落点：两者都验证 `LaneStore`/`WindowComposer` 的 ops 重建路径，本阶段不额外搬移（C-06，§11.3-15）。
4. scroll slot：集合变化重映射；virtual list 增删 + popup 开关；`ScrollTable` 无 stale slot。P0 用结构/damage/scroll 表断言锁定（无 stale slot、`delta.scroll` 仅真实滚动帧为 1、全程 `!damage_full`），未接持久像素；ui_core 已有等价像素门禁（C-07，§11.3-16）。
5. 像素回归（C9）：**持久增量通路**（真实窗口或新 harness），基线对比排除当前 popup `paint_rect`；`damage_full==false`；打开→稳定窗口→关闭→重开全流程。基线不透明断言排除最外 1 行/列（TestApp 渲染器既存 3 像素 `alpha=218` 边缘伪影），内容区要求完全不透明（Q-07，§11.3-18）。
6. 现有 `paint/pass.rs` 测试 + `layout_postpass`/`perf` bench 守护。

### 7.3 组件行为测试（kit）

- Dropdown/Select：位置/match width/键盘/typeahead/多选/点击回填/外部关闭/滚动跟随/Closing toggle。
- Tooltip：延迟/关闭、穿透、不关菜单、hover 区域含自身 rect、Esc 只关自身、不取焦。
- Menu/ContextMenu（P2 已落地；`menu.rs`/`menu_chain.rs`/`context_menu.rs` 共 **109 条 `#[test]`**（menu 76 + menu_chain 13 + context_menu 20））：§4.8 全键盘表（↑↓/Home/End/PageUp/PageDown/←→/Enter/Space/Esc/Tab/Shift+Tab/Alt+F10）+ typeahead/checkable（Checkbox/Radio）+ 2–3 级级联 + 兄弟替换/safe triangle/grace Ignore + 外部 Replay/Consume + Esc 逐层 + 滚动/禁用/分隔线/分组 + 最新在上 + a11y 链嵌套/expanded/active_descendant。边界：ContextMenu secondary-button 自动绑定未内建（D-P2-4）；typeahead 不消费 text/IME 字符（D-P2-5）。
- Dialog/Alert：模态阻断、Tab 循环、初始焦点（默认按钮优先）、scrim、Esc/按钮、关闭动画、重复打开。
- ColorPicker：内容闭包、局部坐标、tab 重新测量、hover 预览、hex、外部/Esc、无残影。
- 所有组件：open 失败回滚、释放后重开、窗口关闭清理。

### 7.4 三端验收（D8）

- 每端：`gallery --idle-check`、`--theme-check`、`--debug-stack`（P0 时命令名为 `--dump-overlay`，P1-12 改名；自行退出 0/3）、`--pixel-check`（headless 残影门禁）、`--real-pixel-check`（真实窗口截图门禁，P3 新增）、3 级菜单、残影像素采样。
- **P3 完成（P3-6）**：真实窗口像素采用「截图 + 品红标记环」方案（延续 P0 用户裁决：不做 GPU readback）：`--real-pixel-check` 固定 960×720 always-on-top 窗口 + 内容边缘 6 px `#FF00FF` 标记环；全屏截图解码后由标记框给出内容原点/物理尺寸/DPR，引擎 popup rect 按窗口 scale 映射为物理像素区域；序列 baseline → ColorPicker open/close → 3 级 MenuChain open/close → Dialog+scrim open/close，打开必须出现可见变化、关闭必须逐像素恢复（coverage floor + 分散像素 + 触发器环排除）；负向自检注入偏移/改色必须判红；exit 0/3，证据 `target/real-pixels/<platform>/report.txt` + 逐阶段 PNG。
- **三端最终证据（2026-11-27，HEAD `35c9381d` clean）**：**Windows/macOS/Ubuntu 各 106 checks / 0 failures PASS**——Windows 45.9 s/frame 280（本地隔离 43.3/277）、macOS 72.2 s/frame 283、Ubuntu 20.3 s/frame 311；三端 identity 均为 `revision=35c9381d status=clean diff_sha=e69de29… identity=source-id-file`；pixel-check 94、idle/theme/debug-stack 均 PASS，menu 度量 `(9, 270.0, 180.075)`；证据路径见 §14.4。
- 记录 OS/DPI/窗口/截图/帧号/`damage_full`/耗时：运行时证据落 `target/real-pixels/<platform>/`（远端原始 `evidence_dir` 见 §14.4）；Wayland/macOS 差异与跳过规则写明；失败退出码 3（前置缺失如无 Wayland/grim 时脚本 exit 2）。
- 评审 02 Major 21 要求：每平台写明启动命令、截图 API、像素判定（§14.4 表）。

### 7.5 门禁

`cargo test`（workspace）、`cargo +nightly clippy -- -D warnings`、`cargo +nightly fmt`；每阶段 `cargo check --workspace --all-targets`；`rg` 旧 API 零引用（P1 起）；gallery 自动化 PASS；像素回归绿；性能基线对比。

**P0 追加像素门禁（可复制，Windows PowerShell；P1-12 已迁移到新 popup API，命令名/退出码不变）**：

```powershell
$env:CARGO_TARGET_DIR='E:\kytrix\target'
cargo run -p ui_kit --example gallery -- --pixel-check   # exit 0=PASS，3=FAIL（残影/增量重绘门禁）
cargo run -p ui_kit --example gallery -- --debug-stack                  # 真实窗口：栈快照 + 首可见帧/rect 断言（自行退出 0/3；--auto-close 仅 ≥30s 看门狗）
cargo run -p ui_kit --example gallery -- --real-pixel-check             # 真实窗口截图像素门禁（P3；exit 0/3；macOS/Linux 用 tools/ 脚本）
```

`--pixel-check` 已从 `kit::overlay`/`ColorPicker` 迁到 `PopupPanel`/`popup_state`/`popup_node`（P1-12，94 checks + 采样动画下限，见 §12.3-9/§12.5）；`--dump-overlay` 已改名为 `--debug-stack`（§12.3-8），旧名与未知参数均显式 **exit 2** 并打印提示/usage（不再静默假绿）。门禁不得静默移除（§8 已登记，Q-13）。

---

## 8. 迁移与删除清单

| 对象 | 处置 | 阶段 |
|---|---|---|
| `App::create_overlay/place_overlay/bring_overlay_to_front/close_overlay/overlays/dismiss_overlay` | 删除 | P1 |
| `interaction::{Overlay,OverlayOptions,OverlayPlacement,OverlaySlot,OverlayDismiss}` | 删除 | P1 |
| `app/overlay.rs`、`interaction/overlay.rs`、`overlay_style`、`note_overlay_container/overlay_containers` | 删除 | P1 |
| `kit::overlay::{Overlay,...}` 全部类型 | 删除 | P1 |
| `kit::layer::{Layer,Band,LayerSlot,Entry}` | 删除 | P1 |
| `kit::overlay::AnimationRouter` | 删除（当前 rg 显示仅 popup/宿主注入使用；若 P1 起点复核发现非 popup 使用，先记录再决定） | P1 |
| Scrim（`PopupTier::Scrim`） | 新增：引擎在 modal open 时按 `PopupSpec.scrim` 合成、与 modal 生命周期绑定并回收 | P1 |
| `ItemPanel`（`pub(crate)`） | 升级为 `PopupPanel` 的 menu 视图；`MenuMode::ListBox` 删除 | P1 |
| `crates/widget` `WidgetServices.layer/animation_router`、`Cx::layer()/animation()`、`binding` 注入 | 删除（**不新增宿主浮层 API**）：`WidgetServices` 只保留 `frame_router`，组件构造统一为 `new(app, theme, key, options)`，浮层由组件自持的 `PopupPanel` 直接调用引擎 `App::open_popup/...`。**原设计的 `PopupHost` 类型最终未实现**（终审 Q1-06 已核实，见 §8 完成情况） | P1 |
| `crates/kytrix` composition/dialogs 的 `Layer/AnimationRouter` 创建与注入 | 删除（不再有全局浮层服务；Dialog/Menu 等组件自持 `PopupPanel`） | P1 |
| 14 个 widgets crate 的 `cx.layer()/animation()` | 删除调用；**原设计的 `cx.popup_host()` 最终未实现、无此 API**（组件只收 `theme`，浮层由组件自身的 `PopupPanel` 管理） | P1 |
| `Dialog::recenter`、Dialog 自有 `closing/reason` | `Dialog` 自有 `closing/reason` 已删除（改用引擎 `state` + `on_close(reason)`，P1）；`recenter` 保留到 P3（D-P1-7） | P1 / P3 |
| `ColorPicker` 空 open + `build_panel` 二次构建 | 删除（内容闭包） | P1 |
| `Band::order()`、`MenuMode::ListBox` | 删除 | P1 |
| gallery 旧 `--dump-overlay` 节点树打印 | 已改 `--debug-stack`（`popup_stack` 快照）+ 像素断言（提前到 P1-12 完成） | P1-12 |
| gallery `--pixel-check`（P0-5/P0-6、P0-8） | 由旧 `Overlay`/`ColorPicker` API 驱动（`Overlay::revealed`/`panel_node`/`open`、首可见帧 rect 断言、headless `PersistentPixels`）；P1-11 删除这些 API 后必须重写为 `debug_stack` + 像素断言并保留命令名/退出码 | P1-12 |
| kit 首帧定位测试 `the_first_visible_frame_already_has_the_settled_rect` / `text_dominated_content_measures_before_the_first_visible_frame` / `open_reveals_and_paints_the_panel_without_a_host_sync` / `the_first_visible_panel_frame_matches_the_settled_rect` | 随 `Overlay` 删除一起迁移为 PopupPanel settle 断言（首可见帧 == 稳定帧、无宿主 sync 也能显示） | P1-12 |

**P1 完成情况（2026-11-27，详见 §12.2/§12.3）**

- 上表 P1/P1-12 行全部完成：旧 `overlay/layer/Band/LayerSlot/AnimationRouter/Overlay*/ItemPanel 旧形态` 已删除，`rg` 7 组组合模式零命中（§12.9）；`ColorPicker` 二次构建、`MenuMode::ListBox` 已删除；kit 首帧测试已迁为 `popup_behavior`/组件首可见帧断言。
- **仅 `Dialog::recenter` 一项未按原计划完成**：保留到 P3（D-P1-7）；`Dialog` 自有 `closing/reason` 已删除（P1 完成）。
- **原表两处规划中的替代 API 未采用（终审 Q1-06 核实）**：`PopupHost` 类型与 `cx.popup_host()` 均未实现；最终形态是「组件自持 `PopupPanel` + 引擎 `open_popup` 系列 API」，`WidgetServices` 只删字段、不新增服务。上表已按最终形态更正。
- **最终替代 API 对照（供迁移/审查）**：kit 浮层组件 → `PopupPanel` + `PopupSurface`；引擎查询/调试 → `App::popup_stack`/`App::popup_rect`/`App::popup_state`/`App::popup_diagnostics`；测试支持 `App::overlays` → `App::popup_stack`/`popup_rect`（`crates/*/test_support`）；旧 `Overlay::panel_node` → `PopupPanel::panel_node`/`popup_node`。
- gallery `--dump-overlay` → `--debug-stack` 与 `--pixel-check` 迁移**提前到 P1-12 完成**（原表标 P3 的“gallery 旧节点树打印”行随之完成）。

---

## 9. 风险与开放问题

### 9.1 风险

| # | 风险 | 影响 | 缓解 |
|---|---|---|---|
| R1 | damage 模型改动影响所有绘制路径 | 回归面大/性能 | P0 独立阶段 + 现有测试/bench；先加断言 |
| R2 | settle 与 text/virtual list/canvas text 的多 pass 交互 | 首帧抖动/性能 | P1-2 专项用例；2 轮上限；gallery 首帧断言 |
| R3 | Canvas 局部坐标迁移 132 处/41 文件（含 14 个 widget crate；设计原估的 45 文件数为子串口径误计，见 §5.3/§11.3-7） | 跨 crate 破坏性大 | P0 单独迁移 + inventory + golden 更新；`cargo check --all-targets` |
| R4 | 原子迁移面大（引擎 + kit + widget 宿主） | 中间态不可合并、工期长 | P1 按引擎 API+单测 → 组件 → 宿主 → 删除旧 API 顺序提交，同一阶段收口；gallery 保持可运行 |
| R5 | Slide paint_offset 需要 popup subtree 偏移支持 | 实现/性能风险 | 只对小型 popup 启用；对话框 fade-only；bench 守护 |
| R6 | scale 动画需 lane transform | 额外 2–3 天 + 渲染风险 | D10 采用 P3 追加；P1 先 fade+slide，lane transform 单独验收 |
| R7 | 三端差异（Wayland 输入、macOS a11y、Windows DPI/IME） | 验收风险 | P3 三端脚本 + 差异登记 |
| R8 | 评审 02 指出的 R1/M-D 代码矛盾 | 修错方向/浪费 | P0-0 判别实验先行；最小修法 |
| R9 | `painted_extent` 每节点持久内存/更新成本 | 内存/CPU | 实测是**两张侧表**：`baked_screen_bounds: Option<LogicalRect>`（20 B/节点，Canvas/Geometry 屏幕烘焙缓存）+ `painted_extents: Option<PaintedExtent>`（L 空间 `LogicalRect` + 1 物理 px pad，24 B/节点）≈ **44 B/节点**，40k 节点 ≈ 1.7 MB；`Node` 结构体大小未变。P3 bench 对照（§11.3-4）。 |

### 9.2 开放问题（全部已关闭）

| # | 问题 | 结论 |
|---|---|---|
| A | scale 动画 | **D10：P3 追加 lane transform（0.98→1）** |
| B | MessageBox | **D11：不新建，从计划移除**（当前项目无此组件） |
| C | Tooltip 交互内容 | **D12：纯提示**（Hint、穿透） |
| D | hover 是否夺焦 | **D13：不夺焦**；键盘事件才移入 DOM focus |
| E | 外部点击 replay | **D14：replay**（关闭整链后重派 press+release） |
| F | `AnimationRouter` 去留 | 删除（P1 起点 `rg` 复核；当前仅 popup/宿主注入使用） |
| G | 菜单项激活时机 | **D15：release 激活** |
| H | Dialog 初始焦点 | **D16：默认按钮优先** |

---

## 10. 验收标准（DoD）

> **P1 结束时状态（2026-11-27，详见 §12）**：DoD 1/2/4/5 在 P1 范围内达成；DoD 3 的“2–3 级子菜单”与 DoD 6 三端未达成（P2/P3）；DoD 7 的设计文档回写（§11/§12）与 `.docs/gui-migration/spec-deltas/kit-gallery.md` §13 同步均已完成。**P2/P3 完成后（§13/§14）：DoD 1–7 全部达成**；P4 剩余项见 §14.7。

1. **缺陷清零**：palette 残影消失；打开/移动/关闭/重开全程无旧像素；首可见帧 `hit_rect` == 稳定帧；残影可被门禁检测 —— P0 实现为 headless 持久增量像素门禁 `gallery --pixel-check`（旧浮层区域对打开前基线逐像素扫描 + 注入故障负向自检，exit 0/3），真实窗口 `overlay-check`/`--debug-stack` 仍只断言栈/rect；**P3-6 已用 `--real-pixel-check` 补上三端真实窗口像素验收（HEAD `35c9381d` clean 各 106 checks PASS，§14.4）**。**P1：`--pixel-check` 迁到新 popup API 仍 PASS（94 checks，§12.5），`--debug-stack` overlay-check 首可见帧==稳定帧 PASS。**
2. **架构**：旧 overlay/layer/Band/AnimationRouter API 全部删除；`rg` 零引用；popup 栈/placement/dismiss/focus/a11y 全不变量单测；无两套机制。**P1：已达成（§12.2 P1-11、§12.9）。**
3. **组件**：D9 全部组件达到 §4/§7；2–3 级子菜单真实窗口全路径可用。**P1：D9 组件全部迁移并 parity 可用；2–3 级级联仍只有 1 级 parity（D-P1-24），属 P2-1/P2-7。P2：2–3 级链与真实窗口路径已达成（§13.2）。**
4. **渲染不变量**：I1–I9 全部有测试；非模态 popup 移动/关闭/打开不触发 `damage_full`；移动/删除 damage 含绘制外溢；Canvas 全量迁移（132/41，§5.3）。**P1：I7/I8/I9 与 popup damage 局部性有 `popup/tests.rs`+`popup_behavior` 覆盖；模态/Scrim 打开关闭按设计走整帧重发（§12.3-17）。**
5. **门禁**：`cargo test`、`cargo +nightly clippy -- -D warnings`、`cargo +nightly fmt`、`cargo check --workspace --all-targets` 全绿；性能基线在预算内。**P1：`cargo test -p ui_core -p ui_kit` 3780 passed/clippy+fmt+workspace check PASS；workspace 测试与性能基线未跑（§12.6）。P3：**3882 passed / 2 ignored**（workspace 103 binaries：**9337 passed / 1 failed / 30 ignored**，唯一失败为 socket 资源耗尽环境 flake、单测重跑 PASS），性能实测 idle p95≈0.6–1 µs、open 中位≈29 µs（首帧冷启动 p95≈1.0–1.13 ms）、close 中位≈26 µs，`damage_full=false`（§14.5）。**
6. **三端**：Windows/macOS/Linux 真实窗口验收（截图/像素判定）通过并归档；**真实窗口 `damage_full` 为 `unavailable`（截图门禁不读该字段）**，`damage_full_frames=0` 由 headless `gallery --pixel-check` 覆盖断言。**P0/P1 只完成 Windows 本机 headless 像素门禁与真实窗口 rect/栈检查；P3-6 已完成三端 HEAD `35c9381d` clean 真实窗口截图验收（**各 106 checks/0 PASS**，pixel/idle/theme/debug-stack 均 PASS；identity 三端一致，§14.4）**。
7. **文档**：实施后在本文件 §11 回写偏差、最终 API、测试/性能基线；`.docs/gui-migration/spec-deltas/kit-gallery.md` 同步。**P1：设计文档 P0（§11）+ P1（§12）回写已完成；`kit-gallery.md` §13 已同步（`--debug-stack`/94 checks/采样下限/P1 完成状态，见该文 §13.5）。P2/P3：设计 §13/§14 与 kit-gallery §13/§14 已同步，`implementation/p2-summary.md`/`p3-summary.md` 产出。**。**

---

## 11. 实施结果与偏差登记（P0 回写，2026-11-26；P1 回写见 §12）

### 11.1 提交内容

- 分支/worktree：`dsh-1d175414-1a41`，基线 HEAD `71a5a5cc`；P0 全部改动在本 worktree 的未提交工作树中（`git diff` + 新文件 `crates/ui_core/src/paint/extent.rs`），由 lead 统一提交并附本条 §11。
- 范围：`crates/ui_core`（paint/instance/pass/canvas/primitive/extent、node/tree/*、window/paint、render/{damage,compose,delta}、test_support/{pixels,record,snapshot,diagnostics}、removal_tests、canvas_text、error.rs、app/frame 相关测试）、`crates/ui_kit`（kit/{overlay,color_picker,spinner}、examples/gallery.rs）、`crates/widget`（test_support）、`crates/widgets`（17 个 canvas 迁移文件 + 6 个指针换算文件及其测试）。
- 新增公开错误面：`ui_core::error::EngineError::PixelHarness { cause: PixelHarnessCause }` 与 `PixelHarnessCause`（`NoCommittedFrame` / `SurfaceResized` / `NotInlineWindow` / `UnrepresentableSurface`），用于 `PersistentPixels`/`present_record` 的强类型失败（Q-05），替代字符串错误。
- 实施任务：P0-0、P0-1、P0-2（防御性测试 + R-04 生产修复，见 §11.3-21）、P0-3（并入 P0-7）、P0-4a/P0-4b、P0-5、P0-6、P0-7、P0-8。

### 11.2 P0 任务结果

| 任务 | 结果 | 关键证据 |
|---|---|---|
| P0-0 | 完成：M-D/M-E refuted，M-C 收窄为「打开路径在错误位置画一次、旧位置未进 damage」，M-A confirmed | `evidence/p0-diagnosis.md` |
| P0-1 | `PaintedExtent` 持久记录 + damage 覆盖 + remove 子树并集 + hide/空 lane/geometry 清空；侧表 44 B/节点 | `paint/extent.rs`、`pass.rs`、`arena.rs`；P0 新增测试：`removal_tests` +11 条、`pass.rs` +19 条、`test_support/pixels.rs` +7 条 |
| P0-2 | 按 P0-0 收窄：lane slot 释放/顺序变化/stale generation 不变量测试（M-D refuted；C-06 落点）；**同时含 R-04 生产修复**——`LaneStore::apply` 对 generation 不匹配的 removal note 直接跳过、不再误删被复用 slot 上的存活 lane，`note_removed` 清 `versions[slot]` 前校验 generation（详见 §11.3-21） | `render/delta.rs`、`render/compose.rs` 测试（`a_removal_note_with_a_stale_generation_keeps_the_live_lane`、`a_released_lane_is_not_resurrected_by_the_covered_delta`、`absorb_drops_an_old_generation_lane_when_the_slot_is_reused`） |
| P0-3 | 并入 P0-7（scroll slot 一致性作为结构增量前置；M-E refuted） | `window/paint.rs`、`removal_tests`、`pixels.rs` |
| P0-4a | `CanvasDraw { canvas, offset }`；encoder 递归平移 quad/shadow/image/glyph/clip/path/geometry/嵌套 canvas；`Canvas` 节点 damage 用屏幕外延；`baked_screen_bounds` 侧表使 screen-baked lane 在位置/滚动变化时重建 | `paint/{canvas,primitive,instance,pass}.rs`；`pass.rs` 新增 19 条测试（P0 新增总数，含 P0-4a/P0-7/R-09/R2-01） |
| P0-4b | `crates/widget` + `crates/widgets` 17 文件迁移为局部坐标；6 处指针换算改为 `NodeTree::hit_test` 的 `local_point`（R-08） | `cargo check --workspace --all-targets`；六个 crate 补跑 1216 passed（§11.5） |
| P0-5 | `PersistentPixels`（同 persist 纹理跨帧、damage scissor、readback、逐像素比较）+ P0 新增 7 条像素测试（含漏 damage 反例自检、移动、开关重开、阴影+Canvas 子树增删、滚动重放） | `test_support/pixels.rs` |
| P0-6 | ColorPicker 面板/内容根 `overflow_hidden`；gallery `--pixel-check`（108 条 record 断言 = 45 采样帧逐帧 + 20 空闲帧 + 20 background-residue + 3 restored + 2 close-frames + 18 条具名不变量；含旧浮层区域基线扫描 + 覆盖量下限 + ≥2 分散像素注入自检 + 首可见帧 rect） | `color_picker.rs`、`gallery.rs` |
| P0-7 | 结构变化增量绘制：收窄 `!removed.is_empty()`/structure epoch/无 lane·clip_key 三条全量路径；order 增量（`rebuild_order_only`/`scene.set_order`）；滚动容器注册/移除才安全降级全量；新增结构增量测试；virtual-list+popup 开关+`ScrollTable` 用结构/damage/scroll 表断言锁定（无 stale slot、`delta.scroll` 仅真实滚动帧为 1、全程 `!damage_full`，C-07） | `window/paint.rs`、`pass.rs`、`removal_tests` 5 条 |
| P0-8 | 旧 kit `Overlay` 首帧：`open` 内 measure（`refresh_text`+`compute_layout`）→ place → reveal；`measure`/`place` 共用定位内核（Q-15）；`ColorPicker::open -> Result<bool, OverlayError>` 且 measure 失败回滚（Q-04）；新增文本主导高度/无 sync reveal 首帧测试 | `kit/overlay.rs`、`kit/color_picker.rs` |

### 11.3 设计偏差登记

1. **P0-3 移除**：P0-0 判定 M-E（scroll slot 不匹配）refuted，P0-3 不再单独实施；其“node→scroll slot 表在新增/删除/顺序变化时一致”的要求并入 P0-7 作为结构增量前置（§6）。
2. **P0-7 追加（用户裁决 A）**：C8/I6 要求非模态 popup 开关/移动不 `damage_full`，而原引擎在新增/删除节点时强制全量（`removed`/structure epoch/无 lane 三条路径）。用户裁决在 P0 追加 P0-7，并把 P0 验收 ④ 改为“非模态 popup/面板打开、关闭、重开 `damage_full==false`”。结构帧 order 重建仍为 O(n) DFS（与原 full paint 同阶），滚动容器注册/移除会安全降级一次全量绘制（damage 仍局部）。
3. **P0-8 追加（用户裁决 A）**：旧 kit `Overlay` 首帧定位（R3/M-A）在 P0 内最小修复，属 **P1-11 会整体删除的过渡 API**；不改 popup 架构，`Overlay`/`Layer`/`Band` 仍按 §8 在 P1 删除，P1-12 必须迁移依赖它的 `--pixel-check` 与 首帧相关测试（overlay 5 条 + ColorPicker 1 条）（§8、Q-13）。另：`Overlay::open` 语义变为“open 内完成 measure+place+reveal”，不再要求宿主逐帧 `sync()` 才显示；`place()` 不再承担首次 reveal（R-05）。
4. **`painted_extent` 空间与内存（C-02/Q-01 最终口径）**：实现为**绝对布局空间 L** 的 `LogicalRect` + 固定 1 物理 px pad（设计原写 `DeviceRect + PhysicalPx`）。`arena::remove()` 已按「被删子树每节点相对直接父节点发射空间的滚动差」**逐节点**换算后并入父 damage——覆盖“直接父自身 scroll offset”与“子树内嵌套滚动容器 offset”；offset≠0、嵌套滚动、像素回归均有测试（`removal_tests::removing_a_scrolled_container_child_damages_its_screen_position` 等）。`place_subtree` 的移动 damage 记在被移动节点自身并用其自身 total 发射，已验证正确。**未覆盖**：跨滚动上下文重挂（`append_child` 在 A/B 两滚动容器之间搬子树）时旧 damage 会用新上下文 total 发射、差新旧 total；当前生产调用方都不在滚动容器内（P0 用户不可见），P1 列表复用/reparent 前必须给 `append_child` 补同样的滚动差换算。内存：两张侧表 `baked_screen_bounds`（20 B）+ `painted_extents`（24 B）≈ 44 B/节点，40k 节点 ≈ 1.7 MB；`Node` 结构体大小未变（R9 已更新）。
5. **damage 阈值口径**：全帧判定改为按 frame 裁剪后的**可见面积**计算（窗外面积不计入）；damage 矩形本身不变，`plan()` 的裁剪语义与阈值判据同源，不会错误降级（`render/damage.rs`）。
6. **Canvas 局部原点口径**：统一为**节点内容盒左上**（border box 左上 + border/padding，即 `bounds.origin + content_offset`）；当前全仓没有带 padding/border 的 Canvas 节点，`content_offset = 0`，与设计“节点左上”在现状等价，P1 引入 padding 时必须遵守此口径。
7. **Canvas 计数修正（第 3 轮实测）**：设计 rev.6 的 45 文件数为子串口径误计（含 `ChartCanvas::new` 等非 `Canvas` 构造点；文件数实为 41）。实际 `Canvas::new(` 构造点 **132/41**（统计口径：`crates/**/*.rs` 上匹配 `(?<![A-Za-z_])Canvas::new\(`；`ui_core` 93/24、`ui_kit` 9/2、`widget` 5/2、`widgets` 25/13；其中 P0 新增测试 +8：首轮 124/41 + 后续修复轮 5 个（其中 R2-01 再 +3 个））。语义迁移 132/132（含 C-04 的 `test_support/diagnostics.rs`）。
8. **真实窗口像素替代方案（C-01/验收②，用户裁决）**：P0 的真实窗口不接受像素 readback/截图门禁；残影门禁由 headless `gallery --pixel-check`（`TestApp` + 离屏 `GpuRenderer` + `PersistentPixels`）承担，真实窗口 `--dump-overlay` 仅断言 rect/children。P3-6 补三端真实窗口像素判定（readback 或截图 + 像素比较；Windows 可行、macOS/Linux 待定），并把 headless 与真实窗口断言合并为同一退出码。验收③：本轮按用户要求只跑 `ui_core`/`ui_kit` 门禁；`widgets`/picker 相关测试以 P0 审查独立复跑 `golden_*` 8 passed 与 P0-4b 自证 9 crate 1639 passed 作为证据，未在本轮门禁内复跑（§11.5/§11.6）。
9. **`--pixel-check` 语义、退出码与迁移登记**：`cargo run -p ui_kit --example gallery -- --pixel-check` 是 **headless 持久增量像素门禁**；exit 0 = PASS，exit 3 = FAIL。残影判定 = ①每帧 `!damage_full`；②变化像素 ⊆ damage；③打开后/popup 外背景对打开前基线逐像素比较（排除当前 popup/trigger 外扩 16 px）；④移动后对“旧浮层区域 − 新浮层 − 新旧 trigger”全区域扫描基线（含边缘与内部，不再只查角点），**并有覆盖量下限**：`compared ≥ fixture 设备像素 × 30‰`（实测本机 26104/480000 ≈ 5.44%，下限留 1.8× 余量），防止排除区膨胀把检查吞成假绿；⑤**负向自检**在旧浮层区域注入 **≥2 个分散像素**（比较集首/尾各一个）后必须检测到 `mismatched ≥ 2`；⑥首可见帧 rect == 稳定帧；⑦关闭/重开恢复基线。idle 采样为 137 ms × 20（与常见动画周期互质，降低采样混叠；先显式断言“无动画源”，避免把动画误判为残影）；容差 span ≤ 2。`checks` 计数 = `record()` 调用数（无手工增量）：**108 条 = 45 条逐帧采样断言（44 changed-outside-damage + 1 full-replay 首帧）（采样帧）+ 20 idle-frames + 20 background-residue + 3 restored + 2 close-frames + 18 条具名不变量**（baseline-opaque、open/open-settle、first-visible-rect、idle-no-animation、move-closed(-return)-settle、move-settle、old-region、old-region-coverage、residue-detector、move-panel、close、close-restore-settle、reopen(-settle/-rect/-close)）。该门禁绑定 `kit::overlay`/`ColorPicker` API（`Overlay::revealed/panel_node/open`）→ **P1-12 必须重写并保留命令名/退出码**（§8）。
10. **`Overlay::measure` 的文本覆盖边界（C2-03 如实口径）**：measure 运行 `App::refresh_text` + `Window::compute_layout`，覆盖**静态文本**（菜单/提示/对话框文案）的高度；`bind_text` 内容在 style pass 才求值（引擎未公开 measure 入口），此类内容首帧仍可能位移，由 P1-2 settle 统一解决。调用次数：`Overlay::open` 先对空内容 measure（1 次 `refresh_text` + 1 次 `compute_layout`）；ColorPicker `open_panel` 构建内容后再 measure 一次（第 2 次 `refresh_text` + 第 2 次 `compute_layout`），即 **ColorPicker 路径每次 open 共 2 次 text pass + 2 次 layout**，第二次 `refresh_text` 通常为空操作（文本未变）；非逐帧成本。
11. **`ColorPicker::open`/`open_panel` 强类型错误**：返回 `Result<bool, OverlayError>`；panel 内容构建失败或 measure 失败都会关闭回滚并返回错误，不再静默丢弃（`kit/color_picker.rs`）。
12. **滚动容器内 widget 指针换算（R-08）**：6 处 P0-4b 迁移点（candlestick `chart_position`/`chart_contains`、cryptocurrency_etf/perpetual_statistics `point_at`、volume_heatmap `Layout.origin`+`hit_test`、prediction_market `sync`、global_shipping_chokepoint `bounds`/`attribution_contains`/`vessel_at`/`zoom_at`）原先用 `layout().bounds.origin`（L 空间）换算，在滚动祖先下整体偏移 scroll offset。修法：统一改走既有 public API `NodeTree::hit_test(point) -> HitResult { node, local_point }`（内部按祖先滚动平移，`widgets/x` 已用）；不新增引擎 API。各 crate 新增“滚动容器内 hover/zoom/pan”测试（candlestick、cryptocurrency_etf、perpetual_statistics、volume_heatmap、prediction_market、global_shipping_chokepoint）。补跑时发现的是**测试装置**问题而非生产换算：滚动容器内页面默认 `flex-shrink: 1` 会被压缩到视口大小，scroll offset 失去可换算空间（volume_heatmap 首跑 `hovered()==None`）；已给各测试页加 `flex_none()`，并按多热图块 fixture 收紧断言（不得放宽）。六个 crate 已补跑 PASS（§11.5）。
13. **其它登记**：结构帧 order 重建 O(n) DFS；gallery 像素对比容差 span ≤ 2、排除 popup/trigger 各外扩 16 px；旧 `--dump-overlay` 语义保留；`App::refresh_text`/`Window::compute_layout`、`NodeTree::hit_test` 为既有公开面，未新增配置层/开关；`PaintedExtent` 仍不计 lane 级 clip（保守外扩），但 Canvas **内部** `ClipPush` 已在 `canvas_content_extent` 中按 clip 栈求交（R2-01）：画布内显式 clip 会收紧 extent，未裁剪的越界内容仍按实际并集覆盖（C-10）。
14. **I6 的 Geometry 边界（R-06）**：独立 `NodeKind::Geometry` 节点的任何结构变化（新增/布局移动/删除）都会升格为 `damage_full`——这是 P0 前既有的 `geometry_damage_escalation` 兜底（Geometry 块 AABB 走 transform 坐标、节点盒走 L 坐标，目前未统一）。因此 I6「非模态 popup 开关不 `damage_full`」**只对内容闭包不含独立 Geometry 节点的 popup 成立**（ColorPicker 满足）；P1 的内容闭包应禁止独立 Geometry，或先统一其 damage 坐标语义。
15. **C-06 落点**：order-only 不变量测试实际在 `render/compose.rs`（`releasing_a_lane_rebuilds_the_ops_without_the_freed_range`、`reordering_lanes_without_removal_rebuilds_the_ops`、`a_removal_note_with_a_stale_generation_keeps_the_live_lane`），而非设计 I2 原提的 `paint/instance.rs`/`render/delta.rs`；接受该落点（同一 `LaneStore`/`WindowComposer` 的 ops 重建路径），本阶段不搬家。
16. **C-07 的验证手段**：P0-3/P0-7 的 virtual-list + popup 开关 + `ScrollTable` 一致性目前用**结构/damage/scroll 表断言**锁定（无 stale slot、`delta.scroll` 仅真实滚动帧为 1、全程 `!damage_full`），**未接持久像素**；ui_core 已有等价的像素门禁（`test_support/pixels.rs` 的滚动重放用例）覆盖同一路径。P1 的 Select/Dropdown 列表回收应把“滚动 + 增删 + popup 开关”三联用例接到 `PersistentPixels`。
17. **C-10 取舍（Canvas 内容外延）**：选择「emit 时递归并集 Canvas **实际图元**外延」而非「声明 `Canvas::bounds()` 契约 + debug 断言」：`canvas_content_extent` 递归累计 quad/glyph/image/path、shadow 的物理 blur pad、Geometry 经 `draw.transform` 的 AABB、嵌套 canvas 的 `offset`，再 `translate(origin) ∪ 节点盒`，同一个并集同时用于 `painted_extent` 与 damage。成本：Canvas lane 重建时多一次 O(图元数) 矩形并集（lane 复用不执行）；内容外延不再受 `Canvas::bounds()` 声明限制（C-10 由此关闭，契约按实际并集语义写进 §4.11）。R2-01 起另加两道收紧：①画布内 `ClipPush/Pop` 栈逐层求交；②Geometry 只并集 `draw.range` 命中的 chunk AABB（新增 `geometry_draw_extent`，经 `chunk_draw_range` 与 encoder `push_part` 同构；空 `meta.bounds` 才回退 `snapshot.bounds()`），使 map 等「大 Mercator AABB + clip + zoom」的 lane 重建不再退化为 `damage_full`。
18. **Q-07 断言范围**：持久像素测试的“基线完全不透明”断言排除最外 1 行/列（TestApp/渲染器既存的 3 个 `alpha=218` 边缘伪影），内容区要求完全不透明；gallery 的 `baseline-opaque` 检查是独立实现（自己的窗口/背景，不引用该排除）。
19. **C-08/§11 的验收命令**：见 §7.5 与 §11.5 的可复制命令块；本轮实际执行记录见 §11.5 表。
20. **`FrameRecord.primitives` 空间契约（R-09，C2-04 收窄后的如实口径）**：**混合空间，不能笼统称为“统一 lane/绘制空间”**。`FrameRecord.primitives[]` 中 Quad/GlyphRun/Image/Path/Shadow/ClipPush/Canvas 按 lane/绘制空间记录（Canvas 内部图元按 `CanvasDraw.offset` 累积平移，嵌套 Canvas 继续累加，含 Path）；**`Geometry`、`ClipPop`、`Text` 三类仍回退为节点 `layout().bounds`（L 空间）**——在滚动容器/滚动窗口下，这三类与上述 lane 空间图元混用两种坐标，消费方必须按 `PrimitiveKind` 区分（P1 统一前不得依赖单一口径；该回退是既有行为，P0 未改）。`FrameRecord.canvases` 与 `FrameRecord.quads` 保持「节点自有不透明条目」语义（`CanvasRecord.canvas` 是节点局部坐标画布，`QuadRecord` 只含节点顶层 quad）。
21. **P0-2 含 R-04 生产修复（C2-01 更正）**：§11.2 P0-2 原写“无生产改动”不准确。`render/delta.rs` 实际有两处 R-04 生产修复：① `LaneStore::apply` 的 removal 循环先校验 `stored.generation == removed.generation()`，不匹配则 `continue`，不再把被复用 slot 上的当前存活 lane 误删（`delta.rs:394-408`）；② `SceneDeltaBuilder::note_removed` 清 `versions[slot]` 前校验该 slot 的 generation，避免旧 lane 的 removal 清掉新 lane 的版本记录（`delta.rs:339-345`）。对应测试：`render/compose.rs::a_removal_note_with_a_stale_generation_keeps_the_live_lane`（旧 removal note 不得让存活 lane 的 ops 消失）、`render/delta.rs` 的吸收/释放用例 `a_released_lane_is_not_resurrected_by_the_covered_delta`、`absorb_drops_an_old_generation_lane_when_the_slot_is_reused`、`applying_the_same_delta_twice_is_idempotent`。
22. **R2-01（Canvas extent 的 clip/range 收紧）**：见 §11.3-17 的补充（画布内 clip 栈求交 + Geometry 只取 `draw.range` 命中块）。新增测试 `a_canvas_clip_tightens_a_huge_geometry_extent_to_the_canvas_box`、`a_zoomed_map_style_canvas_redraw_stays_local`、`a_geometry_draw_range_narrows_the_canvas_content_extent`（round-2 实现下必红）。

### 11.4 最终 API（P0 相关）

- `ui_core::paint::primitive::{CanvasDraw, Primitive::Canvas}`：`CanvasDraw { canvas: Arc<Canvas>, offset: LogicalPoint }`（`Deref<Target = Canvas>`），嵌套 canvas 的 offset 累积；`Canvas` 内部坐标为节点内容盒局部坐标。
- `ui_core::paint::extent::PaintedExtent`（crate 内私有）：`painted_extent` 侧表 + damage 推送。
- `ui_core::test_support::pixels::PersistentPixels`（feature `test-support`）：`new/present/present_record/readback`；`FrameRecord.damage/damage_full`；`Window::scroll_slots` 的 cfg 放宽到 `test-support`。
- `ui_core::error::{EngineError, PixelHarnessCause}`：`EngineError::PixelHarness { cause }` 携带 `PixelHarnessCause::{NoCommittedFrame, SurfaceResized, NotInlineWindow, UnrepresentableSurface}`（Q-05），由 `PersistentPixels`/`present_record` 返回；`Display` 为每个 cause 提供可读原因。
- `Window::compute_layout`、`App::refresh_text`、`NodeTree::hit_test`：既有 public API，P0 起被 ui_kit measure 与 widgets 指针换算使用（无签名变更）。
- `ui_kit::kit::color_picker::ColorPicker::{open, open_panel}` → `Result<bool, OverlayError>`；`Overlay::{measure,revealed}` 为 P0-8 临时面（P1-11 删除）。
- gallery `--pixel-check`（P0 像素门禁命令；P1-12 迁移）。

### 11.5 测试与门禁基线（本机 Windows 11 / D3D12 WARP / 125% DPI，2026-11-26）

> **历史基线说明（P1 追加注记，不改下表 P0 数据）**：本表为 P0 当时实测记录。P1-12 已把 `--dump-overlay` 更名为 `--debug-stack`（§12.3-8/§12.5），`--pixel-check` 迁移后为 **94 checks**（构成与采样下限口径见 §12.3-9/§12.5，非本表 108 条）；现行可复跑命令以 §7.5/§12.5 为准。

| 门禁 | 结果 |
|---|---|
| `cargo test -p ui_core -p ui_kit` | **3554 passed，0 failed，2 ignored**（ui_core lib 2720 passed/1 ignored、frame_record 8、layout 4、snapshot 14、demo 39；ui_kit lib 769；P0 新增测试含 overlay 首帧/文本 measure、ColorPicker 错误路径、pixels 7 条、pass 19 条、removal 11 条等） |
| `cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings` | PASS |
| `cargo +nightly fmt --all -- --check` | PASS |
| `cargo check --workspace --all-targets` | PASS（含 widgets 迁移与 R-08 指针换算；widgets 测试仅编译未执行） |
| `cargo run -p ui_kit --example gallery -- --pixel-check` | **PASS，108 条 record 断言（45 条逐帧采样断言（44 changed-outside-damage + 1 full-replay 首帧） + 20 idle-frames + 20 background-residue + 3 restored + 2 close-frames + 18 具名不变量），sampled_frames=45，damage_full_frames=0**；覆盖下限 30‰（实测 compared 26104 / fixture 480000 设备像素 ≈5.44%）、2 点注入自检通过；first_visible == stable == (496,91,300×375)，moved=(496,4,300×375)；退出码 0 |
| `cargo run -p ui_kit --example gallery -- --dump-overlay --auto-close=9`（真实窗口） | PASS（overlay-check 旧语义：rect/children；不含像素 readback） |
| 审核证据（C-01） | P0 审查（conformance）独立复跑 `golden_*` **8 passed**；P0-4b 自证 9 个 widgets crate **1639 passed**（非本轮门禁，未复跑）；`--pixel-check` 首帧=稳定帧（496,91,300×375）两轮一致 |
| R-08 六 crate 补跑（lead 裁决 A） | candlestick 351、cryptocurrency_etf 137、perpetual_statistics 164、volume_heatmap 117、prediction_market 267、global_shipping_chokepoint 180，合计 **1216 passed / 0 failed** |

复跑命令（Windows PowerShell，可复制）：

```powershell
$env:CARGO_TARGET_DIR='E:\kytrix\target'
cargo test -p ui_core -p ui_kit
cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings
cargo +nightly fmt --all -- --check
cargo check --workspace --all-targets
cargo run -p ui_kit --example gallery -- --pixel-check   # exit 0=PASS / 3=FAIL
cargo run -p ui_kit --example gallery -- --dump-overlay --auto-close=9
```

### 11.6 未执行项与原因（P3 衔接）

1. **workspace `cargo test` 未跑**：用户本轮限制只跑 `cargo test -p ui_core -p ui_kit`；`crates/widgets`、`crates/widget`、`crates/kytrix` 等测试未执行（AGENTS §8 的 workspace 测试需在下一轮补）。
2. **R-08 六 crate 已补跑 PASS**：`cargo test -p candlestick -p cryptocurrency_etf -p perpetual_statistics -p volume_heatmap -p prediction_market -p global_shipping_chokepoint` → **1216 passed / 0 failed**（candlestick 351、cryptocurrency_etf 137、perpetual_statistics 164、volume_heatmap 117、prediction_market 267、global_shipping_chokepoint 180），含 6 条新增滚动容器用例。其余 widgets crate 仍未跑（用户限制本轮只跑 ui_core/ui_kit；上述六 crate 由 lead 授权补跑）。
3. **真实窗口像素判定与三端**：P3-6；计划待定（readback vs 截图 + 像素比较；macOS/Linux 后端能力确认后写入 §7.4）。
4. **性能 bench**：P3-5；P0 只记录 `--pixel-check` 约 3.6 s/45 采样帧（WARP）与 damage 局部性事实（`damage_full_frames=0`）。
5. **提交**：本 worktree 未 `git add/commit/stash`；提交说明与 §11 由 lead 统一完成。

### 11.7 P1 起点必须衔接的事项

- 删除 `kit::overlay`/`kit::layer` 及其引擎 API 前，先按 §8 迁移 `--pixel-check` 与首帧相关测试（overlay 5 条 + ColorPicker 1 条）（否则 P1-11 编译失败或像素门禁静默丢失）。
- `PaintedExtent`/`damage` 的滚动容器删除修复（R-02/C-02）与 widgets 指针换算（R-08）直接服务 P1 的 Select/Dropdown/virtual list 行回收与弹层内容，P1 需保留对应回归测试。
- P1-2 settle 需接管 `Overlay::measure` 的文本覆盖边界（`bind_text` 内容、virtual list/scroll clamp），并删除 P0-8 的临时 measure。

---

## 12. 实施结果与偏差登记（P1 回写，2026-11-27）

> 本节是 P1 阶段的回写，编号独立于 §11（P0 回写）。§4.2/§4.4/§4.13/§5.1/§5.2/§8/§10 中与实现不一致处以本节与正文内“P1 落地口径（rev.7/rev.8）”注记为准。**2026-11-27 终审修复轮（task-43/44/45 + round-3 task-48/49/50）后，§12.1/§12.2/§12.5/§12.6 的数字与状态已按最终工作树重测；第 1 轮终审处置见 §12.10，第 2 轮 r2/round-3 处置见 §12.11。**

### 12.1 提交内容与范围

- 基线：P0 已提交 `d9949aca`（worktree `dsh-1d175414-1a41`）；P1 全部改动在同一 worktree 的未提交工作树，由 lead 统一提交。
- 规模（`git diff --shortstat HEAD -- . ':(exclude).docs'`，**口径 = 总行数含空行**，与 `git diff --stat` 一致）：**115 tracked 文件，+8671 / −9410**；删除 4 个文件共 **4350 行**（`kit/overlay.rs` 2871、`kit/layer.rs` 964、`ui_core/app/overlay.rs` 184、`ui_core/interaction/overlay.rs` 331）。注：`.docs/gui-migration/spec-deltas/kit-gallery.md` 是 git **跟踪**文件（不受 `.docs` 忽略规则约束），其 §13 同步改动若计入则全量 tracked diff 为 **116 文件 +8692/−9415**。
- 新增文件（**7 条 untracked 路径 / 展开 14 个文件**，共 **15334 行**，含测试）：`ui_core/src/popup.rs` 633、`ui_core/src/popup/{anchor 73, animation 187, dismiss 582, focus 290, placement 1109, settle 408, stack 775, tests 1626}`、`ui_core/src/app/popup.rs` 878、`ui_core/src/a11y/popup.rs` 349、`ui_core/src/a11y/popup_tests.rs` 1719、`ui_core/tests/popup_behavior.rs` 5277、`ui_kit/src/kit/popup_panel.rs` 1428。
- 范围：`crates/ui_core`（popup 子系统与 settle/dismiss/focus/a11y/animation；`app/frame.rs`、`app/input.rs`、`app/focus.rs`、`app/window_set.rs`、`window.rs`、`window/paint.rs`、`node/tree/*`、`layout/hit_test.rs`、`paint/pass.rs`、`a11y/*`、`test_support/*`、`tests/*`）、`crates/ui_kit`（`kit/popup_panel.rs` + 全部组件 + `examples/gallery.rs`）、`crates/widget`（`WidgetServices`/`WidgetCx` 去掉 `layer/animation_router`）、`crates/kytrix`（composition/dialogs）、14 个 widgets crate 与 5 个业务 ui crate 宿主、测试支持（`App::overlays`/`overlay_wrapper` 移除）。
- 结论：**旧 overlay/layer 机制与旧引擎 overlay API 已全部删除，浮层只剩「引擎 popup 栈 + kit `PopupPanel`」一套机制**；inventory §5.3 的 7 组组合模式旧 API 零命中（`dump-overlay` 仅剩 gallery 的改名提示，见 §12.9）。

### 12.2 P1 任务结果（P1-0…P1-12）

| 任务 | 结果 | 关键证据 |
|---|---|---|
| P1-0 | **0×0 绝对 popup root 方案可行并采用**，未回退到 viewport-size layer：popup 内容根 auto-size、其绝对子节点按窗口坐标定位、打开 popup 不改变主树布局、root 自身不参与命中/焦点/a11y（`HitFocusSkip::Container`） | `popup/tests.rs::{popup_content_auto_sizes_to_its_children, absolute_popup_children_are_positioned_in_window_coordinates, opening_a_popup_does_not_change_the_main_tree_layout, the_popup_root_never_wins_the_hit_test, tab_focus_skips_the_popup_root_container}` |
| P1-1 | `popup` 模块落地：`PopupId`（window/slot/generation 防 ABA）、`PopupStack`（entries 墓碑 + 槽复用 + `order` 按 z + 每父 ≤1 子 + `scrim_for`）、`PopupEntry/Spec/Tier/DismissPolicy/FocusPolicy/Animation/A11y/PopupRect`；root 子节点固定 `[...内容, popup_root, debug_hud]`；`NodeTree::reorder_children` 全排列重排且**不置 structure epoch**（只标 order/paint + damage 子树 painted_extent） | `popup/tests.rs` 43 条；`node/tree.rs::reorder_children`、`tests/hud.rs` |
| P1-2 | settle 接入真实帧序：`app/frame.rs` 在 canvas-text eviction 之后、`ensure_visible/blink/IME` 之前；≤2 轮（第 1 轮 max-size + 重跑 layout/text/virtual/clamp/canvas-text，第 2 轮求 origin 并一次性 apply inset）、按深度浅→深用父 `solved delta` 修正子锚点、dirty+viewport+animating 闸门、LayoutReport/TextLayoutReport 合并进 FrameStats、`Unsettled` 诊断下一帧再 settle | `popup/settle.rs`；`popup_behavior::{opening_a_popup_settles_geometry_in_the_first_frame, content_growth_settles_in_one_frame_without_a_second_displacement, three_menu_levels_follow_the_parent_without_a_frame_lag, idle_frames_do_no_popup_settle_work, popup_settle_rounds_never_exceed_two}` |
| P1-3 | `solve_placement` 纯函数：候选序列去重、avoid 严格相交、flip bestFit、shift/limitShift、max-size 收缩 + `resized`、margin clamp、非法几何强类型错误；**34 条单测**（终审删除 `match_anchor_width` 相关 2 条）；子菜单 `Right→Left` 由引擎 `effective_placement` 强制（调用方传 Top/Bottom 也会被改写为 Right，fallbacks 只留 Left/Right） | `popup/placement.rs`；`popup_behavior::{a_submenu_ignores_vertical_fallbacks_and_opens_horizontally, submenu_placement_flips_right_to_left_with_exact_geometry, a_node_anchor_*}` |
| P1-4 | 运行时 `NodeTree::set_max_size` + `Window::set_max_size`（后者同步失效 popup 的 max-size 缓存，避免下一帧被 settle 覆盖）；`PopupPanel` 消费引擎 `constrained_size`：`available_body_height` 收敛 Menu 列表/Dialog 内容区高度并内部滚动（footer 固定），不只靠 max-height 裁剪。**P1 终审修复 E-04/Q1-01 补齐 kit 消费面**（原回写声称已实现但 kit 无消费者，终审后为真）；**round-3 补横向（E-20）**：Dialog 面板宽 clamp 到视口−2*margin，内容/滚动区同步（320×220 下 confirm 可达） | `node/tree.rs`、`window.rs`、`popup/settle.rs`、`kit/popup_panel.rs::available_body_height`、`kit/{menu,dialog}.rs`；`popup_behavior::{an_oversized_popup_is_clamped_to_the_viewport_margin, a_constrained_popup_exposes_its_applied_size_without_unsettled_churn, external_max_size_writes_are_reapplied_by_the_next_settle}`、`kit::menu::a_small_viewport_shrinks_the_menu_and_scrolls_its_last_row_into_view`、`kit::dialog::a_tall_dialog_scrolls_its_content_and_keeps_the_footer_reachable` |
| P1-5 | dismiss 矩阵：modal scope 优先、Auto 链 outside 优先、opener Toggle、命中祖先只关后代、Esc 跳过 Closing 且 Hint 优先、默认 `OutsidePress::Replay`（D14，`Consume` 显式 opt-in）、Closing 中再点 opener `cancel_close` 回 Opening、`ParentClosed/FocusLost` 事件真正派发、窗口失焦只关 Auto/Hint（保留 Modal/Scrim）、`anchor_lost` 可按策略关整链、**`DismissPending/Closing` 的 Modal（含 Scrim）继续阻断背景输入（终审 E-01 修复）** | `popup/dismiss.rs`、`popup/stack.rs::topmost_modal`（`is_visible`=非 Closed）；`popup_behavior` 中 20 余条 dismiss/窗口生命周期用例 + `a_closing_modal_swallows_background_presses_until_it_is_removed`、`a_closing_modal_keeps_background_hover_and_scroll_blocked` |
| P1-6 | 焦点单一真值：`effective_focus` 由 popup 栈决定（Trap/Take 的 owner），paint/命中/a11y/cursor/IME/text_edit 同源；`RestoreFocus::{Opener, Previous}` 两分支（P3 删除 RootFallback，opener 失效回退由引擎内部处理） + opener 失效 fallback 最近可聚焦祖先→root；关闭只恢复一次且不抢新 popup；Trap 内 Tab 循环；IME composition 开/关两侧与 `sync_ime_cursor_area` | `popup/focus.rs`；`popup_behavior::{opening_a_take_popup_moves_focus_and_closing_restores_it_once, dialog_tab_cycles_inside_the_modal_scope, restore_*, closing_a_popup_does_not_steal_the_newer_popup_focus, opening_a_popup_from_an_editable_cancels_ime_composition, escape_during_ime_composition_cancels_it_and_keeps_the_popup}` |
| P1-7 | a11y 链嵌套：物理树平铺、按逻辑父重组（`a11y_parent` → 父 popup 内容根 → opener；modal 无父挂窗口 root；**round-3 起 kit 子菜单写 `a11y_parent=父 menuitem`，A-29 已闭**）；`has_popup/controls/owns/described_by/active_descendant/expanded` 接线（**round-3 起 kit 静态声明 haspopup，A-25 已闭**）；角色 Menu/MenuItem/CheckBox/Radio/Separator(→Splitter)/Dialog/AlertDialog/Tooltip/Group；scrim hidden、背景 hidden、关闭开始同帧清 expanded；AT dismiss 映射（`CustomAction(-1)`/`HideTooltip`/`Dismiss` 定位到目标节点所属 popup） | `a11y/popup.rs`、`a11y/tree.rs`、`a11y/adapter.rs`；`a11y/popup_tests.rs` 21 条 |
| P1-8 | 动画：`Fade`/`FadeSlide`，`paint_offset`（+ 节点侧 `PopupTransform{offset,opacity,blocked}`）不参与布局；paint/hit 跟随偏移、damage 覆盖「带偏移 ∪ 最终」；关闭 90ms 后移除；`Closing/DismissPending` 立即不可命中/聚焦；modal fade-only；**kit 未启用 FadeSlide（均 Fade/None）** | `popup/animation.rs`；`popup_behavior::{slide_animation_offsets_paint_and_hit_without_moving_the_settle_rect, slide_animation_damage_covers_the_offset_and_the_final_rect, closing_animation_blocks_hit_immediately_then_removes_the_node, modal_animation_is_fade_only}` |
| P1-9 | `PopupPanel`（`PopupSurface`/`PanelWidth{Auto,Exact,AtLeast}`/`PanelMeasure`/节点树 `popup-content→{key}:panel→{key}:body`+`{key}:measure`）+ 全组件迁移：Dropdown/Select（`PanelWidth::AtLeast` = 锚点宽匹配、键盘/多选 parity；typeahead/搜索未实现，属 P2-4）、Tooltip（Hint+穿透+500/100ms）、Menu/ContextMenu（1 级子菜单 parity、200ms hover、release 激活）、Dialog（Modal+Trap{DefaultButton,Previous}+Scrim+Explicit）、Alert（Dialog 内容面板，不新建 popup）、ColorPicker（内容闭包、局部坐标 Canvas、tab `request_measure`） | `kit/popup_panel.rs`；组件测试计数见 §12.5 |
| P1-10 | 宿主迁移：`crates/widget` 删除 `WidgetServices.layer/animation_router` 与 `Cx::layer()/animation()`；`crates/kytrix` composition/dialogs 去 Layer/AnimationRouter；14 个 widgets crate（15 个生产文件）与 5 个业务 ui crate（含 2 处直接 `Overlay::new`）改用 kit 组件/`PopupPanel`；测试支持 `App::overlays`→`popup_stack`/`popup_rect` | `cargo check --workspace --all-targets` PASS；`rg` 零引用（§12.9） |
| P1-11 | 删除旧 API：`kit::{overlay,layer}` 两文件、`ui_core::{app/overlay.rs, interaction/overlay.rs}`、`App::{create_overlay,close_overlay,place_overlay,bring_overlay_to_front,overlays,dismiss_overlay}`、`interaction::{Overlay,OverlayDismiss,OverlayOptions,OverlayPlacement,OverlaySlot,AnchorSide,resolve_placement}`、`overlay_style`、`NodeInteraction.overlay`、`NodeTree::{overlay_containers,note_overlay_container,overlay_slot}`、`test_support::overlay_wrapper`、`AnimationRouter`、`MenuMode::ListBox`、focus/a11y 旧 overlay scope 回落 | §12.9 的 7 组 `rg` 全零 |
| P1-12 | 测试重写与 gallery 迁移：`interaction_tests` 6 改写 + 9 删除（每项给覆盖去向）、`test_support/{a11y,gui_smoke}`、`a11y/action`、`interaction/focus`、`tests/hud` 迁移；kit overlay/layer 的 51 条测试随文件删除、行为由 popup/组件测试承接；gallery `--dump-overlay`→`--debug-stack`（popup_stack 快照 + `on_frame` 首可见帧采样）、`--pixel-check` 迁到 PopupPanel/popup_state 并新增 `sampled-animation-floor`（open ≥4 / close#1 ≥3 / close#2 ≥3；终审后 checks=94） | `p1-cleanup-w7.md` §3–§4；gallery 实测见 §12.5 |

### 12.3 设计偏差登记（D-P1-1…D-P1-30）

1. **D-P1-1 Menu 由「模态/独占」改为 Auto 非模态 + Take**（旧 kit Menu 自带 modal scope 与独占层）：根菜单 `PopupTier::Auto` + `FocusPolicy::Take{restore: Previous}`；hover 打开的子菜单 `FocusPolicy::None`（D13 不夺焦），键盘/点击打开的子菜单 `Take`；Esc 由引擎逐层关闭，不再存在旧 modal scope。**用户已在 P1 终审裁决 D6（单栈、Auto 非模态）保持不变**；上下文菜单/Dropdown/Select 同批按 D6 处理（`market_participants_ui` 直接 consumer 仍用 `Modal+Trap` 保持旧语义，属调用方自选 tier，非默认）。
2. **D-P1-2 outside press 默认 Replay（D14，用户已确认）**：kit Menu/Dropdown/Select/ColorPicker 的 `DismissPolicy::Chain{outside: Replay, escape: CloseTopmost, anchor_lost: true, focus_lost: true}`；`Consume` 为显式 opt-in（`outside_press_consume_is_explicit_opt_in`）。旧「模态 + outside Consume」parity 不再保留（K1-02：原 inventory 列为待裁决项，用户裁决 D14 Replay，此处即为裁决记录）。
3. **D-P1-3 子菜单 hover 延迟 150ms → 200ms**：`menu.rs` `SUBMENU_DELAY = 200ms`，可由 `MenuOptions::submenu_open_delay` 覆盖（**P2 改为 `HoverTimings.open_delay`，§13.3**）；与 §3.4 设计值一致（旧 kit 150ms）。
4. **D-P1-4 Alert 允许叠放**：旧 `LayerSlot::Exclusive`（Alert/ContextMenu 独占 band）随 `layer.rs` 删除而消失；Alert 现为 Dialog 内容面板（D9/D11，不新建 popup），多个 Dialog/Alert 同时打开时纯按 z 叠放，无独占限制。
5. **D-P1-5 Hint-in-modal 仅背景阻塞（用户裁决 B）**：modal 打开期间背景 Hint 仍隐藏/拒绝（`PopupError::HintBlockedByModal`）；但 opener 属于该 modal scope（`owner_modal == 该 modal`）的 Hint 允许显示在 modal 之上（Dialog 内控件 tooltip）；modal 关闭后背景 tooltip 由 hover 重新触发。
6. **D-P1-6 ColorPicker 保持 `role=Dialog` 但为非模态 + `RestoreFocus::Previous`**：tier=Auto、`FocusPolicy::Take{Previous}`、`PopupA11y{role: Dialog}`（沿用旧语义）；不是 modal，不阻断背景。
7. **D-P1-7 Dialog recenter 留 P3**：`Dialog::recenter`（把面板 `set_anchor` 到窗口中心）保留 parity 实现，未在 P1 删除（§8 原计划 P3-1）。
8. **D-P1-8 `--dump-overlay` → `--debug-stack`（P1-12 提前完成；终审 K1-04/Q1-07 + round-3 收口）**：dump 内容为 `popup_stack` 快照（id/tier/parent/z/state/hit/paint/a11y/owner_modal/focus/node/paint_offset/constrained_size）+ probe rect；`overlay-check` 改为 popup 语义（栈条目数=1、menu tier、菜单度量、首可见帧 rect==稳定帧、关闭后无残留、结束时栈空）。**`--debug-stack` 在最终波次完成后自行关窗并以 0/3 退出，`--auto-close` 仅抬升 ≥30s 看门狗（可省略）**；K1-04/Q1-07 的“未知参数静默 exit 0”已修为 exit 2。**旧 `--dump-overlay` 已从参数表删除**：传入旧名会打印 `gallery: --dump-overlay was renamed; use --debug-stack --auto-close=9` 并 **exit 2**；任何未知参数同样打印 usage 并 exit 2（不再“忽略后 exit 0”假绿）。命令名已同步到 `.docs/gui-migration/spec-deltas/kit-gallery.md` §13.3/§13.5。
9. **D-P1-9 `--pixel-check` 检查数 108 → 94、新增采样动画下限**：采样帧 45 → 32（场景迁到 PopupPanel 后采样窗口变化）；新增 3 条 `sampled-animation-floor` 断言（open 采样 ≥4、close#1 ≥3、close#2 ≥3；终审 K1-06 修复后由“两次关闭累计 ≥3”改为单次分段，实测 8 / 6+6），防止动画常量变短导致采样静默缩水；命令名与退出码（0 PASS / 3 FAIL）保留。**94 条构成 = 32 条逐帧采样断言（`changed-outside-damage`，首帧整帧重放时为 `full-replay`）+ 16 条 `background-residue`（open/reopen 采样窗各 8 帧）+ 46 条静态断言（20 `idle-frames` + 3 `restored` + 2 `close-frames` + 18 具名不变量 + 3 `sampled-animation-floor`）**。与 P0 108 条不是逐条等价：迁到 PopupPanel 后 idle 期不再出绘制帧（`PixelFrame::Idle` 提前返回），idle 期的 `background-residue` 不再逐像素执行，由 20 条 `idle-frames` + `idle-no-animation` 覆盖（终审 Q1-08 口径说明）。
10. **D-P1-10 真实窗口像素判定仍为 P3**：`--debug-stack` 只 dump rect/栈，真实窗口不读像素；残影像素门禁仍由 headless `gallery --pixel-check`（`PersistentPixels`）承担（延续 §11.3-8/9 语义），真实窗口 readback/截图与三端 P3-6。
11. **D-P1-11 `match_anchor_width` 由 kit 负责（A-09 处置 + 终审删除）**：设计语义「宽度 = max(锚点宽, 内容最小宽)，并夹到视口可用宽」由 kit `PanelWidth::AtLeast{min}` + `PanelMeasure` 测量回写实现（Dropdown/Select/ItemPanel）；引擎 `PlacementSpec.match_anchor_width` 因无生产消费者，已在 P1 终审修复中**删除**（E-09，`rg` 零引用），不再有待复核项。
12. **D-P1-12 子菜单 a11y 父已指向父 menuitem（A-29 round-3 修复）**：`a11y/popup.rs::resolve_logical_parent` 顺序仍为 `a11y_parent` → 父 popup 内容根 → opener，但 kit Menu 打开子菜单时写入 `a11y_parent = 父 menuitem`（`menu.rs`，含 2/3 级链测试）；hover `active_item` 的 `A11yPatch.active_descendant` 引擎通道已有测试，**kit 仍未写入（残余见 §12.6-15）**。
13. **D-P1-13 静态 aria-haspopup 已由 kit 声明（A-25 round-3 修复）**：Menu 子菜单行/root、Dropdown、Select 的 trigger 在关闭态通过 `A11yPatch.has_popup`（Menu/ListBox）持久声明，打开时 expanded 由 popup 投影派生、关闭恢复静态值；引擎保活/不重复暴露 controls 的语义不变。
14. **D-P1-14 A-24 旧 overlay 层序随删除消解**：旧 overlay 的 `pin_tail_child` 插入位与旧 `bring_overlay_to_front` 不再存在，固定层序 `[...内容, popup_root, debug_hud]` 成为唯一真值；`reorder_children` 的 damage 已遍历子树 `painted_extent`（A-21 修）。
15. **D-P1-15 性能登记（P3-5）**：动画期间每个动画帧对 popup 节点 `mark_paint`（lane 重建，无 layout）；结构帧 order 重建仍是 O(n) DFS；settle 触发条件含 `layout_ran && !stack.is_empty()`（主树任一布局帧都会重解 popup，宽于设计 §4.3 的纯 PopupDirty 闸门，B-16）；a11y 每次投递全树 DFS（A-32）。以上均未在 P1 优化，记 P3-5。
16. **D-P1-16 滚动上下文 reparent 缺口 P1 未修**：P0 §11.3-4 登记的「`append_child` 在 A/B 两滚动容器间搬子树时旧 damage 用新上下文 total 发射」仍存在；P1 无生产调用方（`append_child` 仅测试使用），留待列表复用/reparent 场景（P2/P3）。
17. **D-P1-17 Modal/Scrim 打开与关闭走整帧重发**：`force_full_republish()`（`damage_full`），非模态 popup 保持局部 damage；I6 断言只覆盖非模态（`opening_and_closing_a_non_modal_popup_never_forces_a_full_repaint`）。
18. **D-P1-18 `PopupState::Opening` 在首帧 settle 后即置 `Open`**（B-12）：因此 kit 的 `is_open` 用 `is_dismissable()`（Opening|Open）、`is_closing` 覆盖 DismissPending/Closing/Closed；`--pixel-check` 的采样动画下限按「已提交帧增量」口径（open=8 / close#1=6 / close#2=6，合计 12，终审后按单次分段断言）。
19. **D-P1-19 kit 未启用 FadeSlide（终审 K1-11 未关闭，留 P3）**：全部组件仍为 Fade（Dialog 可 None）；`FadeSlide` 的引擎实现（`paint_offset` + `PopupTransform`）有测试覆盖（paint/hit/damage/子锚点跟随），生产无使用者。终审认为这是与旧 kit parity（旧只有 fade）的偏差，D7/§4.12 的默认 Fade+Slide 仍未在组件落地，登记为 P3 遗留（不阻塞 P1）。
20. **D-P1-20 `App::modal_scope` 保留**：公共 API 语义改为「最上层 modal popup 的 content 节点」；`interaction/focus.rs::next_focusable` 去掉 scope 参数（无旧 modal 机制后恒等价）。
21. **D-P1-21 外部 `remove_node(popup content)` 语义收窄与焦点修复（终审 E-05）**：不再执行旧 overlay 的 opener 焦点还原/modal scope 标记；栈条目由 `reap_orphan_scrims`/`prune_missing` 修剪，**`PopupStack::remove` 在移除 owner 时清 `focus.owner`，`popup::focus::validate_owner` 在下一输入帧修复失效 owner 并把焦点回落到主树（Tab 可恢复）**。受支持路径仍是 `close_popup`/`PopupPanel::close`；直接删 popup 节点的调用方（仅测试支持/回滚）不保证回 opener，但不再出现“焦点永久失效”。证据：`popup/stack.rs::remove`、`popup/focus.rs::validate_owner`；测试 `a_stale_focus_owner_is_repaired_when_the_next_tab_runs`、`removing_a_take_popups_content_node_restores_main_focus_and_tab`（P1 终审修复前该路径焦点永久失效，D-P1-21 原表述不成立，已按最终实现改写）。
22. **D-P1-22 `PopupError` 变体增加 2 个、`InvalidGeometry` 增 `avoid`**：新增 `HintBlockedByModal{window}`、`StackSync{source: EngineError}`；`InvalidGeometry{anchor,size,viewport,avoid}`（见 §4.13）。
23. **D-P1-23 `Anchor::Node` 语义改为完整 border box（P1 审查修复 + 终审收尾）**：`Bottom` 以 rect.bottom 为基准、`Top` 以 rect.top、`Left/Right` 以对应边，gap 从对应边起算，align 在边跨度内；翻面到对侧不再与锚点节点重叠。**P1 终审修复删除 `Anchor::Node.edge` 与 `AnchorEdge`（E-09/Q1-02）**：`Anchor::Node` 现为 `{ node }`，边唯一由 `PlacementSpec.side` 表达；`PlacementSpec.match_anchor_width` 同时删除（无生产者，kit 用 `PanelWidth::AtLeast`）。
24. **D-P1-24 Menu/ContextMenu 子菜单为 1 级 parity（P2 已由 MenuChain 替换，§13.5）**：靠引擎 `PopupSpec.parent` 链实现（子层 z 在父上、Esc 只关一层、outside 关整链、父关闭级联），2–3 级 hover intent/safe-triangle/grace/typeahead/checkable 键盘表属 P2（`MenuChain` 未落地）。
**终审修复轮新增（D-P1-25…D-P1-30，2026-11-27）**

25. **D-P1-25 遮罩颜色改为主题色（E-02）**：`ScrimSpec` 增加 `color: Color`，引擎按 `color.with_opacity(density)` 绘制；Dialog 传主题 `Role::Scrim`/`ComponentColors.backdrop`（Light alpha 0.42 / Dark 0.55）与 `density=1.0`，修复“100% 不透明黑 + 主题遮罩色死字段”；§4.2/§4.6 已同步。
26. **D-P1-26 Trap 显式初始焦点（K1-05；E-21 收窄）**：`App::set_popup_initial_focus(id, node) -> Result<bool, _>` 供打开后指定（仅 Trap、节点须在 popup scope 且可聚焦；不适用返回 false 不报错）；Dialog 打开后锁定 confirm，`InitialFocus::{DefaultButton,FirstFocusable,Container}` 仅作 fallback。**原 `PopupSpec.initial_focus` 字段已删除（E-21：`open_popup` 的 content 闭包在 open 内才建节点，公开字段不可达）。**
27. **D-P1-27 死字段删除（E-09/Q1-02）**：`Anchor::Node` 改为 `{node}`（删除 `AnchorEdge`/`edge`），`PlacementSpec.match_anchor_width` 删除（宽度匹配由 kit `PanelWidth::AtLeast` 承担）；§12.9-8 零引用。
28. **D-P1-28 模态关闭事务期间继续阻断输入（E-01）**：`topmost_modal` 以 `PopupState::is_visible()` 判定（含 `DismissPending/Closing`）；关闭期按下/hover/scroll 仍按 modal scope 处理（命中外部时 Consume，不派发背景 action），modal 移除后恢复。
29. **D-P1-29 菜单/右键菜单滚动与 resize 关链矩阵（E-07/K1-01）**：引擎新增 `popup_anchor_rect`/`popup_anchor_visible`/`window_scroll_epoch`/`Window::scroll_epoch`；kit `AnchorWatch` 按 `AnchorChange::{Lost,Resized,Moved,Scrolled}` 分派：Menu/ContextMenu 关整链，Dropdown/Select/ColorPicker 重锚定或关闭，Tooltip 重定位/隐藏（设计 §4.7 矩阵落地）。
30. **D-P1-30 popup 阴影不再被内容根裁剪（E-03）**：引擎 `popup-content` 移除 `overflow_hidden`（panel/body 各自裁剪内容），`PopupSurface::shadows` 在 P1 恢复可见；`popup_panel::the_panel_shadow_is_not_clipped_by_the_popup_content_root` 锁定。

### 12.4 最终 API（P1）

`ui_core::popup`（模块根，公开）：`PopupId{window,slot,generation}`、`PopupRect{hit,paint,a11y}`、`PopupTier{Auto,Hint,Modal,Scrim}`、`PopupState{Opening,Open,DismissPending,Closing,Closed}`、`PopupDirty{geometry,order}`、`OutsidePress{Consume,Replay}`、`EscapePolicy{CloseTopmost,Ignore}`、`DismissPolicy{Chain{outside,escape,anchor_lost,focus_lost},Explicit{outside,escape},Hint{escape}}`、`RestoreFocus{Opener,Previous}`、`InitialFocus{DefaultButton,FirstFocusable,Container}`、`FocusPolicy{None,Take{restore},Trap{restore,initial}}`、`PopupAnimation{None,Fade,FadeSlide,FadeScale}`、`HasPopup`、`PopupA11y{role,label,expanded_of,owns,has_popup,described_by,a11y_parent}`、`ScrimSpec{density,color}`、`PopupSpec{parent,tier,anchor,placement,dismiss,focus,opener,pointer_transparent,animation,a11y,scrim}`（无 `initial_focus` 字段，E-21）（`PopupSpec::new(anchor, placement)` 给默认值：Auto/Fade/Take{Opener}/Newest/Replay+CloseTopmost/`anchor_lost=true`/`focus_lost=true`）、`PopupDismiss{id,reason,pointer}`、`DismissReason`（8 变体）、`DismissOutcome{Close,CloseChain,Ignore}`、`PopupStackSnapshot{window,root,entries}`、`PopupSnapshotEntry`、`Side/Alignment/Placement/ShiftPolicy/Anchor/PlacementSpec`（`PlacementSpec::submenu()`；`Anchor::Node{node}`/`WindowRect(LogicalRect)`/`NodeRect{node,rect}`；`AnchorEdge` 与 `match_anchor_width` 已删除）、`PlacementInput{anchor,size,viewport,avoid,spec}`、`PlacementResult{rect,side,resized}`、`solve_placement`、`PopupError`（见 §4.13）、`PopupDiagnostic{Unsettled,AnchorLost,Resized}`、常量 `MAX_POPUP_DEPTH=32`/`MAX_POPUP_ENTRIES=1024`。

```rust
impl App {
    pub fn open_popup(&mut self, window, spec: PopupSpec,
        content: impl FnOnce(&mut App, WindowId, NodeId) -> Result<(), EngineError>,
        on_dismiss: Option<Box<dyn Fn(&mut App, PopupDismiss) -> DismissOutcome>>,
    ) -> Result<PopupId, PopupError>;
    pub fn close_popup(&mut self, id: PopupId) -> Result<(), PopupError>;          // 立即（含子树，子先于父）
    pub fn close_popup_chain(&mut self, id: PopupId) -> Result<(), PopupError>;    // 立即关整链（自链根）
    pub fn begin_close_popup(&mut self, id: PopupId, reason: DismissReason) -> Result<bool, PopupError>; // 事务：DismissPending→Closing→动画后移除；false=不可关闭
    pub fn popup_state(&self, id: PopupId) -> Option<PopupState>;
    pub fn set_popup_a11y(&mut self, id: PopupId, a11y: PopupA11y) -> Result<bool, PopupError>; // 变更→bool
    pub fn set_popup_initial_focus(&mut self, id: PopupId, node: NodeId) -> Result<bool, PopupError>; // Trap 初始焦点（P1 终审补 K1-05）
    pub fn raise_popup(&mut self, id: PopupId) -> Result<(), PopupError>;
    pub fn set_popup_anchor(&mut self, id: PopupId, anchor: Anchor) -> Result<(), PopupError>;
    pub fn request_popup_measure(&mut self, id: PopupId) -> Result<(), PopupError>;
    pub fn popup_rect(&self, id: PopupId) -> Option<PopupRect>;
    pub fn popup_anchor_rect(&self, id: PopupId) -> Option<LogicalRect>;     // 锚点当前窗口 rect（E-07）
    pub fn popup_anchor_visible(&self, id: PopupId) -> Option<bool>;         // 锚点是否在裁剪/视口内（E-07）
    pub fn window_scroll_epoch(&self, window: WindowId) -> Option<u64>;      // 滚动纪元（kit 检测祖先滚动）
    pub fn popup_stack(&self, window: WindowId) -> Option<PopupStackSnapshot>;
    pub fn popup_pointer(&self, window: WindowId) -> Option<LogicalPoint>;   // 最近一次指针位置
    pub fn dismiss_topmost(&mut self, window: WindowId, reason: DismissReason) -> bool; // 走关闭事务
    pub fn popup_diagnostics(&self, window: WindowId) -> Vec<PopupDiagnostic>;
    pub fn modal_scope(&self, window: WindowId) -> Option<NodeId>;           // 最上层 modal popup 的 content 节点
}

impl Window {
    pub fn node_screen_rect(&self, node: NodeId) -> Option<LogicalRect>;      // 含祖先滚动与 popup paint_offset
    pub fn set_max_size(&mut self, node: NodeId, max: node::tree::MaxSize) -> Result<(), EngineError>; // 同步失效 popup max-size 缓存
    pub fn scroll_epoch(&self) -> u64;                                      // 滚动纪元（P1 终审补 E-07）
}

impl NodeTree {
    pub fn set_max_size(&mut self, node: NodeId, max: MaxSize) -> Result<(), EngineError>;
    pub fn reorder_children(&mut self, parent: NodeId, order: &[NodeId]) -> Result<(), EngineError>; // 全排列，不置 structure epoch
    pub fn hit_test(&self, point: LogicalPoint) -> Option<HitResult>;        // 既有 API，被 widgets 指针换算复用
}
```

`ui_kit::kit::popup_panel`：`PopupSurface{background,corner_radius,padding,shadows}`（`menu()/dialog()/tooltip()`）、`PanelWidth{Auto,Exact(LogicalPx),AtLeast{min}}`、`PanelMeasure{labels,extra,text_size}`（`new(labels, extra)`/`text_size`）、`PopupPanelOptions{surface,width,measure}`（`new`/`measure`）；`PopupPanel::{new(app,theme,key,options), key, open_state, on_close, id, state, is_open, is_closing, panel_node, body_node, popup_node, open(app,window,spec,content)->Result<PopupId,PopupError>, close, close_now, set_anchor, request_measure, set_measure, set_width, set_a11y, sync, release}`；节点树 `popup-content`（引擎）→ `{key}:panel` → `{key}:body` → 内容，`AtLeast` 另有 `{key}:measure`。

组件公开面（P1 最终，签名摘要）：
- `Dropdown::{new(app,theme,key,options), open(app)->Result<bool,PopupError>, close, close_now, sync, dispatch, release, set_items, set_selected, on_select, trigger_node, panel_node, popup_node, open_state, is_open, selected}`；`Select::{new(app,theme,key,options), open, close, close_now, sync, dispatch, release, set_items, set_chosen, clear, on_change, chosen, highlighted, is_open, open_state, tooltip_text}`
- `Menu::{new(app,theme,key,entries,options), open(app,window,anchor)->Result<bool,PopupError>, open_at, close, close_now, sync, release, on_activate, on_close, set_entries, highlighted, row_count, open_state, is_open, panel_node, popup_node}` 与 `MenuOptions{submenu_open_delay,...}`（P1 最终；P2 增量见本节末注记）
- `ContextMenu::{new(app,theme,key,entries,options), open, open_at, close, close_now, sync, release, on_item_click, on_close, set_entries}` + `MenuAnchor`
- `Tooltip::{new(app,theme,key,options), attach, set_content, content, is_visible, panel_node, popup_node, sync, release}` + `TooltipOptions{show_delay=500ms, close_delay=100ms, placement, max_width}`
- `DialogBuilder::{new(title), alert, content, confirm, cancel, header, close_button, close_label, width, outside, escape, fade, on_close, build}` + `Dialog::{open(app,window)->Result<bool,DialogError>, close, close_now, sync, is_open, is_closing, state, title, set_title, popup_node, panel_node, body_node, confirm_button, cancel_button, close_button, with_alert, release}`（`DialogClose`/`DialogError`）
- `AlertBuilder::{ok, ok_cancel, input_ok, input_ok_cancel, ok_text, cancel_text, validate, ok_action, cancel_action, build}`、`Alert::{root_node, title, ok_button, cancel_button, input, error, has_error, sync, release}`
- `ColorPicker::{new(app,theme,key,initial_color,size), open(app,window)->Result<bool,PopupError>, close, sync, release, is_open, open_state, popup_state, is_closing, panel_node, popup_node, trigger_node, color, draft, active_tab, hex_text, hovered_swatch, labels, set_labels, set_active_tab, set_color, on_change, on_open, on_close}`。

删除（P1-11 已完成，`rg` 零引用）：`kit::overlay::{Overlay,OverlayOptions,OverlaySurface,OverlayA11y,OverlayError,AnimationRouter,Scrim,Anchor,anchor_rect,DismissPolicy,Side,Alignment,Placement,PlacementConfig,resolve_origin}`、`kit::layer::{Layer,Band,LayerSlot,Entry}`、`kit::test_support::overlay_wrapper`、`MenuMode::ListBox`、`App::{create_overlay,close_overlay,place_overlay,bring_overlay_to_front,overlays,dismiss_overlay}`、`overlay_style`、`interaction::{Overlay,OverlayDismiss,OverlayOptions,OverlayPlacement,OverlaySlot,AnchorSide,resolve_placement}`、`app/overlay.rs`、`interaction/overlay.rs`、`NodeInteraction.overlay`、`NodeTree::{overlay_containers,note_overlay_container,overlay_slot}`、`--dump-overlay`。保留（新形态）：`ItemPanel`（`pub(crate)`，现已基于 `PopupPanel` + `List`，是 `PopupPanel` 的 menu 视图）、`A11yPopupOverlay`（a11y 投影，非旧类型）、`DamageOverlay`（test_support，与浮层无关）。

**P2 后的增量 API（P1 最终 → P2，见 §13.3）**：`MenuOptions` 去 `submenu_open_delay`、增 `timings: HoverTimings`/`outside_press`；`MenuAction.checked` → `check`（`MenuCheckState` + `checkbox()/radio()`）；`MenuActivation` 增 `check`；`Menu` 增 `open_submenu`/`bind_trigger`；`ContextMenuOptions` 增 `outside_press`；`ContextMenu` 增 `open_on`/`bind_trigger`；引擎 `PopupA11y` 增 `active_descendant`（`PopupError` 仍 13 变体，非法值清空 + 诊断）。

### 12.5 测试与门禁基线（本机 Windows 11 / D3D12 / 125% DPI，2026-11-27）

| 门禁 | 结果 |
|---|---|
| `cargo test -p ui_core -p ui_kit` | **3780 passed，0 failed，2 ignored**（ui_core lib 2804 + 1 ignored、frame_record 8、gui_smoke 0 + 1 ignored、layout 4、popup_behavior 110、snapshot 14、demo 示例单测 39、ui_kit lib 801）。P0 基线 3554 → **+226**（P1 修复轮净增：popup_behavior 92→110、ui_kit 773→801） |
| `cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings` | PASS |
| `cargo +nightly fmt --all -- --check` | PASS |
| `cargo check --workspace --all-targets` | PASS |
| `cargo run -p ui_kit --example gallery -- --pixel-check` | **PASS**，`checks=94 sampled_frames=32 open_animation_frames=8 close_animation_frames=6+6=12 damage_full_frames=0`，`first_visible==stable=(496,91,300×375)`、`moved=(496,4,300×375)`，exit 0（覆盖下限 30‰ 与 2 点注入自检通过） |
| `cargo run -p ui_kit --example gallery -- --debug-stack` | **PASS**（overlay-check：menu 度量 `(6, 180.0, 127.5125)`、picker children=4、menu_frames=429、picker_frames=365、最终 entries=0；`menu_frames`/`picker_frames` 运行间浮动，断言只要求 >0）。**命令自行在波次完成后关窗并以 0/3 退出；`--auto-close` 仅 ≥30s 看门狗**（带 9 与不带均 PASS） |
| `rg` 旧 API 零引用 | 7 组组合模式旧 API 零命中（`dump-overlay` 仅 gallery 的改名提示 3 处，§12.9）；`AnchorEdge`/`match_anchor_width` 也零引用 |

测试规模（`#[test]` 计数口径，round-3 最终）：引擎 `popup/tests.rs` 43、`tests/popup_behavior.rs` 110、`a11y/popup_tests.rs` 21、`popup/placement.rs` 34；kit `PopupPanel` 18、`Dropdown` 27、`Select` 32、`Menu` 30、`ContextMenu` 14、`Tooltip` 28、`Dialog` 54、`Alert` 11、`ColorPicker` 80。

`--pixel-check` 的 94 条构成：32 条逐帧采样断言 + 16 条 `background-residue`（open/reopen 各 8 帧）+ 46 条静态断言（20 `idle-frames` + 3 `restored` + 2 `close-frames` + 18 具名不变量 + **3 条 `sampled-animation-floor`**：open ≥4、close#1 ≥3、close#2 ≥3；终审 K1-06 修复后由累计下界改为单次关闭分段校验）。

复跑命令（Windows PowerShell，可复制）：

```powershell
$env:CARGO_TARGET_DIR='E:\kytrix\target'
cargo test -p ui_core -p ui_kit
cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings
cargo +nightly fmt --all -- --check
cargo check --workspace --all-targets
cargo run -p ui_kit --example gallery -- --pixel-check                     # exit 0=PASS / 3=FAIL（未知参数/旧 --dump-overlay 为 exit 2）
cargo run -p ui_kit --example gallery -- --debug-stack                  # 真实窗口：栈快照 + overlay-check（自行退出 0/3；--auto-close 仅 ≥30s 看门狗）
```

### 12.6 未执行项、待复核与 P2/P3 衔接

**未执行（原因）**

1. **workspace `cargo test` 未跑**：沿用 P0 的用户限制，本轮只跑 `ui_core`/`ui_kit`；`crates/widgets`/`widget`/业务 crate 测试未执行（`cargo check --workspace --all-targets` 绿）。
2. **真实窗口像素判定与三端验收**：P3-6（`--debug-stack` 只断言栈/rect；macOS/Linux 未执行）。
3. **性能基线（open/close 帧预算、空闲帧 popup 工作）**：P3-5，未测（§12.3-15 仅登记热点）。
4. **`.docs/gui-migration/spec-deltas/kit-gallery.md` §13 同步**：**已在 P1 终审文档修复中完成**（§13.3 改为 `--debug-stack`；新增 §13.5 P1-12 回写：`--debug-stack` 语义、`--pixel-check` 93→94 checks 构成/采样下限/与 P0 108 的覆盖差异、P1 迁移完成状态、旧 `--dump-overlay` 移除说明）。P3-7 的“kit-gallery 未同步”项已提前关闭。
5. **P0 回写中依赖旧 API 的表述**：§11.3-9/§11.4/§11.7 保留为 P0 历史记录，未删改；其“P1-12 必须迁移”的要求已在 §12.2 完成；§11.5 仅追加 P1 命令改名注记（不改 P0 数据）。

**待复核（终审后仍未闭环；不臆断，列证据）**

1. **`MaxSize` 只剩 `node::tree::MaxSize`（数值 `{width,height}`）**：P3 已删除 `popup::MaxSize`/`PlacementSpec.max_size`，A-27 命名歧义关闭；`Window::set_max_size` 同步失效缓存（P3）。
2. **settle 触发含 `layout_ran`**（B-16）：主树每次布局都会重解 popup，与设计 §4.3「只有 PopupDirty 置位才运行 settle / 空闲帧 0 popup 工作」不同（`idle_frames_do_no_popup_settle_work` 只覆盖无布局帧）；P3-5 用基线判定是否需要收紧。
3. **a11y 混合父链环检测（A-26）**：仅 `a11y/tree.rs::patch_parent_cycle` 处理 patch 链；popup 逻辑父与 patch 交叉成环仍未见测试/诊断，终审未新增证据（P3）。
4. **`PopupPanel::open` 已打开时的复用语义**：对 `is_dismissable` 的已有 popup 只做 `set_popup_anchor` + `raise_popup` 并返回原 id（不重建内容）；对 Closing 中的 popup 走 `close_popup()`（立即）再重建。终审未改该分支（P3 记录）。
5. **`PopupDiagnostic` 的消费面**：`Unsettled/AnchorLost/Resized` 已能 push，`App::popup_diagnostics` 暴露；`FrameRecord.popup_damage/z_changes`（§4.14）未实现（A-31 部分，P3）。
6. **E-12 测试真实性残余**（P3，round-3 后仍余）：`popup_panel` 的 at_least 命名已修（`an_at_least_panel_honours_minimum_width_changes`）；Menu 指针命名与 caret+`paint_offset` 直接断言已闭（`app::tests::ime_cursor_area_includes_ancestor_popup_transform_offsets`）；仍余 P1 popup 套件无变异自检 harness、`--pixel-check` 仅覆盖 ColorPicker（未覆盖 Dialog/Alert/Tooltip/Menu 像素，设计 P3-4 gallery 扩展）。
7. **K1-11 `FadeSlide` 未启用**（P3）：kit 全组件仍 Fade/None，D7 的默认 Fade+Slide 未落地（D-P1-19）。
8. **K1-12 宿主测试装置放宽**（P3，accepted）：`profile_ui`/`market_participants_ui` 的 settle/fade 推进、`instrument_selector` 的 `closed || find().is_none()` 属关闭动画适配，强度略降但记录不阻塞。
9. **R2-01/R3-01 受约束 popup 的内部高度/宽度在首帧未 settle**（P3）：`sync_height`/`sync_scroll`/`sync_width` 只在 sync 生效，`open` 当帧的小视口首帧可能仍按未夹取内容布局；默认 Fade 下肉眼风险低，`fade(false)` 高内容/窄视口对话框最可能可见。
10. **R2-02 anchor 矩阵关闭 reason 固定 `Programmatic`**（P3）：`PopupPanel::close` 不能区分 AnchorLost；当前生产 Menu/ContextMenu `on_close` 消费者为 0，P2 MenuChain 前需补 `close_with_reason` 或映射。
11. **R2-04 无 `.confirm()` 的自建 footer 对话框初始焦点回退 Cancel**（P3）：符合 D16 fallback；`settings_ui` proxy editor 属此情形；可加 `DialogBuilder::initial_focus` 或按登记接受。
12. **R2-05 观察项**（不计 finding）：Scrim 色在 open 时解析一次（打开期间切 Appearance 不刷新）；Dialog 内部 ScrollBar 覆盖内容右缘；ColorPicker 首帧测试在第 2 帧采样。
13. **外部滚动语义宽于“祖先滚动”字面**（P3 文档/语义精度）：`window_scroll_epoch` 对任意非 popup 子树滚动 +1，Menu/ContextMenu 会对无关滚动容器关链；建议把 §4.7/D-P1-29 措辞改为“外部滚动（含祖先）”。
14. **终审已关闭项（留档，round-3 最终）**：E-08（AT 激活）、K1-07（宿主 total-stack 断言）、K1-08 b/e/f（Tooltip TOP/instrument Dialog 宽度/Dialog resize recenter 测试）、K1-09（`PanelState.height` 为高度缓存）、Q1-04（widget `dialog_engine_error` 保留 `PopupError`）、E-14/E-18（生产 `expect` 清除）、E-17（raise 整组）、E-20（Dialog 宽度 clamp）、E-21（`PopupSpec.initial_focus` 字段删除）、A-29（子菜单 a11y 父）、A-25 静态 `has_popup`、`AnchorEdge`/`match_anchor_width` 删除、IME caret `paint_offset`、`interaction_tests` 删除 9 条等价强度。
15. **A-25 残余：`active_descendant` kit 未声明**（P2/P3；**P2 已关闭**）：P1 时引擎 `A11yPatch.active_descendant` 通道已具备并有测试（`a11y_patch_active_descendant_is_a_hover_channel`），kit Menu/List hover 未写入；**P2 改为 `PopupA11y.active_descendant` 投影到 content 根并接入 Menu（§13.3/D-P2-1/§13.5）**；静态 `has_popup` 已由 round-3 修复（D-P1-13）。

**P2/P3 衔接（状态）**

> **P2 后状态（2026-11-27）**：下表 P2-1…P2-7 已随 P2 全部关闭（结果/证据/偏差见 §13.2/§13.4/§13.5）；本表保留 P1 收口时的状态作历史记录，P3 项在 §13.7 更新。

| 阶段 | 内容 | P1 状态 |
|---|---|---|
| P2-1 | `MenuChain` per-level 状态机（timer 绑定 level、close siblings/descendants、children-first） | 未开始（P1 只有 1 级 `Menu` 子菜单 + 200ms timer） |
| P2-2 | 5 点 safe triangle/grace（镜像 bleed、300ms、open/flip/scroll 重算） | 未开始（§3.4 数值已定，代码无 grace polygon） |
| P2-3 | 菜单面板行高亮/滚动/ensure_visible/子菜单指示/分组/分隔线/勾选 | 部分（List 滚动、分隔线、勾选渲染已有；子菜单指示/分组 hover 语义属 P2） |
| P2-4 | 完整键盘表 + typeahead + checkable 语义 | 部分（Up/Down/Home/End/Left/Right/Enter/Esc 已有；typeahead/PgUp/PgDn/Alt/F10 未做） |
| P2-5 | 链式 dismiss：命中祖先只关后代、Esc 逐层、tooltip 优先、D14 replay | 引擎侧已完成并被测试锁定；kit grace/链几何 `Ignore` 属 P2 |
| P2-6 | ContextMenu：WindowRect、右键/Shift+F10/Menu、子菜单共享 MenuChain | 部分（`Anchor::WindowRect` + `MenuAnchor` 已有；右键/Shift+F10 事件绑定属 P2） |
| P2-7 | 3 级测试 + a11y 链嵌套测试 + gallery 3 级演示 | 未开始（引擎 `three_menu_levels_follow_the_parent_without_a_frame_lag` 仅 3 层几何） |
| P3-1 | Dialog/Alert 细化（初始焦点/Tab 循环/outside/escape/restore；删 recenter/双份 closing） | 大部分已在 P1 完成（D16 由 `InitialFocus::DefaultButton` + kit 默认按钮约定）；recenter 删除留 P3（D-P1-7） |
| P3-2 | `Scale` 动画（D10） | 未开始（引擎无 lane transform） |
| P3-3 | ColorPicker 细化 | 已在 P1 完成（内容闭包、局部坐标、tab `request_measure`、open/close 回调） |
| P3-4 | gallery ⑫⑬⑭ 新 API、3 级菜单、残影像素采样、`--dump-overlay`/`--probe` | 大部分已在 P1-12 完成（`--dump-overlay` 改名 `--debug-stack`）；3 级演示留 P2 |
| P3-5 | 性能预算（open/close 帧、空闲帧、O(n) 路径） | 未开始（§12.3-15 已登记热点） |
| P3-6 | 三端真实窗口验收（像素 readback/截图、退出码合并） | 未开始 |
| P3-7 | 文档回写与 `kit-gallery.md` 同步 | **已完成**：设计文档 P1 回写=本节；`kit-gallery.md` §13.3/§13.5 已同步（P1 终审文档修复） |

### 12.7 P1 早审 finding 处置状态

P1 期间两份早审报告（`implementation/review-p1-engine-behavior.md` B-01…B-17、`implementation/review-p1-engine-api.md` A-01…A-34）在 P1 收口前经历了并发修复轮（task-30/31/32/33/35）。下表只列**可由最终代码/测试复核**的状态；“待复核”项见 §12.6。

| Finding | 状态 | 证据 |
|---|---|---|
| A-01 侧表槽位复用 stale transform/transparent | 已修 | `node/tree/arena.rs:596-599` remove 清两张侧表；`node/tree/tests.rs` 槽位复用用例 |
| A-02 模态 a11y 挂 hidden trigger | 已修 | `a11y/popup.rs::resolve_logical_parent`：`is_modal()` → `None`（挂窗口 root）；`a_modal_popup_marks_the_dialog_hides_the_background_and_hides_the_scrim` |
| A-03 `ZPolicy` 失败路径孤儿节点/panic | 已修 | `invalid_z_policy_open_leaves_no_residue_and_next_open_works` |
| A-04 默认 feature 下 `cargo test -p ui_core` 编译失败 | 已修 | `ui_core/Cargo.toml` `[[test]] popup_behavior required-features=["test-support"]` |
| A-05 Scrim 非全视口 | 已修 | `settle.rs` 对 `tier==Scrim` 跳过 max-size/placement、`set_absolute_size(viewport)+inset(ZERO)`；`a_modal_scrim_covers_the_full_viewport_and_damages_it` |
| A-06 modal+scrim 关闭绕过事务 | 已修 | `close_entry` 双向回收 + `DismissPending`；`a_modal_scrim_closes_with_the_modal`、`programmatic_begin_close_animates_the_fade_and_removes_after_it` |
| A-07 modal 打开后 Hint 仍可打开且 z 更高 | 已修 | `open_popup` 的 `HintBlockedByModal`（仅允许 `owner_modal==modal` 的 Hint）；`a_background_tooltip_is_rejected_while_a_modal_is_open` |
| A-08 AT 动作 scope 走旧物理链 | 已修 | `a11y/action.rs` 用 popup scope；`a_modal_dialog_and_its_inner_popup_accept_at_actions` |
| A-09 `match_anchor_width` 无落地通路 | 已迁移 kit + 引擎字段删除（终审） | D-P1-11；kit `PanelWidth::AtLeast`；引擎 `PlacementSpec.match_anchor_width` 已删（`rg` 零引用） |
| A-10 子菜单方向无强制 | 已修 | `effective_placement` 强制 Right/Left + fallbacks 过滤；`a_submenu_ignores_vertical_fallbacks_and_opens_horizontally` |
| A-11 `a11y_parent` 可指向 skip-container | 已修 | `resolve_parent` 沿 `HitFocusSkip::Container` 上溯；`InvalidA11yParent` 诊断 |
| A-12 两套 a11y 父通道多父 | 已修 | `resolve_logical_parent` 单通道早退（a11y_parent → parent → opener） |
| A-13 Scrim 规格可伪造 + density 未用 | 已修 | `open_popup` 拒绝 `tier==Scrim`；`scrim_paint(density)`；`a_user_supplied_scrim_tier_is_rejected` |
| A-14 `EscapePolicy::CloseChain`（P3 删除，链关闭由 `DismissOutcome::CloseChain` 表达）未实现 | 已修 | `dismiss.rs::escape` 按策略 `begin_close_chain`；`escape_close_chain_policy_closes_the_whole_chain` |
| A-15 `anchor_lost=true` 未关整链 | 已修 | `dismiss::anchor_lost` 按 `close_chain`；`an_anchor_lost_chain_closes_the_whole_chain` |
| A-16 Closing 中 Toggle 不取消关闭 | 已修 | `cancel_close`；`pressing_the_opener_while_closing_cancels_the_close` |
| A-17 `owner_modal` 传递缺口 | 已修 | `validate_open` opener 路径走 `nearest_modal`；`a_popup_opened_inside_an_owner_modal_popup_keeps_the_modal_scope` |
| A-18 `dismiss_topmost`/AT Dismiss 绕过事务 | 已修 | `dismiss_topmost` 走 `request_close`；`dismiss_topmost_runs_the_closing_transaction` |
| A-19 limitShift 与 margin 冲突 | 已修（margin 优先） | `solve_placement` fit 分支最终双轴 clamp；`limit_shift_clamps_into_the_viewport_margin` |
| A-20 `resized` 信号链断裂 | 已修（终审补齐 kit 消费面） | `PopupEntry.constrained_size` + `PopupDiagnostic::Resized`；`PopupPanel::available_body_height` 被 Menu/Dialog 消费（E-04/Q1-01）；`a_constrained_popup_exposes_its_applied_size_without_unsettled_churn`、`a_small_viewport_shrinks_the_menu_and_scrolls_its_last_row_into_view`、`a_tall_dialog_scrolls_its_content_and_keeps_the_footer_reachable` |
| A-21 reorder damage 不含后代外延 | 已修 | `reorder_children` 遍历子树 `painted_extent`；`raise_damages_descendant_painted_extent` |
| A-22 `paint_offset` 不进派生几何 | 已修（终审关闭） | 焦点环 damage 用 `node_screen_rect`（含 transform）、子 popup 锚点跟随；**IME caret 已并入 popup transform 偏移**（`window/focus.rs::text_caret_rect` 累计 `popup_transform.offset`，E-11） |
| A-23 `sync_order`/`prune_missing` 静默失败 | 已修 | `PopupError::StackSync`；`reap_orphan_scrims`；`an_orphaned_scrim_is_reaped_when_the_modal_node_disappears` |
| A-24 旧 overlay 固定层序 | 已消解（删除） | D-P1-14 |
| A-25 瞬态 haspopup/controls 重复暴露 | 已修（静态 has_popup + P2 `active_descendant` 落点） | 引擎 `static_haspopup_controls_and_expanded_survive_the_popup_lifecycle`；kit 关闭态静态声明 `has_popup`（Menu 子菜单行 / Dropdown / Select），打开期由 popup 投影 expanded，关闭恢复（D-P1-13）；`menu::a_submenu_row_keeps_static_haspopup_and_projects_expanded`、`dropdown/select::the_trigger_reports_the_selection_and_the_expanded_state`；`active_descendant` kit 未写（§12.6-15） |
| A-26 混合 a11y 父链环 | 待复核 | 仅 `a11y/tree.rs::patch_parent_cycle`；§12.6 待复核 3 |
| A-27 两个 `MaxSize` + 外部写入被覆盖 | 部分修 | `Window::set_max_size` 失效缓存；`external_max_size_writes_are_reapplied_by_the_next_settle`；命名未收敛（§12.6 待复核 1） |
| A-28 placement 极端输入 NaN | 已修 | `extreme_finite_inputs_never_produce_a_non_finite_rect` |
| A-29 子菜单 a11y 默认父 | 已修（round-3） | kit Menu 打开子菜单时写 `PopupA11y.a11y_parent = 父 menuitem`；`menu::a_two_level_submenu_chain_attaches_to_its_parent_menu_items`（2/3 级链）；D-P1-12 |
| A-30 死字段/诊断 | 已修 | `settled_size/solved_origin` 已删（改 `constrained_size`）；`an_anchor_lost_dismissal_records_a_diagnostic` |
| A-31 调试/可观测缺口 | 部分（P3） | `popup_stack/popup_diagnostics/settle_rounds` 有；`FrameRecord.popup_damage/z_changes`、`debug_stack` 引擎命名未加 |
| A-32 a11y 性能 O(n) | 未修（P3） | §12.3-15 |
| A-33 测试真实性缺口 | 部分改善（P3 残余） | 新增首帧/锚点/约束/透明穿透/关闭顺序等强断言；**`slide_animation_offsets_paint_and_hit_*` 已断言 FrameRecord 的 Quad 绘制位置跟随 `paint_offset`（终审 E-12 修复）**；DPI 用例真正切换 DPR；残余见 §12.6-6 |
| A-34 规范核查 | 通过（expect 口径见 E-18） | clippy `-D warnings` PASS；新增生产文件无注释/`allow`/`unwrap`/新依赖（`rg`/抽查）；**新增 popup 代码生产 `expect`=0（E-14/Q1-03），E-18 的 `Window::new` expect 亦已清除**；仓库 P1 之前既有生产 `expect` 不在本轮引入范围 |
| B-01 侧表泄漏（=A-01） | 已修 | 同 A-01 |
| B-02 Scrim 可见遮罩/全视口/density | 已修（终审补主题色） | `scrim_style` + `scrim_paint(ScrimSpec{color,density})`；`a_modal_scrim_covers_the_full_viewport_and_damages_it`、`scrim_paints_the_configured_color_and_density`；Dialog 用主题 `Role::Scrim`/`backdrop`（E-02），`the_scrim_uses_the_theme_backdrop_color_instead_of_opaque_black` |
| B-03 事件模型不完整 | 已修 | `ParentClosed/FocusLost` 派发（`parent_close_emits_parent_closed_for_children`、`window_deactivation_emits_focus_lost`）；`DismissOutcome::CloseChain` 真实关链（P3 删除 `EscapePolicy::CloseChain`） |
| B-04 窗口失焦误关 Modal/Scrim | 已修 | `window_deactivated` 只筛 `Auto|Hint` 根；`window_deactivation_keeps_a_modal_with_scrim` |
| B-05 Closing 中 Toggle / Opening 无回退 | 已修 | `cancel_close`；`pressing_the_opener_while_closing_cancels_the_close` |
| B-06 §7.1 必需测试缺失 | 已修 | 文本主导高度/虚拟列表/DPI/max-size clamp/modal hover+scroll/owner_modal 六类均有测试（见 §12.2 P1-2/P1-4/P1-6 证据列） |
| B-07 `required-features` | 已修 | `ui_core/Cargo.toml:52-55` |
| B-08 死字段/死 API | 已修 | `solved_origin/settled_size/PopupEntry::rect/PopupDirty.measure` 均已不存在 |
| B-09 close/begin_close 语义不对称 | 文档化（保留） | `close_popup`=立即、`begin_close_popup`=事务（§12.4 注释）；`closing_an_opening_popup_keeps_later_nodes_clean` |
| B-10 外部删除 modal 节点留孤立 scrim | 已修 | `reap_orphan_scrims`；`an_orphaned_scrim_is_reaped_when_the_modal_node_disappears` |
| B-11 生产 `expect` | 已修（终审；E-18 后口径澄清） | `sync_order` 已改 `StackSync`；新增 popup 代码（`popup.rs`/`popup/*`/`app/popup.rs`/`a11y/popup.rs`/`PopupPanel`）生产 `expect`=0（E-14/Q1-03）；E-18 的 `Window::new`（`PopupStack::install`）expect 已清除；其余命中位于 `#[cfg(test)]` 或 P1 之前既有代码 |
| B-12 Opening 状态语义 | 文档化（保留） | D-P1-18；kit 用 `is_dismissable/is_closing` 收口 |
| B-13 `DefaultButton` 只是“第一个 Button” | 已修（终审 K1-05；E-21 收窄） | 新增 `App::set_popup_initial_focus(id, node)`（D-P1-26；E-21 后 `PopupSpec.initial_focus` 字段已删）；Dialog 打开后显式指定 confirm 节点，`a_content_button_before_the_footer_does_not_take_the_default_focus`、`set_popup_initial_focus_moves_the_focus_within_a_trap`；`InitialFocus` 启发式仅作 fallback |
| B-14 a11y Dismiss 目标非请求节点 | 已修 | `at_dismiss_closes_the_popup_that_owns_the_target`、`hide_tooltip_closes_the_tooltip_even_below_another_popup` |
| B-15 指针/穿透卫生 | 已修（终审） | `pointer_transparent` 已消费（`pointer_transparent_popup_lets_clicks_and_hover_reach_the_content_below` 等 3 条）；**`suppress_release` 已在 `cancel_pointer_session`/窗口失焦时重置**（E-06），`window_focus_loss_does_not_swallow_the_next_background_click` |
| B-16 settle 触发含 `layout_ran` | 保留（P3） | §12.3-15 / §12.6 待复核 2 |
| B-17 测试真实性细节 | 部分（P3） | 首可见帧测试已断言 rect 与帧数；负向/变异自检未成体系，留终审 |

### 12.8 P1 与设计正文的对齐摘要（供审查对照）

- §4.2：`Anchor::Node` 现为 `{node}`（`AnchorEdge`/`edge` 已删，边由 `PlacementSpec.side` 决定）且解析为完整 border box；`PopupEntry` 用 `constrained_size`/`a11y_rect` 取代 `settled_size`/`solved_origin`；`ScrimSpec{color,density}` 由引擎按 `color.with_opacity(density)` 绘制；**`PopupSpec` 无 `initial_focus` 字段（E-21），初始焦点走 `App::set_popup_initial_focus`**；`PopupTier::Scrim` 不可由调用方构造。
- §4.4：`PlacementInput.avoid` = 父 popup 已求解 rect；子菜单方向由引擎强制 Right/Left；margin 是硬约束（A-19 裁决）；`match_anchor_width` 引擎字段已删除，宽度匹配由 kit `PanelWidth::AtLeast` 承担。
- §4.6：遮罩使用主题 `Role::Scrim`/`ComponentColors.backdrop` 色（Light alpha 0.42 / Dark 0.55），不再硬编码 100% 黑（E-02）。
- §4.13：`PopupError` 13 变体（无新增）；`begin_close_popup`/`set_popup_a11y`/`set_popup_initial_focus` 返回 `Result<bool, _>`（`false`/`true` 为“是否生效/变化”，参数不适用不报错）。
- §5.1/§5.2：最终 API 见 §12.4；`ItemPanel` 保留为新形态、`AnimationRouter`/`MenuMode::ListBox` 删除；`PopupPanel` 消费 `constrained_size` 做内部滚动（`available_body_height`/`available_body_width`）、popup 内容根不再 `overflow_hidden`（阴影可见）、Dialog 用 `set_popup_initial_focus` 锁默认按钮并在窄视口 clamp 宽度（E-20）。
- §8：除 `Dialog::recenter`（P3）外全部完成；gallery `--dump-overlay`→`--debug-stack` 提前到 P1-12；**原表规划的 `PopupHost`/`cx.popup_host()` 最终未实现**（已按最终形态更正）。
- §10：DoD 1（残影/像素门禁）与 4/5（不变量/门禁）在 P1 范围内达成；DoD 3（2–3 级子菜单）与 6（三端）未达成，属 P2/P3；DoD 7 文档回写=本节 + `kit-gallery.md` §13 已同步。
- §7.5 门禁命令已改为 `--debug-stack`（自行退出 0/3，`--auto-close` 仅 ≥30s 看门狗；`--pixel-check` 保留原命令/退出码；未知参数/旧 `--dump-overlay` 显式 exit 2）。

### 12.9 `rg` 零引用结果（P1-11/P1-12 判据）

口径：`crates/**/*.rs`（inventory §5.3 的 7 组组合模式；本机以 ripgrep 等价语义复核，2026-11-27）。

| # | 模式 | 结果 |
|---|---|---|
| 1 | `kit::overlay\|overlay::\{\|OverlayOptions\|OverlaySurface\|OverlayA11y\|OverlayError\|AnimationRouter` | 0 |
| 2 | `kit::layer\|layer::\{\|Layer::new\|LayerSlot\|Band::` | **0**（大小写敏感、判据收紧为 `\bLayer::new`；原始 `Layer::new` 子串有 1 处保留词命中：`ui_core/src/render/macos/swapchain.rs` 的 `CAMetalLayer::new()`，macOS 纹理 layer，非旧 `kit::layer`，inventory 已登记排除） |
| 3 | `create_overlay\|close_overlay\|place_overlay\|bring_overlay_to_front\|resolve_overlay_placements\|dismiss_overlay\|\.overlays\(` | 0 |
| 4 | `OverlaySlot\|OverlayDismiss\|OverlayPlacement\|overlay_style\|note_overlay_container\|overlay_containers\|overlay_slot` | 0 |
| 5 | `\.layer\(\)\|\.animation\(\)\|animation_router\|services\.layer` | 0 |
| 6 | `overlay_wrapper\|MenuMode::ListBox` | 0 |
| 7 | `dump-overlay\|dump_overlay` | **3**（均在 `gallery.rs`：旧名显式失败分支 + banner 的“已删除”提示；旧 API/命令无实际使用） |
| 8 | `AnchorEdge\|match_anchor_width`（P1 终审删除项） | 0 |

保留词（非判据，已在 inventory §5.3 登记）：`Overlay` 单词命中 `A11yPopupOverlay`（新 a11y 投影）与 `DamageOverlay`（`test_support/diagnostics.rs`，与浮层无关）；`ItemPanel` 是新形态（`pub(crate)`，基于 `PopupPanel`+`List`）。

### 12.10 P1 终审 finding 处置状态（review-p1-final-{engine,kit,quality}）

> 终审三份报告：`implementation/review-p1-final-engine.md`（E-01…E-16）、`review-p1-final-kit.md`（K1-01…K1-13）、`review-p1-final-quality.md`（Q1-01…Q1-10）；**第 2 轮 r2 复核（E-17…E-21、R2-01…R2-05、Q2-01/Q2-02）见 §12.11**。下表为**round-3 修复（task-43/44/45 + 48/49/50）后的最终处置**；证据以最终代码/测试为准。修复轮净增测试 `popup_behavior` 92→110、`ui_kit` 773→801，门禁 3780 passed。

**引擎（E-01…E-16）**

| Finding | 严重度 | 处置 | 证据/位置 |
|---|---|---|---|
| E-01 模态 DismissPending/Closing 期间背景输入未阻断 | P0 | **fixed** | `popup/stack.rs::topmost_modal` 以 `state.is_visible()`（非 Closed）判定，关闭事务期间 modal 仍生效；`popup/dismiss.rs::on_pointer_down` 在存在 modal 且命中 scope 外时 `Consume`（`Explicit{outside}` 只对 `is_dismissable` 生效）；`a_closing_modal_swallows_background_presses_until_it_is_removed`、`a_closing_modal_keeps_background_hover_and_scroll_blocked`、`closing_the_lower_modal_cascades_and_keeps_input_blocked_until_all_leave` |
| E-02 Dialog 遮罩 100% 不透明黑、主题 Scrim 死字段 | P1 | **fixed** | `ScrimSpec{color,density}` + `scrim_paint = color.with_opacity(density)`；Dialog 用主题 `Role::Scrim`/`backdrop`（Light 0.42 / Dark 0.55）与 density=1.0；`scrim_paints_the_configured_color_and_density`、`the_scrim_uses_the_theme_backdrop_color_instead_of_opaque_black` |
| E-03 PopupSurface 阴影被 popup 内容根 `overflow_hidden` 裁剪 | P1 | **fixed** | `app/popup.rs::popup_content_style` 移除 `overflow_hidden`（panel/body 各自裁剪内容）；`popup_panel::the_panel_shadow_is_not_clipped_by_the_popup_content_root` |
| E-04 `constrained_size` 无 kit 消费者（P1-4 内部滚动未落地） | P1 | **fixed** | `PopupPanel::available_body_height` 读 `constrained_size`，Menu 列表/Dialog 内容区收敛高度并内部滚动、footer 固定；`kit::menu::a_small_viewport_shrinks_the_menu_and_scrolls_its_last_row_into_view`、`kit::dialog::a_tall_dialog_scrolls_its_content_and_keeps_the_footer_reachable`；§12.2 P1-4 已改写 |
| E-05 外部删除 popup content 后 `focus.owner` 悬挂 | P2 | **fixed** | `PopupStack::remove` 清 `focus.owner`；`validate_owner` 对缺失 entry/节点清 owner 并回落主树；`a_stale_focus_owner_is_repaired_when_the_next_tab_runs`、`removing_a_take_popups_content_node_restores_main_focus_and_tab`；D-P1-21 已改写 |
| E-06 `suppress_release` 窗口失焦不重置 | P2 | **fixed** | `app/input.rs::cancel_pointer_session`/`on_window_focus_lost` 清 `suppress_release`；`window_focus_loss_does_not_swallow_the_next_background_click` |
| E-07 Menu/ContextMenu 祖先滚动/resize 关链矩阵未实现 | P2 | **fixed** | 引擎新增 `popup_anchor_rect`/`popup_anchor_visible`/`window_scroll_epoch`/`Window::scroll_epoch`；kit `AnchorWatch/AnchorChange`：Menu/ContextMenu 在 Lost/Resized/Moved（及外部 Scrolled）`close_chain`，Dropdown/Select 重锚定/关闭，Tooltip 重定位/隐藏；`menu::an_ancestor_scroll_closes_the_whole_menu_chain`、`context_menu::scrolling_the_page_closes_the_open_context_menu`、`popup_behavior::popup_anchor_queries_track_scrolling_and_visibility`、`window_scroll_epoch_advances_only_when_an_offset_changes` |
| E-08 a11y 无法激活 Menu/Dropdown/Select 选项 | P2 | **fixed（r2 复核）** | `List::set_row_action` 给行节点绑定 Click（`list.rs:769-771,1142-1144`）；`ItemPanel` 接 `on_activate`（`menu.rs:282-284`）；`effective_actions` 对可点击节点补 `Click`（`a11y/mapping.rs:327-331`）；`menu::an_a11y_click_activates_the_menu_entry_it_targets`、`dropdown/select::an_a11y_click_chooses_the_option_it_targets`（均断言只激活一次）；指针路径经 `action_target` 无双重派发 |
| E-09 `AnchorEdge`/`match_anchor_width` 死字段 | P3 | **fixed** | `Anchor::Node` 改为 `{node}`、`AnchorEdge` 与 `PlacementSpec.match_anchor_width` 删除（`rg` 零引用，§12.9-8）；kit/测试构造点同步 |
| E-10 `ShowTooltip` 被广告但未处理 | P3 | **fixed（收窄）** | 引擎不再向 AT 广告 ShowTooltip（`a11y/popup.rs` 只对 Tooltip 广告 HideTooltip）；`a11y::popup_tests::a_tooltip_describes_its_trigger_and_at_hide_tooltip_closes_only_the_tooltip` 断言 trigger 的 actions 不含 ShowTooltip；引擎对无 handler 的 ShowTooltip 请求仍保留 `UnhandledAction` 诊断 |
| E-11 IME caret 未含 `paint_offset` | P3 | **fixed** | `window/focus.rs::text_caret_rect` 累计 `popup_transform.offset`（与焦点环同源）；测试 `a_child_popup_anchor_follows_the_parent_paint_offset` 等覆盖 transform 几何 |
| E-12 测试真实性/门禁覆盖缺口 | P3 | **partial（主要项 fixed，残余 P3）** | 删除 `[dbg]` eprintln；DPI 用例真正切换 DPR 1→2→1；slide 用例断言 FrameRecord 绘制位置跟随 `paint_offset`；新增 close 顺序、关闭期输入等强断言；round-3 又修 `popup_panel` at_least 命名、Menu 指针命名、caret+offset 直接断言；残余（变异自检 harness、`--pixel-check` 仅 ColorPicker 场景）见 §12.6-6 |
| E-13 §12 回写与代码不一致 | P3 | **fixed** | 本轮 §12.1/§12.2/§12.5/§12.7/§12.8/§12.9 已按最终代码与实测重写（行数/交付声称/命令/93→94 checks 等） |
| E-14 生产 `expect` 兜底 | P3 | **fixed（E-18 后口径澄清）** | 新增 popup 代码（`popup.rs`/`popup/*`/`app/popup.rs`/`a11y/popup.rs`/`PopupPanel`）生产 `expect` 为 0，Q1-03 同闭；**E-18 清除的 `Window::new`（`PopupStack::install`）`expect` 也已随 round-3 修复消失**；仓库其他 P1 之前既有生产 `expect`（如 `App::window_signal`、`input` pending drag、`app/frame` 测试辅助）不在本轮引入范围（A-34/B-11 同口径） |
| E-15 立即关闭 modal 的后代关闭顺序祖先先于子 | P3 | **fixed** | `app/popup.rs` 按子树深度降序（`Reverse(depth)`）通知/关闭；`immediate_modal_close_notifies_owned_descendants_children_first` |
| E-16 `raise_popup(modal)` 不重排 scrim z | P3 | **fixed** | `stack.rs::raise` 先移动 `scrim_of(modal)` 再移动子树；`raising_a_modal_moves_its_scrim_to_the_adjacent_z_slot` |

**kit（K1-01…K1-13）与质量（Q1-01…Q1-10）**

| Finding | 严重度 | 处置 | 证据/位置 |
|---|---|---|---|
| K1-01 = Q1-09 滚动/resize 关链矩阵 | P2 | **fixed** | 同 E-07（引擎查询 API + kit AnchorWatch） |
| K1-02 Menu/ContextMenu/Dropdown/Select 非模态 + outside Replay 未裁决 | P2 | **rejected（按用户裁决 D6/D14 保留）** | 用户已确认 D6（Auto 非模态）/D14（Replay）；登记 §12.3 D-P1-1/D-P1-2；旧“模态 + Consume”parity 不再保留 |
| K1-03 = Q1-10 Dropdown/Select 首帧测试空断言 | P2 | **fixed** | 测试改为 `press_release` 后只 `drain` 一帧取样再 settle 比较；`dropdown::the_first_visible_frame_already_has_the_settled_rect`、`select::...` |
| K1-04 = Q1-07 旧 `--dump-overlay` 静默 no-op | P2 | **fixed** | `parse_flags`：旧名打印改名提示并 **exit 2**，未知参数打印 usage 并 exit 2；§12.3 D-P1-8、kit-gallery §13.5 |
| K1-05 = B-13 默认按钮退化为“第一个 Button” | P2 | **fixed** | `App::set_popup_initial_focus`（E-21 后 `PopupSpec.initial_focus` 字段删除，仅保留 setter）；Dialog 打开后指定 confirm；`a_content_button_before_the_footer_does_not_take_the_default_focus`、`set_popup_initial_focus_moves_the_focus_within_a_trap` |
| K1-06 `sampled-animation-floor` 只保证累计下界 | P3 | **fixed** | 改为 open ≥4、close#1 ≥3、close#2 ≥3 三条分段断言（checks 93→94，实测 8 / 6+6） |
| K1-07 宿主 `popup_count()` 过滤 Scrim | P3 | **fixed（round-3）** | 4 个业务 ui + 2 个 widgets 测试 helper 新增 `popup_total()`（不过滤 Scrim，`entries.len()`），释放/关闭路径测试显式断言 total==0（instrument_selector 6 处、financial_index/coin_market_cap 等）；§12.6-14 留档 |
| K1-08 parity/覆盖缺口（a–g） | P3 | **fixed（b/e/f 均关闭）** | a Alert 叠放=设计决策（D-P1-4）；d ColorPicker role=Dialog 非模态=设计决策（D-P1-6）；g 200ms=设计值（D-P1-3）；c typeahead 属 P2（P2-4）；**b fixed**（Tooltip 显式 TOP 测试）、**f fixed**（Dialog resize recenter 测试）、**e fixed**（instrument Dialog 宽度/窄窗 clamp 测试：`the_selector_dialog_draws_its_designed_width_and_keeps_the_footer_reachable`、`a_narrow_window_clamps_the_selector_dialog_and_keeps_the_footer_reachable`） |
| K1-09 `PanelState.height` 死字段 | P3 | **fixed（结论反转）** | E-04 内部滚动落地后该字段是 `sync_height` 高度缓存，被 `menu.rs:562` 读取；不再是死字段（§12.6-14 留档） |
| K1-10 生产 `expect` | P3 | **fixed** | `color_picker.rs` 生产 `expect` 已删（剩余命中均在 `#[cfg(test)]`） |
| K1-11 `FadeSlide` 未启用 | P3 | **deferred** | kit 仍全 Fade/None；D-P1-19 登记 P3 |
| K1-12 宿主测试 settle 放宽 | P3 | **accepted（记录）** | 适配关闭动画推进的合理改动；强度略降，未回退（§12.6-8） |
| K1-13 `PopupPanel::release` 触发 `on_close` | P3 | **fixed** | `release` 先清 `on_close` 回调再 `close_now`；`popup_panel::release_does_not_report_a_close_callback` |
| Q1-01 = E-04 P1-4 内部滚动 | P2 | **fixed** | 同 E-04；§12.2 P1-4 已改写 |
| Q1-02 = E-09 死字段 | P3 | **fixed** | 同 E-09 |
| Q1-03 = E-14 生产 `expect` | P3 | **fixed** | 同 E-14 |
| Q1-04 widget 层 `DialogError::Popup(_)` 折叠为 `UnknownWindow` | P3 | **fixed（round-3）** | `widget::error::{dialog_engine_error,popup_engine_error}` 统一映射：`DialogError::Popup(e)` → 保留 `PopupError` 语义（ContentBuild/StackSync 取 source，其余映射为对应 `EngineError`），7 个宿主改为调用该 helper；§12.6-14 留档 |
| Q1-05 §12.1 行数系统性偏小（空行口径/7 路径 vs 14 文件） | P3 | **fixed** | §12.1 改用总行数（含空行）口径并列出 14 文件：删除 4350、新增 15334（round-3 最终）、diff 115 文件 +8671/−9410（`.docs` 除外；含 kit-gallery 为 116/+8692/−9415） |
| Q1-06 §12.9 矛盾 + §8 `PopupHost` 残留 + kit-gallery 未同步 | P3 | **fixed** | §12.9-2 改为「原始模式 1 命中 = CAMetalLayer::new，判据收紧后 0」；§8 两行改为“无 PopupHost/cx.popup_host，组件只收 theme”；kit-gallery §13.3/§13.5 已同步 |
| Q1-07 = K1-04 旧命令假绿 | P3 | **fixed** | 同 K1-04 |
| Q1-08 93 checks 覆盖差异未说明 | P3 | **fixed** | §12.3-9/§12.5/kit-gallery §13.5 补 93→94 构成、idle 期 background-residue 不再执行、floor 改为单次关闭分段 |
| Q1-09 = K1-01 | P2 | **fixed** | 同 E-07 |
| Q1-10 = K1-03 | P2 | **fixed** | 同 K1-03 |

**结论**：终审 3 份报告（第 1 轮）+ r2 复核（第 2 轮）的 **P0/P1/P2 已全部关闭**；P3 在 round-3 修复后剩余项（K1-11、K1-12 accepted、E-12 残余、R2-01/R2-02/R2-04、A-26/A-27/A-31、性能/三端等）逐条登记在 §12.6 与 §12.3-15；round-3 新修复与复核见 §12.11。

### 12.11 第 2 轮终审（r2）与 round-3 修复处置（2026-11-27）

> 依据：`implementation/review-p1-final-engine-r2.md`、`review-p1-final-kit-r2.md`、`review-p1-final-quality-r2.md`（10:27–10:55 复核）；下表为 round-3 修复（task-48/49/50）完成后的**最终代码状态**（代码自 11:19:13 起稳定）。最终门禁：`cargo test -p ui_core -p ui_kit` 3780 passed / 2 ignored、clippy/fmt/workspace check PASS、`--pixel-check` 94 checks、`--debug-stack` 自行退出 0。

| Finding | 严重度 | 最终状态 | 证据/位置 |
|---|---|---|---|
| **E-17** `raise_popup` 把外层 modal+scrim 抬到 owner-modal 后代之上 | P2 | **fixed** | `popup/stack.rs::raise` 收集 modal+scrim+子树+`owner_modal_descendants`（含各自 scrim），按既有 `order` 整组抬升保持组内相对顺序；`popup_behavior::raising_a_modal_keeps_its_owner_modal_chain_above_it`（外层 A+内层 B+无关 C，断言 topmost/输入/a11y 仍是 B） |
| **E-18** `Window::new` 的生产 `expect` | P3 | **fixed** | `PopupStack::install(id, root) -> Self` 改为不可失败（不再 `Result`/expect）；`Window::new` 直接组装；E-14/Q1-03/A-34/B-11 的“新增 popup 代码无生产 expect”口径已成立 |
| **E-20** 320×220 窄视口下默认 384 宽 Dialog 横向裁剪 | P3 | **fixed** | `PopupPanel::available_body_width` + Dialog 内容宽 clamp 到 viewport−margin，内容/滚动区同步；`dialog::a_narrow_viewport_clamps_the_default_dialog_width_and_keeps_the_buttons_reachable` |
| **E-21** `PopupSpec.initial_focus` 公开字段不可达/无生产写入 | P3 | **fixed** | 删除该字段（`popup.rs` 无 `initial_focus`），初始焦点统一走 `App::set_popup_initial_focus`；§4.2/§12.3-26/§12.4 已同步 |
| **E-13** §12/§4.5 文档残余（旧 overflow_hidden/旧状态/旧计数） | P3 | **fixed** | §4.5 改为“内容根不裁剪、panel/body 各自裁剪”（E-03）；§12.1/§12.5/§12.6/§12.10/§12.11 按最终代码重测（3780/801、115/+8671/−9410）；E-08/K1-07/K1-08/K1-09/Q1-04 状态更新；生产 `expect` 声明同步（E-14/E-18） |
| **E-12 残余** | P3 | **partial（已登记）** | 见 §12.6-6；round-3 修了 at_least 命名/Menu 指针命名/caret+offset 直接断言，余变异自检 harness 与 pixel 覆盖（P3-4） |
| **K1-07** 宿主 `popup_count()` 过滤 Scrim | P3 | **fixed** | 4 业务 ui + 2 widgets helper 新增 `popup_total()`（`entries.len()` 不过滤），释放/关闭路径断言 total==0（instrument_selector 6 处、financial_index、coin_market_cap 等） |
| **K1-08 b/e/f** | P3 | **fixed** | b：`tooltip::an_explicit_top_placement_puts_the_tooltip_above_the_trigger`；f：`dialog::resizing_the_window_recenters_the_dialog`；e：instrument Dialog 设计宽度 + 窄窗 clamp 两条测试 |
| **K1-11** `FadeSlide` 未启用 | P3 | **deferred（已登记）** | §12.6-7；kit 全组件仍 Fade/None（D-P1-19） |
| **K1-12** 宿主测试 settle 放宽 | P3 | **accepted（记录）** | §12.6-8 |
| **R2-01** 受约束 popup 首帧内部滚动高度未 settle | P3 | **deferred（已登记）** | §12.6-9；`sync_height`/`sync_scroll` 仅在 sync 生效，默认 Fade 下风险低 |
| **R2-02** anchor 矩阵关闭 reason 固定为 `Programmatic` | P3 | **deferred（已登记）** | §12.6-10；生产 `on_close` 消费者为 0，P2 MenuChain 前补 `close_with_reason` |
| **R2-04** 无 `.confirm()` 的自建 footer 初始焦点回退 Cancel | P3 | **accepted（D16 fallback）** | §12.6-11；可后续加 `DialogBuilder::initial_focus` |
| **R2-05** 观察项（scrim 打开期主题刷新/滚动条覆盖/首帧采样） | 观察 | **registered** | §12.6-12 |
| **Q2-01** 设计 §12 / p1-summary 停在 E-08 前快照 | P3 | **fixed** | 本轮 §12.1/§12.5/§12.6/§12.7/§12.10/§12.11 与 p1-summary 全部按 11:19 最终代码重测 |
| **Q2-02** `p1-cleanup-w7.md` 仍记 93 checks/W7 数字 | P3 | **fixed** | 该文文首“终审后注记”+ §4/§6 行内注记（93→94、3734→3780/773→801、命令语义） |
| **R2-03** §12 状态/计数不一致 | P3 | **fixed** | 3777→3780、ui_kit 798→801、Dropdown 27/Select 32/Menu 30、+217→+226、E-08/K1-09/K1-08 状态已更新 |
| **A-25**（早审 P2）静态 aria-haspopup / active_descendant 未由 kit 声明 | P2 | **fixed（静态 round-3；active_descendant 由 P2 关闭，§13.5）** | Menu 子菜单行（`has_popup: Some(HasPopup::Menu)`）、Dropdown（Menu）、Select（ListBox）关闭态静态声明 + 打开期 expanded 投影；`menu::a_submenu_row_keeps_static_haspopup_and_projects_expanded`、`dropdown/select::the_trigger_reports_the_selection_and_the_expanded_state`；`active_descendant` kit 仍未写入（§12.6-15，引擎通道有测试） |
| **A-29**（早审 P2）子菜单 a11y 默认父是内容根 | P2 | **fixed** | Menu 打开子菜单时写 `PopupA11y.a11y_parent = 父 menuitem`；`menu::a_two_level_submenu_chain_attaches_to_its_parent_menu_items`（2/3 级链断言） |

**r2 结论对齐**：引擎 r2 的 P2 E-17 与 kit r2 的 P0–P2 全 0；quality r2 的 P3 文档项 Q2-01/Q2-02 由本轮关闭；剩余开放项均为 §12.6 的 P3/观察项（已逐条登记）。

## 13. P2 实施结果与偏差登记（2026-11-27）

> 范围：设计 §6 P2-1…P2-7（§4.8 菜单链状态机、§4.7 链式 dismiss、§3.4 数值、§4.10 a11y、§7.3 组件测试）。
> 基线：P1 已提交 `152616b1`；P2 全部改动在同一 worktree 的未提交工作树。
> 结论：**P2-1…P2-7 落地（P2-6 为部分 done：右键自动绑定留 P3，见 D-P2-4）**；只改 `ui_kit`（`menu.rs` / 新增 `menu_chain.rs` / `context_menu.rs` / `popup_panel.rs` / `gallery.rs`）+ 引擎小补（`PopupA11y.active_descendant` 投影；非法目标清空 + `A11yDiagnostic` 诊断，不新增错误变体）。详细任务/API/测试清单见 `implementation/p2-summary.md`，本节给出回写口径与偏差。
>
> **审查状态（2026-11-27）**：P2 审查 `review-p2-menu.md`（M2-01…M2-10）与 `review-p2-quality.md`（Q2-01…Q2-09）的阻塞项（M2-01/02/03）与 Q2-03（删除退化的桥接几何）由 task-58 修复；本文按 task-58 最终代码口径回写；数字为最终门禁实测。

### 13.1 提交内容与规模

- 代码 diff（`git diff --shortstat HEAD -- . ':(exclude).docs'`，总行数含空行）：**14 tracked 文件，+5311 / −1475**；另有 **1 个新文件** `crates/ui_kit/src/kit/menu_chain.rs` **959 行**（untracked）。
- 主要文件：`menu.rs` +4424/−1449（6012 行，76 测试）；`menu_chain.rs` 新增 959 行（13 测试）；`context_menu.rs` +348/−8（1147 行，20 测试）；`gallery.rs` +171/−8（4062 行，3 级链 overlay-check）；`popup_panel.rs` +21/−6（`surface_action`/`on_dismiss`）；`dropdown.rs`/`select.rs` 各 +1 内部字段；`kit/mod.rs` +1（公开 `menu_chain`）。
- 引擎小补：`popup.rs` +1 字段（`PopupA11y.active_descendant`；`PopupError` 仍 13 变体）；`app/popup.rs` +36/−4（打开/更新时校验、非法目标清空 + `A11yDiagnostic::InvalidActiveDescendant`）；`a11y/{popup,tree}.rs` +6（声明值优先于焦点推导）；`a11y/popup_tests.rs` +145（+1 测试）；`tests/popup_behavior.rs` +152（+1 测试）。
- 任务：task-52（P2 实施）、task-53（引擎 `PopupA11y.active_descendant`）、task-55（active_descendant 落点 + safe-triangle Ignore）全部 completed；P2 审查修复由 task-58 落地（M2-01/02/03 阻塞项、Q2-03 删除桥接几何、M2-04/05/06/07 等 P3）；P2 收口 task-62 补齐测试真实性（恒真断言、derived_safe_triangle 隔离、failed-open `panel.open` 分支、ContextMenu `open_on`/重复绑定/`on_item_click` check、reconcile pointer-close）。

### 13.2 P2 任务结果（P2-1…P2-7）

> 注：下列测试名/证据按 task-58 最终代码。

| # | 结果 | 关键证据 |
|---|---|---|
| P2-1 `MenuChain` 状态机 | **done** | 新模块 `menu_chain.rs`：`LevelId`、`LevelState{Closed,Opening,Open,Closing}`、`OpenSource{Pointer,Keyboard,Click}`、每层 `parent/child/row/rect/anchor/exit/grace/active`；`open_root/begin_open/confirm_open/cancel_open/close/mark_closing/reopen/revive/deepest_open/deepest_kept/open_levels/closing_levels`；关闭先子后父（`the_chain_assigns_states_per_level_and_closes_children_first`）、pending open 可取消复用（`cancelling_a_pending_open_returns_the_slot_to_closed_and_reusable`）；**`mark_closing/reopen/revive` 与引擎 `DismissPending/Closing`/Toggle 同步**（`a_trigger_press_during_the_close_transaction_reopens_the_menu`）；**open 失败 `reset` 并清理 level**（`a_failed_open_resets_the_chain_and_the_retry_keeps_submenus_working`）；**root `Opening` 期间跳过 anchor watch**；grace 关闭后同一行重入重新 PendingOpen（`reentering_the_same_submenu_row_after_the_grace_close_reopens_it`）；**M2-02R 父层复位口径**：子层关闭时仅当 `parent.last_hovered == Some(closed_row)` 且指针已离开该行（`!over_row`）才清 `last_hovered/trigger_over` 并把父层 active 指回该行（`menu.rs` `panel.on_close` 分支）；timer 绑定具体 level |
| P2-2 safe triangle / grace | **done** | 5 点多边形（退出点 ±5px bleed 镜像 + 子菜单近/远端上下角）；打开/翻面/滚动重算（`compute_grace`）；**grace 300ms 自 open/几何变化起算并过期清多边形**（`arm_grace_timer/rearm_grace_timer`，M2-06）；兄弟替换保护（`pointer_through_the_safe_triangle_keeps_the_child_and_ignores_siblings`、`a_pointer_outside_the_triangle_replaces_the_open_sibling`、`leaving_the_chain_closes_the_pointer_opened_levels_after_the_grace`、`reentering_the_chain_during_the_grace_cancels_the_close`）；外部按下落在 **grace 三角 ∪ 即时 safe triangle** 内返回 `DismissOutcome::Ignore`（D-P2-2；无桥接几何；`a_press_inside_the_safe_triangle_keeps_the_chain_open`、`a_press_outside_the_safe_triangle_closes_the_chain_and_replays`） |
| P2-3 菜单面板行为 | **done** | active_item 高亮 + ensure_visible/滚动；子菜单 `IconKind::CaretRight` 指示；Group（a11y `Label`）/Separator（a11y `Separator`、不聚焦）/disabled；Checkbox/Radio 勾选渲染与 role（`MenuItemCheckBox`/`MenuItemRadio`）；长菜单内部滚动；父行滚出视口 = **整链关闭**（`scrolling_the_parent_row_out_of_view_closes_the_whole_chain` 断言 `!menu.is_open() && stack.is_empty()`，M2-05 统一） |
| P2-4 完整键盘表 + typeahead + checkable | **done** | ↑↓/Home/End/PageUp/PageDown/←→/Enter/Space/Esc/Tab/Shift+Tab/Alt+F10；`the_keyboard_table_pages_and_jumps_inside_a_long_menu`、`right_opens_the_submenu_with_focus_and_left_returns_to_the_parent_row`、`tab_closes_the_chain_and_moves_focus_past_the_opener`、`alt_f10_on_the_trigger_opens_the_menu_with_the_first_row_active`；typeahead 2s 单层、换层清空、跳过 disabled/separator（`typeahead_cycles_matches_skips_disabled_and_appends_characters`、`typeahead_appends_characters_and_expires_after_the_timeout`、`a_text_event_never_feeds_the_typeahead_buffer`）；checkable：Space 切 Checkbox 不关、Enter 选择关链、Radio 同层互斥（`space_toggles_a_checkbox_without_closing_and_enter_selects_and_closes`、`radio_members_are_mutually_exclusive_and_expose_their_roles`）；**typeahead 超时按层清理**（`clear_typeahead_for(level)`，timer 绑定 level，只清该层 buffer） |
| P2-5 链式 dismiss | **done** | 命中祖先层只关后代（`a_press_on_an_ancestor_level_closes_only_its_descendants`）；Esc 逐层 children-first（`escape_closes_one_level_at_a_time_children_first`、`escape_returns_focus_to_the_parent_level`）；D14 链外 Replay/显式 Consume（`a_consuming_menu_does_not_replay_an_outside_press`）；safe triangle/grace 内 Ignore 不关链（无桥接几何）；tooltip 优先沿用 P1 引擎 Hint tier |
| P2-6 ContextMenu | **部分 done（右键入口见 D-P2-4）** | `ContextMenuOptions{outside_press}`、`open_on`（节点底边左端点）、`bind_trigger`（**仅键盘 Shift+F10/ContextMenu 键**，`shift_f10_on_the_focus_node_opens_the_menu_at_the_node`）；WindowRect 指针锚与节点锚；2–3 级共享 MenuChain（`a_context_menu_chain_opens_three_levels_and_closes_children_first`、`a_submenu_stacks_above_the_root_and_escape_closes_it_first`）；外部 Replay/Consume 与页面滚动关链（`a_press_outside_closes_the_menu_and_replays_the_press_to_the_page`、`a_consuming_context_menu_swallows_the_outside_press`、`scrolling_the_page_closes_the_open_context_menu`）；**`on_item_click` 新签名 `Fn(&mut App, SmolStr, Option<MenuCheckState>)`**；trigger 可聚焦由宿主负责（D-P2-4） |
| P2-7 3 级 + a11y + gallery | **done** | 3 级几何/hover 级联（`a_three_level_hover_cascade_stacks_newest_on_top`）、3 级 a11y 链（`a_three_level_a11y_chain_nests_and_projects_expanded`、2/3 级父=父 menuitem）、expanded/haspopup（`a_submenu_row_keeps_static_haspopup_and_projects_expanded`）；gallery `--debug-stack` overlay-check 新增 chain-open/stack-size/z-order/tier/parent-level2/parent-level3/close/entries-zero 阶段 |

### 13.3 API 变化（P1 → P2）

- **`MenuOptions`**：删除 `submenu_open_delay: Duration`；新增 `timings: HoverTimings` 与 `outside_press: OutsidePress`（默认 `Replay`），builder 同名方法。
- **`HoverTimings`**（`ui_kit::kit::menu_chain::HoverTimings`，`pub`）：`open_delay=200ms`、`close_grace=300ms`、`typeahead_timeout=2s`（`Default` 即设计 §3.4 值；可配）。
- **`MenuAction`**：`checked: Option<bool>` → `check: Option<MenuCheckState>`；`checked(bool)` → `checkbox(bool)` / `radio(bool)`；新类型 `MenuCheckState { kind: MenuCheck{Checkbox,Radio}, checked: bool }`。
- **`MenuActivation`**：`{ id }` → `{ id, check: Option<MenuCheckState> }`；`Menu::on_activate` 回调携带 check 状态。
- **`Menu`** 新增 `open_submenu(app, row) -> Result<Option<Menu>, PopupError>`（程序化打开子层，供 2–3 级链/测试/gallery）与 `bind_trigger(app, window, trigger) -> Result<(), EngineError>`（**仅键盘 Alt+F10**；重复绑定先释放旧 action；锚点取 trigger 节点矩形；trigger 需已可聚焦，鼠标绑定由宿主负责）；其余公开面不变。
- **`ContextMenuOptions`** 新增 `outside_press: OutsidePress`；**`ContextMenu`** 新增 `open_on(app, window, node)`（取节点**底边左端点**为 WindowRect 锚；节点不存在返回 `PopupError::AnchorNodeMissing`）与 `bind_trigger(app, window, trigger)`（**仅键盘 Shift+F10/ContextMenu 键**，无裸 F10；重复绑定先释放旧 action；左键/右键由宿主绑定）。
- **引擎 `PopupA11y.active_descendant: Option<NodeId>`**：投影到引擎创建的 popup content 根 a11y 节点；目标须在 content 子树内或 `owns` 内；`App::set_popup_a11y` 同帧更新并标脏；非法目标采用“清空 + 诊断”路径（`A11yDiagnostic::InvalidActiveDescendant`），`PopupError` 仍 13 变体（不新增错误面）。
- **`ContextMenu::on_item_click`**：`Fn(&mut App, SmolStr)` → `Fn(&mut App, SmolStr, Option<MenuCheckState>)`（回调携带菜单项 check 状态，ContextMenu 与 Menu 的 checkable 语义一致）。
- **`PopupPanel`（内部）**：`OpenRequest.surface_action: Option<Action>`（**Menu 自己传入**给 `ItemPanel::open`，使 panel surface 承载行点击）；ContextMenu 无额外挂载，Dropdown/Select 传 `None`。

### 13.4 行为偏差登记（D-P2-1…D-P2-10）

| ID | 偏差 | 决策与证据 |
|---|---|---|
| D-P2-1 | `active_descendant` 落点 = popup content 根（role=Menu），不再用 Menu surface 的 `A11yPatch` | P1 的 surface 兜底删除；引擎声明值优先于焦点推导（`a_declared_active_descendant_is_projected_updated_and_cleared`、`popup_a11y_active_descendant_is_validated_and_cleared`、`the_active_descendant_follows_the_focused_menu_item`） |
| D-P2-2 | 外部按下落在 **grace 三角 ∪ `derived_safe_triangle`** 内 → `DismissOutcome::Ignore`（不关链、不 replay） | Q2-03 后**不保留 6 点桥接几何**；`MenuChain::safe_area_contains` = 当前链各层 grace 多边形 ∪ `derived_safe_triangle`（常驻，由 anchor/rect 即时计算），grace 300ms 自 open/几何变化起算、到期只清 grace（M2-06）；链外默认 Replay（D14），显式 `outside_press(Consume)` 消费 |
| D-P2-3 | 长菜单 anchor 变化一律关整链（M2-05 统一，不再声称“内部滚动豁免/只关子级”） | `sync_anchor`：`Lost/Resized/Moved` → `close_chain`（含内部 List 滚动导致父行滚出视口）；`Scrolled` 仅 ContextMenu 的 `closing_on_scroll()` 显式关链；`scrolling_the_parent_row_out_of_view_closes_the_whole_chain` 断言整链关闭 |
| D-P2-4 | ContextMenu 未内建 secondary-button 自动绑定；`bind_trigger` 仅键盘入口 | `Menu::bind_trigger` 仅 Alt+F10；`ContextMenu::bind_trigger` 仅 Shift+F10/ContextMenu 键（裸 F10 未绑定）；**trigger 可聚焦/左键/右键均由宿主负责**（`bind_trigger` 只装键盘绑定与重复绑定释放）；右键调用 `open(point)`/`open_on(node)`；登记 P4 候选 |
| D-P2-5 | typeahead 不消费 text/IME 事件 | 只绑定 `KeyCode`+Modifiers 的可打印键 action；IME 组合不会进入 buffer（`a_text_event_never_feeds_the_typeahead_buffer`）；桌面键盘路径覆盖，软键盘/输入法直投字符不参与 |
| D-P2-6 | 键盘焦点归属：hover 不夺焦；首次键盘事件把 focus 移入最深 hover 层 | `a_hover_opened_child_keeps_its_own_active_item_while_the_pointer_moves`；`active_descendant` 只引用本层 item |
| D-P2-7 | `MenuChain` 状态命名与设计不同（`Opening/Closing` + per-layer timer） | 设计写 `PendingOpen(timer,level_id)/PendingClose(timer,level_id)`；实现 `LevelState{Closed,Opening,Open,Closing}` + `Menu` 的 pending/grace/close timer（仍绑定 level，语义等价，不引入全局 topmost） |
| D-P2-8 | Radio 互斥范围 = 同一菜单层的 radio 列表（无 group id） | `toggle_checked` 清空同层其它 radio；a11y role/checked 同步；设计未要求跨层 group |
| D-P2-9 | Menu 与 ContextMenu 滚动关闭语义差异显式化 | Menu 默认 `scroll_closes=false`（anchor Lost/Resized/Moved 一律关整链，含内部滚动；见 D-P2-3）；ContextMenu 构造时 `closing_on_scroll()`（外部页面滚动关整链）；P1 矩阵延续 |
| D-P2-10 | Group/Separator 的 a11y role 与设计 §4.10 对照 | **P3 已修正（P3K-03）**：Separator=`A11yRole::Separator` → accesskit `Role::Splitter` + `hidden`（设计 “Splitter + 不可聚焦/hidden” 本已满足）；Group 行由 `A11yRole::Label` 改为 `A11yRole::Group`（菜单组头用 Group 语义，`group_and_separator_rows_expose_their_a11y_roles` 固化）；无 accesskit 角色缺失，无 P4 待办 |

### 13.5 P1 §12.6 P2 遗留关闭

| P1 §12.6 项 | P1 状态 | P2 结果 |
|---|---|---|
| P2-1 `MenuChain` 状态机 | 未开始 | **closed**（§13.2：含 mark_closing/reopen/revive、open 失败 reset、root Opening 跳过 anchor watch） |
| P2-2 5 点 safe triangle/grace | 未开始 | **closed**（5 点 + 300ms grace（自 open/几何变化起算并过期）+ grace 三角∪即时 safe triangle 内 Ignore；无桥接几何） |
| P2-3 面板行为 | 部分 | **closed**（高亮/滚动/指示/分组/分隔线/勾选） |
| P2-4 键盘表 + typeahead + checkable | 部分 | **closed**（PgUp/PgDn/Alt+F10/typeahead/checkable） |
| P2-5 链式 dismiss（kit 几何 Ignore） | 引擎侧完成 | **closed**（`on_dismiss` + safe triangle/grace Ignore） |
| P2-6 ContextMenu（WindowRect/键盘/子菜单共享链） | 部分 | **mostly closed**（键盘入口 `bind_trigger`=Shift+F10/ContextMenu 键；`on_item_click` 传 check；右键/左键与 trigger 可聚焦由宿主负责，见 D-P2-4） |
| P2-7 3 级测试 + a11y 链 + gallery 演示 | 未开始 | **closed** |
| A-25 残余 `active_descendant` | P3 残余 | **closed**（D-P2-1） |
| P1 只有 1 级子菜单 / 缺 typeahead/PgUp/PgDn/Alt+F10 | — | **closed** |

### 13.6 测试与门禁基线（P2；本机 Windows 11 / D3D12 / 125% DPI，2026-11-27；P3 最终基线见 §14.5）

| 门禁 | 结果 |
|---|---|
| `cargo test -p ui_core -p ui_kit` | **3847 passed / 0 failed / 2 ignored**（ui_core lib 2805 + 1 ignored、frame_record 8、gui_smoke 0 + 1 ignored、layout 4、popup_behavior 111、snapshot 14、demo 39、ui_kit lib 866）。P0 基线 3554 → **+293**；P1 3780 → **+67**（ui_kit 801→866、popup_behavior 110→111、ui_core lib 2804→2805） |
| `cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings` | PASS |
| `cargo +nightly fmt --all -- --check` | PASS |
| `cargo check --workspace --all-targets` | PASS |
| `gallery --pixel-check` | **PASS**，`checks=94`（32 sampled + 16 background-residue + 46 static）、`close 6+6=12`、`damage_full_frames=0`，exit 0 |
| `gallery --debug-stack` | **PASS**，overlay-check 含 3 级链阶段（chain-open/stack-size/z-order/tier/parent-level2/level3/close/entries-zero）；menu 度量/`menu_frames`/`picker_frames` 运行间浮动（只断言 >0），exit 0 |

逐文件 `#[test]` 计数：引擎 `popup/tests.rs` 43、`popup/placement.rs` 34、`tests/popup_behavior.rs` 111、`a11y/popup_tests.rs` 22；kit `menu` 76、`menu_chain` 13、`context_menu` 20、`popup_panel` 18、`dropdown` 27、`select` 32、`dialog` 54、`tooltip` 28、`alert` 11、`color_picker` 80。

复跑命令：

```powershell
$env:CARGO_TARGET_DIR='E:\kytrix\target'
cargo test -p ui_core -p ui_kit
cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings
cargo +nightly fmt --all -- --check
cargo check --workspace --all-targets
cargo run -p ui_kit --example gallery -- --pixel-check
cargo run -p ui_kit --example gallery -- --debug-stack
```

### 13.7 剩余 P3/待复核

| 项 | 状态 | 去向 |
|---|---|---|
| K1-11 `FadeSlide` 未启用 | P3 deferred | P2 菜单仍 Fade/None；§12.6-7 |
| E-12 测试真实性残余（菜单指针测试命名、变异自检、pixel 覆盖、caret+offset 直接断言） | P3 | §12.6-6 |
| R2-01 受约束 popup 首帧内部滚动高度未 settle | P3 deferred | §12.6-9 |
| R2-02 anchor 矩阵关闭 reason 固定 `Programmatic` | P3 deferred | §12.6-10 |
| R2-04 无 `.confirm()` 自建 footer 初始焦点回退 Cancel | P3 accepted | §12.6-11 |
| R2-05 观察项（scrim 主题刷新/滚动条覆盖/首帧采样） | 观察 | §12.6-12 |
| A-26 混合 a11y 父链环；A-27 `MaxSize` 命名；A-31 `FrameRecord.popup_damage/z_changes` | P3 | §12.6-1/3/5 |
| 外部滚动语义宽于“祖先滚动”字面 | P3 文档精度 | §12.6-13 |
| D-P2-4 ContextMenu 右键自动绑定 | P3 候选 | §13.4 |
| D-P2-5 typeahead 不消费 text/IME 字符 | 行为限制 | §13.4 |
| P2 审查 P3（M2-04 bind_trigger 仅键盘/宿主可聚焦、M2-05 内部滚动=整链关闭、M2-06 grace 300ms 过期、M2-07 active_descendant 文案与 `set_popup_a11y` 幂等性观察） | **fixed（task-58）/已回写**；M2-07 幂等性仍 P3 观察 | §13.3/§13.4 |
| D-P2-10 Separator/Group a11y role（Separator vs Splitter、Group=Label） | **fixed（P3，P3K-03）**：Separator→Splitter 本已满足；Group 改 `A11yRole::Group` | §13.4/§14.2 |
| P2 收口 task-62 补齐的测试（恒真断言修复、derived_safe_triangle 隔离、failed-open `panel.open` 分支、ContextMenu `open_on` 错误/重复绑定/`on_item_click` check、reconcile pointer-close） | **fixed（task-62）** | §13.1/§13.2 |
| Q2-07 kit 主动关闭 reason 固定 Programmatic（R2-02 未闭环） | P3 deferred（映射未实现） | §12.6-10 |
| Q2-09 gallery/文档交接（kit-gallery P2 未同步） | fixed（task-59 §13.6） | kit-gallery.md §13.6 |
| P3-1 Dialog recenter 删除、P3-2 `Scale` 动画、P3-5 性能基线、P3-6 三端真实窗口、P3-4 gallery 真实像素/截图 | **fixed（P3，§14）** | §14 |
| menubar / MessageBox | 范围外 | D9/D11 |
### 13.8 与设计正文对齐摘要

- §4.2：`PopupA11y` 增 `active_descendant`（P2）；`PopupError` 仍 13 变体（非法目标清空 + 诊断，不新增错误面）。
- §3.4：300ms grace 已落地（`HoverTimings.close_grace`；自 open/几何变化起算并过期），200ms/2s 可配；外部按下在 grace 三角∪`derived_safe_triangle` 内 Ignore（无桥接几何；`derived_safe_triangle` 常驻，grace 300ms 到期只清 grace）。
- §4.7：链外外部按下仍 Replay；grace 三角∪`derived_safe_triangle` 内 Ignore（D-P2-2）。
- §4.8：状态机/timer/兄弟替换/右键入口与实现对照（D-P2-4/D-P2-7）；§4.8 已加“P2 落地口径（rev.9）”；`on_item_click` 新签名、grace 过期、链 `mark_closing/reopen/revive` 见 §13.2/§13.3。
- §4.10：`active_descendant` 落点 = popup content 根（role=Menu），不再用 surface patch（D-P2-1）。
- §5.2：Menu/ContextMenu API 表已更新（HoverTimings/MenuAction.check/MenuActivation.check/open_submenu/bind_trigger/open_on/outside_press、`ContextMenu::on_item_click` 新签名）；`MenuChain` 由“P1 未落地”改为 P2 已落地。§13.3 记录 API 差异，§13.4 记录 D-P2-1…D-P2-10 行为偏差。
- §7.3：Menu/ContextMenu 测试落点已更新（P2 覆盖清单）。
- §12.6：P2/P3 衔接表的 P2 行保留 P1 当时状态；P2 结果见本节（新增交叉注记）。

## 14. P3 实施结果与偏差登记（2026-11-27）

> 范围：设计 §6 P3-1…P3-7（Dialog/Alert 收口、`Scale` 动画/D10、ColorPicker 复核、gallery 真实像素、性能基线、三端真实窗口验收、文档回写）。基线：P2 已提交 `f24a3441`；P3 全部改动在同一 worktree 的未提交工作树中，由 lead 统一提交并附本节。

### 14.1 提交内容与规模

- P3 提交（相对 P2 HEAD `f24a3441` 至最终 HEAD `35c9381d`）：**45 files changed，+7839 / −558**——`crates/` 38 files +7432/−556（含 `ui_core`/`ui_kit`/`settings_ui`/`list` 等最终审查修复）、`tools/` 5 个脚本 **+377**（`macos-gates-runner.sh`、`real-pixel-check-{windows.ps1,macos.sh,linux.sh}`、`record-source-id.ps1`）、tracked `kit-gallery.md`/`.gitignore`；提交链 `fa41987c → 5df859b8 → 5374c4c8 → 00927ff9 → 35c9381d`（HEAD，clean）；另有新增脚本 `tools/real-pixel-check-windows.ps1`、`tools/real-pixel-check-macos.sh`、`tools/real-pixel-check-linux.sh`、`tools/macos-gates-runner.sh`（已加入工作树，diff 另计）。
- 引擎：`popup.rs`（`PopupAnimation::FadeScale`、`POPUP_SCALE_FROM=0.98`、`PopupAnimationState.scale_from/scale_to`、`PopupEntry::paint_center`、`PopupSnapshotEntry.paint_scale`）、`popup/animation.rs`（scale 插值与 `PopupAnimationTick`）、`popup/dismiss.rs`（关闭动画的 scale/pivot，FadeScale 非模态反向到 0.98）、`popup/settle.rs`（pivot 跟随 paint center）、`paint/pass.rs`（`PopupScale` lane transform 与 damage 契约）、`node/tree.rs`/`arena.rs`/`content.rs`（`PopupTransform.offset/opacity/scale/pivot/blocked` + `paint_scale` 侧表）、`app/{frame,popup}.rs`（`FrameStats.frame_cpu/popup_opening/popup_closing`、`paint_scale` 快照）、`test_support/record.rs`。
- kit：`menu.rs`（`ItemPanel` 打开动画改 `FadeSlide`，Menu/ContextMenu/Dropdown/Select 共用；`close_with_reason`；动画期间 anchor watch 忽略 `Moved`）、`context_menu.rs`/`dialog.rs`/`dropdown.rs`/`select.rs`/`color_picker.rs`（锚点丢失上报 `DismissReason::AnchorLost`、显式 reason 的关闭；Dialog 删除 `recenter`、Dialog/ColorPicker 改 `FadeScale`）、`popup_panel.rs`（`close_with_reason`）。
- gallery：新增 `--real-pixel-check` 真实窗口截图验收（`gallery.rs` **+3963/−7**，含标记检测/截图命令/像素断言/负向自检/报告与证据落盘）；三条命令语义见 §14.4/§14.5。
- 任务：P3-1…P3-7 全部 completed；本轮回写只改 `.docs`。

### 14.2 P3 任务结果（P3-1…P3-7）

| # | 结果 | 关键证据 |
|---|---|---|
| P3-1 Dialog/Alert 收口 | **done** | 删除 `Dialog::recenter`（resize 不再回到新视口中心，`resizing_the_window_keeps_the_anchor_opened_with`）；fade=true 时切 `FadeScale`（modal fade-only，`the_modal_dialog_degrades_fade_scale_to_fade_only`）；FocusPolicy::Trap 初始焦点/Tab/outside/escape/restore 已在 P1 完成（D16）。Alert 是 Dialog 内容面板，不新建 popup（D9/D11） |
| P3-2 `FadeScale`（D10） | **done** | 引擎新增 `PopupAnimation::FadeScale` + lane transform scale：非模态 0.98→1、120ms、EaseOutCubic、以 popup paint center 为 pivot；关闭 1→0.98、90ms；**modal fade-only**（不缩放，`the_modal_dialog_degrades_fade_scale_to_fade_only`/`modal_fade_scale_is_fade_only`）；`settle` 同帧同步 pivot 但**不 mark_paint**；damage 用 layout rect 平移超集（scale≤1 向内）；dpr 1/2 几何一致（`fade_scale_paints_the_same_logical_geometry_at_dpr_one_and_two`）。**`FadeSlide` 启用 Menu/ContextMenu/Dropdown/Select**（四者都经 `ItemPanel::open` 设置 `FadeSlide`；`the_menu_slides_in_without_moving_the_settled_rect`） |
| P3-3 ColorPicker 复核 | **done** | 非模态取色器改 `FadeScale`（首帧 0.98→1、settle rect 不变、局部 damage、关闭帧覆盖旧 rect；`the_panel_fades_and_scales_in_without_moving_the_settled_rect`）；锚点丢失关闭上报 `DismissReason::AnchorLost`；内容闭包/局部坐标/tab `request_measure`/open-close 回调沿用 P1 复核 |
| P3-4 gallery / 真实像素 | **done** | `--real-pixel-check`：960×720 always-on-top 窗口 + 内容边缘 6 px `#FF00FF` 标记环；全屏截图解码后由标记给出内容原点/物理尺寸/DPR，按窗口 scale 映射 popup rect；序列 baseline → ColorPicker open/close → 3 级 MenuChain open/close → Dialog+scrim open/close，打开必须改变 popup 区域、关闭必须逐像素恢复（coverage floor；触发器环排除）；偏移/改色负向自检必须判红；exit 0/3，证据 `report.txt` + 逐阶段 PNG；`--pixel-check` 仍为 headless 94 checks（32 sampled/8 open/6+6 close/damage_full=0） |
| P3-5 性能基线 | **done** | `FrameStats.frame_cpu/popup_opening/popup_closing/popup_animation_records`；`PopupSnapshotEntry.paint_scale`；实测 **idle p95≈0.6–1 µs；open 首帧冷启动 p95≈1.0–1.13 ms（含 atlas/text 冷启动、小样本 p95≈max），其余帧中位≈29 µs；close 中位≈26 µs、p95≈64–138 µs（尾部）**；`damage_full=false`；**守卫用中位帧 ≤ idle p95+2 ms（与设计目标一致）**，p95 仅记录；稳定后空闲帧 0 settle/0 damage/0 popup 工作（`idle_frames_do_no_popup_settle_or_damage_work`、`popup_open_and_close_frames_stay_within_the_frame_budget`；P3E-03/P3E-04 修复后口径） |
| P3-6 三端真实窗口验收 | **done** | **三端 HEAD `35c9381d` clean 各 106 checks/0/PASS**：Windows 45.9 s/frame 280（隔离 43.3/277）、macOS 72.2 s/frame 283、Ubuntu 20.3 s/frame 311；交互验收 hover/safe-triangle/sibling/Esc/外部点击/键盘/滚动；identity 三端一致（`diff_sha=e69de29…`, source-id-file）；证据见 §14.4。三端 pixel-check 94/idle/theme/debug-stack 均 PASS；证据与 run.log 见 §14.4 |
| P3-7 文档回写 | **done** | 本节 + `implementation/p3-summary.md` + `.docs/gui-migration/spec-deltas/kit-gallery.md` §14；§5.2/§7.4/§10 已同步 |

### 14.3 引擎实现口径

- **动画枚举与常量**：`PopupAnimation { None, Fade, FadeSlide, FadeScale }`；`POPUP_OPEN_DURATION=120 ms`、`POPUP_CLOSE_DURATION=90 ms`、`POPUP_SLIDE_DISTANCE=4 px`、`POPUP_SCALE_FROM=0.98`。
- **`PopupTransform`（`pub(crate)`）**：`{ offset: LogicalPoint, opacity: f32, scale: f32, pivot: LogicalPoint, blocked: bool }`；`paint_scale()` 对非法值回退 1.0 并 clamp 到 `[0,1]`。pivot = `PopupEntry::paint_center()`（paint rect 中心，空则 hit rect 中心）。
- **paint lane scale**：`paint/pass.rs` 的 `PopupScale { factor, pivot }` 对 Quad/Shadow/Image/GlyphRun/Path/Geometry/Clip/Canvas 递归应用（glyph 位置/尺寸、Canvas 内部图元、Geometry transform 均缩放）；`paint_scale` 侧表由 paint pass 写入，供 `PopupSnapshotEntry.paint_scale` 快照。**popup content 全部平铺在 popup root 下，单层 transform 足够；`PopupScale::compose` 已删除**（`inherited_scale` 仅把最近层 scale 传给子节点，不再做 compose/嵌套变换；P3E-06/P3K-04）。**P3K-06 closed**：`Primitive::Text` 现为恒等直通（`PopupScale` 不再缩放 CanvasText），CanvasText 只在 Canvas push 前解析为 glyph run；不再有“Text 只缩放 origin”的缺口。
- **damage 契约**：动画 tick 每帧 damage 加入 `rect.translate(previous.offset)` 与 `rect.translate(offset)`（**layout rect 而非缩放后的 paint rect**）；因 `scale ∈ [0.98,1]`（≤1 向内收缩），layout rect 是 paint rect 的超集，不会漏 damage。`settle` 在 rect 同步后把 scaled popup 的 pivot 改成新 paint center，**不 mark_paint**：pivot 是纯绘制派生值，但安全前提是同帧 tick 已 mark_paint；缓存键含 pivot 保证不误用旧 pivot；settle 同步 pivot 不额外 mark_paint 以免 `place_subtree` 迁移 damage 并入同帧导致 `damage_full`。
- **modal fade-only**：`FadeScale` 在 `PopupEntry::is_fade_only()`（`is_modal() || tier∈{Modal,Scrim}`）时 scale 固定 1.0，只淡入淡出；`FadeSlide` 模态时 offset 为 0（同样退化为 fade）。
- **关闭 reason API**：`PopupPanel::close_with_reason(app, reason)` 与 `begin_close_popup(id, reason)` 贯通；kit 锚点丢失/滚出视口统一上报 `DismissReason::AnchorLost`（Menu/ContextMenu/Dropdown/Select/ColorPicker；Dialog 为 modal 无锚点关闭路径），程序化关闭默认 `Programmatic`；on_close 透传。
- **FrameStats**：`frame_cpu: Option<Duration>`（**覆盖 `App::frame` 全程，含 popup tick/paint；不是单 popup tick 计时，与 idle 帧区间不完全对等，P3E-03**）、`popup_opening`/`popup_closing`（本帧是否有动画工作）、`popup_animation_records`（本帧真实动画 tick 记录数，P3E-04 修复后避免“默认值恒真”）；配合 `popup_settle_rounds/popup_unsettled` 断言空闲帧 0 popup 工作。

### 14.4 三端真实窗口像素验收

**`gallery --real-pixel-check` 语义（最终 HEAD `35c9381d`，三端均 106 checks）**：固定 960×720 always-on-top 窗口，内容边缘画 6 px `#FF00FF` 标记环；截图命令按平台取全屏；解码后由品红框给出内容原点/物理尺寸/DPR，引擎 popup rect 经窗口 scale 映射为物理像素区域；每一阶段：打开后该区域必须出现可见变化（覆盖量下限 + 分散像素），关闭后必须逐像素恢复到 baseline。最终版含交互验收（hover/safe-triangle/sibling/Esc/外部点击 Replay/键盘 Tab/外部滚动重锚）与 self-check 负向自检。**FQ-01 关闭：三端报告均为 `revision=35c9381d status=clean diff_sha=e69de29bb2d1d6434b8b29ae775ad8c2e48c5391 identity=source-id-file`，与提交树对账一致。**

| 平台 | 命令 | 结果 | 内容尺寸/DPR | 耗时 | 截图 API |
|---|---|---|---|---|---|
| Windows 11 | `cargo run -p ui_kit --example gallery -- --real-pixel-check` | **106 checks / 0 failures PASS** | 1200×900 / 1.25（screen 4800×3072） | **45.9 s**（report `elapsed_ms=45865`、frame 280；本地隔离复跑 43.3 s/frame 277） | PowerShell `System.Drawing.Graphics.CopyFromScreen`（虚拟屏幕） |
| macOS | `tools/real-pixel-check-macos.sh`（`cargo build` + `open -a Terminal` + osascript 执行并等待 exit 文件；TCC 需 Terminal） | **106 checks / 0 failures PASS** | 1914×1437 / 2.0（screen 3360×2100；期望 1920×1440，标记容差 8） | **72.2 s**（report `elapsed_ms=72198`、frame 283） | `screencapture -x`（全屏、无光标/声音） |
| Ubuntu（Wayland） | `tools/real-pixel-check-linux.sh`（自起 fresh nested `sway --unsupported-gpu`（parent wayland-0→wayland-1）、trap 清理，探测 grim-capable socket 后运行） | **106 checks / 0 failures PASS** | 1276×670 / 1.0（screen 1280×720） | **20.3 s**（report `elapsed_ms=20297`、frame 311） | `grim`（wl_output 布局、无光标） |

**证据路径**：三端 per-run 证据 `target/real-pixels/windows/20261127T143605Z-e69de29b/`、`target/real-pixels/macos/20261127T143706Z-e69de29b/`、`target/real-pixels/linux/20261127T143842Z-e69de29b/`（远端）；本地副本 `target/real-pixels/macos-head35c9/`、`target/real-pixels/linux-head35c9/`（仓库本机可见 `macos-remote`/`linux-remote` 最终 report 同内容）。各含 `report.txt`（checks/failures/result、source_revision/status/diff_sha/source_identity、scrim_alpha、last_frame/elapsed_ms）、`run.log`、`pixel-check.log`、`idle-check.log`、`theme-check.log`、`debug-stack.log` 与 00–17 PNG。

**平台差异与已知限制**：
- macOS：`screencapture` 需要启动会话具备 **Screen Recording（TCC）**；普通 ssh 会话只能拍到壁纸 ⇒ 必须通过 Terminal 启动（`tools/real-pixel-check-macos.sh` 用 `open -a Terminal` + osascript），脚本以 exit 文件收集结果；无该权限时检测不到标记环 ⇒ exit 3。
- Linux/Wayland：`grim` 抓整个 wl_output 布局；nested sway 的 tiled 窗口会被 compositor 调整为输出尺寸，因此内容尺寸是 compositor 尺寸而非请求的 960×720（1276×670）；脚本自动从父 socket 启动 nested sway 并选择 grim-capable socket；无 Wayland/无 grim 时 exit 2（前置失败）。
- Windows：多显示器需从标记框反解内容原点与 DPR；`CopyFromScreen` 抓虚拟屏幕（4800×3072）。
- **Dialog scrim 覆盖标记环**：dialog-open 阶段先用品红被 scrim 调暗后的 tone 重试标记检测，否则回退到 baseline origin（打印 note）；关闭后重新检测标记并与 baseline 比较。三端最终证据均走 Scrim tone；`dialog-marker-under-scrim` 检查只在 tone==Scrim 时记录（Full tone 可检测时会跳过）。
- 标记物理厚度 = 6 px × DPR（Windows 8、macOS 10、Linux 6），尺寸容差 `max(4, 4×DPR)`；内容尺寸容差 Windows 5、macOS 8、Linux 4；三端断言数量均为 **106**。
- **独占桌面**：`--real-pixel-check` 需独占桌面/前台真实窗口；并发运行其它 gallery/截图/窗口操作会遮挡标记环或截到错误窗口 ⇒ 假红（exit 3）。macOS 还需 TCC 屏幕录制权限并经 Terminal 启动。**并发桌面下 `--debug-stack` 可能 300 s timeout**；验收脚本对 debug-stack 自动重试（macOS `tools/macos-gates-runner.sh`、Linux `tools/real-pixel-check-linux.sh` 各 3 次尝试）；**Windows 曾出现 attempt1 300 s timeout、attempt2 PASS（R2-03）**；单次 timeout 只记录该次 exit/日志，不改变 gates 总结果。
- **证据快照口径**：验收证据以 `report.txt` 的 `source_revision`/`source_status`/`source_diff_sha`/`source_identity` 为准——**最终三端一致：HEAD `35c9381d` + `clean`，`diff_sha=e69de29bb2d1d6434b8b29ae775ad8c2e48c5391`，`source_identity=source-id-file`**。`tools/record-source-id.ps1` 生成 `tools/source-id.txt`（未跟踪生成物，提交前重生成；gallery 优先读该文件，无则回退 git/unknown；远端 tar 树可上报同一 diff_sha）。
- **圆角 AA 假绿修复**：`baseline-stable`/`close-residue` 比较区域按标记框内缩 `REAL_PIXEL_CORNER_INSET=4` 物理 px，排除 OS 窗口圆角抗锯齿造成的假绿；三端 106/106 稳定。`gallery.rs` 最终 SHA256=`abf26d49176b613be5b3012ca4137a36e70bd25ebd39fb08c784f34aa250d02f`。
### 14.5 测试与门禁基线（本机 Windows 11 / D3D12 / 125% DPI，2026-11-27）

| 门禁 | 结果 |
|---|---|
| `cargo test -p ui_core -p ui_kit` | **3882 passed / 0 failed / 2 ignored**（ui_core lib 2802 + 1 ignored、frame_record 8、gui_smoke 0 + 1 ignored、layout 4、popup_behavior 124、snapshot 14、demo 39、ui_kit lib 891） |
| `cargo test --workspace --no-fail-fast` | **103 binaries：9337 passed / 1 failed / 30 ignored**。唯一失败 `download_provider::fetch::tests::a_download_shorter_than_expected_fails_and_keeps_the_part` 为并行负载下 OS socket 资源耗尽（WSAENOBUFS 10055）环境 flake，**该用例单独重跑 PASS**，与本 feature 无关 |
| `cargo +nightly clippy -p ui_core -p ui_kit --all-targets -- -D warnings` | PASS |
| `cargo +nightly fmt --all -- --check` | PASS |
| `cargo check --workspace --all-targets` | PASS |
| `gallery --pixel-check` | **PASS 94 checks**（32 sampled / 8 open animation / 6+6 close / damage_full_frames=0；首可见帧 rect==稳定帧） |
| `gallery --debug-stack` | **PASS**（3 级链/取色器栈/首可见帧/rect/Z/parent；exit 0；menu 度量 `(9, 270.0, 180.075)`，P2 历史基线为 `(6, 180.0, 127.5)`） |
| `gallery --real-pixel-check` | **三端 HEAD `35c9381d` clean 各 106 checks / 0 failures PASS**：Windows 45.9 s/frame 280（本地隔离 43.3/277）、macOS 72.2 s/frame 283、Ubuntu 20.3 s/frame 311；identity 三端 `diff_sha=e69de29…`；pixel-check 94 / idle / theme / debug-stack 均 PASS |
- 逐文件 `#[test]` 计数（最终 HEAD）：引擎 `popup/tests.rs` 41、`popup/placement.rs` 32、`tests/popup_behavior.rs` 124、`a11y/popup_tests.rs` 22；kit `menu` 79、`menu_chain` 13、`context_menu` 21、`popup_panel` 20、`dropdown` 35、`select` 39、`dialog` 55、`tooltip` 28、`alert` 11、`color_picker` 83。
- **P3 新增测试（相对 P2 3847，净 +35）**：`popup_behavior` 111→124（+13）；`ui_kit` 866→891（+25：Dropdown/Select slide、color_picker 边界 Tab、popup_panel scrim、reason 不泄漏、真实窗口交互验收等）；`ui_core` lib 2805→2802（−3：API 死代码/死测试删除抵新增）。

### 14.6 行为偏差登记（D-P3-1…D-P3-9）

| ID | 偏差 | 决策与证据 |
|---|---|---|
| D-P3-1 | Dialog 删除 `recenter`（P3-1，关闭 D-P1-7） | 只保留打开时的 anchor 居中；resize 不再重新居中（`resizing_the_window_keeps_the_anchor_opened_with`）；resize 不关闭对话框 |
| D-P3-2 | `FadeScale` 的 modal/scrim 退化为 fade-only | `is_fade_only()`（modal/Modal/Scrim）时 scale 固定 1.0，只淡入淡出（`the_modal_dialog_degrades_fade_scale_to_fade_only`、`modal_fade_scale_is_fade_only`、`fade_scale_modal_scrim_is_fade_only`）；非模态才 0.98→1；scrim 关闭缩放已在 P3E-02/P3K-01 修复 |
| D-P3-3 | damage 用 layout rect 平移超集，而非缩放后的 paint rect | scale≤1 向内 ⇒ layout rect 覆盖 paint rect；动画 tick 同时加旧 offset 与新 offset（`fade_scale_close_reverses_the_scale_and_keeps_damage_local` 断言关闭帧 damage 覆盖旧 rect） |
| D-P3-4 | `settle` 同步 pivot 不 `mark_paint` | pivot 是纯绘制派生值，但安全前提是同帧 tick 已 mark_paint；缓存键含 pivot 保证不误用旧 pivot；settle 同步 pivot 不额外 mark_paint 以免 `place_subtree` 迁移 damage 并入同帧导致 `damage_full`（P3E-07c/P3E-09） |
| D-P3-5 | 真实窗口像素用**截图+标记环**而非 GPU readback | P0 用户裁决不接受 readback；P3-6 用平台截图 API + 品红标记框反解内容几何，统一 exit 0/3；macOS 需 TCC、Linux 需 Wayland/grim（§14.4） |
| D-P3-6 | 三端截图 API/退出前置不同 | Windows CopyFromScreen、macOS screencapture（Terminal/TCC）、Linux grim + nested sway；无 Wayland/grim 时脚本 exit 2；Dialog scrim 覆盖标记时回退 baseline origin |
| D-P3-7 | 性能统计/守卫口径 | 守卫用**中位帧** ≤ idle p95+2 ms（与设计目标一致）；p95 仅记录（10 帧样本 p95≈max，open 首帧含 atlas/text 冷启动）；`frame_cpu` 覆盖 `App::frame` 全程（非单 popup tick）；`damage_full=false` 由 pixel-check 与测试共同断言（P3E-03） |
| D-P3-8 | **FQ-02**：ColorPicker/profile 菜单 Tab 边界与 Closing 重开 | ColorPicker 内部 tab/hex/confirm 正常 Tab 导航，仅最后一个控件 Tab 或第一个控件 Shift+Tab 时关闭并移焦；profile 自定义 Take `PopupPanel` 内部遍历 entry，逃逸时 `close_now`+焦点回 opener 之后/之前，`tabbed_out` 防止关闭动画期重开；补完整 Tab/Shift+Tab 序列测试 |
| D-P3-9 | **FQ-08**：`leave_owner_scope` 不绕回 popup 内容 | Shift+Tab 逃逸只在**主树**前后查找可聚焦节点，不进入 popup 内容；`a_non_modal_take_popup_shift_tab_escapes_backwards` 改为断言实际落点（不是仅否定断言） |

### 14.7 剩余 P4/未闭环项

> R2-01/R2-02/R2-03/R2-04、FQ-01/FQ-08、P3A-02/05/06 已在最终轮关闭；下表仅保留真实残余（不影响 P0–P3 完成）。

| 项 | 状态 | 去向 |
|---|---|---|
| P3K-07 scale 不作用于 hit/a11y/IME 的契约未登记（动画期 hit 用稳定 rect） | P4 文档/行为契约 | §14.3 |
| P3K-08 可观测/门禁缺口：`--debug-stack` 不含 `paint_scale`；`--pixel-check`/`--real-pixel-check` 只验证稳定态，不采样动画帧 | P4 | §14.4/§14.5 |
| R2-e `WindowRect` 锚点口径：设计注释写“指针点扩展的 1×1 rect”，实现 `ContextMenu::open_at` 用 0-size point rect；两者放置/命中等价 | P4 文档口径 | §4.2 |
| R2-f 子菜单句柄回调：`open_submenu` 返回句柄的 `on_activate/on_close` 仅 root 生效，子层静默无效（已在 §5.2 注明） | P4 文档/API | §5.2 |
| 外部滚动语义：`window_scroll_epoch` 对任意非 popup 子树滚动 +1，Menu/ContextMenu 会被无关滚动容器关闭（宽于“祖先滚动”字面） | P4 文档/行为偏差 | §4.7 |
| reconcile Closing churn：pointer-close 标 `Closing` 后下一帧 panel 仍 Open 时立即 `reopen`，`Closing` 几乎只存活一帧；无用户可见影响 | P4 状态机清理 | §14.3 |
| P3A-03 acceptance 目录归档未落地（命令日志/帧号/scrim 字段已补；报告写失败已 fail-fast） | P4 | §7.4/§14.4 |
| ContextMenu 右键自动绑定（D-P2-4） | P4 候选 | §13.4 |
| typeahead 不消费 text/IME（D-P2-5） | P4 行为限制 | §13.4 |
| macOS TCC 一键化（避免手动 Terminal/权限） | P4 | §14.4 |
| P3-4 gallery 真实像素扩展到更多组件/主题 | P4 | §14.4 |

### 14.8 与设计正文对齐摘要

- §3.4：动画时长/距离/scale 起点按 D10 落地（120/90 ms、4 px、0.98）；200/300 ms hover 时序不变。
- §4.2：`PopupAnimation` 增 `FadeScale`；`PopupTransform` 为 `pub(crate)`，公开快照只暴露 `paint_scale`。
- §5.2：`PopupPanel::close_with_reason`、`DismissReason::AnchorLost` 语义、`FrameStats` 三字段、`PopupSnapshotEntry.paint_scale`、gallery `--real-pixel-check` 已写入 API 表。
- §7.4：三端验收从“待定”改为截图+标记环方案并给出命令/结果/证据/平台差异（§14.4）。
- §10：DoD 1/2/3/4/5/6/7 在 P3 范围内全部达成；P4 项见 §14.7。

### 14.9 三端已知差异（摘要）

- 内容物理尺寸随 DPR/compositor：Windows 1200×900、macOS 1914×1437（容差 8）、Linux 1276×670。
- 截图时机与窗口遮挡：三端均需窗口可见/always-on-top；macOS 需 TCC、Linux 需 Wayland socket 与 grim；Windows 虚拟屏幕可含其他显示器内容（标记框负责定位）。
- Dialog scrim 会遮盖内容边缘标记：先重试 scrim-dimmed tone，失败回退 baseline origin，关闭后恢复检测。
- 标记厚度/尺寸容差按 DPR 缩放（6 px×DPR；容差 max(4,4×DPR)），因此三端断言数量均为 **106** 而非像素值相同。

### 14.10 文档入库与复现路径

- **仓库约定与本次入库**：`.gitignore:3:.*` 忽略 `.docs/`；本次已随提交 `260ebf5f` 以 `git add -f` 入库：`.docs/popup-overlay-redesign-design.md` 与 `.docs/popup-overlay-redesign/implementation/` 全目录（43 个文件：`p0-summary.md`、`p1-summary.md`、`p1-cleanup-w7.md`、`p1-migration-inventory.md`、`p2-summary.md`、`p3-summary.md`、`review-final-*`、`review-p0-*`、`review-p1-*`、`review-p2-*`、`review-p3-{engine,kit,acceptance}{,-r2}.md`）；`.docs/gui-migration/spec-deltas/kit-gallery.md` 已 tracked；`.docs` 其余默认忽略。
- **最终提交链**：`d9949aca → 152616b1 → f24a3441 → fa41987c → 5df859b8 → 5374c4c8 → 00927ff9 → 35c9381d`（产品代码最终，clean）`→ 260ebf5f`（P1-P3 文档回写入库）`→` 本文档口径收口提交（§14.10）；三端验收 source identity 均指向 `35c9381d clean`。
- **§14 引用的 implementation 文件清单**（均 `.docs/popup-overlay-redesign/implementation/`）：
  - `p3-summary.md`（本阶段实施摘要）
  - `p1-summary.md`、`p2-summary.md`、`p0-summary.md`（阶段摘要；`p1-cleanup-w7.md` 为 P1 清理记录）
  - `review-p3-engine.md`、`review-p3-kit.md`、`review-p3-acceptance.md` 与 r2 复核 `review-p3-{engine,kit,acceptance}-r2.md`
  - 最终审查 `review-final-quality.md`、`review-final-arch.md`、`review-final-interaction.md` 与 r2/r3 复核
  - 历史审查（§11–§13 引用）：`review-p0-*.md`、`review-p1-*.md`、`review-p2-*.md`、`review-p0-disposition.md`
- **复现命令**：`cargo test -p ui_core -p ui_kit`；`gallery --pixel-check`/`--debug-stack`/`--real-pixel-check`（Windows 直接运行；macOS/Linux 用 `tools/real-pixel-check-*.sh`，Linux 自起 fresh nested sway）；真实像素证据落 `target/real-pixels/<platform>/<run>/`（report/run.log/PNG，gitignored）。
- **source-id**：`tools/record-source-id.ps1` 生成 `tools/source-id.txt`（未跟踪生成物，提交前重生成；远端 tar 树带上该文件即可上报同一 `diff_sha`）；`.gitignore:8` 已忽略 `tools/source-id.txt`，未生成时 gallery 回退 git/unknown。
---

## 15 P3 后实机 BUG 修复：ColorPicker 关闭残影（2026-11-27）

- **现象（用户实机）**：`gallery` 滚动到 section ⑭ 后打开 ColorPicker，关闭后原浮层位置的 **section 卡片间隙**残留色板/OK 按钮像素；OK、Esc、外部点击三条关闭路径均复现；残影静态不变，只有整帧重绘（如滚轮滚动触发 Full）才清除。
- **根因（不是 damage 漏旧矩形）**：headless 持久增量探针证明关闭帧 `damage_full=false` 且 damage 完整覆盖旧浮层 `(540,321)-(840,696)`；真正缺口是 **gallery 没有不透明全窗背景节点**（只有 `WindowOptions.background` clear color），而 Windows 增量合成在 damage scissor 内只重画与 damage 相交的图元、**不 clear**；浮层覆盖的卡片被重画，卡片之间的 12 logical px 间隙无图元覆盖 → 旧浮层像素保留；FadeScale 关闭动画把半透明帧叠到残留像素上导致 alpha 累积。
- **修复（用户裁决 A：宿主补 backdrop）**：gallery root 增加全窗不透明 backdrop 节点（`gallery.backdrop`，absolute inset 0，`Role::SurfaceCanvas` 主题绑定，root 改 `relative`），与 `crates/kytrix` 的 `WINDOW_BACKDROP` 同款；复用 `WindowOptions.background` 的初始色 + `bind_style` 主题跟随。
- **验证（实机 Windows，DPI 125%，`--scroll-to=3150`）**：OK/Esc/外部三路径关闭后与打开前同位置截图对比，除 trigger focus ring / 交互日志 / 窗口边缘外无差异；修复前 OK 路径整帧 23681 changed（含 3 条间隙横条），修复后 OK 38 px（仅窗底边缘）、Esc 2822 px（仅 trigger focus ring 区）、外部 0 px；headless 无 backdrop 探针残留 3696 px、加 backdrop 后 0。
- **门禁**：`cargo test -p ui_core -p ui_kit`、clippy、fmt、`cargo check --workspace --all-targets`、`gallery --pixel-check` 94、`--debug-stack`、`--real-pixel-check` 106、`--theme-check` 全 PASS。
- **用户裁决**：本次不新增防回归门禁（不新增 real-gallery ghost gate / headless 无 backdrop 测试），相关建议保留在复现报告中供后续 P4 评估。
- **审计**：生产 `kytrix` shell 已有 `WINDOW_BACKDROP`；`gallery` 是本次唯一受影响的产品示例宿主；`ui_core/examples` 等开发工具未纳入本次修复。
- **证据**：`.docs/popup-overlay-redesign/implementation/repro-colorpicker-ghost.md`；实机截图/对比 `E:\kytrix\target\ghost-real\`（`fix-01-before` / `fix-02-open` / `fix-03-closed` / `fix-11-esc-closed` / `fix-12-outside-closed` 与 `*-diff.png`）。
- **残余观察（P4）**：header 文本亮度在浮层交互后轻微变亮、整帧重绘后恢复，疑似同类增量重合成效应，未定位。

---

## 附录 A：证据索引

- `popup-overlay-redesign/evidence/engine-overlay-audit.md` — 引擎审计（F1–F12）
- `popup-overlay-redesign/evidence/kit-overlay-audit.md` — kit 审计（K1–K18）
- `popup-overlay-redesign/evidence/paint-damage-rootcause.md` — paint/damage（evidence-R1–R5、E1–E9、I1–I6）
- `popup-overlay-redesign/evidence/reproduction.md` — 真实窗口复现（O1–O7、截图）
- `tools/real-pixel-check-{windows.ps1,macos.sh,linux.sh}`、`tools/record-source-id.ps1`、`tools/macos-gates-runner.sh` + `target/real-pixels/<platform>/report.txt` — P3 三端真实窗口截图验收（§14.4）
- `popup-overlay-redesign/evidence/external-research.md` — Chromium/Qt/Flutter/Web/平台调研
- `popup-overlay-redesign/evidence/shots/` — `user-report.png`、`09-...`（首可见帧残影）、`10-...`（5s 后）、`18-...`（面板/色板错位转折帧）、`13/14-...`（resize 前后）、`17-...`（卸载清除）、`annotated-ghost-map.png`
- `popup-overlay-redesign/review/01-architecture-review.md`、`02-feasibility-review.md`、`03-interaction-a11y-review.md` — 独立评审

关键源码：`app/frame.rs`、`app/popup.rs`、`popup.rs`、`popup/{anchor,animation,dismiss,focus,placement,settle,stack}.rs`、`node/tree/{layout,arena,content}.rs`、`paint/{pass,instance}.rs`、`render/{delta,compose,damage}.rs`、`kit/{popup_panel,menu,menu_chain,context_menu,dialog,dropdown,select,tooltip,alert,color_picker}.rs`、`a11y/{tree,popup,mapping}.rs`、`test_support/{pixels,record,snapshot,diagnostics}.rs`、`crates/widget/{services,cx,binding}.rs`、`crates/kytrix/composition.rs`、`examples/gallery.rs`。

## 附录 B：外部参考（节选，完整见 `evidence/external-research.md` §10）

- Chromium `bubble_frame_view.cc`: https://github.com/chromium/chromium/blob/main/ui/views/bubble/bubble_frame_view.cc
- Qt `qmenu.cpp`: https://code.qt.io/cgit/qt/qtbase.git/tree/src/widgets/widgets/qmenu.cpp
- Flutter `menu_anchor.dart`: https://github.com/flutter/flutter/blob/master/packages/flutter/lib/src/material/menu_anchor.dart
- Flutter `overlay.dart`: https://github.com/flutter/flutter/blob/master/packages/flutter/lib/src/widgets/overlay.dart
- Radix `menu.tsx`: https://github.com/radix-ui/primitives/blob/main/packages/react/menu/src/menu.tsx
- Floating UI middleware: https://floating-ui.com/docs/middleware
- WAI-ARIA APG Menu/Menubar: https://www.w3.org/WAI/ARIA/apg/patterns/menu/ ；https://www.w3.org/WAI/ARIA/apg/patterns/menubar/
- WCAG 1.4.13: https://www.w3.org/WAI/WCAG22/Understanding/content-on-hover-or-focus.html
- HTML popover/top layer: https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Global_attributes/popover
- Wayland xdg-shell: https://wayland.app/protocols/xdg-shell
- Windows TrackPopupMenuEx: https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-trackpopupmenuex

## 附录 C：评审记录与处置

三轮独立评审（2026-11-26）详见 `popup-overlay-redesign/review/`；经 rev.2→rev.5 四轮复核，三轮评审均确认**无剩余 Blocker/Major**（review-arch 最终结论“rev.5 无阻塞项”，review-feasibility “rev.3 可进入实施评审”，review-interaction “rev.4 无 Blocker/Major”）：

| 评审 | 统计 | 处置 |
|---|---|---|
| 01 架构/需求（review-arch） | 3 Blocker / 13 Major / 5 Minor | 已修：P0/P1 重排（B1/M01）、widget 宿主迁移（B2）、像素门禁基线（B3）、tier/exclusive（M02）、owner_modal（M04）、scrim 独立栈项（M05）、Slide paint_offset（M06）、Closing 输入阻断（M07）、dismiss 矩阵（M08）、API 收敛（M09）、rect 拆分（M10）、settle dirty 闸门（M11）、测试重写（M12）、HUD 层（M13）、I7–I9（m1）、children/墓碑（m2）、文档缺口（m4）、回滚（m5） |
| 02 可行性/证据（review-feasibility） | 6 Blocker / 12 Major / 3 Minor | 已修：§2.3 改为“已证实/推断”分级并保留 evidence-R 编号（B1）、M-D 先判别后修（B2）、popup root/HUD/a11y/skip 语义（B3）、Canvas 全量 132 处迁移（B4）、P1 纳入 Menu/ContextMenu 并消除重复阶段（B5）、持久增量像素 harness（B6）、settle 插入点/2 轮/报告（M7）、painted_extent 存储（M8）、I5 收窄（M9）、scroll slot 表（M10）、FocusManager 清单（M11）、迁移清单（M12）、内容闭包签名（M13）、Slide 纯 fade/偏移（M14）、rect 语义（M15）、bridge API（M16）、max-size 内部滚动（M20）、三端 harness（M21）、file:line/overlay-check/damage 断言修正（m17–m19） |
| 03 交互/a11y（review-interaction） | 5 Blocker / 12 Major / 7 Minor | 已修：scrim（B1）、grace 优先于兄弟替换（B2）、PendingClose 绑定层（B3）、opener toggle（B4）、a11y 链嵌套（B5）、active_item/keyboard target（M1）、完整键盘表（M2）、Esc/Closing/tooltip（M3）、dismiss 类型（M4）、timer 生命周期（M5）、子菜单 Right→Left（M6）、resize/scroll 矩阵（M7）、tier/多链（M8）、焦点事务（M9）、Dialog trap（M10）、IME 时序（M11）、tooltip WCAG（M12）、modal 真值/accesskit 字段/aria-haspopup 目标/checkable 语义/惯例表述/AT dismiss（m1–m7） |

后续：本 rev.2 已按上述处置修订；P0-0 判别实验完成后，如结论与 §2.3 假设不符，在本附录追加诊断记录并更新 §2.3/§6 P0 修法，不改总体架构。

**P0-0 诊断记录（rev.6，2026-11-26）**

- 真实 Windows 窗口（125% DPI，D3D12，持久化通路）稳定复现 palette 残影；单变量实验 E1（局部新 damage 不显影）、E2（1px resize 一帧清零、不出现完整 12 行色板）、E3（3 轮开关不叠加、关闭后 0 残留）、E9（滚 1 tick 一帧清零、无被平移色板）。
- 判定：R1/M-D **refuted**、R4/M-E **refuted**、R2/M-C **confirmed**（收窄为「打开路径在错误位置画一次、旧位置未进 damage」）、R3/M-A **confirmed**（错位来源）、R5 通路事实。
- 修法调整：P0-2 降级为防御性不变量测试（无生产代码改动）；P0-3 从本轮移除；最小修复集 = kit 定位时序（P1 settle/内容闭包）+ Canvas 局部坐标（P0-4）+ `overflow_hidden`（P0-6）+ 持久增量像素门禁（P0-5）；P0-1 保留为引擎正确性项。
- 回归测试约束（P0-5 必须遵守）：残影仅在新进程 + 首次打开、且打开前未 resize 时复现；同进程 resize 后重开不复发，门禁用例必须固定该条件，否则假阴性。
- 证据：`popup-overlay-redesign/evidence/p0-diagnosis.md`（帧/脚本/日志索引见其 §1.4）。

**范围追加（rev.6，用户裁决 A）**：C8/I6 要求非模态 popup 开关不 `damage_full`，但现有引擎在新增/删除节点（结构变化）时强制全量绘制（`window/paint.rs` 的 removed/structure epoch/无 lane 三条路径）。用户裁决在 P0 追加 P0-7「结构变化增量绘制」，并恢复 P0-3 的 scroll slot 一致性要求作为其前置；P0 验收 ④ 改为「非模态 popup/面板打开、关闭、重开 `damage_full==false`」。

**范围追加 2（rev.6，用户裁决 A，2026-11-26）**：旧 kit `Overlay` 首帧定位（R3/M-A：open 用零尺寸求初始 origin、reveal 早于 move，导致首可见帧位置错误与残影驻留）在 P0 追加 P0-8 最小修复：open 内完成 measure（`refresh_text` + `compute_layout`）→ place → reveal，首可见帧 rect == 稳定帧；`ColorPicker::open` 返回强类型错误并在 measure 失败时回滚。该实现是 **P1-11 会删除的过渡 API 的一部分**，P1-12 必须迁移依赖它的 `--pixel-check` 与首帧测试（§8、§11.3-3/9、§11.7）。
