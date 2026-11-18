# acceptance（验收 harness）

`ui_core` 的最终验收 harness：**一个二进制，按模式跑不同的口径**。三平台同一份源码。

| 模式 | 需要窗口？ | 需要的特性 | 测什么 |
|------|-----------|-----------|--------|
| `cpu`（默认） | 否（headless） | 默认特性 | 合成负载的完整 CPU 成本（样式 + 文本 + 布局 + 绘制 + 实例编码） |
| `breakdown` | 否 | 默认特性 | 1000 单元文本帧的 pass 归因（定位用，非判据） |
| `window` | **是**（真机真窗口） | 默认特性 | 帧间隔 / 抖动 / UI 线程帧 CPU |
| `idle` | **是**（真机真窗口） | 默认特性 | 窗口静止 60 s 的进程 CPU |
| `interaction` | 否（headless + 离屏读回） | **`test-support`** | GUI 端到端冒烟：输入 → 布局 → 绘制 → 呈现（真实像素）→ a11y |

## 运行命令

```bash
# CPU / 归因（生产特性；不要带 test-support，否则每帧多采 FrameRecord）
cargo run --release -p ui_core --example acceptance -- cpu
cargo run --release -p ui_core --example acceptance -- breakdown

# 真实窗口帧率 / 空闲 CPU（需要显示服务器 / 合成器）
cargo run --release -p ui_core --example acceptance -- window --seconds 20 --hz 60
cargo run --release -p ui_core --example acceptance -- idle --idle-seconds 60

# GUI 端到端冒烟（6 个交互场景；headless，不建平台窗口，不需要显示服务器）
cargo run -p ui_core --features test-support --example acceptance -- interaction
```

`interaction` 的同一套件也作为集成测试在 CI 里跑（默认 `cargo test` **不跑**，显式 `--ignored`）：

```bash
cargo test -p ui_core --features test-support --test gui_smoke -- --ignored --nocapture
```

## 平台与 headless

- **Windows**：离屏读回走 WARP（D3D12 软件设备，与 `tests/golden/warp/` 同一路径）；
  `window` / `idle` 模式需要交互式桌面会话。
- **macOS**：离屏读回走 Metal（`new_offscreen`）；`window` / `idle` 需要登录图形会话。
- **Linux**：离屏读回走 Vulkan（无 WSI 的离屏设备，CI 里用 lavapipe 即可，与
  `tests/golden/lavapipe/` 同一口径）。**没有 compositor 时**（`WAYLAND_DISPLAY` 缺失）：
  `window` / `idle` 无法运行（没有可 present 的 surface），`cpu` 与 `interaction` 照常可用 ——
  后者就是交互闭环在这些机器上的运行形态。

## 滚动偏移的严格断言（D-1 修复后）

`interaction` 模式的场景 ③ 在**三端同一份口径**下严格断言：喂入合成端滚动槽偏移后，读回像素
必须整体位移 3 行（视口顶部 `row3`、底部 `row5`、固定头部不动）。

> 历史缺陷（D-1，已修）：Windows 后端的 `render/windows/compositor/record.rs::draw_ops` 曾每次
> op 只上传 `words[8..28]`（draw 块），而 `RootUniform::with_scroll` 写的是 frame 块
> （`words[5]/[6]`）⇒ 合成端滚动偏移在 Windows 上永远到不了 GPU（Linux/macOS 每 op 推整份
> 112 B，所以生效）。表现是滚动容器的 `scroll_state` / `damage` / `encoded_lanes` 全对，但窗口
> 内容不动。现在三端都每 op 推整份 `RootUniform`，这条断言不再有按平台的逃生口 —— 任何后端把
> 上传范围收窄回 draw 块，场景 ③ 立刻红。
