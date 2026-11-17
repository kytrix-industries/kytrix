# 像素 golden（L3）

这些 PNG 是各平台离屏读回的字节，按**设备类型**分目录（D14）：

| 目录          | 设备                              | 状态                              |
|---------------|-----------------------------------|-----------------------------------|
| `warp/`       | Windows WARP（D3D12 软件设备）    | 既有                              |
| `lavapipe/`   | Linux Mesa llvmpipe（软件 Vulkan）| 2026-11-13 新增（见下）           |
| `metal-real/` | macOS 实机 GPU（Intel Iris Plus 655 / Metal 3） | 2026-11-13 新增（13 份；`clip_subpixel` 待真机重基） |

`SnapshotCase::golden_path()` 按 `device_dir()` 取目录。三个平台都跑；每个平台只和自己的基线比。

## 字节语义（重要）

golden 是 `SnapshotImage.rgba8` 的**原样**落盘：**预乘 sRGB 编码的 RGBA8**。

- 不反预乘：反预乘要除 alpha，低 alpha 的量化误差会被放大，破坏「逐字节可复现」。
- 因此半透明区域在普通看图器里会比最终显示**偏暗**，这是预期行为。
- 尺寸是**物理像素**：同一个 case 在 `scale=2` 下是逻辑尺寸的两倍。

## 命名

`<scene>_<scale>_<theme>.png`，例如 `shapes_1.25_dark.png`（`scale` 去尾零：`1` / `1.25` / `2`）。

| scene      | 内容                                                                 | 尺寸（逻辑） | scale      | theme       |
|------------|----------------------------------------------------------------------|--------------|------------|-------------|
| `shapes`   | 直角矩形 / 四角不同圆角 / 1px 边框 / 圆角 + 边框                     | 240×160      | 1、1.25、2 | light、dark |
| `shadow`   | 解析阴影（blur 4 与 16）                                             | 240×160      | 1          | light       |
| `gradient` | 线性水平 / 垂直 / 径向（OKLCH + OKLab 端点）                         | 240×160      | 1          | light       |
| `colors`   | 纯色 / 半透明叠加 / 高彩度 OKLCH（gamut 边界）                       | 160×120      | 1          | light、dark |
| `image`    | 图片图集：1:1 / 2× / 半透明调制（16×16 四象限 alpha = 255/170/85/0） | 160×120      | 1、2       | light       |
| `clip`     | 嵌套 clip + 溢出子矩形（整数边界；**0 容差**，见下）                 | 240×160      | 1          | light       |
| `clip_subpixel` | 分数坐标边界（容器物理边界 24.25 / 20.5 / 156.5 / 96.75）加四边溢出的子矩形（**0 容差**） | 240×160 | 1 | light |

**没有文字 case**：字形像素依赖宿主字体文件与版本，跨机必然分叉（`../../design/DEVIATIONS.md` F-144 / V-96）。
字形由 `rendered_text_produces_visible_glyph_pixels`（覆盖率 sanity）与 L2 的 `GlyphRun` 断言覆盖。

## 容差（D19：机制冻结，数值可校准）

| 项                                        | 值      | 含义                                            |
|-------------------------------------------|---------|-------------------------------------------------|
| `Tolerance::default().per_channel`        | `2`     | 单通道允许的最大绝对差（`0..=255`，alpha 也参与） |
| `Tolerance::default().max_mismatch_ppm`   | `1_000` | 不匹配像素的**比例**上限，单位 1e-6（= 0.1%）     |
| `Tolerance::GEOMETRY.max_mismatch_ppm`    | `0`     | **几何边界 case**（`clip` / `clip_subpixel`）专用：0 个不匹配像素 |

判定规则：

```text
mismatched_pixels <= floor(compared_pixels × max_mismatch_ppm / 1_000_000)
```

- **不匹配像素** = 任一通道差 `> per_channel` 的像素（含 alpha 通道）。
- 240×160 的 case（38400 像素）允许 38 个不匹配像素；160×120（19200）允许 19 个；
  2×2 的小图允许 **0** 个 —— 比例容差不是小图的免死金牌。
- **`clip` / `clip_subpixel` 用 `Tolerance::GEOMETRY`（`max_mismatch_ppm = 0`）**：这两个 case
  的内容是「平涂色块被裁到某一条像素列/行」，边界像素的取舍是 0/1 语义。0.1% 的比例容差对
  它们没有检测力 —— 整列 20 px 的越界只占 0.05%，会被静默放过（AR2b P1-N1 就是这样进基线的）。
  `per_channel` 仍是 2（只吸收三平台 sRGB 量化的 1 级差），所以它**不是**逐字节比较。
- `max_channel_diff` / `avg_channel_diff` 只进报告、**不参与判定**：AA 边缘和阴影渐变会让
  它们逐次抖动，拿它们做门禁等于让三个平台比「谁更圆」。
- 比例用整数 ppm 而不是浮点：容差要能 `Eq` 比较，判定必须逐平台逐次可复现。
- 数值只允许在**逐张确认**后调整，且必须同时改 `src/test_support/snapshot.rs` 的
  `Tolerance::default` / `Tolerance::GEOMETRY` 与本表（`tolerance_default_is_the_recorded_value`
  会钉住两者一致）。

## 生成 / 更新（bless）

`UI_CORE_BLESS` 存在即重生成（**不看值**），只写**当前平台**目录：

```powershell
# 生成或更新（Windows / WARP）
$env:UI_CORE_BLESS = '1'
cargo test -p ui_core --features test-support --test snapshot -- --nocapture
Remove-Item Env:UI_CORE_BLESS

# 正常比对
cargo test -p ui_core --features test-support --test snapshot
```

```bash
# Linux / lavapipe（需要一个可用的 Vulkan 设备；无设备时 snapshot 走 test-only 离屏实例）
UI_CORE_BLESS=1 cargo test -p ui_core --features test-support --test snapshot -- --nocapture
cargo test -p ui_core --features test-support --test snapshot

# macOS / Metal（离屏渲染器，无真实窗口；实机 GPU 产出 metal-real/）
UI_CORE_BLESS=1 cargo test -p ui_core --features test-support --test snapshot -- --nocapture
cargo test -p ui_core --features test-support --test snapshot
```

每个被覆盖的文件打印一行 `blessed by UI_CORE_BLESS: <case> -> <path>`，作为改动记录占位。
写完在同一次运行里立刻读回并比较，形成自检：**bless 之后仍然必须通过容差**，否则说明
写盘/解码链路本身有问题。

## 重生成纪律（重要）

bless **不是**「测试红了就重生成」的快捷键。只有两类允许重生成：

1. **有意变更**：新增 case、改合成场景、换设备基线。提交信息里写清变更点。
2. **语义修复**：渲染管线修 bug、颜色/AA/阴影行为按设计变更。

第 2 类必须**逐张记录原因**——每张被覆盖的 PNG 一条记录，写进提交信息或
`.review/` 下的评审记录，格式：

```text
golden/<case>.png: <为什么必须变>（关联 commit / 设计条目）
```

例：`golden/warp/shapes_1_light.png: 1px 边框改为按物理像素对齐，AA 边缘左移半像素（F-2xx）`。

禁止把「看起来差不多」当作重生成理由：容差已经吸收了跨设备的量化差，剩下的差异要么是
语义回归，要么是确实要改的语义 —— 两者都必须有人读过。

## 边界 case 的额外断言（不依赖 golden）

`tests/snapshot.rs` 的 `clip_content_never_leaks_past_the_rect_clip_boundaries` 直接读**本机读回像素**，
断言 `clip` 场景的矩形 clip 四条边界：`x = 147` / `y = 97` 必须是内容，`x ≥ 148` / `y ≥ 98`（以及
`x < 8` / `y < 8`）必须是背景。它不读 golden，所以在**重基之前**就能回答「这次重基是不是把缺陷
bless 掉了」—— AR2b P1-N1 的越界列（lavapipe 旧基线 `x=148,y=18..37` 的 20 px 满不透明 ink）
正是这条断言的对象。

## 跨设备不可比

容差**不**用来让跨设备比同一份 golden 变成绿色。2026-11-17 对当时既有的 13 张 case 实测
（`clip_subpixel` 是后来新增的边界 case，未参与该次跨端实测；它的内容是无 AA 的平涂块，
预期跨端一致）
（`per_channel = 2` 口径，mismatch 像素数 / 总像素）：

| 对比                  | `max_channel_diff` | mismatch 比例范围        | 结论 |
|-----------------------|--------------------|--------------------------|------|
| `warp` vs `metal-real`| 1                  | 0 / 13 case              | 逐通道 1 内完全一致（量化差） |
| `warp` vs `lavapipe`  | 64–164             | 0.004% – 0.151%          | 3/13 case 超过 0.1%（`colors_1_dark` 0.151%、`shapes_1_light` 0.143%、`colors_1_light` 0.120%） |
| `lavapipe` vs `metal-real` | 64–164        | 0.004% – 0.151%          | 与 warp-vs-lavapipe 同分布 |

差异集中在 AA 边缘与阴影渐变（`../../design/DEVIATIONS.md` V-112 / S-138）。
**每个平台只跟自己目录里的基线比**；跨平台比同一份 golden 会永久红。

## 失败时

失败信息（`GoldenError::Diff`）带：

- 不匹配像素数 / 参与比较的像素数、比例、本次容差允许的像素数；
- `max_channel_diff`；
- **首个**不匹配像素坐标；
- 全部不匹配像素的**包围盒** `(min_x,min_y)-(max_x,max_y)`（闭区间）；
- golden 路径与热力图路径。

同时写出热力图到 `target/golden-diff/<case>.png`（不匹配像素涂洋红，其余取实际值的一半亮度），
**不入库**。

## 平台

| 平台 | 状态 |
|------|------|
| Windows | 跑（WARP 是系统组件） |
| Linux | 跑（Vulkan 离屏渲染器，`Renderer::with_headless`，F-150）；宿主无可用设备时 `snapshot` 返回 `Gpu`，该用例失败（CI 必须提供 lavapipe） |
| macOS | 跑（Metal 离屏渲染器，`metal-real/` 实机基线） |

- Windows 上的 `SnapshotError::Gpu` 是**测试失败**（设备/渲染/读回真故障），不是跳过（I10）。
- Linux 上「宿主没有 Vulkan 设备」是环境缺失，`snapshot_result_is_typed_per_platform` 会打印原因后返回；
  CI 的 Linux job 有 lavapipe，必须真跑。
