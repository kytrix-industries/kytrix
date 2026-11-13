# P17：引擎内置 Debug HUD

> 背景：P15 demo 已能展示焦点 / CJK / IME / a11y / 图片与滚动，但没有运行时性能观察面。
> 本阶段给引擎增加一个**可复用的 Debug HUD**：应用一行 API 开启，引擎在窗口右上角维护
> 一个非交互浮层，持续显示帧率、DrawCall 与渲染统计；demo 加 `--features hud` 开启并用 F12 切换。
> 实现与本文不一致处记入 `design/DEVIATIONS.md`。
>
> **P18 迁移**：demo 已并入 `ui_core` 的 example（`design/phase-18-demo-example.md`），
> 不再是独立 crate；`hud` 仍默认关闭，运行 `demo` 时必须显式加 `--features hud`。本文历史
> 验证记录里的 `ui_core_demo` 命令是迁移前状态，当前命令见 README。

---

## 0. 结论摘要

| 项   | 决定                                                                                                                 |
|------|----------------------------------------------------------------------------------------------------------------------|
| 形态 | **引擎内置 API**：`App::set_debug_hud(window, enabled)`；HUD 是 root 末尾的普通子树（不是平台 overlay 窗口）         |
| 位置 | **右上角浮层**：半透明深色面板，不参与文档流、不挡命中、a11y 标记 `hidden`                                           |
| 编译 | 全部 HUD 代码由 `ui_core` 的 **`hud` feature** 门控；未开启时零代码 / 零开销。demo 用 `--features hud` 打开          |
| 指标 | 核心性能、CPU/GPU 耗时、显示与后端；英文缩写                                                                         |
| 刷新 | HUD 可见时按平台帧定时器连续出帧；数字每 **500 ms** 汇总一次，面板每帧重绘                                           |
| demo | 真实窗口加 `--features hud` 后开启，**F12** 隐藏/显示；`snapshot_demo` 不安装 HUD（保持展示截图稳定）                |
| 测试 | 纯格式化、HUD 节点 / 命中 / a11y、开关生命周期、空闲时强制提交、500 ms 汇总、后端 DrawCall 计数、demo F12 与快照隔离 |

## 1. 目标与非目标

### 1.1 目标

- 给 `ui_core` 提供可复用的运行时 Debug HUD，而不是 demo 私有实现。
- 显示常用引擎调试数据：**FPS、平均帧时间、DrawCall、图元数、GPU 实例数、CPU/GPU 帧耗时、
  Upload/Present 耗时、物理分辨率、DPI 缩放、渲染后端**。
- HUD 可见时给出真实的可观察帧率：持续驱动窗口出帧，而不是只在应用恰好重绘时更新。
- HUD 不能改变文档布局、不能截获指针/键盘焦点、不能出现在无障碍树中。
- 通过 Cargo feature 可完全关闭；关闭后不编译 HUD 代码，也不改变平台事件循环行为。

### 1.2 非目标

- 不采集 GPU 显存、驱动内部计数器或进程内存（需要额外平台 API；本期用户未选）。
- 不做图表、历史曲线、日志窗口或性能录制。
- 不引入平台 overlay 窗口；HUD 仍走引擎正常的节点树 / Scene / 三平台渲染器。
- 不对帧率作数值门槛；HUD 是观察工具，不是 benchmark。

## 2. 公开 API 与 feature

### 2.1 `hud` feature

`..`：

```toml
[features]
hud = []
```

- `lib.rs` 只在 feature 打开时声明 `mod debug_hud;`。
- `App` 上所有 HUD 方法用 `#[cfg(feature = "hud")]` 门控；未开启时 API 不存在。
- 平台事件循环里的「连续帧」判断也 gates：未开启时退化为原有「只有动画时定时出帧」。

P18 之后 demo 是 `..`，不再有独立 `Cargo.toml` / 默认 feature。
运行 `cargo run -p ui_core --example demo --features hud` 才启用 HUD；不加 feature 的 demo
没有 HUD 与 F12。`snapshot_demo` 走 `build_into`，不安装 HUD。

### 2.2 `App::set_debug_hud`

```rust
#[cfg(feature = "hud")]
pub fn set_debug_hud(&mut self, window: WindowId, enabled: bool) -> Result<(), EngineError>
```

- 窗口不存在 ⇒ `EngineError::UnknownWindow`（**无论 `enabled` 是 true 还是 false**）。
- `true`：已开启且 HUD 子树仍存在 ⇒ 幂等 no-op；map 中的句柄已陈旧（应用手动删了子树）⇒ 重建。
- 首次开启：在 root 末尾挂载 HUD 子树（节点 id：`debug-hud` / `debug-hud.line-0` … `debug-hud.line-9`），
  重置该窗口的 HUD 提交时间基准并请求一帧。
- `false`：移除 HUD 子树和采样状态；不存在的 HUD 是 no-op（窗口仍必须存在）。
- 关闭后再开启会重新建树、重新从 0 采样；不保留旧文本。

### 2.3 `App::debug_hud_enabled`

```rust
#[cfg(feature = "hud")]
#[must_use]
pub fn debug_hud_enabled(&self, window: WindowId) -> bool
```

返回该窗口当前是否安装 HUD；未知窗口返回 `false`。

### 2.4 不公开 HUD 内部类型

HUD 的节点句柄、采样累计器与格式化函数都是 crate 私有。公开面只有上面的开关和查询；
demo / 应用不需要知道 HUD 内部结构。避免为「未来自定义样式」提前增加配置层（AGENTS §1.2）。

## 3. HUD 视觉与节点结构

### 3.1 视觉

- 固定面板 `244 × 184` 逻辑像素，右上角 `12 px` 边距。
- 背景 `#0A0E14`、约 86% 不透明；1 px 边框 `#374658`，8 px 圆角。
- 单行文本：字号 `11.5`、行高 `1.3`、颜色 `#DCE2EB`；10 行，行距固定 `16 px`。
- 标签全部英文缩写，值宽度用空格补齐；没有字体依赖（沿用引擎 `FontStack::system_ui`）。

面板宽度 / 高度为常量；文本节点用 `LayoutOverride` 固定在面板内。窗口变窄时 HUD 可以越出左边界
（与通用 debug overlay 一致），不改变文档流。

### 3.2 节点

```text
root
└── Box[debug-hud]                    absolute, right:12, top:12, 244×184
    ├── Text[debug-hud.line-0]        LayoutOverride(10, 8, 224, 16)
    ├── Text[debug-hud.line-1]        LayoutOverride(10, 24, 224, 16)
    ├── ...
    └── Text[debug-hud.line-9]        LayoutOverride(10, 152, 224, 16)
```

- 10 个固定文本节点，每行独立 `LayoutOverride`；不使用多行字符串，避免依赖 parley 对
  `\n` 的断行/行高细节，也让行距完全由 HUD 常量决定。
- HUD 根节点 `hit_shape(|_| false)`，每个文本节点同样；所有节点 `A11yOverride { hidden: true }`。
- HUD 不声明 `Action` / `Drag` / `FocusRing`，因此不会进入 Tab 序；**不使用 `bind_style`**
  （`Window::frame` 的 no-bindings 断言对所有窗口成立，HUD 不能破坏这条公共路径）。
- 命中穿透完全由 `hit_shape(false)` 保证（`hit_test` 在 `interactive: None` 时也会询问 `hit_shape`）。
- HUD 是**普通 root 子节点**而不是 `App::create_overlay` 的浮层槽：`Esc` 的 `Dismiss` 只关闭
  overlay 槽，不会误关 HUD。demo 在场景建完后开启 HUD，因此它是最后绘制的 root 子节点。
- `App::create_overlay` / `bring_overlay_to_front` 在追加 overlay 后会重新把 HUD 提到 root 末尾，
  因此普通浮层不会盖住 HUD。应用若直接 `append_child` 追加**任意** root 子节点，新节点仍可能盖住 HUD；
  这是「HUD 是普通子树」的明确定义，不在每帧重排 HUD。应用应在建完静态场景后开启 HUD。
- HUD 的更新入口是 `App::frame`（平台 `Application::run` 走它）；直接调用 `Window::frame` 不会刷新
  HUD 文本，但 HUD 没有绑定，因此不会触发 `Window::frame` 的 no-bindings debug 断言。

### 3.3 文本行

固定 10 行：

```text
FPS     60.0
Frame   16.67 ms
CPU      0.42 ms
GPU      1.10 ms
Upload   0.08 ms
Present  0.30 ms
Draw       12
Prims      48
Inst       52
960x740 @ 1.00x D3D12
```

- `FPS`：采样窗口内**已提交帧**的 `interval_count / interval_sum`（`interval_sum` 单位为秒）；
  没有样本或零间隔时 `--`。
- `Frame`：窗口内相邻 `App::frame` 入口的平均间隔（ms）；与 `FPS` 互为倒数。
- `CPU`：`App::frame` 中从 `begin_frame` 到 `paint_frame` 结束的 CPU 时间（样式 / 文本 / 布局 /
  paint / 实例编码），**不含** GPU 提交与 Present；平均值。
- `GPU`：后端 timestamp（Windows D3D12 query / Metal GPUStart-End / Vulkan query）的平均值；
  计时不可用显示 `--`。
- `Upload` / `Present`：`FrameTimings` 的 CPU 段平均值；不可用显示 `--`。
- `Draw`：上一提交帧实际发出的 API draw call 数（三平台后端逐次计数；含 HUD 自身）。
- `Prims`：上一提交帧 `Scene::primitive_count()`（按绘制序，包含 clip 指令）。
- `Inst`：上一提交帧 `Renderer::submit` 回报的实例数（与 `InstanceBuffer` 条目数同源，
  不因 damage 多矩形重复计）。
- 末行：物理分辨率为窗口逻辑尺寸 `to_physical_covering(scale)` 的结果；DPI 为 `scale.factor()`；
  后端为编译期选定的 `D3D12` / `Metal` / `Vulkan`。
- 所有浮点数固定 2 位（FPS 1 位）；未知值用 `--`，不用 0 冒充。

## 4. 指标来源与定义

### 4.1 帧时间 / FPS

`Window` 记录：

- `frame_cpu = paint_frame 结束 - begin_frame`（**只到 paint 结束**，不含 `submit_gpu`/Present）；
- `frame_interval = 本次 App::frame 入口 - 上一次 App::frame 入口`（首次为 `None`）；
- `last_commit_at` 由 `App::frame` 在 `Committed` 后通过 `note_debug_hud_commit(self.clock)` 写入；
- `set_debug_hud(true)` 调用 `reset_debug_hud_timing()`，因此开启前遗留的提交时刻不会污染第一窗。

`App::clock` 由平台每轮 `sync_clock` 更新；测试用 `TestApp::advance` 推进。`Window` 的 CPU
计时使用真实 `Instant::now()`（不追求测试确定性，只测「有值/格式」；采样窗和帧间隔则由
`App::clock` 驱动，可 deterministic）。

`DebugHudState` 在每次 `App::frame` 开头消费 `Window` 的上一提交帧记录：若 `FrameIndex` 前进了，
把 `frame_interval` / `cpu_time` 加入 500 ms 采样窗，并更新 Draw / Prims / Inst 的「上一帧值」。

采样窗满 500 ms 时：

- `FPS = interval_count / interval_sum`（`interval_sum` 单位为秒；`None`/零间隔不计入）；
- `Frame = interval_sum / interval_count`（毫秒）；
- `CPU/GPU/Upload/Present = sum / count`（各自可用样本数）；
- 生成 `[SmolStr; 10]` 并存入 `DebugHudState.pending_text`，**不**在这里改树；
- 清空累计器，下一窗从 0 开始。

首个 500 ms 窗口内显示初始占位文本；至少两个提交帧后才有 FPS 数值。

文本写入分两段（`App::frame` 内）：

1. 帧门禁之前：`prepare_frame` 采样并只写 `pending_text` + 请求 HUD damage；
2. `refresh_styles` 之后、`refresh_text` 之前：`apply_frame` 把 `pending_text` 直接写入 10 个文本节点
   （`NodeTree::set_text_content`）并 `mark_paint`；
3. 提交成功后 `finish_frame` 才清 `pending_text`。若本帧在中途中止，下一帧 `apply_frame` 会重写同一文本
   并重新标 PAINT，避免无绑定节点被下一次 `refresh_styles` 的 ghost 分支吞掉。

不在 `bind_text` / `Signal` 里做：这样不引入依赖图边，关闭 HUD 时不需要额外清理信号 owner；
`mark_paint` 发生在 `refresh_styles` **之后**，不会被样式 pass 的 `clear_paint` 吞掉；随后
`refresh_text` 按新内容重新 shape。首个 500 ms 内只有占位文本，不需要信号驱动。

10 个文本节点都有 `LayoutOverride`（尺寸固定），因此 `refresh_text` 对它们**不**置 `LAYOUT`：
文本 intrinsic size 变化不影响布局；`layout_text` 仍会按新 `ShapedText` 重排/重建 `ShapedLine`，
HUD 更新不会触发全窗口重排（实现于 `App::refresh_text` 的 `fixed_layout` 分支）。

### 4.2 DrawCall 与实例

`render/stats.rs` 定义 crate 私有 `RenderStats`：

```rust
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RenderStats {
    pub draw_calls: u32,
    pub instances: u32,
}
```

`render/stats.rs` **无条件编译**（非 hud 构建也返回它，只是不保存/不显示）。
三平台 `Renderer::submit(...) -> Result<RenderStats, EngineError>` 返回实际统计；
`render_snapshot` 与既有测试继续用 `?`/`expect` 丢弃值。

- **全帧路径**（`frame.is_full()`，含首帧/fresh/resize/DPI/full damage）：整个 surface 只走一次
  draw pass，`draw_calls` = 非空 `FrameOp` 数。
- **局部路径**：每个 damage scissor 重复一次完整 draw op 列表，`draw_calls` =
  非空 `FrameOp` 数 × scissor 数。
- D3D12：每个非空 `FrameOp::Instances` 的 `DrawInstanced` 计 1，每个 `FrameOp::Geometry` 的
  `DrawIndexedInstanced` 计 1。
- Vulkan：每个非空 `FrameOp::Instances` 的 `cmd_draw` 计 1；Geometry 当前返回既有错误。
- Metal：每个非空 `FrameOp::Instances` 的 `drawPrimitives` 计 1；Geometry 当前返回既有错误。
- `instances` 是 `InstanceBuffer` 的条目数（不按 damage 重复）；`Scene` 的 `Prims` 由
  `primitive_count()` 读取，与渲染后端无关。

`Window` 保存最近一次 `RenderStats`；测试用 `TestApp` 无渲染器时保持默认 0。
Windows 的 WARP 测试同时覆盖 full（1 次）与 partial（2 个 scissor = 2 次）两种规则。

### 4.3 CPU / GPU / Upload / Present

现有 `FrameTimings` 已经由三平台后端填充 `Upload` / `Gpu` / `Present`。给 `FrameTimings` 增加
crate 私有只读 `sample(Section) -> Option<GpuTimerSample>`；HUD 在提交后读取并累积。
`Section` 枚举、样本容量与后端顺序不变。

## 5. 连续帧与平台事件循环

HUD 需要在应用没有其他动画时也持续出帧，否则 FPS 无法反映刷新。新增 crate 私有：

```rust
pub(crate) fn needs_continuous_frames(&self) -> bool {
    if self.has_animations() {
        return true;
    }
    #[cfg(feature = "hud")]
    {
        // 只统计 root 仍存在的 HUD（应用手动删树时不会永久空转）。
        self.debug_hud.iter().any(|(window, state)| {
            self.windows
                .get(window)
                .is_some_and(|target| target.tree().get(state.root()).is_ok())
        })
    }
    #[cfg(not(feature = "hud"))]
    {
        false
    }
}
```

- `App::frame` **在帧门禁之前**调用 `debug_hud::prepare_frame`：
    - 若采样窗到期，生成新文本并暂存到 `DebugHudState.pending_text`；
    - 无论是否到期，都向窗口请求 HUD 面板矩形 damage（`Window::request_damage`），使本帧成为
      真实的 partial 提交；因此 HUD 可见时每个 tick 都有可呈现的像素工作。
- `App::frame` 在 `refresh_styles` 之后、`refresh_text` 之前调用 `debug_hud::apply_frame`：
  写入 `pending_text` 并 `mark_paint`；顺序保证脏位不会被样式 pass 消费掉。
- 平台定时器：
    - Win32：原 `ANIMATION_TIMER_ID` 的判断从 `app.has_animations()` 改为
      `app.needs_continuous_frames()`，并重命名注释/常量语义为「帧定时器」。
    - macOS：`pump` 的 `dispatcher.wake()` 与 `native.set_animating(...)` 都改读
      `needs_continuous_frames()`。
    - Linux：`dispatcher.wake()` 与 `timeout` 判断改读 `needs_continuous_frames()`，常量改名
      `FRAME_TICK`（仍为 16 ms）。Linux 只在 `state.frame_ready()` 时提交；不 ready 时本 tick 的
      wake 会被消费，但下一 tick 会再次 `wake` 重试，不会永久停住。
- 定时器只负责唤醒；真正请求帧仍走 `App::frame` / `Dispatcher::take_frame_wake` 的既有语义。
- HUD 隐藏后 `needs_continuous_frames` 回到 `has_animations()`，空闲时重新阻塞，不持续消耗。
- 本期不做窗口最小化/遮挡检测：HUD 可见时即使窗口最小化也会按 tick 唤醒；这是已知限制（§10）。

## 6. 错误与边界

| 场景                           | 行为                                                                  |
|--------------------------------|-----------------------------------------------------------------------|
| 未知窗口 `set_debug_hud`       | `Err(UnknownWindow)`；状态不变                                        |
| 重复开启 / 关闭                | 幂等 no-op                                                            |
| 关闭后再开启                   | 旧子树已移除；新建树、重新采样                                        |
| 窗口关闭时 HUD 开启            | `App::close_window` 同时删除 HUD 状态；不留下跨窗口句柄               |
| HUD 文本更新被打断             | `pending_text` 保留到提交成功；下一帧重写并重新标 PAINT；陈旧节点跳过 |
| GPU 计时不可用                 | `GPU` 行显示 `--`；其余指标正常                                       |
| 无渲染器（headless / TestApp） | Draw / Inst 保持 0；HUD 仍可测试文本与布局                            |
| HUD 与动画同时存在             | 两者共用连续帧定时器，互不覆盖                                        |
| HUD 与点按重叠                 | `hit_shape` 恒 false，命中穿透到下层节点                              |
| 窗口缩放                       | HUD 的 right/top 锚点随布局移动；整窗 full damage 覆盖旧区域          |
| feature 关闭                   | 公开 API 不存在，平台定时器条件仅保留原有动画逻辑                     |

## 7. demo 接线

- `Scene::open` 的真实窗口路径：建完场景后（`Scene::install` 里）调用 `app.set_debug_hud(window, true)`；
  仅在 `#[cfg(feature = "hud")]` 下编译。
- `Scene::build_into`（`snapshot_demo` 路径）不安装 HUD；`preview` 也不安装。
- `Scene` **不保存** `hud_visible` 布尔：开关状态以引擎 `App::debug_hud_enabled(window)` 为准。
  这样 `Scene::install` 接收 clone 时，外部 `Scene` 与安装副本不会出现状态分叉。
- `Scene::handle_event` 捕获 `Key::Function(12)` + `KeyState::Down` + 无修饰键：
    - `enabled = !app.debug_hud_enabled(window)`；
    - `let _ = app.set_debug_hud(window, enabled);`（错误时保持引擎真实状态）；
    - 不消费其他按键（F12 不被引擎默认绑定）。
- `SUBTITLE` 在 `hud` feature 下追加「F12 调试 HUD」，无 HUD 构建使用不宣传 F12 的版本。
- 不改变 demo 布局卡片。

## 8. 测试计划

| 层   | 位置                               | 断言                                                                                                                                       |
|------|------------------------------------|--------------------------------------------------------------------------------------------------------------------------------------------|
| 单元 | `src/debug_hud.rs`                 | 格式：10 行固定顺序、未知 GPU 为 `--`、FPS/CPU/GPU/Upload/Present、分辨率/DPI/后端；`hud_rect` 随窗口宽高变化                              |
| 单元 | `src/render/stats.rs`              | `RenderStats` 默认值、字段语义                                                                                                             |
| 单元 | `src/render/windows/compositor.rs` | 已知 ops 的 draw call 计数（Instances 1 次 + Geometry 1 次；空 len 不计）                                                                  |
| 单元 | `src/render/timer.rs`              | `FrameTimings::sample` 缺失/存在、同段首条；分段值语义                                                                                     |
| 集成 | `..`                               | 未知窗口 / 幂等 / 外部删树重建；HUD 节点/文本、命中穿透、a11y hidden、overlay 层级、持续提交、关闭请求擦除帧、500 ms FPS 样本              |
| 集成 | `..`                               | 真实路径安装 HUD；F12 切换；`build_into` 快照路径无 HUD                                                                                    |
| 静态 | 三平台                             | `cargo test` / `--features hud` / `--all-features`；macOS `cargo check --features hud`；Linux 交叉构建受宿主 pkg-config 环境限制时记录原因 |
| 验证 | Windows 实机                       | `cargo run -p ui_core --example demo --features hud` 观察右上角 HUD 每秒变化、F12 隐藏/显示；截图存档                                      |

## 9. 验证命令

```powershell
cargo test -p ui_core
cargo test -p ui_core --features hud
cargo test -p ui_core --features "hud,test-support"
cargo test -p ui_core --example demo --features "hud,test-support"
cargo check -p ui_core --example demo
cargo +nightly clippy -p ui_core --all-targets --all-features -- -D warnings
cargo +nightly fmt -p ui_core -- --check
cargo doc -p ui_core --no-deps --features hud
cargo check -p ui_core --features hud --target x86_64-unknown-linux-gnu
cargo check -p ui_core --features hud --target aarch64-apple-darwin
```

## 10. 已知限制 / 后续

- HUD 是普通 root 子树；`App` 的 overlay API 会把 HUD 保持在最上层，但应用直接 `append_child`
  追加的 root 子节点仍可能覆盖 HUD（§3.2）。
- `Draw` 包含 HUD 自身的 draw calls；这是「本帧实际发出」口径，不是「业务场景净额」。
- `Frame`/`FPS` 的采样是相邻 `App::frame` 入口间隔，不是 GPU 完成时刻；Present 阻塞在驱动内时，
  HUD 反映的是引擎提交节奏。
- CPU 行不含 GPU 提交与 Present 等待；GPU/Upload/Present 分开显示。offscreen 窗口没有 swapchain 时
  Present 段可能是 0.00 ms 而不是 `--`。
- 小窗口上 HUD 面板可能占 damage 面积 > 50%，`DamageAccumulator` 会退化为 full damage；此时
  “partial 提交”不成立，但 HUD 仍持续出帧（功能正确，带宽不最优）。
- 窗口最小化/被完全遮挡时不做可见性过滤，HUD 可见期间仍按 tick 唤醒（§5）。
- 本期不显示内存 / 显存 / 三角形数；三角形数需要把 Geometry 的 index count 一并纳入统计，
  但对 2D UI 的调试价值低于图元 / 实例数。
- `hud` feature 关闭时 API 不存在；这是编译期开关，不是运行时配置。

## 11. 验证记录（2026-11-13，Windows 实机）

> P18 迁移前的记录；当时 demo 仍是独立 crate。当前 example 的测试命令见
> `design/phase-18-demo-example.md` §5.3。

- `cargo test -p ui_core`：**668 passed / 1 ignored**。
- `cargo test -p ui_core --features "hud,test-support"`：**729 passed / 1 ignored**；集成
  `hud` 7、`frame_record` 6、`layout` 4、`snapshot` 8。
- `cargo test -p ui_core_demo --all-features`：**23 单元 + 13 集成**。
- `cargo +nightly clippy -p ui_core -p ui_core_demo --all-targets --all-features -- -D warnings`、
  `cargo +nightly clippy ... --no-default-features`（demo）、`cargo +nightly fmt --check` 全过。
- macOS `cargo check -p ui_core --features hud --target aarch64-apple-darwin`
  （`ui_core_SKIP_SHADERS=1`）通过。Linux 交叉构建在本机 Windows 宿主缺少
  `pkg-config` / `wayland-client` / `fontconfig` sysroot 时无法启动 `wayland-sys` 构建脚本；
  这是环境限制，不把未执行的 Linux 编译伪装成通过（V-108）。
- **真机观察**（迁移前命令）：`cargo run -p ui_core_demo` 的默认 HUD，首屏实测
  `FPS 60.2 / Frame 16.62 ms / CPU 1.11 ms / GPU 0.01 ms / Draw 1 / Prims 250 / Inst 761`；
  WScript `AppActivate` + `SendKeys("{F12}")` 后 HUD 消失，再按一次恢复。
  截图见 [`demo-phase17-hud.png`](demo-phase17-hud.png) 与
  [`demo-phase17-hud-off.png`](demo-phase17-hud-off.png)。
