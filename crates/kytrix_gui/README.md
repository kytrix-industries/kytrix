# kytrix_gui

Kytrix 自研跨平台 GUI 引擎（`crates/kytrix_gui`）。三平台原生图形后端（D3D12 / Metal / Vulkan）、
自建事件循环、细粒度依赖追踪 + 脏矩形、Parley 文本、AccessKit 无障碍。

> 本 crate `publish = false`：它是产品的一部分，不是独立发布的库。

## 快速开始

```rust
use kytrix_gui::{app::{App, Application}, node::builder::NodeBuilder, paint::style::PaintSpec, paint::color::Color, math::{Rect, Size, Scale}, window::WindowOptions};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let mut app = App::new(runtime.handle().clone());
    let window = app.create_window(WindowOptions {
        title: "hello".into(),
        size: Size::new(640.0, 400.0),
        scale: Scale::ONE,
    });
    let root = app.window(window).expect("window").tree().root();
    let _ = NodeBuilder::boxed()
        .layout_override(Rect::new(16.0, 16.0, 240.0, 64.0))
        .paint(PaintSpec { background: Color::from_rgba8([0x00, 0x91, 0xFF, 0xFF]), ..PaintSpec::default() })
        .build(app.window_mut(window).expect("window").tree_mut(), root)?;
    Application::new().run(app)?; // 引擎自有事件循环，跑到没有窗口
    Ok(())
}
```

最小示例（真实窗口 + WARP 离屏快照）：

```text
cargo run -p kytrix_gui_demo
cargo run -p kytrix_gui_demo --features snapshot -- --snapshot out\demo.png
```

## 架构（模块与依赖方向）

| 模块 | 职责 |
|---|---|
| `app` / `window` | `Application`（三平台事件循环入口）、`App`（状态表 + 依赖图 + 窗口集合 + runtime 句柄）、`Window` / `FrameScheduler` |
| `node` | `NodeId`（带代际）/ `NodeTree` / `Signal`+`Derived` / 依赖图 / 脏位 |
| `layout` | taffy `LayoutPartialTree` 适配、measure、命中测试 |
| `text` | Parley 塑形、字形光栅（skrifa/swash）、字形图集、`ShapedLine` 缓存、行内流 |
| `paint` | `Color`/`ColorResolver`、8 类 `Primitive`、`Scene`/lane、`GpuPrimitive` 编码、`Canvas`、`Geometry` |
| `render` | 共享层（图集分配器 / damage / retire / timer）+ 三平台后端（按 `cfg` 编译期选定一份） |
| `platform` | 三平台窗口/事件/IME/剪贴板/DPI/AT-SPI/NSAccessibility/UIA |
| `resources` | `ResourceCache`：字节 → PNG/JPEG/WebP 解码 → 线性预乘 sRGB BGRA8 → 图片图集 tile |
| `a11y` | AccessKit 数据模型、节点树映射、Action 分发、诊断 |
| `interaction` | 焦点 / 滚动 / 虚拟列表 / 浮层 / 拖拽 / 动画 |
| `test_support` | **仅 `test-support`**：`TestApp` / `Probe` / `FrameRecord` / 像素快照 / 诊断（不进发布物） |

依赖方向只向下：`app → (node, layout, text, paint, render, platform, resources, a11y, interaction)`；
`render`/`platform` 内部按 `#[cfg]` 选定单一后端（无 trait / 无运行时枚举）。

## 测试 / golden / bench 命令

```powershell
# L0–L2：单测 + 集成（默认 features）
cargo test -p kytrix_gui
# 测试基建（TestApp / 帧记录 / 像素快照 / 诊断）
cargo test -p kytrix_gui --features test-support
cargo test -p kytrix_gui --features test-support --test frame_record

# L3：像素 golden（Windows WARP；mac/Linux 显式 skip 并打印原因）
cargo test -p kytrix_gui --features test-support --test snapshot -- --nocapture
# 生成 / 更新 golden（写完立刻自检）
$env:UPDATE_GOLDEN = '1'; cargo test -p kytrix_gui --features test-support --test snapshot
Remove-Item Env:UPDATE_GOLDEN

# release 下的探针一致性
cargo test -p kytrix_gui --release --features test-support

# L4：基准（criterion；**非合并门禁**，CI 只要求可编译）
cargo bench -p kytrix_gui --bench perf --no-run
cargo bench -p kytrix_gui --bench perf

# 门禁
cargo +nightly clippy -p kytrix_gui -p kytrix_gui_demo --all-targets --all-features -- -D warnings
cargo +nightly fmt -p kytrix_gui -p kytrix_gui_demo -- --check
cargo doc -p kytrix_gui --no-deps
```

- **golden 详情**（字节语义 / 容差 / 平台策略 / 失败热力图）：[`tests/golden/README.md`](tests/golden/README.md)。
- 缺 shader 工具链时设 `KYTRIX_GUI_SKIP_SHADERS=1`（产物是占位符，**GPU 测试会失败**，只用于交叉 `cargo check`）。
- 交叉 target：`cargo check -p kytrix_gui --target {aarch64-apple-darwin, x86_64-unknown-linux-gnu}`
  （Linux 需要 `pkg-config` stub 提供 `fontconfig`，见 `design/DEVIATIONS.md` V-04）。

## 文档与偏差入口

| 文档 | 内容 |
|---|---|
| `../../custom-gui-engine-design.md` | 主设计（决策 D1–D25、全局不变量） |
| `../../design/phase-01…13-*.md` | 每个实施阶段的详细设计（本 crate 的实现规格） |
| `../../design/DEVIATIONS.md` | **与上游文档不同的每个事实**（F/S/V 编号）；实现前先查这里 |
| `../../AGENTS.md` | 仓库纪律（模块/测试/错误/依赖） |
| `src/render/shaders/contract.md` | `GpuPrimitive` 字节布局 + uniform + 颜色/图集语义（**唯一权威**） |

**偏差纪律**：任何与 `design/*` 不同的实现选择，必须先在 `design/DEVIATIONS.md` 追加条目
（`F-xxx` 事实偏差 / `S-xxx` 实现纪律 / `V-xx` 待复核），再改代码；不写「以后再说」的兼容层。

## 依赖纪律

- 所有依赖在 workspace 根 `Cargo.toml` 声明，本 crate 只 `workspace = true`。
- **禁止**：`wgpu` / `naga` / `gpui-*` / `winit` / Vello 系 / `cosmic-text` 主链路；
  `usvg` 不进本 crate（构建期 SVG 归 `base_ui`，`DEVIATIONS` F-11）。
- `image`（资源解码）与 `criterion`（bench，关默认 features）是 P13 新增的两项。
- 测试基建（`test_support`）只在 `--features test-support` 下编译，发布物零符号。
