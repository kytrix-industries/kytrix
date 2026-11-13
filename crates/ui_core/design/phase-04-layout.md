# Phase 04 · 布局与命中测试 — 详细设计

> 阶段：P04（布局与命中测试）｜文档版本：v1（可编码版）｜事实基线：`taffy 0.15.0`（源码逐条核验）
> 上游：`custom-gui-engine-design.md` §0（I5/I11）、§3.4、§4.1–§4.4、§6.2、§8.4、附录 A；
> `design/phase-01-foundation.md` §4（模块根模板）、§4.5（公开路径）、§6.3（`math`）、§7（像素契约）、附录 A.1（taffy 事实）；
> `design/phase-03-node-state.md`（节点/状态/脏位/样式）；`design/DEVIATIONS.md`（F-17/F-34…F-44、S-23…S-39、V-23…V-30）。
> 已落地代码基线：`..`、`node/{tree,dirty,builder,id,signal,dependency}.rs`、
> `paint/{style,scene,canvas,primitive}.rs`。

---

## 0. 结论摘要

| #   | 结论                                                                                                                                                                                                                                                            | 依据                                                                                                                   |
|-----|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|------------------------------------------------------------------------------------------------------------------------|
| C1  | **布局树就是节点树**：`NodeTree` 直接实现 taffy 0.15.0 的 7 个低级 trait（`TraversePartialTree` / `LayoutPartialTree` / `LayoutContainingBlock` / `CacheTree` / `LayoutFlexboxContainer` / `LayoutGridContainer` / `LayoutBlockContainer`），不建第二棵树（I5） | probe `kg-p04-probe` 全绿（§2.4）                                                                                      |
| C2  | **不实现 `RoundTree`**（以及 `TraverseTree` / `PrintTree`）：v1 永不调用 `round_layout`（P01 §7.5 / F-17），`RoundTree::set_final_layout` 与 hoisted 读接口没有任何写者/读者                                                                                    | probe 证明 8 个 trait 都能编译（`RoundTree` 需要多一个 92 B 的 `final_layout` 字段）→ 结论是**不该落**，不是「不能落」 |
| C3  | **绝对坐标由引擎 post-pass 产生**：`set_unrounded_layout` 只写 taffy 的**父相对** `location`，`NodeTree::absolutize()` 一次 DFS 把它换算成窗口绝对 `LayoutBox.bounds`（含出流/hoisted 子树）                                                                    | probe：flex/grid/block/绝对定位/hoisted/剪枝全部断言通过（§7）                                                         |
| C4  | **增量成立**：改一个子节点后，兄弟子树 taffy cache 命中；无变化时整轮 0 重算                                                                                                                                                                                    | probe 实测：改 1 个子节点 ⇒ `laid_out=2, cache_hits=4`；无变化 ⇒ `laid_out=0, cache_hits=2`                            |
| C5  | **不强制圆整**：只走 `compute_root_layout`，实测 `10.5 × 3.25` 原样保留                                                                                                                                                                                         | probe `fractional_sizes_are_not_rounded`                                                                               |
| C6  | **根节点必须显式撑满**（`size: 100% × 100%`）：`compute_root_layout` 只接受 `available_space`，flex/grid 根的 `auto` 是 shrink-to-fit（实测窗口 200×200 时根塌成 0×0）                                                                                          | probe（此坑不在任何上游文档里）                                                                                        |
| C7  | **`taffy::Cache` 走旁表 `Vec<Option<Cache>>`**（按 arena 槽位下标），不内联 `Node`；`Cache` 实测 408 B                                                                                                                                                          | probe 尺寸打印                                                                                                         |
| C8  | **`MeasureFn = Box<dyn Fn(taffy::Size<Option<f32>>, taffy::Size<taffy::AvailableSpace>) -> math::Size>`**（taffy `compute_leaf_layout` 的逐字形状）；**不建独立 measure 缓存**，失效入口是 `invalidate_measure`                                                 | probe：block 下 `available.width=100` 确定、第二轮 measure 零调用                                                      |
| C9  | **`LayoutOverride` 是出流覆盖**：`NodeBuilder::layout_override(rect)` 同时把 `position` 置 `Absolute`；`rect` 相对**父 border box**；父不为它留空间                                                                                                             | probe：override 覆盖 taffy 的出流位置                                                                                  |
| C10 | **`hit_test` 是逆绘制序 DFS**：**只按 clip 剪枝**（`Clip::Bounds` 的 padding box；布局盒不能剪枝，出流子节点可溢出）+ clip 链 + `hit_shape`；`Overlay` 不需要特殊分支（排在 `order` 末尾即可）                                                                  | probe 8 个命中用例全绿 + B2 的两个溢出用例                                                                             |
| C11 | **错误面只落 `InvalidMeasure`**（P01 #9）；`LayoutInvalid`（#10）**不落**（taffy 低级 API 不返回 `Result`，唯一后置校验就是 measure 违规）                                                                                                                      | §14                                                                                                                    |
| C12 | **入口是 `Window::compute_layout(&mut self) -> Result<LayoutReport, EngineError>`**，不新增 `App::layout`（`App` 无任何贡献，只为窗口查找而引入 `Option<Result<…>>` 嵌套）                                                                                      | §13 / 裁决项 #2                                                                                                        |
| C13 | **零新依赖**：`taffy` 已在、`ahash`/`smallvec` 已在；`Cargo.toml` **不变**                                                                                                                                                                                      | §17                                                                                                                    |
| C14 | **`Node` 尺寸**：P03 实测 **888 B** → P04 实测 **1016 B**（+40 字段 + 88 对齐/填充）；`node_size_within_budget` 的上界改为 **1152 B**（给 P05 留余量；超了走 `style: Box<Style>`）                                                                              | §8.3 / V-31                                                                                                            |

**本文档的 5 个需裁决项**（§18）：① 不实现 `RoundTree`；② 不新增 `App::layout`；③ `HitResult` 路径落 `layout::hit_test`（P01
写的是 `event::*`）；
④ `LayoutBox` 增加 `border: Edges`；⑤ `LayoutOverride` 的 builder 副作用（强制 `position: Absolute`）。

---

## 1. 范围与非目标

### 1.1 本阶段交付（新增）

| 文件                       | 形态           | 内容                                                                                                                                      |
|----------------------------|----------------|-------------------------------------------------------------------------------------------------------------------------------------------|
| `src/layout.rs`            | **公共模块根** | 契约类型 `Axis` / `MeasureFn` / `LayoutOverride` / `LayoutReport`；`pub mod hit_test; pub(crate) mod measure; pub(crate) mod taffy_tree;` |
| `src/layout/taffy_tree.rs` | 叶子           | 7 个 taffy trait 的 `impl … for NodeTree`（**只有 trait impl，没有 inherent impl**）+ 自由函数 `run` / `dispatch_child_layout` + id 映射  |
| `src/layout/measure.rs`    | 叶子           | `compute_leaf`（taffy `compute_leaf_layout` 适配 + 违规检测）+ `InvalidMeasure` 首条优先                                                  |
| `src/layout/hit_test.rs`   | 叶子           | `HitResult` / `ClipChain` / `hit_test` 遍历（逆序 + clip + `hit_shape`）                                                                  |

### 1.2 本阶段修改

| 文件                  | 改动                                                                                                                                                                                                                                                                                                                                                                                                                                           |
|-----------------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `src/node/tree.rs`    | `Node` 增加 `local` / `local_border` / `measure` / `hit_shape` / `layout_override` / `laid_out_epoch` 字段；`LayoutBox` 增加 `border`；`NodeTree` 增加 `layout_cache` / `hoisted` / `layout_epoch` / `layout_stats` / `invalid_measure` 字段与**同一 inherent impl 块**里的方法（`insert_child` 收 `NodeSpec`、**`pub fn set_style`**、`invalidate_measure`、`set_layout_override`、`compute_layout`、`absolutize`、`hit_test`、taffy 访问器） |
| `src/paint/style.rs`  | `Style` 增加 `clip: Clip`；新增 `Clip { None, Bounds }`                                                                                                                                                                                                                                                                                                                                                                                        |
| `src/node/builder.rs` | 增加 `.measure()` / `.hit_shape()` / `.layout_override()`；`build` 把 5 个部件装进 `NodeSpec`                                                                                                                                                                                                                                                                                                                                                  |
| `src/node/dirty.rs`   | `depends_on_children` 增加 `layout_override.is_some() → false` 短路（P03 §9.3 表最后一行预埋的扩展点）                                                                                                                                                                                                                                                                                                                                         |
| `src/window.rs`       | 增加 `size()` / `set_size()` / `compute_layout() -> Result<LayoutReport, EngineError>`                                                                                                                                                                                                                                                                                                                                                         |
| `src/error.rs`        | 追加 `EngineError::InvalidMeasure { node, axis, value }`（P01 #9，实现 Phase 由 P05 改为 **P04**）                                                                                                                                                                                                                                                                                                                                             |
| `src/lib.rs`          | 增加 `pub mod layout;`                                                                                                                                                                                                                                                                                                                                                                                                                         |

### 1.3 明确非目标（交给后续 Phase）

| 不做                                                                            | 交给      |
|---------------------------------------------------------------------------------|-----------|
| 文本塑形 / 断行 / 行内流 / 基线（`LayoutBox.baseline`）                         | P05       |
| 图集、`GlyphRun` 的生成                                                         | P05/P06   |
| paint pass / lane 重建 / damage 物理化（`to_physical_covering` + `aa_padding`） | P06       |
| 渲染（三套 backend）                                                            | P07+      |
| 交互事件、焦点、滚动偏移、`Overlay` 栈语义、`DragState`                         | P10 / P13 |
| `Overlay` 节点类型、虚拟列表                                                    | P13       |
| 圆角 clip / 非矩形 `hit_shape` 的 GPU 侧语义                                    | P06       |

---

## 2. 事实基线（全部来自真实源码）

> 根：`E:\.cargo\registry\src\rsproxy.cn-e3de039b2554c837\taffy-0.15.0\src\`，行号相对该目录。
> workspace 的 taffy 配置（`Cargo.toml`）：`=0.15.0`、`default-features = false`、
> `features = ["std", "flexbox", "grid", "block_layout", "calc", "content_size"]`。

### 2.1 七个要实现的 trait 的完整签名（逐字）

```rust
// tree/traits.rs:147
pub trait TraversePartialTree {
    type ChildIter<'a>: Iterator<Item=NodeId>
    where
        Self: 'a;
    fn child_ids(&self, parent_node_id: NodeId) -> Self::ChildIter<'_>;
    fn child_count(&self, parent_node_id: NodeId) -> usize;
    fn get_child_id(&self, parent_node_id: NodeId, child_index: usize) -> NodeId;
}
// tree/traits.rs:167（marker，仅 RoundTree/PrintTree 需要 → 本阶段不实现）
pub trait TraverseTree: TraversePartialTree {}

// tree/traits.rs:173
pub trait LayoutPartialTree: TraversePartialTree {
    type CoreContainerStyle<'a>: CoreStyle<CustomIdent=Self::CustomIdent>
    where
        Self: 'a;
    type CustomIdent: CheapCloneStr;
    fn get_core_container_style(&self, node_id: NodeId) -> Self::CoreContainerStyle<'_>;
    fn resolve_calc_value(&self, val: *const (), basis: f32) -> f32 { 0.0 }   // 默认实现
    fn set_unrounded_layout(&mut self, node_id: NodeId, layout: &Layout);
    fn compute_child_layout(&mut self, node_id: NodeId, inputs: LayoutInput) -> LayoutOutput;
}

// tree/traits.rs:212
pub trait LayoutContainingBlock: LayoutPartialTree {
    type OofItemStyle<'a>: OofItemStyle<CustomIdent=Self::CustomIdent>
    where
        Self: 'a;
    fn get_oof_item_style(&self, node_id: NodeId) -> Self::OofItemStyle<'_>;
    fn clear_hoisted_children(&mut self, node_id: NodeId);
    fn add_hoisted_children(&mut self, node_id: NodeId, hoisted: &[NodeId]);
    fn get_detailed_layout_info(&self, node_id: NodeId) -> &DetailedLayoutInfo<Self::CustomIdent> {
        &DetailedLayoutInfo::None                                                // 默认实现
    }
}

// tree/traits.rs:260
pub trait CacheTree {
    fn cache_get(&mut self, node_id: NodeId, input: &LayoutInput) -> Option<LayoutOutput>;
    fn cache_store(&mut self, node_id: NodeId, input: &LayoutInput, layout_output: LayoutOutput);
    fn cache_clear(&mut self, node_id: NodeId);
}

// tree/traits.rs:275（**本阶段不实现**，见 §5.3）
pub trait RoundTree: TraverseTree {
    fn get_unrounded_layout(&self, node_id: NodeId) -> Layout;
    fn set_final_layout(&mut self, node_id: NodeId, layout: &Layout);
    fn is_out_of_flow(&self, node_id: NodeId) -> bool;
    fn hoisted_child_count(&self, node_id: NodeId) -> usize;
    fn get_hoisted_child_id(&self, node_id: NodeId, index: usize) -> NodeId;
}

// tree/traits.rs:307（feature = "flexbox"）
pub trait LayoutFlexboxContainer: LayoutPartialTree {
    type FlexboxContainerStyle<'a>: FlexboxContainerStyle
    where
        Self: 'a;
    type FlexboxItemStyle<'a>: FlexboxItemStyle
    where
        Self: 'a;
    fn get_flexbox_container_style(&self, node_id: NodeId) -> Self::FlexboxContainerStyle<'_>;
    fn get_flexbox_child_style(&self, child_node_id: NodeId) -> Self::FlexboxItemStyle<'_>;
}

// tree/traits.rs:328（feature = "grid"）
pub trait LayoutGridContainer: LayoutPartialTree {
    type GridContainerStyle<'a>: GridContainerStyle<CustomIdent=Self::CustomIdent>
    where
        Self: 'a;
    type GridItemStyle<'a>: GridItemStyle<CustomIdent=Self::CustomIdent>
    where
        Self: 'a;
    fn get_grid_container_style(&self, node_id: NodeId) -> Self::GridContainerStyle<'_>;
    fn get_grid_child_style(&self, child_node_id: NodeId) -> Self::GridItemStyle<'_>;
    fn set_detailed_grid_info(&mut self, _node_id: NodeId,
                              _detailed_grid_info: DetailedGridInfo<Self::CustomIdent>) { /* 默认空实现 */ }
}

// tree/traits.rs:358（feature = "block_layout"）
pub trait LayoutBlockContainer: LayoutPartialTree {
    type BlockContainerStyle<'a>: BlockContainerStyle
    where
        Self: 'a;
    type BlockItemStyle<'a>: BlockItemStyle
    where
        Self: 'a;
    fn get_block_container_style(&self, node_id: NodeId) -> Self::BlockContainerStyle<'_>;
    fn get_block_child_style(&self, child_node_id: NodeId) -> Self::BlockItemStyle<'_>;
    fn compute_block_child_layout(&mut self, node_id: NodeId, inputs: LayoutInput,
                                  block_ctx: Option<&mut BlockContext<'_>>) -> LayoutOutput {
        self.compute_child_layout(node_id, inputs)                                 // 默认实现
    }
}
```

**associated type 的可实现性（逐条核实）**

| assoc type                                  | 引擎的取值                                                               | 证据                                                                                                                                                           |
|---------------------------------------------|--------------------------------------------------------------------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `LayoutPartialTree::CoreContainerStyle<'a>` | `&'a taffy::Style<SmolStr>`                                              | `impl<S: CheapCloneStr> CoreStyle for Style<S>`（`style/mod.rs:959`）+ `impl<T: CoreStyle> CoreStyle for &'_ T`（`:1067`）                                     |
| `LayoutPartialTree::CustomIdent`            | `SmolStr`                                                                | `CheapCloneStr` 的 blanket impl（`style/mod.rs:67`）要求 `AsRef<str> + From<&str> + From<String> + Eq + Clone + Default + Debug + 'static`，`SmolStr` 全部满足 |
| `LayoutContainingBlock::OofItemStyle<'a>`   | `&'a taffy::Style<SmolStr>`                                              | `impl<S> OofItemStyle for Style<S>`（`:1040`）+ `impl<T: OofItemStyle> OofItemStyle for &'_ T`（`:1148`）                                                      |
| `LayoutFlexboxContainer::*Style<'a>`        | `&'a taffy::Style<SmolStr>`                                              | `:1280` / `:1346` + `&T` 版本 `:1313` / `:1366`                                                                                                                |
| `LayoutGridContainer::*Style<'a>`           | `&'a taffy::Style<SmolStr>`                                              | `:1386` / `GridItemStyle` + `&T` 版本                                                                                                                          |
| `LayoutBlockContainer::*Style<'a>`          | `&'a taffy::Style<SmolStr>`                                              | `:1174` / `:1210` + `&T` 版本 `:1192` / `:1245`                                                                                                                |
| `TraversePartialTree::ChildIter<'a>`        | `std::iter::Map<std::slice::Iter<'a, NodeId>, fn(&NodeId) -> TaffyNode>` | probe 实测（`GAT + where Self: 'a` 语法照抄即可编译）                                                                                                          |

### 2.2 计算函数（`compute/`）

```rust
// compute/mod.rs:77 —— 唯一的顶级入口；**内部不调用 round_layout**
pub fn compute_root_layout(
    tree: &mut (impl crate::tree::LayoutContainingBlock + CacheTree),
    root: NodeId, available_space: Size<AvailableSpace>);

// compute/mod.rs:330
pub fn compute_cached_layout<Tree: CacheTree + ?Sized, F>(
    tree: &mut Tree, node: NodeId, inputs: LayoutInput, compute_uncached: F) -> LayoutOutput
where
    F: FnOnce(&mut Tree, NodeId, LayoutInput) -> LayoutOutput;

// compute/leaf.rs:20 —— 4 个参数；measure 闭包形状 = (known_dimensions, available_space) -> Size<f32>
pub fn compute_leaf_layout<M: FnOnce(Size<Option<f32>>, Size<AvailableSpace>) -> Size<f32>>(
    inputs: LayoutInput, style: &impl CoreStyle,
    resolve_calc_value: impl Fn(*const (), f32) -> f32, measure_function: M) -> LayoutOutput;

// compute/oof.rs:348
pub fn compute_oof_layout(tree: &mut impl LayoutContainingBlock, node_id: NodeId, output: &mut LayoutOutput);

// compute/mod.rs:456
pub fn compute_hidden_layout(tree: &mut (impl LayoutPartialTree + CacheTree), node: NodeId) -> LayoutOutput;

// compute/flexbox.rs:255 / compute/grid/mod.rs:219 / compute/block.rs:374
pub fn compute_flexbox_layout(tree: &mut impl LayoutFlexboxContainer, node: NodeId, inputs: LayoutInput) -> LayoutOutput;
pub fn compute_grid_layout<Tree: LayoutGridContainer>(tree: &mut Tree, node: NodeId, inputs: LayoutInput) -> LayoutOutput;
pub fn compute_block_layout(tree: &mut impl LayoutBlockContainer, node: NodeId, inputs: LayoutInput,
                            block_ctx: Option<&mut BlockContext<'_>>) -> LayoutOutput;
```

**`compute_root_layout` 的三步（`compute/mod.rs:77-183`，逐字要点）**

1. 读根的 `position` / `direction`；`position` 出流且 `available_space` 两轴都确定时，根治走 OOF 定位（否则走 in-flow）；
2. in-flow 路径（`:188-324`）：算 `known_dimensions`（block 根会自带 stretch 宽度）、组
   `LayoutInput { run_mode: PerformLayout, sizing_mode: InherentSize, … }`、
   `let root_is_cached = tree.cache_get(root, &inputs).is_some()`、`tree.compute_child_layout(root, inputs)`，然后
   **手工**组 `Layout` 并 `tree.set_unrounded_layout(root, &layout)`（`:321`）；
3. 若还有未认领的出流候选，跑一次最终定位 pass 并 `tree.add_hoisted_children(root, &hoisted)`（`:180`）。
   **整个函数没有任何圆整调用**（`round_layout` 只被 `TaffyTree::compute_layout_with_measure` 在 `config.use_rounding`
   时调用，`:976-981`）。

### 2.3 三个必须记住的语义事实

| #  | 事实                                                                                                        | 证据                                                                                                                                                                                                    | 对引擎的影响                                                                                   |
|----|-------------------------------------------------------------------------------------------------------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|------------------------------------------------------------------------------------------------|
| F1 | `Layout.location` 是**父相对**（in-flow 相对父 border box；出流相对**包含块 border box**）                  | `compute/mod.rs:284-307`（root 用 ICB 原点）；`compute/flexbox.rs:511-516`（`absolute_position_offset = border`，**不含 padding**）；`compute/oof.rs:672-701`（`location = …position() + area_offset`） | `LayoutBox.bounds` 必须由引擎 post-pass 累加成绝对（§7）；**不能**直接拿 `location` 当窗口坐标 |
| F2 | 出流盒的包含块**可以不是它的树父**（CSS：最近的定位祖先）                                                   | `compute/oof.rs` 的候选冒泡 + `compute_oof_layout` 的 claims（`style/mod.rs:158` 默认 `absolute: position().is_positioned(), fixed: false`）；root 最终 pass 认领全部剩余（`compute/mod.rs:165-181`）   | 出流盒的父相对原点必须按 **taffy 给的 hoisted 列表**解释，不能按树父（§7.3）                   |
| F3 | `compute_root_layout` **不会**把窗口尺寸强加给根                                                            | 实测（probe）：窗口 200×200、根 `size: auto` ⇒ 根 0×0、百分比子节点全 0                                                                                                                                 | `NodeTree::new` 给根 `size: 100% × 100%`（§8.4）                                               |
| F4 | taffy 的 `Cache` key 已包含 `(known_dimensions, available_space, sizing_mode, run_mode, parent_size, mode)` | `tree/cache.rs:254-280`（`CacheKey`）                                                                                                                                                                   | 引擎**不需要**自己的 `(measure_version, available_space)` 缓存（§9.3）                         |
| F5 | `TaffyTree::mark_dirty` 清的是**节点 + 全部祖先**的 cache，且**不早停**                                     | `tree/taffy_tree.rs:935-958`（`ClearState::AlreadyEmpty` 只是「已经清过了」）                                                                                                                           | 引擎的 cache 失效协议同构：`node..root` 无条件清（§6.2）                                       |
| F6 | `taffy::NodeId` 是 `u64` 的 newtype，`new`/`From<u64>` 双向                                                 | `tree/node.rs:13-32`                                                                                                                                                                                    | 直接用 P03 的 `NodeId::to_bits()`（`generation << 32                                           | index`）做双向映射（§5.4） |
| F7 | `taffy::Cache` 实测 **408 B**、`taffy::Layout` **92 B**、`taffy::Style<SmolStr>` **552 B**                  | probe 尺寸打印                                                                                                                                                                                          | cache 旁表 10k 节点 ≈ 4.1 MB（§6.1）；`Node` 不能内联 `Cache`                                  |

### 2.4 probe 记录（可复现）

| 项      | 内容                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
|---------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 位置    | `C:\Users\chenbaiyu\AppData\Local\Temp\kg-p04-probe`（**不在仓库内**）                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| 形态    | 独立 crate：`edition 2024`、`taffy = { version = "=0.15.0", default-features = false, features = [std, flexbox, grid, block_layout, calc, content_size] }`、`smol_str 0.3.7`、`smallvec 1.16.5`；`[lints.clippy]` **逐字复制** workspace 根                                                                                                                                                                                                                                                                       |
| 文件树  | `src/lib.rs`、`src/math.rs`、`src/error.rs`、`src/node.rs` + `node/{tree,builder}.rs`、`src/layout.rs` + `layout/{taffy_tree,measure,hit_test}.rs`、`src/paint.rs` + `paint/style.rs`、`src/window.rs`（共 13 文件 / **3110 行**，含 40 个单测）                                                                                                                                                                                                                                                                  |
| 命令    | `cargo +nightly clippy --all-targets -- -D warnings`（clippy 0.1.99）→ **exit 0**；`cargo test` → **40 passed**；`cargo test --release` → **39 passed**（1 个 `#[cfg(debug_assertions)]` 用例不编译）；`cargo +nightly fmt --check` → **exit 0**；`rustc 1.97.1`                                                                                                                                                                                                                                                  |
| 结论 P1 | **7 个 trait 全部可实现，签名与 §2.1 一致**（含 GAT `type ChildIter<'a> = Map<slice::Iter<'a, NodeId>, fn(&NodeId) -> TaffyNode> where Self: 'a`）                                                                                                                                                                                                                                                                                                                                                                |
| 结论 P2 | **`RoundTree` 也可实现**（额外需要每节点 `final_layout: Layout` = 92 B），但 `set_final_layout` / `hoisted_child_count` / `get_hoisted_child_id` **没有任何非 test 写者/读者** → 落地即违反 AGENTS §1.2（§5.3 裁决）                                                                                                                                                                                                                                                                                              |
| 结论 P3 | `compute_root_layout` 路径**不圆整**：`10.5 × 3.25` 原样保留（§0-C5）                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| 结论 P4 | 增量：改一个子节点 → `LayoutReport { laid_out: 2, moved: 2, cache_hits: 4, cache_misses: 4 }`；无变化 → `{ laid_out: 0, moved: 0, cache_hits: 2, cache_misses: 0 }`                                                                                                                                                                                                                                                                                                                                               |
| 结论 P5 | 绝对坐标 post-pass 正确：嵌套 padding（4,4）、absolute 相对定位祖先、hoisted 到非父祖先（(5,7)，即相对包含块 **border box**）、`LayoutOverride`（(8,16)）                                                                                                                                                                                                                                                                                                                                                         |
| 结论 P6 | 根 `size: auto` 会塌成 0×0（§0-C6）；`size: 100%` 后 flex/grid/block 全部按窗口尺寸工作                                                                                                                                                                                                                                                                                                                                                                                                                           |
| 结论 P7 | measure：block 容器把 `Definite(100)` 传给闭包；第二轮 **零调用**（cache 命中）；flex 主轴尺寸来自 measure 的固有宽度；`align-items: stretch` 会覆盖 cross 轴（布局算法行为，非 bug）                                                                                                                                                                                                                                                                                                                             |
| 结论 P8 | `NaN` measure → `Err(InvalidMeasure { axis: X })`（首条优先）；`-1.0` → `axis: Y`                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| 结论 P9 | **lint 实测（在 probe 内）**：①`clippy::field_scoped_visibility_modifiers = "forbid"` 让 `pub(crate)` **字段**直接编译失败（5 处全报）⇒ 部件结构体只能私有字段 + 构造器；②`clippy::multiple_inherent_impl = "forbid"` 对「同类型两个 inherent impl（**哪怕同文件**）」直接报 `multiple implementations of this structure` ⇒ 全 crate 每个类型**只能有一个** `impl Type { … }` 块；③trait impl 放另一个文件**不触发**该 lint（probe 的 7 个 `impl Trait for NodeTree` 全在 `layout/taffy_tree.rs`，clippy 0 诊断） |

---

## 3. 文件树与模块形态

```text
crates/ui_core/src/
├── layout.rs                 # 公共模块根：契约类型（Axis/MeasureFn/LayoutOverride/LayoutReport）+ 3 个 mod 声明
├── layout/
│   ├── taffy_tree.rs         # 7 个 trait impl + run() + dispatch_child_layout()（free fn）
│   ├── measure.rs            # compute_leaf()：taffy compute_leaf_layout 适配 + 违规检测
│   └── hit_test.rs           # HitResult / ClipChain / hit_test()
├── node/tree.rs              # （改）Node 字段 / LayoutBox.border / NodeTree 旁表 / 单一 inherent impl
├── node/builder.rs           # （改）.measure() / .hit_shape() / .layout_override()
├── node/dirty.rs             # （改）depends_on_children 的 layout_override 短路
├── paint/style.rs            # （改）Style.clip: Clip
├── window.rs                 # （改）size/set_size/compute_layout
├── error.rs                  # （改）InvalidMeasure
└── lib.rs                    # （改）+ pub mod layout;
```

**模块根写法（P01 §4.1 的「公共模块根」模板，逐字）**

```rust
// src/layout.rs
pub mod hit_test;
pub(crate) mod measure;
pub(crate) mod taffy_tree;

// …契约类型定义（公开模块根允许，且 P01 §4.3a 已实测自命名根不触发 definition_in_module_root）…
```

**三条硬约束（probe P9 的证据）**

1. **每个类型只有一个 inherent `impl` 块**（`multiple_inherent_impl`）。因此：
    - `NodeTree` 的全部 inherent 方法（含 `compute_layout` / `hit_test` / `absolutize`）**必须**写在 `node/tree.rs` 的
      **那一个** `impl NodeTree { … }` 里；
    - `layout/taffy_tree.rs` 里**只有** `impl TaffyTrait for NodeTree`，算法逻辑写成自由函数（`run` /
      `dispatch_child_layout`）；
    - `Node` / `LayoutBox` / `NodeKind` 同理（P03 的 `impl Node` 块整体保留，只加方法与字段）。
2. **部件结构体不能有字段的 *scoped* 可见性修饰**（`field_scoped_visibility_modifiers` 禁止 `pub(crate)`/`pub(super)`/
   `pub(in …)`，probe 实测 5 处全报）：`NodeSpec` 用私有字段 +
   `NodeSpec::new(kind, style, measure, layout_override, hit_shape)`（5 个参数，`too_many_arguments` 已 allow）。
3. **不新增 `mod.rs` / `#[path]` / `pub use`**；`layout/taffy_tree.rs` 是 `pub(crate) mod`，其 `run` 只被 `node/tree.rs`
   调用。

---

## 4. 公共 / 私有边界

### 4.1 公开路径表增量（P01 §4.5 / P03 §4.1 的续）

| 类型             | 公开路径                                   | 定义文件                 | 消费 Phase                                                 |
|------------------|--------------------------------------------|--------------------------|------------------------------------------------------------|
| `Axis`           | `ui_core::layout::Axis`                    | `src/layout.rs`          | P04（`InvalidMeasure` payload）                            |
| `MeasureFn`      | `ui_core::layout::MeasureFn`               | `src/layout.rs`          | P05（文本 measure）/ P08（`NodeBuilder::measure`）         |
| `LayoutOverride` | `ui_core::layout::LayoutOverride`          | `src/layout.rs`          | P08 / P13（分栏拖拽）                                      |
| `LayoutReport`   | `ui_core::layout::LayoutReport`            | `src/layout.rs`          | P06（帧记录）/ P08（测帧推进）                             |
| **`HitResult`**  | **`ui_core::layout::hit_test::HitResult`** | `src/layout/hit_test.rs` | P10（事件分发）/ P13（Overlay 命中）/ P14（a11y hit test） |
| **`ClipChain`**  | **`ui_core::layout::hit_test::ClipChain`** | `src/layout/hit_test.rs` | P10 / P13                                                  |
| `Clip`           | `ui_core::paint::style::Clip`              | `src/paint/style.rs`     | P04（hit test）/ P06（clip 图元）                          |
| `NodeSpec`       | **crate 私有**（`pub(crate)`）             | `src/node/tree.rs`       | `NodeBuilder`（唯一构造者）                                |

> **对 P01 §4.5 的修订（→ F-45）**：P01 把 `HitResult` 归到 `ui_core::event::*` / `src/event.rs`。P04 的文件树把命中测试放在
> `layout/hit_test.rs`（本阶段 brief 明确要求），而命中结果是**布局坐标**的产物、`event` 模块（P10）此时还不存在。
> 按 P01 §4.5 自己的判据「公开路径 = 类型实际定义所在的文件路径」，路径取 `layout::hit_test::*`；P01 的路径表里 `HitResult`
> 那一行以本条为准（历史正文不改）。

### 4.2 §1.3 白名单的传递闭包扩充

§1.3 已列 `MeasureFn` / `LayoutOverride` / `HitResult`（「样式 / 布局」与「事件」两行）。本阶段登记由它们**传递带出**的类型：

| 新增公开类型   | 由谁带出                                                                 | 理由                                                                                              |
|----------------|--------------------------------------------------------------------------|---------------------------------------------------------------------------------------------------|
| `Axis`         | `EngineError::InvalidMeasure { axis: Axis }` 的 payload（P01 §6.2.2 #9） | 错误面已经是白名单的传递闭包（P01 §6.2.2）                                                        |
| `ClipChain`    | `HitResult::clip_chain` 的字段类型                                       | `HitResult` 白名单字段的泊松闭包；用定长结构而不是 `SmallVec`，避免把 `smallvec` 版本写进公开 API |
| `LayoutReport` | `Window::compute_layout` 的返回类型                                      | `Window` 白名单，`LayoutReport` 是它的返回值（与 P03 的 `StylePassReport` 同构）                  |
| `Clip`         | `Style::clip` 的字段类型                                                 | `Style` 白名单字段（与 F-35 的「`Style.inner` 泄漏 taffy」同一类登记）                            |

**明确不扩充**：`NodeSpec`、`LayoutStats`、任何 taffy trait 的再导出（应用层若需要 taffy 类型，按 F-35 自行声明
`taffy = { workspace = true }`）。

**方法级公开面增量（P04 新增，登记于此以便 P08 迁移引用）**

| 方法                            | 签名                                                                                                           | 用途                                                                                                                                                        |
|---------------------------------|----------------------------------------------------------------------------------------------------------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `NodeTree::set_style`           | `pub fn set_style(&mut self, node: NodeId, style: Style) -> Result<(), EngineError>`                           | **结构性/静态样式变更**的唯一公开入口（写值 + 标 `LAYOUT\|PAINT` + 清 `node..root` 缓存）。**动态属性请用 `bind_style`**（`StylePatch` 通道），不要每帧调它 |
| `NodeTree::invalidate_measure`  | `pub fn invalidate_measure(&mut self, node: NodeId) -> Result<(), EngineError>`                                | 「measure 的输入变了」（P05 的字体栈/文本内容变更）                                                                                                         |
| `NodeTree::set_layout_override` | `pub fn set_layout_override(&mut self, node: NodeId, over: Option<LayoutOverride>) -> Result<(), EngineError>` | 更新覆盖矩形（P13 分栏拖拽）                                                                                                                                |
| `NodeTree::hit_test`            | `pub fn hit_test(&self, point: Point) -> Option<HitResult>`                                                    | 命中测试（P10 消费；`HitResult` 见 §4.1）                                                                                                                   |
| `Node::clip`                    | `pub fn clip(&self) -> Clip`                                                                                   | 子树裁剪策略（P06 的 clip 图元会读）                                                                                                                        |

### 4.3 每个 `pub(crate)` item 与其**非 test 消费者**（dead_code 审计表）

> 规则（P02 §3.2 / P03 §4.3）：`pub(crate)` item 必须在**非 test 构建**里被调用，否则 `-D warnings` 失败。

| 文件                   | item                                                                                                                                                                                                                                                                | 非 test 消费者                                                                                                                                                               |
|------------------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `layout/taffy_tree.rs` | `run`                                                                                                                                                                                                                                                               | `NodeTree::compute_layout`（`node/tree.rs`）                                                                                                                                 |
|                        | `dispatch_child_layout`                                                                                                                                                                                                                                             | `LayoutPartialTree::compute_child_layout` / `LayoutBlockContainer::compute_block_child_layout`（同文件）                                                                     |
|                        | `to_taffy` / `to_taffy_ref` / `to_engine`                                                                                                                                                                                                                           | 同文件的 7 个 trait impl                                                                                                                                                     |
| `layout/measure.rs`    | `compute_leaf`                                                                                                                                                                                                                                                      | `dispatch_child_layout` 的 `(_, false)` 分支                                                                                                                                 |
| `layout/hit_test.rs`   | `hit_test`（`pub(crate) fn hit_test(&NodeTree, Point)`）                                                                                                                                                                                                            | `NodeTree::hit_test`                                                                                                                                                         |
|                        | `ClipChain::{push, truncate}`                                                                                                                                                                                                                                       | `hit_test` 遍历                                                                                                                                                              |
| `node/tree.rs`         | `NodeSpec::new`                                                                                                                                                                                                                                                     | `NodeBuilder::build`                                                                                                                                                         |
|                        | `NodeTree::{taffy_style, taffy_children, taffy_cache, set_local_layout, stats_mut, note_invalid_measure, take_invalid_measure, measure_of, layout_override_of, hoisted_clear, hoisted_push, begin_layout, absolutize, take_report, is_out_of_flow}`（`pub(crate)`） | `layout/taffy_tree.rs` 的 7 个 trait impl 与 `run`（前 9 个 + `absolutize`/`begin_layout`/`take_report`/`take_invalid_measure`）、`NodeTree::absolutize`（`is_out_of_flow`） |
|                        | `NodeId::from_bits`                                                                                                                                                                                                                                                 | `to_engine`（taffy → 引擎）                                                                                                                                                  |
|                        | `NodeTree::invalidate_layout`（`pub(crate)`）                                                                                                                                                                                                                       | `Window::{set_size, set_scale}`、`set_style`、`invalidate_measure`、`set_layout_override`（同一个「输入变了 → 标 LAYOUT + 清缓存」的实现）                                   |
|                        | `LayoutStats::{note_cache_hit, note_cache_miss, note_laid_out, into_report}`                                                                                                                                                                                        | `impl CacheTree for NodeTree`、`dispatch_child_layout`、`run`                                                                                                                |
|                        | `NodeKind::is_leaf`（P03 已有）                                                                                                                                                                                                                                     | `dirty::depends_on_children`、`NodeBuilder::build` 的 debug 断言                                                                                                             |
|                        | `Node::generation_matches`（私有）                                                                                                                                                                                                                                  | `NodeTree::{get, slot_mut}`                                                                                                                                                  |
|                        | `NodeTree::{slot_mut, invalidate_caches_upwards, child_insert_position, subtree_len, place_subtree}`（**私有**，仅 `node/tree.rs` 内部）                                                                                                                            | 同文件的 `get`/`absolutize`/`insert_child` 等                                                                                                                                |
| `paint/style.rs`       | `Clip`                                                                                                                                                                                                                                                              | `hit_test`（读）、P06（写 clip 图元，暂由 hit test 承担唯一读者）                                                                                                            |

> `NodeTree::{set_style, invalidate_measure, set_layout_override, hit_test}` 是 `pub`（公开面，不受 `dead_code` 约束，登记见
> §4.1
> 的「方法级公开面增量」表）；`NodeTree::compute_layout` 是 `pub(crate)`，其消费者是 `Window::compute_layout`（§13）。
>
> **方法级 `pub` 预留给下一 Phase 的入口清单**（本阶段只落签名与最小语义）：
>
> | 方法                            | 预留消费者                                           | 本阶段的语义完整度      |
> |---------------------------------|------------------------------------------------------|-------------------------|
> | `NodeTree::set_style`           | P08 `base_ui` 的主题/密度切换（结构性样式）          | 完整（§8.5）            |
> | `NodeTree::invalidate_measure`  | P05 的字体栈/文本内容变化                            | 完整                    |
> | `NodeTree::set_layout_override` | P13 的分栏拖拽                                       | 完整                    |
> | `NodeTree::hit_test`            | P10 的事件分发、P13 的 Overlay、P14 的 a11y hit test | 完整（§12）             |
> | `Node::clip`                    | P06 的 `Clip` 图元                                   | 完整（读 `Style.clip`） |

**落地判据**：每条 `pub(crate)` 都能在本表找到调用点；表外的项一律删除（AGENTS §4）。

---

## 5. `layout/taffy_tree.rs`（taffy 集成，逐方法）

### 5.1 id 映射（无额外表）

```rust
use taffy::NodeId as TaffyNode;

/// 引擎 `NodeId` → taffy `NodeId`。编码复用 P03 的 `to_bits()`：`generation << 32 | index`。
fn to_taffy(node: NodeId) -> TaffyNode {
    TaffyNode::new(node.to_bits())
}

/// 迭代器元素版本（`ChildIter` 需要 `fn(&NodeId) -> TaffyNode`）。
fn to_taffy_ref(node: &NodeId) -> TaffyNode {
    TaffyNode::new(node.to_bits())
}

/// taffy → 引擎；非法编码（代际 0）回落根 + debug 断言（release 安全，不 panic）。
fn to_engine(tree: &NodeTree, node: TaffyNode) -> NodeId {
    NodeId::from_bits(u64::from(node)).unwrap_or_else(|| {
        debug_assert!(false, "taffy handed back a NodeId that the engine never issued");
        tree.root()
    })
}
```

`node/id.rs` 增加 **一个方法**（消费者 `to_engine`）：

```rust
impl NodeId {
    /// `to_bits` 的逆运算；代际为 0 的编码非法（`None`）。
    #[must_use]
    pub(crate) const fn from_bits(bits: u64) -> Option<Self> {
        let generation = (bits >> 32) as u32;
        match std::num::NonZeroU32::new(generation) {
            Some(generation) => Some(Self { index: bits as u32, generation }),
            None => None,
        }
    }
}
```

> 为什么不用 `HashMap<TaffyNode, NodeId>`：`taffy::NodeId` 是 `u64` 的 newtype，P03 的 `NodeId::to_bits()` 已经是
> `generation << 32 | index`（`node/id.rs`，带 `#[must_use]`，消费者是结构化日志）。双向映射零成本、零分配，且
> 引擎与 taffy 的 id 集合**按构造**一致（taffy 只可能拿到我们从 `child_ids` 里给出的 id）。

### 5.2 `dispatch_child_layout`（分派，逐字）

与 taffy 自带的 `TaffyView::compute_child_layout`（`tree/taffy_tree.rs:287-343`）结构逐字对应：

```rust
fn dispatch_child_layout(
    tree: &mut NodeTree,
    taffy_node: TaffyNode,
    inputs: LayoutInput,
    block_ctx: Option<&mut BlockContext<'_>>,
) -> LayoutOutput {
    // 1) 祖先 display:none ⇒ 隐藏布局，不进 cache（taffy 的固定入口顺序，TaffyView:295）
    if inputs.run_mode == taffy::RunMode::PerformHiddenLayout {
        return compute_hidden_layout(tree, taffy_node);
    }

    // 2) 通用路径：cache 命中直接返回；未命中才跑算法并写缓存
    compute_cached_layout(tree, taffy_node, inputs, |tree, taffy_node, inputs| {
        let node = to_engine(tree, taffy_node);
        if inputs.run_mode == taffy::RunMode::PerformLayout {
            tree.stats_mut().note_laid_out();
        }
        let display = tree.taffy_style(node).display;
        let has_children = !tree.taffy_children(node).is_empty();

        let mut output = if matches!(display, Display::None) {
            // N3：隐藏优先于 override —— 隐藏的覆盖节点不得有非零盒
            compute_hidden_layout(tree, taffy_node)
        } else if let Some(overridden) = tree.layout_override_of(node) {
            // root 逃生口：尺寸由应用给定，位置由 post-pass 落（`NodeTree::absolutize`）
            LayoutOutput::from_outer_size(TaffySize {
                width: overridden.rect.size.width,
                height: overridden.rect.size.height,
            })
        } else {
            match (display, has_children) {
                // N2：`FlowRoot` 不透传 `block_ctx`（与 taffy `TaffyView` 一致）
                (Display::Block, true) => {
                    compute_block_layout(tree, taffy_node, inputs, block_ctx)
                }
                (Display::FlowRoot, true) => compute_block_layout(tree, taffy_node, inputs, None),
                (Display::Flex, true) => compute_flexbox_layout(tree, taffy_node, inputs),
                (Display::Grid, true) => compute_grid_layout(tree, taffy_node, inputs),
                // 仅 `Display::None` 能走到这里（上面已提前返回），保守按隐藏处理
                (_, true) => compute_hidden_layout(tree, taffy_node),
                (_, false) => measure::compute_leaf(tree, node, inputs),
            }
        };

        // 3) 出流定位 pass 只在完整布局里跑（measure pass 写 hoisted 布局，而「最终布局 cache 命中」
        //    时不会重写它们 —— TaffyView:337 的同一理由）
        if inputs.run_mode == taffy::RunMode::PerformLayout {
            compute_oof_layout(tree, taffy_node, &mut output);
        }
        output
    })
}
```

**分派表的 4 个非显然决策**

| 决策                                               | 结论                                                    | 理由                                                                                                                                                                   |
|----------------------------------------------------|---------------------------------------------------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `LayoutOverride` 在**分派前**短路                  | 直接返回 `LayoutOutput::from_outer_size(override.size)` | 覆盖节点的尺寸由应用定；不能落进 `compute_leaf_layout`（那会用 style 的 min/max/padding 再夹一遍）。位置不进 `LayoutOutput`，由 post-pass 落（§10）                    |
| `(_, false)` 走 `measure::compute_leaf`            | 即使没有 `measure` 闭包也走                             | `compute_leaf_layout` 要处理 padding/border/aspect-ratio/min-max；空闭包返回 `Size::ZERO` 与 taffy 的 `TaffyTree::compute_layout` 一致（`tree/taffy_tree.rs:987-989`） |
| `Display::None` 有子节点 → `compute_hidden_layout` | 同 taffy                                                | 隐藏子树必须逐节点零化，否则上一帧的布局盒会残留                                                                                                                       |
| `perform_layout` 时才 `compute_oof_layout`         | 同 taffy                                                | `RunMode::ComputeSize` 的 pass 若写 hoisted 布局，后续「最终布局 cache 命中」不会重写 → 位置停留在中间态                                                               |

### 5.3 七个 impl（可直接照抄的骨架）

```rust
// layout/taffy_tree.rs 的完整 import（N11）
use smallvec::SmallVec;
use smol_str::SmolStr;
use taffy::{
    AvailableSpace, BlockContext, CacheTree, Display, Layout, LayoutContainingBlock,
    LayoutFlexboxContainer, LayoutGridContainer, LayoutBlockContainer, LayoutInput, LayoutOutput,
    LayoutPartialTree, NodeId as TaffyNode, Size as TaffySize, Style,
    TraversePartialTree, TraverseTree,
    compute_block_layout, compute_cached_layout, compute_flexbox_layout, compute_grid_layout,
    compute_hidden_layout, compute_oof_layout, compute_root_layout,
};
use crate::{
    error::EngineError,
    layout::{LayoutReport, measure},
    node::tree::{NodeId, NodeTree},
};
```

```rust
impl TraversePartialTree for NodeTree {
    type ChildIter<'a>
    = std::iter::Map<std::slice::Iter<'a, NodeId>, fn(&NodeId) -> TaffyNode>
    where
        Self: 'a;

    fn child_ids(&self, id: TaffyNode) -> Self::ChildIter<'_> {
        self.taffy_children(to_engine(self, id)).iter().map(to_taffy_ref)
    }
    fn child_count(&self, id: TaffyNode) -> usize {
        self.taffy_children(to_engine(self, id)).len()
    }
    fn get_child_id(&self, id: TaffyNode, index: usize) -> TaffyNode {
        match self.taffy_children(to_engine(self, id)).get(index) {
            Some(child) => to_taffy(*child),
            None => {
                debug_assert!(false, "taffy asked for an out-of-range child index");
                to_taffy(self.root())
            }
        }
    }
}

impl LayoutPartialTree for NodeTree {
    type CoreContainerStyle<'a> = &'a taffy::Style<SmolStr>
    where
        Self: 'a;
    type CustomIdent = SmolStr;

    fn get_core_container_style(&self, id: TaffyNode) -> Self::CoreContainerStyle<'_> {
        self.taffy_style(to_engine(self, id))
    }
    fn set_unrounded_layout(&mut self, id: TaffyNode, layout: &taffy::Layout) {
        self.set_local_layout(to_engine(self, id), layout);
    }
    fn compute_child_layout(&mut self, id: TaffyNode, inputs: LayoutInput) -> LayoutOutput {
        dispatch_child_layout(self, id, inputs, None)
    }
    // resolve_calc_value 走默认实现（`0.0`），理由见 §14.3
}

impl LayoutContainingBlock for NodeTree {
    type OofItemStyle<'a> = &'a taffy::Style<SmolStr>
    where
        Self: 'a;

    fn get_oof_item_style(&self, id: TaffyNode) -> Self::OofItemStyle<'_> {
        self.taffy_style(to_engine(self, id))
    }
    fn clear_hoisted_children(&mut self, id: TaffyNode) {
        self.hoisted_clear(to_engine(self, id));
    }
    fn add_hoisted_children(&mut self, id: TaffyNode, hoisted: &[TaffyNode]) {
        let node = to_engine(self, id);
        let mapped: SmallVec<[NodeId; 2]> = hoisted.iter().map(|h| to_engine(self, *h)).collect();
        self.hoisted_push(node, &mapped);
    }
    // get_detailed_layout_info 走默认实现（`DetailedLayoutInfo::None`）：v1 不记录 grid 详细轨道信息，
    // 代价是 grid 容器内的 absolute 子盒按 padding box 定位而不是按 grid area（§19 V-35）
}

impl CacheTree for NodeTree {
    fn cache_get(&mut self, id: TaffyNode, input: &LayoutInput) -> Option<LayoutOutput> {
        let hit = self.taffy_cache(to_engine(self, id)).get(input);
        if hit.is_some() { self.stats_mut().note_cache_hit(); } else { self.stats_mut().note_cache_miss(); }
        hit
    }
    fn cache_store(&mut self, id: TaffyNode, input: &LayoutInput, output: LayoutOutput) {
        self.taffy_cache(to_engine(self, id)).store(input, output);
    }
    fn cache_clear(&mut self, id: TaffyNode) {
        self.taffy_cache(to_engine(self, id)).clear();
    }
}

impl LayoutFlexboxContainer for NodeTree {
    type FlexboxContainerStyle<'a> = &'a taffy::Style<SmolStr>
    where
        Self: 'a;
    type FlexboxItemStyle<'a> = &'a taffy::Style<SmolStr>
    where
        Self: 'a;
    fn get_flexbox_container_style(&self, id: TaffyNode) -> Self::FlexboxContainerStyle<'_> {
        self.taffy_style(to_engine(self, id))
    }
    fn get_flexbox_child_style(&self, id: TaffyNode) -> Self::FlexboxItemStyle<'_> {
        self.taffy_style(to_engine(self, id))
    }
}

impl LayoutGridContainer for NodeTree {
    type GridContainerStyle<'a> = &'a taffy::Style<SmolStr>
    where
        Self: 'a;
    type GridItemStyle<'a> = &'a taffy::Style<SmolStr>
    where
        Self: 'a;
    fn get_grid_container_style(&self, id: TaffyNode) -> Self::GridContainerStyle<'_> {
        self.taffy_style(to_engine(self, id))
    }
    fn get_grid_child_style(&self, id: TaffyNode) -> Self::GridItemStyle<'_> {
        self.taffy_style(to_engine(self, id))
    }
    // set_detailed_grid_info 走默认空实现（不存 `DetailedGridInfo<SmolStr>`：没有读者）
}

impl LayoutBlockContainer for NodeTree {
    type BlockContainerStyle<'a> = &'a taffy::Style<SmolStr>
    where
        Self: 'a;
    type BlockItemStyle<'a> = &'a taffy::Style<SmolStr>
    where
        Self: 'a;
    fn get_block_container_style(&self, id: TaffyNode) -> Self::BlockContainerStyle<'_> {
        self.taffy_style(to_engine(self, id))
    }
    fn get_block_child_style(&self, id: TaffyNode) -> Self::BlockItemStyle<'_> {
        self.taffy_style(to_engine(self, id))
    }
    fn compute_block_child_layout(&mut self, id: TaffyNode, inputs: LayoutInput,
                                  block_ctx: Option<&mut BlockContext<'_>>) -> LayoutOutput {
        dispatch_child_layout(self, id, inputs, block_ctx)   // 必须转发，否则 block 的 margin collapse 失效
    }
}
```

**`RoundTree` / `TraverseTree` / `PrintTree`：不实现。** 依据：

| 证据                                                                                                                                                   | 说明                                                                                                        |
|--------------------------------------------------------------------------------------------------------------------------------------------------------|-------------------------------------------------------------------------------------------------------------|
| `round_layout` 的唯一调用点是 `TaffyTree::compute_layout_with_measure`（`tree/taffy_tree.rs:976-981`，`config.use_rounding`）                          | 我们走低级 `compute_root_layout`，永不调用                                                                  |
| `RoundTree` 的 5 个方法里 `set_final_layout` / `hoisted_child_count` / `get_hoisted_child_id` 只被 `round_layout_inner` 读（`compute/mod.rs:379-435`） | v1 无消费者                                                                                                 |
| 落地代价                                                                                                                                               | 每节点 `final_layout: Layout` = **92 B**（probe 实测），`Node` 从 ≈796 B 涨到 888 B，逼近 1024 预算（V-30） |
| probe P2                                                                                                                                               | 能实现（已编译通过）→ 这是**决策**，不是能力限制；将来若引入「按物理网格圆整」需要它，additive 补即可       |

> 若后续要 `print_tree` 调试（P18）：`PrintTree` 需要 `get_final_layout`，届时同样 additive 加 `final_layout` 字段；
> 本阶段不为了调试能力预留字段（AGENTS §1.2）。

### 5.4 `run`（布局入口，逐字）

```rust
pub(crate) fn run(
    tree: &mut NodeTree,
    available: taffy::Size<Option<f32>>,
) -> Result<LayoutReport, EngineError> {
    tree.begin_layout();                       // epoch += 1、stats 清零
    let available_space = TaffySize {
        width: available.width.map_or(AvailableSpace::MaxContent, AvailableSpace::Definite),
        height: available.height.map_or(AvailableSpace::MaxContent, AvailableSpace::Definite),
    };

    compute_root_layout(tree, to_taffy(tree.root()), available_space);   // 不圆整
    tree.absolutize();                                                   // 父相对 → 窗口绝对

    match tree.take_invalid_measure() {
        Some(error) => Err(error),
        None => Ok(tree.take_report()),
    }
}
```

调用顺序的**为什么**：

| 步骤                                          | 必须在此时的理由                                                                                        |
|-----------------------------------------------|---------------------------------------------------------------------------------------------------------|
| `begin_layout()` 在 taffy 之前                | `set_unrounded_layout` 用 epoch 判「本轮是否布局过」；epoch 必须在第一次写之前就位                      |
| `absolutize()` 在 taffy 之后                  | taffy 只写父相对 `location`；且父的绝对原点可能比子晚知道（taffy 先算子后写父）                         |
| `take_invalid_measure()` 在 `absolutize` 之后 | 违规时仍要落一个**安全**的（可能空的）布局盒；`Rect::is_empty` 对 NaN 返回 `true` ⇒ hit/damage 自然跳过 |
| 返回 `Err` 前不撤销任何状态                   | `LAYOUT` 已消费、缓存已写；调用方（P06/P09）**丢弃本帧**并在下一帧重试（不该 commit 半成品）            |

---

## 6. 缓存旁表与增量

### 6.1 `Vec<Option<Cache>>` vs `HashMap<NodeId, Cache>`

| 维度                  | **`Vec<Option<Cache>>`（选定）**                                                                          | `ahash::HashMap<NodeId, Cache>`                                             |
|-----------------------|-----------------------------------------------------------------------------------------------------------|-----------------------------------------------------------------------------|
| 访问                  | `caches[node.index()]` O(1)，无哈希                                                                       | 每次 cache_get/store 一次 ahash（~10 ns）                                   |
| 内存                  | 槽位数 × (408 + 8) B（`Option<Cache>` 有判空位）；10k 节点 ≈ 4.2 MB                                       | 只给「有缓存条目」的节点，但每个条目多 24–32 B 控制字；全树都布局时反而更大 |
| 槽位复用              | arena 复用槽位时**必须**清（`insert_child` 里 `caches[index] = None`），漏清 = 新节点继承上一代节点的布局 | 键带代际 ⇒ 陈旧条目不命中，但会**泄漏**到 `remove` 显式删除为止             |
| 迭代/调试             | 顺序与 arena 一致，可 `caches.iter()` 做调试快照                                                          | 无序                                                                        |
| 生命周期与 arena 对齐 | 完全对齐（`slots.len() == caches.len()` 不变式，`debug_assert!`）                                         | 两套生命周期，需要两处清理点                                                |

**结论**：选 `Vec<Option<Cache>>`。理由：①节点树是**稠密**的（taffy 会对每个参与布局的节点建缓存，几乎没有稀疏性）；
②槽位复用是我们自己的不变量（arena 已经保证），一个 `= None` 就够；③尺寸可算（10k 节点 4.2 MB，与 P03 的
`Node` 预算同量级，P18 基准再评估）；④无哈希、无随机性（布局顺序确定 ⇒ 帧记录可复现）。

```rust
// node/tree.rs（字段）
pub struct NodeTree {
    slots: Vec<Option<Node>>,
    generations: Vec<u32>,
    free: Vec<u32>,
    root: NodeId,
    order: Vec<NodeId>,
    /// taffy 布局缓存旁表（按 arena 槽位下标；`Node` 不内联 `Cache`，见 P03 N6）
    layout_cache: Vec<Option<taffy::Cache>>,
    /// 出流盒 → 其包含块（taffy `add_hoisted_children` 的落点；唯一读者是 `absolutize`）
    hoisted: ahash::HashMap<NodeId, SmallVec<[NodeId; 2]>>,
    /// 布局运行代次（0 保留，首轮为 1；`u32` 回绕在 40 亿轮内不可达）
    layout_epoch: u32,
    layout_stats: LayoutStats,
    /// 首条 measure 违规（`InvalidMeasure` 的唯一来源）
    invalid_measure: Option<EngineError>,
}
```

**三处清理点（实现时逐条对齐）**

| 时机                      | 动作                                                               | 不做的后果                                                          |
|---------------------------|--------------------------------------------------------------------|---------------------------------------------------------------------|
| `insert_child` 复用槽位   | `layout_cache[index] = None`（+ `hoisted.remove(&id)`）            | 新节点继承上一代节点的缓存 → 布局直接错                             |
| `NodeTree::remove` 逐节点 | 对子树每个 id：`layout_cache[index] = None`、`hoisted.remove(&id)` | 槽位泄漏；后续复用同一槽位时命中残留                                |
| 槽位新增                  | `layout_cache.push(None)`（保持 `len == slots.len()`）             | 索引越界（`taffy_cache` 里用 `resize_with` 兜底 + `debug_assert!`） |

### 6.2 失效协议：为什么必须清到根，且不能早停

taffy 的 cache 命中会让算法**整段跳过**：`compute_root_layout` 先 `cache_get(root)`，命中则
`compute_child_layout(root)` 也命中，直接返回 —— **一个子节点都不会被访问**。因此「要让某个子树的脏标记生效」，
必须保证从根到那个子树的**每一跳都 cache miss**。

```text
失效协议（写路径，node/tree.rs）：
  mark_layout(node) / mark_structure(node) / set_style(node, style) / invalidate_measure(node)
      → touch(node, LAYOUT | PAINT)                     # P03 的位与 damage 语义不变
      → bubble_layout(node)                              # P03 的冒泡（可早停：那是 damage 语义）
      → invalidate_caches_upwards(node)                  # 无条件 node..root 的 cache.clear()
```

```rust
fn invalidate_caches_upwards(&mut self, node: NodeId) {
    let mut cursor = Some(node);
    while let Some(current) = cursor {
        if let Some(cache) = self.layout_cache.get_mut(current.index() as usize).and_then(Option::as_mut) {
            cache.clear();
        }
        cursor = self.get(current).and_then(|slot| slot.parent);
    }
}
```

**两条走路线的分工（关键）**

| 走线                                    | 终点                                    | 能否早停          | 作用                                                              |
|-----------------------------------------|-----------------------------------------|-------------------|-------------------------------------------------------------------|
| `bubble_layout`（P03 已有）             | 冒泡直到「父不依赖子内容」/ 出流 / 隐藏 | ✅ 能（P03 §9.3） | **damage** 与 `LAYOUT` 位（祖先盒已覆盖子树的像素变化，不必继续） |
| `invalidate_caches_upwards`（P04 新增） | **恒定到根**                            | ❌ 不能           | **缓存**：任一祖先命中都会截断 taffy 的递归                       |

> 反例（说明为什么不能复用 `bubble_layout`）：`root → A(固定 200×200) → B`，改 B。`bubble_layout` 在 A 处早停
> （A 尺寸不依赖子），于是 `root` 没有 `LAYOUT`。若此时不清 `root` 的 cache，下一轮 `compute_root_layout` 的
> `cache_get(root)` 命中 ⇒ **B 的改动完全丢失**（不是「多算」，是「少算」）。
> 这也是 taffy 自己 `TaffyTree::mark_dirty` 的行为（`tree/taffy_tree.rs:935-958`）。

### 6.3 `LAYOUT` 的消费点与 `LayoutReport` 的计数语义

| 事件                                           | 动作                                                                                            | 位置                                       |
|------------------------------------------------|-------------------------------------------------------------------------------------------------|--------------------------------------------|
| taffy 写某节点的布局（`set_unrounded_layout`） | `slot.local = …; slot.local_border = …; slot.laid_out_epoch = epoch; slot.dirty.remove(LAYOUT)` | `NodeTree::set_local_layout`               |
| 完整布局且 cache 未命中                        | `stats.laid_out += 1`                                                                           | `dispatch_child_layout` 的 cache-miss 闭包 |
| `cache_get` 命中/未命中                        | `stats.cache_hits += 1` / `cache_misses += 1`                                                   | `impl CacheTree for NodeTree`              |
| 绝对盒变化                                     | `stats.moved += 1`                                                                              | `absolutize`                               |

`LayoutReport` 的四个字段因此有**互不重叠**的语义：

```rust
pub struct LayoutReport {
    /// 被 taffy 重算布局的节点数（PerformLayout 且 cache 未命中）。
    pub laid_out: usize,
    /// 绝对布局盒发生变化的节点数。
    pub moved: usize,
    /// taffy cache 命中次数（含 `compute_root_layout` 的那一次预探）。
    pub cache_hits: usize,
    /// taffy cache 未命中次数。
    pub cache_misses: usize,
}
```

**实测基线（probe，用于测试期望值）**

| 场景                         | report                                                                                   |
|------------------------------|------------------------------------------------------------------------------------------|
| 首轮：根 + 3 个 fixed 子节点 | `laid_out = 4`（block/flex 的 measure pass 不计）                                        |
| 改中间子节点宽度后重排       | `{ laid_out: 2, moved: 2, cache_hits: 4, cache_misses: 4 }`（= 根 + 该子节点；兄弟命中） |
| 无任何变化再跑一轮           | `{ laid_out: 0, moved: 0, cache_hits: 2, cache_misses: 0 }`                              |

> 断言时必须用 `>=` / `<=` 的**方向性**断言而不是精确数字（`cache_hits` 会随 taffy 的 measure pass 次数变化）；
> 只有「第二轮 `laid_out == 0`」是稳定契约。

---

## 7. 绝对坐标 post-pass（`NodeTree::absolutize`）

### 7.1 数据流

```text
taffy 算法（flex/grid/block/oof）
   ├─ 父算法对每个（流内 / 认领的）子节点调用 `set_unrounded_layout(child, Layout { location, size, border, … })`
   │     → Node.local = Rect { origin: location, size }      # **父相对**（含父的 border+padding 偏移）
   │     → Node.local_border = Edges::new(border.top, right, bottom, left)
   │     → Node.laid_out_epoch = epoch（本轮布局过）
   └─ 出流盒：其**包含块**的 `compute_oof_layout` 认领并写 local（相对包含块 border box）

引擎 post-pass（一次 DFS）
   ├─ 流内子：abs = 父 abs.origin + child.local.origin
   ├─ 出流子：不在父的 children 循环里处理（跳过），由包含块的 hoisted 列表驱动，参照系仍是包含块 border box
   └─ 剪枝：`!fresh && !changed` ⇒ 整棵子树跳过
```

### 7.2 逐字实现

```rust
pub(crate) fn absolutize(&mut self) {
    let epoch = self.layout_epoch;
    self.place_subtree(self.root, Point::ZERO, epoch, /* force */ true);
}

fn place_subtree(&mut self, node: NodeId, reference: Point, epoch: u32, force: bool) {
    let (abs, old, changed, fresh, children) = {
        let Some(slot) = self.get(node) else { return };
        // LayoutOverride 直接覆盖 taffy 给的父相对盒（位置与尺寸都取应用给的值）
        let local = slot.layout_override.map_or(slot.local, |over| over.rect);
        let abs = Rect::from_origin_size(reference + local.origin, local.size);
        (abs, slot.layout, abs != slot.layout.bounds, slot.laid_out_epoch == epoch, slot.children.clone())
    };
    if !force && !fresh && !changed {
        return;                                  // 整棵子树跳过
    }

    if changed {
        self.layout_stats.moved += 1;
        if let Some(slot) = self.slot_mut(node) {
            slot.damage = slot.damage.union(old.bounds).union(abs);   // 旧位置 ∪ 新位置
            slot.prev_layout = old;
            slot.layout = LayoutBox { bounds: abs, border: slot.local_border };
        }
    } else if fresh {
        if let Some(slot) = self.slot_mut(node) {
            slot.layout.border = slot.local_border;   // 尺寸没变但 border 宽可能变了
        }
    }

    let hoisted: SmallVec<[NodeId; 2]> = self.hoisted.get(&node).cloned().unwrap_or_default();
    for child in children {
        if self.is_out_of_flow(child) {
            continue;                             // 由包含块的 hoisted 列表驱动（避免算两次）
        }
        self.place_subtree(child, abs.origin, epoch, false);
    }
    for child in hoisted {
        self.place_subtree(child, abs.origin, epoch, false);
    }
}
```

### 7.3 三个必须固化的语义

| #  | 语义                                                                             | 证据 / 后果                                                                                                                                                                                                                                                                                                                                                                                                           |
|----|----------------------------------------------------------------------------------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| A1 | **出流盒的 `local` 相对包含块 border box**（不是 padding box、也不是树父）       | `compute/flexbox.rs:511-516`：`absolute_position_offset = border`（**不含 padding**）；`compute/oof.rs:672-701`：`location = position() + area_offset`。probe：容器 padding 10、inset 5 ⇒ `local = (5, 7)`，与容器绝对原点相加即正确                                                                                                                                                                                  |
| A4 | **`LayoutOverride` 节点按树父 border box 放置**（不走包含块）                    | taffy 会把 `position: absolute` 的 override 节点挂到最近的**定位**祖先（无则 root/ICB），它的 `local` 因此相对包含块；但引擎的 override 契约是「相对父 border box」。实现：`absolutize` 的 children 循环**不跳过** override 子节点（用父的 `abs.origin + override.rect`），hoisted 循环**跳过**它们（避免用包含块 local 再算一次）。probe/用例：父被 spacer 推到 y=40/100 时 override 的绝对坐标分别 (10,60)/(10,120) |
| A5 | 隐藏子树（自身或祖先 `display: none`）的所有后代（含 override 与出流盒）**零化** | taffy 的 hidden pass 已把 `local` 写成零；`absolutize` 对 `hidden` 分支强制 `Rect::default()`，避免 P06 读到上一帧的幽灵盒（N1/N3）                                                                                                                                                                                                                                                                                   |
| A2 | `hoisted` 列表**必须存**（这是本阶段唯一需要它的地方）                           | taffy 的文档说它「consumed by round_layout」（`tree/traits.rs:236-239`），但**引擎还需要它做绝对坐标**：出流盒的树父可能不是它的包含块，DFS 不能从树父推出参照系。probe：`static_mid` 下挂的出流孙节点被 container 认领，最终 (5,7) 而非 (0+5,0+7) 之外的错误值                                                                                                                                                       |
| A3 | 跳过规则 `!fresh && !changed` 是**正确性**要求，不只是优化                       | `fresh`（本轮布局过）必须在「盒没变但内部排列变了」时继续下钻（例：固定尺寸容器的子节点位置变）；`changed`（父移动）必须在「本节点没重算」时继续下钻修正后代                                                                                                                                                                                                                                                          |

### 7.4 复杂度

| 场景                     | 访问节点数                                           |
|--------------------------|------------------------------------------------------|
| 无变化                   | 1（根）+ 根的直接子节点各 1 次比较 = O(1 + 根的子数) |
| 一个叶子变化（尺寸不变） | O(深 × 兄弟数)（沿路径下钻）                         |
| 一个容器移动/改尺寸      | O(受影响的子树)（必须修正后代的绝对坐标）            |
| 全量首帧                 | O(n)                                                 |

---

## 8. `LayoutBox` / `Node` / `NodeTree` 的字段增量

### 8.1 `LayoutBox`

```rust
/// 布局结果（逻辑像素，I11：**只有逻辑像素**，物理化在 paint/present 边界）。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LayoutBox {
    /// 窗口绝对 border box AABB（半开区间）。
    pub bounds: Rect,
    /// 四边 border 宽度（逻辑像素；来源 taffy `Layout.border`）。
    ///
    /// 消费者：`hit_test` 的 clip 链要 padding box（= `bounds.inset(border)`）；P06 的背景/边框绘制与
    /// 圆角内缩同样要它。
    pub border: Edges,
}
```

| 决策                                                   | 结论                                                                                                | 理由                                                   |
|--------------------------------------------------------|-----------------------------------------------------------------------------------------------------|--------------------------------------------------------|
| 为什么不存 `content_size` / `scrollable_overflow_rect` | **不加**                                                                                            | P04 无读者（P06 的 damage 需要时 additive 加）。→ V-33 |
| 为什么不存 `baseline`                                  | 不加                                                                                                | P05 的文本行内流（P03 §11.3 已裁决）                   |
| 为什么 `border` 现在就加                               | **有 P04 读者**：`hit_test` 的 clip 链必须是 CSS 的 padding box；`bounds`（border box）不足以推出它 | 一个 4 字节级的字段换掉「clip 差一个边框」的系统性错误 |
| 为什么 `bounds` 是绝对坐标                             | P03 §11.3 冻结、P08 `Probe::bounds` 直接返回它、P06 的 damage 用它                                  | 相对坐标的累加放在 post-pass（§7），不在每次查询时做   |

### 8.2 `Node` 字段增量（在 P03 §11.1 的表上追加）

| 字段                     | 类型                                 | 大小    | 写者                         | 非 test 读者                          |
|--------------------------|--------------------------------------|---------|------------------------------|---------------------------------------|
| `measure`                | `Option<MeasureFn>`                  | 8       | `insert_child`（builder）    | `measure::compute_leaf`               |
| `hit_shape`              | `Option<Box<dyn Fn(Point) -> bool>>` | 16      | `insert_child`               | `layout::hit_test`                    |
| `layout_override`        | `Option<LayoutOverride>`             | 20      | `insert_child`               | `dispatch_child_layout`、`absolutize` |
| `local`                  | `Rect`                               | 16      | `set_local_layout`           | `absolutize`                          |
| `local_border`           | `Edges`                              | 16      | `set_local_layout`           | `absolutize`                          |
| `laid_out_epoch`         | `u32`                                | 4       | `set_local_layout`           | `absolutize`                          |
| `layout` / `prev_layout` | `LayoutBox`                          | 32 + 32 | `absolutize`                 | `Probe`（P08）、`hit_test`、P06       |
| `damage`                 | `Rect`                               | 16      | `touch`（P03）+ `absolutize` | P06                                   |

合计新增 **+40 B**（`local` 16 + `local_border` 16 + `epoch` 4 + 对齐）。`Node::layout()` / `prev_layout()` / `damage()` 的
公开面**不变**（P03 已加）。

> **`Node` 的手写 `Debug` 必须同步（N4）**：P03 的 `impl fmt::Debug for Node` 逐字段打印（因为 `bind_style` 是
> `Box<dyn Fn>`）。P04 新增的 6 个字段必须加进去：`measure` / `hit_shape` 打成 `Some("<closure>")` / `None`
> （与 `bind_style` 同形），`layout_override` / `local` / `local_border` / `laid_out_epoch` 正常打印。
> **漏加会让 debug 输出缺失布局信息，测试失败信息与 P08 的 `Probe` 诊断都会变差。**

### 8.3 尺寸预算（probe 实测 + 推算）

| 类型                    | 实测/推算                                              | 说明                                                                           |
|-------------------------|--------------------------------------------------------|--------------------------------------------------------------------------------|
| `taffy::Style<SmolStr>` | **552 B**（实测）                                      | `Node.style` 内联（P03）                                                       |
| `taffy::Layout`         | **92 B**（实测）                                       | 只在 probe 里作为 `final_layout` 出现；生产**不存**                            |
| `taffy::Cache`          | **408 B**（实测）                                      | 不进 `Node`                                                                    |
| `LayoutBox`             | **32 B**（实测）                                       | `Rect` 16 + `Edges` 16                                                         |
| **P03 的 `Node`**       | **888 B**（P03 实测）                                  | P03 的 `node_and_handle_sizes` 断言 `<= 1024`                                  |
| **P04 的 `Node`**       | **1016 B**（= 888 + 40 字段增量 + 88 对齐/填充，实测） | 仍 `<= 1024`，但只剩 8 B 余量                                                  |
| 断言上界                | **1152 B**（`node_size_within_budget`）                | 给 P05 的 `Text`/`canvas` 载荷留余量；超了走 `style: Box<Style>`（公开面不变） |

### 8.4 根节点默认撑满（`NodeTree::new`）

```rust
// NodeTree::new()
let mut root_style = Style::default ();
root_style.inner.size = taffy::geometry::Size {
width: taffy::style::Dimension::percent(1.0),
height: taffy::style::Dimension::percent(1.0),
};
```

| 决策                       | 结论                                 | 理由                                                                                                                                                                                   |
|----------------------------|--------------------------------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 根为什么默认 `100% × 100%` | 必须                                 | `compute_root_layout` 只接受 `available_space`，不会把它强加给根；flex/grid 根的 `size: auto` 是 shrink-to-fit。probe 实测：窗口 200×200、根 auto ⇒ 根 0×0、`width: 100%` 的子节点全 0 |
| 为什么不改成 block 根      | 不选                                 | 会改变默认 `display`（P03 已冻结 `Display::DEFAULT = Flex`，且 `Style::default()` 的语义被 `style_default_is_taffy_default` 用例固化）                                                 |
| 为什么不每帧写根的 `size`  | 不选                                 | 根是应用可见节点，每帧改写它的 style 会让「应用改了根样式」这件事无声失效；默认值一次写入更可预测                                                                                      |
| 应用能改吗                 | 能（`Window::tree_mut()`），但不该改 | rustdoc 写明「根默认撑满窗口；改成固定尺寸只影响根自己的盒，不会改变窗口」                                                                                                             |

### 8.5 `NodeTree` 的其余增量

```rust
// 新增（P04）
pub(crate) struct LayoutStats {
    cache_hits: usize,
    cache_misses: usize,
    laid_out: usize,
    moved: usize
}
// 字段私有 + note_* / into_report 方法（`field_scoped_visibility_modifiers` 禁止字段的 scoped 修饰，见 probe P9）

impl NodeTree {
    /// 布局入口（消费者：`Window::compute_layout`）。
    pub(crate) fn compute_layout(&mut self, available: taffy::Size<Option<f32>>)
                                 -> Result<LayoutReport, EngineError> { crate::layout::taffy_tree::run(self, available) }

    /// 命中测试（P10 的消费面；P04 的用例也用它）。
    #[must_use]
    pub fn hit_test(&self, point: Point) -> Option<HitResult>;

    /// 换静态样式：写 style + 标 `LAYOUT|PAINT` + 气泡 + 清 node..root 的缓存。
    /// 结构性样式变更的唯一公开入口（写值 + 标 `LAYOUT|PAINT` + 清 `node..root` 的缓存）。
    ///
    /// **动态属性请用 `bind_style`**（`StylePatch` 通道）：每帧调本方法会清掉整条祖先链的 taffy 缓存，
    /// 让增量布局退化为全量。
    ///
    /// # Errors
    /// `EngineError::StaleNode { node }`：句柄陈旧。
    pub fn set_style(&mut self, node: NodeId, style: Style) -> Result<(), EngineError>;

    /// 「measure 的输入变了」：标 `LAYOUT` + 清 node..root 的缓存（P05 的字体栈/文本变更入口）。
    ///
    /// # Errors
    /// `EngineError::StaleNode { node }`：句柄陈旧。
    pub fn invalidate_measure(&mut self, node: NodeId) -> Result<(), EngineError>;

    /// 更新覆盖矩形（P13 的分栏拖拽）：写值 + 标 `LAYOUT|PAINT` + 清 `node..root` 缓存。
    ///
    /// # Errors
    /// `EngineError::StaleNode { node }`：句柄陈旧。
    pub fn set_layout_override(
        &mut self,
        node: NodeId,
        over: Option<LayoutOverride>,
    ) -> Result<(), EngineError>;

    /// taffy 访问器（逐条见 §4.3 的审计表）。
    pub(crate) fn taffy_style(&self, node: NodeId) -> &taffy::Style<SmolStr>;
    pub(crate) fn taffy_children(&self, node: NodeId) -> &[NodeId];
    pub(crate) fn taffy_cache(&mut self, node: NodeId) -> &mut taffy::Cache;
    pub(crate) fn set_local_layout(&mut self, node: NodeId, layout: &taffy::Layout);
    pub(crate) fn measure_of(&self, node: NodeId) -> Option<&MeasureFn>;
    pub(crate) fn layout_override_of(&self, node: NodeId) -> Option<LayoutOverride>;
    pub(crate) fn hoisted_clear(&mut self, node: NodeId);
    pub(crate) fn hoisted_push(&mut self, node: NodeId, hoisted: &[NodeId]);
    pub(crate) fn stats_mut(&mut self) -> &mut LayoutStats;
    pub(crate) fn note_invalid_measure(&mut self, node: NodeId, axis: Axis, value: f32);
    pub(crate) fn take_invalid_measure(&mut self) -> Option<EngineError>;
    pub(crate) fn begin_layout(&mut self);
    pub(crate) fn absolutize(&mut self);
    pub(crate) fn take_report(&mut self) -> LayoutReport;
    pub(crate) fn is_out_of_flow(&self, node: NodeId) -> bool;
}
```

> **`taffy_style` 的陈旧 id 兜底**：taffy 只可能拿到我们给它的 id，所以陈旧是「不可能」；但 trait 方法不能返回
> `Result`。实现为 `debug_assert!(false, "…") + 回落根的样式`（release 下安全、debug 下立刻暴露），不用 `expect`/`unwrap`
> （AGENTS 偏好「debug 断言 + 安全默认」）。
>
> **一处口径差异（实现时注意）**：P03 的生产 `NodeTree::get` 返回 `Result<&Node, EngineError>`（不是 `Option`）。
> 本文 §7.2 / §8.5 的 snippet 为可读性写成 `Option` 形态（与 probe 一致）；实现时在 `NodeTree` 内部统一用
> `self.get(node).ok()`（或 `match`）适配，**不要**为了 snippet 而改 `get` 的签名（P03 的公开面已冻结）。

---

## 9. measure

### 9.1 类型（`src/layout.rs`）

```rust
pub type MeasureFn =
Box<dyn Fn(taffy::Size<Option<f32>>, taffy::Size<taffy::AvailableSpace>) -> Size<f32>>;
```

| 决策                                               | 结论            | 理由                                                                                                                                                                                  |
|----------------------------------------------------|-----------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 为什么带 `Size<AvailableSpace>`                    | 必须            | 文本换行必须区分 `Definite(w)`（折行宽度）/ `MinContent`（最长词）/ `MaxContent`（单行固有宽度）。只给 `known_dimensions` 无法表达固有尺寸测量                                        |
| 为什么输入是 taffy 的 `Size<T>`、输出是引擎 `Size` | 分工明确        | 引擎 `Size` 按 P01 §6.3 冻结为纯 `f32`，无法表达 `Option`/三态；taffy 已在公开面（F-35）                                                                                              |
| 为什么 `Box<dyn Fn>` 而不是 `FnMut`                | 纯函数契约      | 需要 `&mut` 状态的闭包在**注册处就编译失败**；这也是设计 §4.3「measure 必须是纯函数」唯一的**类型级**保障（`Cell`/`RefCell` 仍可绕过，靠 code review + P05 的 `ShapeCache` 只读设计） |
| 为什么没有 `scale` 参数                            | 不让 DPI 进布局 | I11：布局全用逻辑像素。文本塑形若需要「按物理像素 hinting」，由 P05 的 `ShapeCache` 内部按 `scale` 分桶，**不回流到布局输入**                                                         |
| 为什么不是 `Box<dyn Fn(MeasureInput) -> Size>`     | 少一个公开类型  | 直接对齐 taffy 的 `MeasureFunction`，适配层是零成本转发（§9.2）                                                                                                                       |

### 9.2 适配（`layout/measure.rs`，逐字）

```rust
pub(crate) fn compute_leaf(tree: &mut NodeTree, node: NodeId, inputs: LayoutInput) -> LayoutOutput {
    // 首条违规，按 taffy 的求值顺序；`Cell` 让闭包在共享借用下记录
    let violation: Cell<Option<(Axis, f32)>> = Cell::new(None);

    let output = {
        let style = tree.taffy_style(node);          // &taffy::Style<SmolStr>
        let measure = tree.measure_of(node);         // Option<&MeasureFn>
        taffy::compute_leaf_layout(
            inputs,
            style,
            |_ptr, _basis| 0.0,                       // v1 不解析 calc（§14.3）
            |known, available| {
                let size = match measure {
                    Some(measure) => measure(known, available),
                    None => Size::ZERO,               // 空叶子（与 TaffyTree 的默认闭包一致）
                };
                if !size.width.is_finite() || size.width < 0.0 {
                    violation.set(Some((Axis::X, size.width)));
                } else if !size.height.is_finite() || size.height < 0.0 {
                    violation.set(Some((Axis::Y, size.height)));
                }
                taffy::Size { width: size.width, height: size.height }
            },
        )
    };

    if let Some((axis, value)) = violation.get() {
        tree.note_invalid_measure(node, axis, value); // 首条优先
    }
    output
}
```

**为什么在闭包里检测（而不是在 `output.size` 上）**：`compute_leaf_layout` 会对 measure 结果做
`maybe_clamp(min_size, max_size)` 与 `padding/border` 加法（`compute/leaf.rs:148-156`），NaN 可能被 clamp 吞掉或被
padding 放大。**检测原始的闭包返回值**才能定位到应用代码的错误。

### 9.3 为什么**没有** `(measure_version, available_space)` 缓存

| 证据                                                                                                                   | 说明                                                                                    |
|------------------------------------------------------------------------------------------------------------------------|-----------------------------------------------------------------------------------------|
| taffy `Cache` 的 key 已含 `known_dimensions` / `available_space` / `sizing_mode` / `run_mode` / `parent_size` / `mode` | `tree/cache.rs:254-280`；probe 实测：**第二轮 measure 零调用**（`calls 2 → 2`）         |
| 唯一缺的信号是「闭包捕获的状态变了」                                                                                   | 这正是 `NodeTree::invalidate_measure(node)`：标 `LAYOUT` + 清 `node..root` 缓存         |
| 独立缓存 = 双份真相                                                                                                    | AGENTS §1.2 / §1.4；而且 `measure_version` 会成为**没有读者的字段**（AGENTS §2.1 禁止） |

**`measure` 纯函数契约的落地方式**

| 手段         | 内容                                                                                                                                          |
|--------------|-----------------------------------------------------------------------------------------------------------------------------------------------|
| 类型         | `Box<dyn Fn>`（不是 `FnMut`）                                                                                                                 |
| 文档         | `MeasureFn` 的 rustdoc 写明「不得读 `Signal`、不得写状态、不得依赖帧序号」                                                                    |
| 无 `ReadCtx` | 闭包拿不到 `ReadCtx`/`Signal`/`StateStore`，**物理上读不到 Signal**（这是比 runtime 断言强的保障）                                            |
| P05 的交接   | 文本 measure 只读 `ShapeCache` + 捕获的字号/字体栈版本；`font_stack_version` 变化时对受影响的文本节点调 `invalidate_measure`                  |
| 不做         | 不做「debug 下检测节点 deps 变化」的运行时断言：布局期根本不登记依赖（`deps` 只在 `refresh_styles` 里写），没有可比对的基线（只会产生假阳性） |

### 9.4 `InvalidMeasure` 的产生与消费

| 项                     | 内容                                                                                                                    |
|------------------------|-------------------------------------------------------------------------------------------------------------------------|
| 产生点                 | `compute_leaf` 的 measure 闭包（原始返回值非有限或负）                                                                  |
| 顺序                   | **首条优先**（`note_invalid_measure` 只在 `invalid_measure.is_none()` 时写），保证诊断确定、可复现                      |
| 消费                   | `run()` 在 `absolutize` 之后 `take` 并返回 `Err`；`EngineError` 不落 `Node` 状态                                        |
| 违规后布局的可观测状态 | 该节点的 `bounds` 很可能为空（`Rect::is_empty` 对 NaN/负尺寸返回 `true`）⇒ hit test/damage 自然跳过；调用方**丢弃本帧** |
| 是否 panic             | **不**（I10 的「不静默降级」由强类型错误 + `Display` 承担，不是 panic）                                                 |

---

## 10. `LayoutOverride`（root 逃生口）

### 10.1 语义（一句话）

**「这个节点的布局盒由应用给，引擎不为它跑 taffy 的子布局与叶子测量；它仍是父级命中测试与 damage 的一部分。」**

### 10.2 类型与 API

```rust
// src/layout.rs
/// 布局覆盖（设计 §4.3 的 root 逃生口）。
///
/// 坐标空间：`rect` 相对**父节点 border box 左上角**（= taffy `Layout.location` 的参照系），因此父节点移动时
/// 覆盖盒自动跟随，应用不需要重算绝对坐标。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LayoutOverride {
    pub rect: Rect
}

impl LayoutOverride {
    #[must_use]
    pub const fn new(rect: Rect) -> Self { Self { rect } }
}

// src/node/builder.rs
impl NodeBuilder {
    /// 布局覆盖：**同时**把 `style.inner.position` 置为 `Position::Absolute` 并写入覆盖矩形。
    ///
    /// 出流是该语义的必要条件（父不该为「自己定位置」的子节点留流内空间），因此由 builder 强制，
    /// 调用方不必（也不该）再自己设 `position`。
    #[must_use]
    pub fn layout_override(mut self, rect: Rect) -> Self;
}

// src/node/tree.rs
/// 挂载后更新覆盖矩形（P13 的分栏拖拽）：写值 + 标 `LAYOUT|PAINT` + 清 node..root 的缓存。
pub fn set_layout_override(&mut self, node: NodeId, over: Option<LayoutOverride>)
                           -> Result<(), EngineError>;
```

### 10.3 与 taffy 父布局的交互（逐条）

| 问题                                      | 答案                                                                          | 实现                                                                                                                                                    |
|-------------------------------------------|-------------------------------------------------------------------------------|---------------------------------------------------------------------------------------------------------------------------------------------------------|
| 父是否为它留空间？                        | **不留**（出流）                                                              | builder 强制 `position: Absolute` ⇒ taffy 把它当 OOF 候选，流内算法跳过它                                                                               |
| 位置由谁决定？                            | **应用**（`rect.origin`）                                                     | `absolutize` 用 `map_or(slot.local, \|over\| over.rect)`（§7.2 的 `let local = …`） 覆盖 taffy 给的 `local`                                             |
| 尺寸由谁决定？                            | **应用**（`rect.size`）                                                       | `dispatch_child_layout` 直接返回 `LayoutOutput::from_outer_size(rect.size)`，不再过 min/max/padding                                                     |
| 父的 `inset`/对齐还有效吗？               | **无效**                                                                      | 覆盖盒的 origin 完全由 `rect` 决定（taffy 的 static position / inset 被忽略）                                                                           |
| `min`/`max`/`padding`/`border` 还生效吗？ | **尺寸不生效**（`rect` 就是 border box）；`border` 仍用于 clip 的 padding box | `LayoutBox.border` 来自 taffy 的 `Layout.border`                                                                                                        |
| 能挂子节点吗？                            | **不能**（它是布局叶子）                                                      | `NodeBuilder::build` 的 debug 断言：父是 `kind.is_leaf()` **或** `layout_override.is_some()`                                                            |
| 命中测试？                                | 参与（正常按 `bounds` + `hit_shape`）                                         | §12                                                                                                                                                     |
| damage？                                  | 参与（`bounds` 变化走 `absolutize` 的 `damage ∪= old ∪ new`）                 | §7                                                                                                                                                      |
| LAYOUT 冒泡？                             | 在它自己处停止（出流不推动父）                                                | P03 `affects_parent` 已对 `Absolute` 返回 `false`；`depends_on_children` 额外对 `layout_override.is_some()` 返回 `false`（P03 §9.3 表最后一行的预埋点） |

### 10.4 分栏拖拽（`DragState`）的用法示例

```rust
// 应用层（P13 的 DragState 把拖拽位移放进 Transient，只在需要时写一次 LAYOUT）
let splitter = NodeBuilder::boxed().style(gutter_style()).build(tree, root) ?;   // 6px 抓手
let left  = NodeBuilder::canvas().layout_override(Rect::new(0.0, 0.0, 320.0, 600.0))
.build(tree, root) ?;
let right = NodeBuilder::canvas().layout_override(Rect::new(326.0, 0.0, 474.0, 600.0))
.build(tree, root) ?;

// 拖拽中（一次性，不是每帧）：应用自己算新矩形 → 写覆盖 + 标 LAYOUT（清缓存）+ 重排
tree.set_layout_override(left, Some(LayoutOverride::new(Rect::new(0.0, 0.0, new_width, 600.0))));
window.compute_layout() ?;
```

### 10.5 为什么不做「保留流内空间」的变体

| 方案                                                      | 否决理由                                                                                                                                                                                           |
|-----------------------------------------------------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 覆盖盒参与父的流内尺寸（父为它留 `rect.size`）            | 分栏/画布的语义是「我自己占这块像素」，父不该二选一：要么留空间（则拖拽时父会跟着重排，坐标打架），要么不留。CSS 的 `position: absolute` 就是后者                                                  |
| 用 `Style.inner.size/inset` 表达（不加 `LayoutOverride`） | 那测量仍会走 `compute_leaf_layout`（min/max/padding/aspect-ratio 都可能改写应用给的数字），而且 `Canvas`/`Geometry` 没有子内容可测量，语义上空转。设计 §4.3 明确把 `LayoutOverride` 列为独立逃生口 |

> **适用范围（N12）**：`LayoutOverride` 只给「几何由应用自己算、且**不需要引擎为它布局子节点**」的叶子（K 线画布、地图视口、自绘分栏面板）。
> **分栏里的容器如果还需要子布局**（例如带标题栏的面板里有按钮），**不要**用 Override：用普通容器 + `Style` 的尺寸/
> `flex_grow`，
> 让 taffy 正常排它的子节点；否则该面板的整棵子树都不会产生布局盒（`build` 的 debug 断言也会拦）。

---

## 11. clip

### 11.1 类型

```rust
// src/paint/style.rs
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Style {
    pub inner: taffy::Style<SmolStr>,
    /// 子树裁剪策略（P04 新增；命中测试的 clip 链来源，P06 的 `Clip` 图元同源）。
    pub clip: Clip,
}

/// 裁剪策略。v1 只有矩形（`Bounds`）；圆角由 P06 的 `CornerRadius` 追加变体（`Clip::Rounded`）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Clip {
    /// 不裁剪。
    #[default]
    None,
    /// 按本节点的 **padding box**（`bounds` 内缩 `border`）裁剪整棵子树。
    Bounds,
}
```

### 11.2 语义表

| 项                         | 结论                                                     | 理由                                                                                                                                           |
|----------------------------|----------------------------------------------------------|------------------------------------------------------------------------------------------------------------------------------------------------|
| 裁剪区域                   | `bounds.inset(border)` = **padding box**                 | CSS `overflow: hidden` 的裁剪盒是 padding box；`LayoutBox.border` 就是为它加的                                                                 |
| 裁剪谁                     | **只裁子树（后代）**，不裁本节点自己的 border/background | CSS 同义；probe 的 `clip_prunes_subtree_and_fills_chain` 固化了「边框带仍可命中自己」                                                          |
| 是否读 taffy 的 `overflow` | **不读**                                                 | taffy 的 `Overflow` 同时影响布局（滚动条槽、可滚动溢出矩形），与「裁剪」是两件事；引擎用独立字段表达裁剪，避免「设了 overflow 就悄悄改了布局」 |
| 圆角                       | v1 不做                                                  | 需要 `CornerRadius`（P06）与 SDF 判定；命中测试届时按圆角矩形判定（`Clip::Rounded` 追加变体）                                                  |
| 深度上限                   | 8（`paint::scene::MAX_CLIP_DEPTH`）                      | 三处一致：`Lane::push` 断言、`Canvas::push_clip` 门限、P10 shader 的定长数组。`ClipChain` 复用同一上限                                         |
| 滚动偏移（P10）            | **在 `clip_chain` 的矩形上加偏移**，不改布局             | 滚动是 `Transient`（§3.7），只改 uniform/clip；`HitResult.clip_chain` 已经是绝对矩形，P10/P12 只需在命中前把矩形与点同时平移                   |

### 11.3 `Node.clip` 字段的裁决

P03 §11.1 明确「`clip: Option<ClipId>` 推迟（P06 才有 clip 表）」。P04 **不引入 clip 表**：裁剪矩形是**布局盒的派生量**
（`bounds.inset(border)`），不是独立资源。因此 `clip` 不是 `Node` 字段，而是 `Style.clip` + `Node::clip()` 读取器：

```rust
impl Node {
    /// 子树裁剪策略。
    #[must_use]
    pub fn clip(&self) -> Clip { self.style.clip }
}
```

---

## 12. 命中测试（`layout/hit_test.rs`）

### 12.1 契约类型

```rust
/// clip 栈深度上限：**复用** `paint::scene::MAX_CLIP_DEPTH`（`u8`，值 8），不新增第四份常量。
const MAX_CLIP_DEPTH: usize = usize::from(crate::paint::scene::MAX_CLIP_DEPTH);

/// 命中路径上的 clip 矩形（定长、`Copy`、零分配；祖先 → 目标顺序）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClipChain {
    len: u8,
    rects: [Rect; MAX_CLIP_DEPTH]
}

impl Default for ClipChain {
    fn default() -> Self { Self { len: 0, rects: [Rect::default(); MAX_CLIP_DEPTH] } }
}

impl ClipChain {
    /// 由祖先到目标顺序的 clip 矩形（窗口绝对）。
    #[must_use]
    pub fn as_slice(&self) -> &[Rect];
    #[must_use]
    pub fn len(&self) -> usize;
    #[must_use]
    pub fn is_empty(&self) -> bool;
    fn push(&mut self, rect: Rect);          // 超过 8 层则丢弃（不 panic；debug 端由 Lane::push 的断言先行暴露）
    fn truncate(&mut self, len: usize);
}

/// 命中结果（设计 §4.4）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HitResult {
    /// 命中的节点（逆绘制序的第一个）。
    pub node: NodeId,
    /// 命中点在目标节点 **border box 局部坐标** 下的位置（原点 = 节点左上角）。
    pub local_point: Point,
    /// 到达目标节点经过的 clip 矩形（窗口绝对，祖先 → 目标）。
    pub clip_chain: ClipChain,
}
```

| 决策                                                         | 结论                                                                                  | 理由                                                                                    |
|--------------------------------------------------------------|---------------------------------------------------------------------------------------|-----------------------------------------------------------------------------------------|
| 为什么 `ClipChain` 是定长结构而不是 `Vec<Rect>` / `SmallVec` | 定长 `Copy`（136 B）、零分配、`HitResult: Copy`；且不把 `smallvec` 的版本写进公开 API | 指针事件频率虽低，但 `HitResult` 会被 P10/P13/P14 共享，`Copy` 让分发代码不必借生命周期 |
| 为什么 `clip_chain` 存**矩形**而不是 `NodeId`                | 矩形是「已解析」的最终结果：P10 分发时不需要再回查节点（节点可能在同一帧里被卸载）    | 也避免 `HitResult` 持有可能陈旧的 `NodeId` 列表                                         |
| 为什么 `local_point` 是 border box 相对                      | 与 CSS 的 offsetX/offsetY 一致；P10 换算 `local = window - bounds.origin` 一行        | —                                                                                       |

### 12.2 算法（逐字）

```rust
> * * 适配 P03 的 `get` * * ：`NodeTree::get` 返回 `Result< & Node, EngineError>`（P03 冻结），本文* * 全文* * snippet 里的
> `tree.get(x)` 都按 * * `.ok()` * * 适配；下面已逐处写成 `.ok() ? ` / ` let Ok(..) =..else `。

```rust
pub(crate) fn hit_test(tree: &NodeTree, point: Point) -> Option<HitResult> {
    let root = tree.root();
    if !tree.get(root).ok()?.layout().bounds.contains(point) {
        return None;                              // 窗口外（根 100% 撑满 ⇒ 等价于「点在窗口外」）
    }
    let mut chain = ClipChain::default();
    pick(tree, root, point, &mut chain)
}

/// 逆绘制序：`order` 是 DFS 先序 ⇒ 兄弟**逆序** + 先子树后自身 = 从上层到下层。
fn pick(tree: &NodeTree, node: NodeId, point: Point, chain: &mut ClipChain) -> Option<HitResult> {
    let slot = tree.get(node).ok()?;
    let bounds = slot.layout().bounds;
    let depth = chain.len();                      // 本节点**未**入链前的深度（N3：目标自身的 clip 不计入 chain）

    // 本节点的 clip 只作用于**子树**（不裁自己的 border）；点在 clip 外时不下钻但可以命中自己
    let mut clipped = false;
    if slot.clip() == Clip::Bounds {
        let clip_rect = bounds.inset(slot.layout().border);
        if clip_rect.contains(point) { chain.push(clip_rect); } else { clipped = true; }
    }

    // 不可见/隐藏的节点整体不参与命中（N9：对**目标自身**也成立，不只是对子树）
    if matches!(slot.style().inner.display, taffy::style::Display::None) { return None; }
    if slot.style_patch().visible == Some(false) { return None; }

    if !clipped {
        for child in slot.children().iter().rev() {
            let Ok(child_node) = tree.get(*child) else { continue };
            // B2：**布局盒不能作为子树剪枝依据**（出流/溢出子节点可落在父盒之外）。
            // 唯一安全的子树剪枝是「本节点 `Clip::Bounds` 且点在 padding box 外」，由 `pick` 的
            // `clipped` 处理；这里只跳过「叶子 + 盒外」这种零代价的明显不可能项。
            let leaf = child_node.children().is_empty();
            if leaf && !child_node.layout().bounds.contains(point) { continue; }
            if let Some(hit) = pick(tree, *child, point, chain) {
                chain.truncate(depth);
                return Some(hit);
            }
        }
    }

    // 自身：先看布局盒（允许溢出后代，但自身不声称盒外命中），再看 `hit_shape`（局部坐标）
    if !bounds.contains(point) {
        chain.truncate(depth);
        return None;
    }
    let local_point = Point::new(point.x - bounds.origin.x, point.y - bounds.origin.y);
    if let Some(shape) = slot.hit_shape()
        && !shape(local_point)
    {
        chain.truncate(depth);
        return None;
    }
    // N3：`clip_chain` 只含**祖先**的 clip（`truncate(depth)` 去掉本节点自己压入的那一个）
    let mut clip_chain = *chain;
    clip_chain.truncate(depth);
    chain.truncate(depth);
    Some(HitResult { node, local_point, clip_chain })
}
```

**五个必须固化的点**

| #   | 点                                                                                   | 说明                                                                                                                                                                                                                                      |
|-----|--------------------------------------------------------------------------------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| H1  | 逆序 + 「先子树后自身」                                                              | 与 `order`（DFS 先序）严格互为逆序 ⇒ 后创建/后绘制的子树先命中；子内容压过父背景                                                                                                                                                          |
| H2  | clip 只裁子树、且在**下钻前**判定                                                    | 点在 padding box 外 ⇒ 跳过整个子树（剪枝），但本节点仍可命中（边框带）。probe：`(1,1)` 命中 scroller 自身、`(10,10)` 命中被 clip 的子节点                                                                                                 |
| H2c | **布局盒不是剪枝依据**（B2）                                                         | 出流/溢出子节点可以落在父盒之外，所以「父盒不含该点」不能跳过整棵子树；只有 `Clip::Bounds` 祖先才能剪。代价：最坏 O(n)（P10/P18 可加空间索引，见 V-38）                                                                                   |
| H2b | **目标自身的 clip 不进 `clip_chain`**（N3）                                          | `clip_chain` 的语义是「到达目标前经过的裁剪」；目标自己的 padding box 由应用从 `bounds`/`border` 自己算（否则 P10 的滚动容器会拿到一个多余的自身矩形）。实现：进入函数时记 `depth`，返回前 `truncate(depth)`                              |
| H3  | `hit_shape` 收**局部坐标**；拒绝即 `None`（继续测下层的兄弟）                        | `Canvas` / `Geometry` / 自绘节点的唯一命中入口；probe：左半边可命中、右半边落到根                                                                                                                                                         |
| H3b | **不可见节点整体不可命中**（N9）                                                     | `display: none` 或 `StylePatch.visible == Some(false)` 的节点（含目标自身）在进入 `pick` 时直接返回 `None`——与 P03 的「visible=false 不参与命中」契约、以及 P06 的「不绘制」一致                                                          |
| H4  | `Overlay` **没有特殊分支**（N10：契约是「**浮层子树必须 append 到 `order` 末尾**」） | 「浮层优先」= 浮层子树排在 `order` 末尾（绘制序靠后）⇒ 逆序先命中。P13 的 `Overlay` 栈**必须**在挂载/重排时把浮层子节点 append 到父的末尾（或根的末尾），否则命中顺序会错；probe 用「最后一个绝对定位子节点压过更大的底层节点」固化了这条 |
| H5  | `ClipChain::truncate` 必须回溯                                                       | 子树未命中时若不清 `chain`，下一个兄弟/自身的 `HitResult.clip_chain` 会带上别人的矩形                                                                                                                                                     |

**坐标换算**（唯一的两处）

| 换算                | 公式                                                  | 消费者                       |
|---------------------|-------------------------------------------------------|------------------------------|
| 窗口逻辑 → 节点局部 | `local = window - node.bounds.origin`                 | `local_point`、`hit_shape`   |
| 物理像素 → 窗口逻辑 | `Scale::to_logical(physical)`（P01 §6.3，唯一反向点） | P10 的输入事件（不在本阶段） |

### 12.3 `hit_shape` 的类型与契约

```rust
// Node 字段（不进公开类型别名，避免多一个白名单条目）
hit_shape: Option<Box<dyn Fn(Point) -> bool> >,
// NodeBuilder
/// 自定义命中形状：`point` 是**节点 border box 局部坐标**；返回 `false` = 该点穿透到下层。
/// 纯函数（不得读 Signal / 写状态）；与 `measure` 的纯函数要求同一理由（§9.3）。
#[must_use]
pub fn hit_shape(self, shape: impl Fn(Point) -> bool + 'static) -> Self;
```

| 决策                                                            | 结论                                                                                     | 理由                                                                                   |
|-----------------------------------------------------------------|------------------------------------------------------------------------------------------|----------------------------------------------------------------------------------------|
| 为什么是 `bool` 而不是「返回 `Option<HitResult>`/子节点重定向」 | 最小能力：命中形状只回答「这个点是否落在我的形状内」；重定向属 P13 的 `Overlay`/事件冒泡 | 形状回调若能把命中转给别的节点，就等于在 hit test 里开了第二条分发通道（难测、易循环） |
| 为什么收局部坐标而不是窗口坐标                                  | 形状是节点的局部定义（圆/多边形/路径），窗口坐标随父移动，局部坐标稳定                   | 也避免应用在闭包里持父节点位置                                                         |
| 默认（`None`）                                                  | 用 `bounds` 矩形判定                                                                     | 90% 的节点是矩形                                                                       |

---

## 13. 入口与报告

### 13.1 `Window`（`src/window.rs`）

```rust
impl Window {
    /// 逻辑尺寸（根布局的可用空间）。
    #[must_use]
    pub fn size(&self) -> Size;

    /// 逻辑尺寸变化（P12 的 resize 事件）：标根 `LAYOUT` + 清 `root` 的 taffy 缓存。
    pub fn set_size(&mut self, size: Size);

    /// 用窗口逻辑尺寸重排，返回本次统计。
    ///
    /// # Errors
    /// `EngineError::InvalidMeasure { node, axis, value }`：某个 measure 闭包返回 NaN / 负数（首条优先）。
    /// 出错时布局盒可能为空，调用方**不得**提交本帧。
    pub fn compute_layout(&mut self) -> Result<LayoutReport, EngineError>;
}
```

实现（逐字）：

```rust
pub fn compute_layout(&mut self) -> Result<LayoutReport, EngineError> {
    self.tree.compute_layout(taffy::geometry::Size {
        width: Some(self.size.width),
        height: Some(self.size.height),
    })
}

pub fn set_size(&mut self, size: Size) {
    self.size = size;
    self.tree.invalidate_layout(self.tree.root());
}
```

**帧内的位置**（P03 §14.4 的 pass 表续）：

```text
drain（写 → 标脏） → App::refresh_styles(window) → Window::compute_layout() → P06 paint lane → P06 damage
                                                       ↑ 本阶段
```

| 决策                             | 结论                                                             | 理由                                                                                                                                           |
|----------------------------------|------------------------------------------------------------------|------------------------------------------------------------------------------------------------------------------------------------------------|
| 为什么不给 `App::layout(window)` | 不加                                                             | `App` 只需要「查窗口」；返回 `Option<Result<…>>` 是双层失败语义（窗口不存在 / measure 违规）。P06 的帧驱动拿 `&mut Window` 直接调。→ 裁决项 #2 |
| 为什么 `available` 不由调用方传  | 不传                                                             | 根可用空间**就是窗口逻辑尺寸**（P12 写入 `size`）；多一个参数就多一个「传错尺寸」的入口                                                        |
| `scale` 是布局输入吗             | **不是**                                                         | I11：布局全用逻辑像素。`set_scale` 只保守地标根 `LAYOUT`（P03 行为），probe 断言「换 scale 后逻辑盒逐位不变」                                  |
| `LAYOUT` 何时清                  | taffy 写布局时（`set_local_layout`），即「这个节点本轮已经排过」 | 与 P03 的 `clear_paint` 对称；`LAYOUT` 的读者是 `mark_layout` 的早停与 `StylePassReport.layout_dirty`                                          |

### 13.2 `LayoutReport` 的消费者

| 字段                          | P06 用途                       | P08 用途                             |
|-------------------------------|--------------------------------|--------------------------------------|
| `laid_out`                    | 帧记录里的「重排节点数」       | 测帧推进的断言（`drain()` 后应为 0） |
| `moved`                       | damage 的规模指示              | 同上                                 |
| `cache_hits` / `cache_misses` | 缓存健康度（P18 基准的另一面） | L1 测试的「只重算脏子树」断言        |

> `LayoutReport` **不含** damage 矩形：damage 是**跨帧累积**的（P03 §9.4 的 `touch` 并集 + `absolutize` 再并），
> 由 P06 在帧初清零并物理化；放进 layout 报告会促使调用方把它当成「本帧 damage」而丢帧外的累积。

---

## 14. 错误面

### 14.1 追加 `InvalidMeasure`（P01 §6.2.2 #9，逐字）

```rust
// src/error.rs，位置按 P01 的编号顺序（CyclicDerived 之后）
/// measure 闭包返回了非有限值或负尺寸（设计 §4.8）。
#[error(
    "invalid measure for {node:?} on the {axis:?} axis: {value} \
     (measure closures must return finite, non-negative sizes)"
)]
InvalidMeasure { node: NodeId, axis: Axis, value: f32 },
```

| 项               | 结论                                                                                    |
|------------------|-----------------------------------------------------------------------------------------|
| 变体名 / payload | 与 P01 §6.2.2 #9 **逐字一致**                                                           |
| 实现 Phase       | 表内写 P05 → **改为 P04**（measure 适配在 P04 落地）                                    |
| `source()`       | `None`（不包装外部错误）；`error.rs` 的 `all_variants()` 与 `source_is_none` 测试要加它 |
| `Display`        | 英文 + 结构化字段（`NodeId(n3.1)` / `X` / `NaN`），与 P03 同风格                        |
| 首个违规         | 由 `note_invalid_measure` 保证（`Option` 只写一次）                                     |

### 14.2 `LayoutInvalid`（P01 #10）：**本阶段不落地**

| 理由                                      | 说明                                                                                    |
|-------------------------------------------|-----------------------------------------------------------------------------------------|
| 没有生产者                                | taffy 的低级 API 全部返回 `LayoutOutput`（不返回 `Result`），没有「布局非法」的错误路径 |
| 唯一的后置校验已经独立成 `InvalidMeasure` | 尺寸非有限/负 = measure 的错，语义比「布局非法」精确                                    |
| 冻结表允许空置                            | P01 §6.2.1 明确「实现按 Phase 分批」；P04 只是继续把 #10 留给需要它的阶段               |
| 不留占位                                  | 变体是 `pub` 的，加了没有生产者的变体只会在 `all_variants()` 里制造假覆盖               |

### 14.3 `calc()` 不解析（已核实）

| 项                                   | 内容                                                                                                                                                                                  |
|--------------------------------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 事实                                 | `taffy::style::Calc(*const ())` 是**不透明指针**（`style/dimension.rs:106-109`），只暴露 `calc_value() -> *const ()`；taffy 0.15 **没有**公开的 calc 求值器                           |
| taffy 自带树的行为                   | `impl LayoutPartialTree for TaffyView { fn resolve_calc_value(&self, _val: *const (), _basis: f32) -> f32 { 0.0 } }`（`tree/taffy_tree.rs:400-403`）                                  |
| 引擎的做法                           | **走默认实现（0.0）**，与 taffy 自带树逐字一致                                                                                                                                        |
| 影响                                 | 应用层不得使用 `Dimension::Calc` / `LengthPercentage::Calc`（布局会得到 0）。写进 `Style.inner` 的 rustdoc 与 P08 的迁移注意项；不新增错误变体（没有运行时可判定的「用了 calc」信号） |
| 为什么 workspace 仍开 `calc` feature | P01 §5.3④ 的冻结配置（关掉会让 `Dimension` 的变体集变化，`dirty.rs` 的 `ExpandedDimension::Calc` 分支会编译失败，见 F-42）。保留 feature + 「不解析」是零成本的一致选择               |

---

## 15. 测试清单（计划 **60** 项 / 实测落地 **71** 项，全部内联 `#[cfg(test)] mod tests`）

> 组织：每文件的用例放在该文件末尾（AGENTS §5.1）。断言值来自 probe 的**实测**（标 ✅ 的已在 probe 里跑通）。

### 15.1 `layout/taffy_tree.rs`（20）

| #     | 用例                                                     | 断言要点                                                                                                                                                      |
|-------|----------------------------------------------------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 1 ✅  | `flex_row_places_children_left_to_right`                 | 3 个 `10/20/30` 宽的 fixed 子节点 x = 0/10/30；根 100% 撑满                                                                                                   |
| 2     | `flex_column_stacks_children`                            | `flex_direction: Column` ⇒ y = 0/h0/h0+h1                                                                                                                     |
| 3 ✅  | `grid_two_columns_place_items`                           | `grid_template_columns = [30, 70]` ⇒ 第二列起点 x = 30                                                                                                        |
| 4 ✅  | `block_stacks_children`                                  | block 容器内两个子节点 y = 0 / 10                                                                                                                             |
| 5 ✅  | `fractional_sizes_are_not_rounded`                       | `10.5 × 3.25` 原样保留（**不**调 `round_layout` 的证据）                                                                                                      |
| 6 ✅  | `padding_offsets_child_origin`                           | 父 `padding: 4` ⇒ 子绝对原点 (4,4)                                                                                                                            |
| 7     | `border_and_padding_offset_child_origin`                 | 父 `border: 2 + padding: 3` ⇒ 子原点 (5,5)，且 `child.layout().border == Edges::ZERO`                                                                         |
| 8     | `min_max_size_clamp_measured_leaf`                       | measure 返回 200×10、`max_width: 50` ⇒ 盒宽 50                                                                                                                |
| 9     | `percent_size_resolves_against_parent`                   | 父 100×100、子 `50%` ⇒ 子 50×50                                                                                                                               |
| 10    | `display_none_zeroes_subtree`                            | 隐藏节点与其子节点盒为空（`is_empty()`），根尺寸不受影响                                                                                                      |
| 11 ✅ | `root_fills_available_space`                             | 窗口 200×100 ⇒ 根 `bounds = (0,0,200,100)`                                                                                                                    |
| 12 ✅ | `layout_override_wins_over_flow_position`                | override `(8,16,40,20)` ⇒ 盒 `(8,16,40,20)`（taffy 的出流位置被覆盖）                                                                                         |
| 13 ✅ | `absolute_child_relative_to_positioned_ancestor`         | 定位祖先 inset 12/8 ⇒ 子盒 (12,8)                                                                                                                             |
| 14 ✅ | `out_of_flow_hoisted_to_ancestor_uses_containing_block`  | 树父是 static、包含块是祖父 ⇒ 盒相对**包含块** border box；含 padding 的容器仍按 border 参照（实测 (5,7)）                                                    |
| 15 ✅ | `second_run_without_changes_does_no_work`                | `laid_out == 0 && moved == 0 && cache_hits >= 1`                                                                                                              |
| 16 ✅ | `incremental_layout_hits_sibling_caches`                 | 改中间子节点 ⇒ `laid_out <= 2`、`cache_hits > 0`、兄弟盒未变、被改节点后的兄弟 x 位移                                                                         |
| 17    | `change_below_fixed_size_ancestor_still_relayouts`       | 固定尺寸祖先下的子节点改动仍被重排（证明「清缓存到根」不能早停）；**同时**断言 `cache_hits > 0`（无关兄弟仍命中）与**未受影响子树的位置/尺寸逐位不变**（N14） |
| 18 ✅ | `measure_receives_definite_width_in_block_and_is_cached` | `available.width == 100`（block）；第二轮 measure **零新增调用**；高度来自 measure                                                                            |
| 19 ✅ | `flex_item_intrinsic_width_from_measure`                 | flex 主轴 = measure 的固有宽度 80；`align-items: flex-start` 下不被 stretch                                                                                   |
| 20 ✅ | `invalid_measure_reports_node_axis_value`                | `NaN` ⇒ `Err(InvalidMeasure { node, axis: X, value: NaN })`                                                                                                   |

### 15.2 `layout/measure.rs`（9）

| #    | 用例                                       | 断言要点                                                                                                                  |
|------|--------------------------------------------|---------------------------------------------------------------------------------------------------------------------------|
| 1    | `measure_fn_is_boxed_fn`                   | `let _: MeasureFn = Box::new(\|_, _\| Size::ZERO);` 编译；`Box<dyn FnMut>` 不能赋给 `MeasureFn`（类型级证据，写在注释里） |
| 2    | `leaf_without_measure_is_zero_size`        | Canvas/Geometry 叶子、无 measure ⇒ 盒尺寸 = padding+border                                                                |
| 3 ✅ | `fixed_style_leaf_does_not_call_measure`   | 两侧 `known_dimensions` 已知 ⇒ taffy 早退，measure 调用 0 次                                                              |
| 4    | `measure_records_available_space_variants` | 三种 `AvailableSpace` 都原样到达闭包（用 `Rc<Cell<Option<AvailableSpace>>>` 记录）                                        |
| 5    | `nan_width_is_invalid_measure_x`           | `axis == X`、`value.is_nan()`                                                                                             |
| 6 ✅ | `negative_height_is_invalid_measure_y`     | `axis == Y`；错误 payload 逐字段断言                                                                                      |
| 7    | `first_violation_wins`                     | 两个坏叶子 ⇒ 报告的 `node` 是同一次布局里先被求值的那个（同树两次重排结果一致）                                           |
| 8    | `invalid_measure_leaves_empty_boxes`       | 出错后该节点 `layout().bounds.is_empty()`（后续 hit/damage 安全）                                                         |
| 9    | `invalidate_measure_forces_remeasure`      | 调用计数 +1、`LAYOUT` 被置、缓存被清；再排一轮计数不再增长                                                                |

### 15.3 `layout/hit_test.rs`（15）

| #     | 用例                                                | 断言要点                                                                      |
|-------|-----------------------------------------------------|-------------------------------------------------------------------------------|
| 1 ✅  | `deepest_last_child_wins`                           | 两个重叠浮层 ⇒ 后创建的命中；`local_point` 正确                               |
| 2 ✅  | `child_beats_parent_background`                     | 子节点区域内命中子；父背景区域命中父                                          |
| 3 ✅  | `clip_prunes_subtree_and_fills_chain`               | clip 内命中子且链长 1（矩形 = padding box）；padding box 外的边框带命中父自身 |
| 4     | `nested_clips_accumulate_outer_to_inner`            | 两层 clip ⇒ 链长 2，`as_slice()[0]` 是外层                                    |
| 5     | `clip_chain_caps_at_eight`                          | 9 层嵌套 ⇒ 链长 8（不 panic）                                                 |
| 6 ✅  | `hit_shape_filters_local_points`                    | `x < 20` 的自定义形状：左侧命中 canvas、右侧落到根                            |
| 7     | `hit_shape_receives_node_local_coords`              | 节点在 (10,20) ⇒ 闭包收到 `(point - (10,20))`                                 |
| 8 ✅  | `display_none_is_not_hittable`                      | 隐藏节点被跳过，落到根                                                        |
| 9 ✅  | `points_outside_the_window_miss`                    | 负坐标 / 超出根 ⇒ `None`                                                      |
| 10 ✅ | `overlay_like_last_sibling_wins_regardless_of_size` | 排在末尾的小浮层压过更大的底层节点                                            |
| 11 ✅ | `override_node_is_hittable_at_its_rect`             | override 区域内命中 canvas；区域外命中父                                      |
| 12    | `visible_false_skips_node`                          | `StylePatch { visible: Some(false) }` ⇒ 不参与命中（P03 契约）                |
| 13    | `local_point_is_border_box_relative`                | 节点盒 (10,20,30,40) + 命中 (15,25) ⇒ local (5,5)                             |
| 14 ✅ | `clip_chain_is_copy_and_default_is_empty`           | `ClipChain::default()` 空、`Copy`                                             |
| 15    | `siblings_are_tested_later_to_earlier`              | 三个水平相邻子节点：每个 x 区间命中对应节点（逆序剪枝不误伤）                 |

### 15.4 `node/tree.rs`（10）

| #    | 用例                                         | 断言要点                                                                                                     |
|------|----------------------------------------------|--------------------------------------------------------------------------------------------------------------|
| 1    | `node_id_bits_roundtrip`                     | `NodeId::from_bits(id.to_bits()) == Some(id)`；代际 0 的 bits ⇒ `None`                                       |
| 2 ✅ | `node_id_is_8_bytes`                         | `size_of::<NodeId>() == 8`、`Option<NodeId>` == 8                                                            |
| 3    | `layout_box_is_32_bytes`                     | `LayoutBox = Rect(16) + Edges(16)`                                                                           |
| 4 ✅ | `node_size_within_budget`                    | `size_of::<Node>() <= 1152`（P04 实测 1016；超上界 → V-31 的 `style: Box<Style>`），并打印实测值供回填       |
| 5 ✅ | `order_stays_dfs_preorder`                   | 插入 4 个节点后 `order == [root, a, a1, b]`                                                                  |
| 6 ✅ | `slot_reuse_drops_cache`                     | 复用槽位后 `layout_cache[index].is_none()`、旧句柄 `get` 返回 `None`                                         |
| 7    | `root_style_is_percent_100`                  | 根的 `style.inner.size` 两轴都是 100%；窗口 200×100 ⇒ 根 200×100                                             |
| 8    | `set_style_clears_caches_to_root`            | 改深层节点 style 后：`root..node` 的 `Cache::is_empty()` 全为真，兄弟子树仍非空；陈旧句柄 ⇒ `Err(StaleNode)` |
| 9 ✅ | `damage_unions_prev_and_new_absolute_bounds` | 节点从 (10,0) 移到 (30,0) ⇒ damage = (10,0,30,10)                                                            |
| 10   | `remove_drops_cache_and_hoisted_entries`     | 卸载子树后对应槽位的 `layout_cache` 与 `hoisted` 条目被清（无泄漏）                                          |

### 15.5 `window.rs`（4）

| #    | 用例                                          | 断言要点                                             |
|------|-----------------------------------------------|------------------------------------------------------|
| 1 ✅ | `window_size_is_root_available_space`         | 窗口 120×60 ⇒ 根 120×60；`laid_out >= 2`             |
| 2 ✅ | `dpi_scale_does_not_change_logical_boxes`     | `scale 1 → 1.5` 后逻辑盒**逐位相同**（物理化在 P06） |
| 3 ✅ | `resize_relayouts_percent_children`           | `100%` 子节点随 `set_size` 变化（100×100 → 300×50）  |
| 4    | `set_size_marks_root_layout_and_clears_cache` | `root.dirty().contains(LAYOUT)`、根缓存空            |

### 15.6 `error.rs` + `paint/style.rs`（2）

| # | 用例                                           | 断言要点                                                                                       |
|---|------------------------------------------------|------------------------------------------------------------------------------------------------|
| 1 | `invalid_measure_display_names_axis_and_value` | 文本含 `invalid measure` / `NodeId(n2.1)` / `Y`；`source()` 为 `None`（加进 `all_variants()`） |
| 2 | `clip_default_is_none_and_eq`                  | `Clip::default() == Clip::None`；`Style::default().clip == Clip::None`                         |

> **计划 60**（20 + 9 + 15 + 10 + 4 + 2）/ **实测落地 74**（多 14，全部是覆盖强化与边界用例）：
> `layout/taffy_tree.rs` 21、`layout/hit_test.rs` 16、`layout/measure.rs` 10、`node/tree.rs` 9 新增、
> `node/builder.rs` 3 新增、`window.rs` 4 新增、`node/dirty.rs` 1、`error.rs` 1、`paint/style.rs` 1、`node/id.rs` 1。
> `cargo test -p ui_core` = **271**（P03 结束 200 + P04 71），`--release` = **264**（7 个 debug-only 用例不编译）。
> `--release` 下带 `#[cfg(debug_assertions)]` 的用例（本阶段 2 个：`leaf_parent_is_debug_assert`、
> `layout_override_leaf_parent_is_debug_assert`）不编译，其余断言在 debug/release 下必须逐位一致（主设计 §6.2 的「零差异判定」）。

**L1 清单（主设计 §6.2）的覆盖对照**

| 设计 §6.2 的类别                                                                | 本阶段用例                                                       |
|---------------------------------------------------------------------------------|------------------------------------------------------------------|
| 盒模型：flex / grid / block、固定宽高、min/max、padding/margin/border           | 15.1-1…9                                                         |
| 文本                                                                            | 交给 P05（measure 的接口与 mock 覆盖：15.1-18/19、15.2）         |
| 逃生口：自定义 measure、分栏 `LayoutOverride`、`Canvas`/`Geometry` 不参与子布局 | 15.1-12/14/19、15.3-11、15.2-2                                   |
| 高分屏                                                                          | 15.5-2（逻辑盒不变；物理化 P06）                                 |
| 命中：z 序、clip 链、浮层优先、`Geometry` hit_shape、滚动后换算                 | 15.3-1…15（滚动偏移接口属 P10，`clip_chain` 已给绝对矩形）       |
| 增量：只改一个 label → dirty 集合恰为该节点；尺寸变化才冒泡                     | 15.1-15/16/17、15.4-8                                            |
| 动态属性（disabled / hover / 主题）                                             | P03 已有（`refresh_styles` 的用例）；P04 只消费 `affects_layout` |
| 回归：release 与 debug 探针一致                                                 | §16-1 的两条命令                                                 |

---

## 16. 验收门禁

```powershell
# ── 1) 单元测试（debug + release） ──
cargo test  -p ui_core
cargo test  -p ui_core --release

# ── 2) clippy（workspace lint 由 [lints] workspace = true 继承） ──
cargo +nightly clippy -p ui_core --all-targets -- -D warnings

# ── 3) 格式 ──
cargo +nightly fmt -p ui_core -- --check

# ── 4) 文档零警告 ──
cargo doc -p ui_core --no-deps

# ── 5) 三平台依赖解析（不链接） ──
cargo fetch
cargo metadata --format-version 1 --filter-platform x86_64-pc-windows-msvc   | Out-Null
cargo metadata --format-version 1 --filter-platform aarch64-apple-darwin     | Out-Null
cargo metadata --format-version 1 --filter-platform x86_64-unknown-linux-gnu | Out-Null

# ── 6) 零 unsafe / 零 allow / 零 expect（bitflags 宏内的 allow 不算） ──
Get-ChildItem crates/ui_core/src -Recurse -Filter *.rs |
  Select-String -Pattern '\bunsafe\b|#\[allow|#\[expect' |
  Where-Object { $_.Line -notmatch 'bitflags' }
#   期望无输出

# ── 7) **不圆整**（本阶段的核心契约）：源码里不得出现 round_layout / final_layout / impl RoundTree ──
Get-ChildItem crates/ui_core/src -Recurse -Filter *.rs |
  Select-String -Pattern 'round_layout|final_layout|impl RoundTree'
#   期望无输出（P01 §7.5 / F-17；本阶段裁决不实现 RoundTree）

# ── 8) dead_code 审计：三条 grep 逐项与 §4.3 表对照 ──
Get-ChildItem crates/ui_core/src -Recurse -Filter *.rs |
  Select-String -Pattern 'pub\(crate\) (fn|struct|enum|trait|type|const)'
Get-ChildItem crates/ui_core/src -Recurse -Filter *.rs |
  Select-String -Pattern 'pub\(crate\) \w+:'
Get-ChildItem crates/ui_core/src -Recurse -Filter *.rs |
  Select-String -Pattern 'pub\(crate\) use'
#   第 2 条（`pub(crate)` 字段）**必须无输出**（field_scoped_visibility_modifiers = "forbid"，probe P9 实测）
#   第 3 条在 layout/ 下必须无输出（公共模块根不做扁平出口，M-07）

# ── 9) 文件边界（不得创建 P05+ 的文件） ──
Get-ChildItem crates/ui_core/src -Recurse -Filter *.rs | Select-Object FullName
#   期望：lib.rs error.rs math.rs app.rs window.rs node.rs paint.rs layout.rs
#         node/{id,signal,dependency,dirty,tree,builder}.rs
#         paint/{color,primitive,canvas,geometry,atlas,scene,style}.rs
#         layout/{taffy_tree,measure,hit_test}.rs
Get-ChildItem crates/ui_core/src -Recurse -Filter *.rs |
  Where-Object { $_.Name -in @('mod.rs','text.rs','event.rs','interaction.rs','a11y.rs','resources.rs',
                               'test_support.rs','render.rs','platform.rs','instance.rs','damage.rs') }
#   期望无输出

# ── 10) 模块根形态 + inherent impl 数量（probe P9：同类型只能有一个 inherent impl 块） ──
Get-Content crates\ui_core\src\lib.rs          # 期望 7 行 pub mod（+layout）
Get-Content crates\ui_core\src\layout.rs       # 期望 `pub mod hit_test; pub(crate) mod measure; pub(crate) mod taffy_tree;` + 契约类型
Select-String -Path crates\ui_core\src\node\tree.rs -Pattern '^impl NodeTree \{|^impl Node \{|^impl LayoutBox \{'
#   期望各恰好 1 行
Select-String -Path crates\ui_core\src\layout\taffy_tree.rs -Pattern '^impl '
#   期望 7 行，全部形如 `impl <Trait> for NodeTree {`（无 `impl NodeTree {`）

# ── 11) 依赖增量：必须为零 ──
git diff --stat -- Cargo.toml crates/ui_core/Cargo.toml Cargo.lock
#   期望无输出
cargo tree -p ui_core -e normal --depth 1
#   期望与 P03 结束时一致（taffy/ahash/smallvec 已在）

# ── 12) taffy 单版本（B1） ──
cargo tree -p ui_core -e normal | Select-String 'taffy v'
#   期望恰好一行 "taffy v0.15.0"
```

**「完成」判据**（2026-11-12 实测）：1–12 全绿；`cargo test -p ui_core` = **271**（200 + 71），
`cargo test -p ui_core --release` = **264**；`size_of::<Node>()` = **1016 B**（断言上界 1152 B）。

---

## 17. Cargo 依赖

**零变化。** `..` 与 `Cargo.lock` 都不动：

| 需要的依赖  | 现状                                              |
|-------------|---------------------------------------------------|
| `taffy`     | P01 已声明（`=0.15.0`，features 见 §2）           |
| `ahash`     | P03 已声明（`hoisted` 的 `HashMap`）              |
| `smallvec`  | P03 已声明（`hoisted` 的值类型、`Node.children`） |
| `smol_str`  | P01 已声明（`CustomIdent`、`Style`）              |
| `thiserror` | P01 已声明（`InvalidMeasure` 的 `#[error]`）      |

---

## 18. 需裁决项（5 项，含建议）

| # | 事项                                                    | 建议                                                   | 备选                                                                                                       | 影响面                                                                                                                         |
|---|---------------------------------------------------------|--------------------------------------------------------|------------------------------------------------------------------------------------------------------------|--------------------------------------------------------------------------------------------------------------------------------|
| 1 | **是否实现 `RoundTree`（+`TraverseTree`/`PrintTree`）** | **不实现**（probe 已证明可实现）                       | 实现（代价：每节点 +92 B `final_layout`、3 个无读者方法；换来将来 `round_layout`/`print_tree` 的一行接入） | 若裁决要求实现：`Node` 加 `final_layout` 字段、`taffy_tree.rs` 加 1 个 impl；`Node` 从 1016 B 涨到 ≈1108 B（仍在 1152 上界内） |
| 2 | **入口放 `Window` 还是 `App`**                          | `Window::compute_layout()`；**不加** `App::layout`     | `App::layout(window, available) -> Option<Result<LayoutReport, EngineError>>`                              | 若裁决要求在 `App`：签名变成双层失败语义，P06 的帧驱动多一次 `unwrap`；`refresh_styles` 已按窗口，layout 也应同层              |
| 3 | **`HitResult` 的公开路径**                              | `ui_core::layout::hit_test::HitResult`（+`ClipChain`） | 留在 P10 的 `event::HitResult`（则 P04 的命中结果不能公开，L1 测试断言受限）                               | 路径表（F-45）；P10 迁移文档                                                                                                   |
| 4 | **`LayoutBox` 是否加 `border: Edges`**                  | **加**（clip 的 padding box 需要它，P04 有真实读者）   | 不加：clip 用 border box（差一个边框宽，CSS 语义错误）                                                     | `LayoutBox` 从 16 B → 32 B；P03 的结构体字面量构造点（仅测试）要改                                                             |
| 5 | **`NodeBuilder::layout_override` 的副作用**             | builder **强制** `position: Absolute`（文档写明）      | 不强制 + `debug_assert!` 要求调用方自己设                                                                  | 若选后者：分栏场景的应用代码要多设一次 position；漏设时 override 只剩「尺寸」语义，行为不明显                                  |

---

## 19. 风险与待复核项

| 编号     | 风险                                                                                                                                           | 等级 | 处置                                                                                                                                                                         |
|----------|------------------------------------------------------------------------------------------------------------------------------------------------|------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| **V-31** | `size_of::<Node>()`：P03 = 888 B → P04 = **1016 B**（实测），距 1024 只剩 8 B；P05 的 `Text` 载荷/`canvas` 扩容会立刻顶破                      | 中   | 断言上界改为 **1152 B**（§8.3）；P05 开工前若预测超 1152 → `style: Box<Style>`（公开面不变）；**实现第一步复测并回填 §8.3**                                                  |
| **V-32** | 布局叶子（`Canvas`/`Geometry`/`LayoutOverride`）的子节点**永不被布局** ⇒ 它们的 `LAYOUT` 位永不清，`StylePassReport.layout_dirty` 可能长期为真 | 低   | 只有 release 下（debug 有断言）能构造；P04 文档写明；若 P06 的「有无脏位」判定受影响，在 `remove`/`begin_frame` 里对不可达子树清位                                           |
| **V-33** | damage 覆盖：祖先盒不包含**出流溢出**的后代（`position: absolute` 跑到父盒外）时，只按祖先 AABB 累积 damage 会漏                               | 中   | P06 落地 damage 时给 `LayoutBox` additive 加 `overflow: Rect`（taffy `Layout.scrollable_overflow_rect`，feature `content_size` 已开）并把它并进 damage；本阶段不加（无读者） |
| **V-34** | taffy 的出流 inset 基准：`absolute_position_offset = border`（不含 padding），与 CSS「padding box 定位」有差异                                 | 低   | 这是 taffy 0.15 的既定行为（`compute/flexbox.rs:511`）；引擎**照抄不重算**（改它要 fork taffy）。P08 迁移时在文档里写明「absolute 子节点的 inset 相对 border box」           |
| **V-35** | 不实现 `get_detailed_layout_info` ⇒ grid 容器内的 absolute 子盒按 padding box 而非 grid area 定位置                                            | 低   | taffy 的默认实现即为 `DetailedLayoutInfo::None`；需要时 additive 存 `DetailedGridInfo<SmolStr>`（每 grid 容器一份 Box）                                                      |
| **V-36** | `ClipChain` 定长 8：超过 8 层嵌套的 clip 链会被截断（点仍按前 8 层裁剪，但 `clip_chain` 不完整）                                               | 低   | 与 `MAX_CLIP_DEPTH` 同一上限（P10 shader 的固定数组），不是本阶段引入的限制；`Lane::push` 的 debug 断言先行暴露                                                              |
| **V-38** | `hit_test` 不再按布局盒剪枝（B2 的正确性要求）⇒ 最坏 O(n)（宽而深的树 + 大量溢出节点）                                                         | 低   | 指针事件频率低于帧率；P10 落地滚动/浮层后若基准显示热点，加「子树包围盒（含出流）」缓存或四叉树索引                                                                          |
| **V-37** | 增量正确性依赖「结构变更也清缓存」：`append_child`/`remove` 必须调 `mark_structure` → 清 `node..root`                                          | 中   | P03 的 `mark_structure` 已在这两处调用；P04 只需保证 `mark_structure` 内部接上 `invalidate_caches_upwards`（用例 15.4-10）                                                   |

---

## 20. 本阶段追加到 `design/DEVIATIONS.md` 的条目（§八）

**事实性偏差（F）**

| 编号 | 标题                                                                                                              |
|------|-------------------------------------------------------------------------------------------------------------------|
| F-45 | `HitResult` / `ClipChain` 的公开路径改为 `layout::hit_test::*`（P01 §4.5 写 `event::*`）                          |
| F-46 | `LayoutBox` 增加 `border: Edges`（P03 §11.3 只有 `bounds`）                                                       |
| F-47 | **不实现 `RoundTree` / `TraverseTree` / `PrintTree`**（v1 永不 `round_layout`；probe 已证明可实现）               |
| F-48 | `LayoutContainingBlock` 的 hoisted 列表**必须存储**（taffy 文档说只被 `round_layout` 消费，但引擎用它做绝对坐标） |
| F-49 | 根节点默认 `size: 100% × 100%`（`compute_root_layout` 不接受 `known_dimensions`，auto 根会塌成 0×0）              |
| F-50 | `MeasureFn` 的输入用 taffy `Size<Option<f32>>` / `Size<AvailableSpace>`，输出用引擎 `Size`                        |
| F-51 | `InvalidMeasure` 的实现 Phase 由 P05 改为 **P04**；`LayoutInvalid` 不落地                                         |
| F-52 | `calc()` 不解析（`resolve_calc_value` 走默认 `0.0`，与 taffy 自带 `TaffyTree` 一致）                              |

**补充决策（S）**

| 编号 | 标题                                                                                                                                           |
|------|------------------------------------------------------------------------------------------------------------------------------------------------|
| S-40 | `NodeTree::insert_child` 收 `NodeSpec`（私有字段 + `new`）：`field_scoped_visibility_modifiers` 禁止字段修饰（probe P9 实测）                  |
| S-41 | 布局入口 = `Window::compute_layout()`；`NodeTree::compute_layout` 是 `pub(crate)`，不新增 `App::layout`                                        |
| S-42 | taffy `Cache` 旁表 = `Vec<Option<Cache>>`（按 arena 槽位下标），槽位复用/卸载/新增三处清理                                                     |
| S-43 | `LAYOUT` 的唯一消费点是 `set_unrounded_layout`；`prev_layout` / damage 在绝对坐标 post-pass 更新                                               |
| S-44 | `Style.clip: Clip { None, Bounds }`：只裁子树、裁到 padding box、不读 taffy `overflow`                                                         |
| S-45 | `hit_test` = 逆绘制序 DFS + **clip 剪枝**（布局盒不剪枝，B2）；`Overlay` 无特殊分支（靠 `order` 末尾）；`ClipChain` 定长 8（不含目标自身，N3） |
| S-46 | `LayoutReport { laid_out, moved, cache_hits, cache_misses }` 的计数语义（§6.3）与实测基线                                                      |
| S-47 | 不建独立 measure 缓存；`invalidate_measure` 是唯一失效入口（P05 的字体栈/文本变更用）                                                          |
| S-48 | 每类型只能有一个 inherent `impl` 块（`multiple_inherent_impl`），trait impl 全在 `layout/taffy_tree.rs`                                        |
| S-49 | `LayoutOverride` 的 builder 副作用（强制 `position: Absolute`）+ `set_layout_override` 的写入口                                                |
| S-50 | `get_detailed_layout_info` / `set_detailed_grid_info` 走默认实现（grid 内的 absolute 按 padding box 定位）                                     |

**待复核（V）**：V-31…V-37（见 §19）。

---

## 附录 A · 实现者检查单（按执行顺序）

1. `src/layout.rs`：写 4 个契约类型 + 3 行 mod 声明；`cargo check`（先建空的 `taffy_tree.rs`/`measure.rs`/`hit_test.rs`）。
2. `src/paint/style.rs`：加 `Clip` + `Style.clip`；跑既有 `style.rs` 用例。
3. `src/node/tree.rs`：
   a. `LayoutBox.border`；`Node` 的 6 个新字段（含 `measure`/`hit_shape`/`layout_override`）；
   b. `NodeSpec`（私有字段 + `new`）+ `insert_child` 改签名；
   c. `NodeTree` 的 6 个新字段 + `LayoutStats`（字段私有 + `note_*`/`into_report`）；
   d. taffy 访问器（§8.5 的清单，逐个在 §4.3 表里能找到消费者）；
   e. `set_local_layout` / `absolutize` / `invalidate_caches_upwards` / `invalidate_layout` / **`set_style`（pub，返回
   `Result`）** / `invalidate_measure` / `set_layout_override` / `compute_layout` / `hit_test`；
   f. **手写 `Debug` 同步**（`measure`/`hit_shape` 打 `<closure>`，`layout_override`/`local`/`local_border`/
   `laid_out_epoch` 正常打印）；
   g. 根 `100%`；`LayoutBox::default()` 的边界用例。
4. `src/node/builder.rs`：3 个新方法 + `build` 装 `NodeSpec`；debug 断言扩到 `layout_override.is_some()`。
5. `src/node/dirty.rs`：`depends_on_children` 的 `layout_override` 短路（+ 用例）。
6. `src/layout/taffy_tree.rs`：7 个 impl → `dispatch_child_layout` → `run`。
7. `src/layout/measure.rs`：`compute_leaf` + 违规检测。
8. `src/layout/hit_test.rs`：`ClipChain` / `HitResult` / `hit_test`。
9. `src/window.rs`：`size` / `set_size` / `compute_layout`。
10. `src/error.rs`：`InvalidMeasure` + `all_variants()` 更新。
11. 逐文件补用例（§15），跑 §16 的 12 条门禁。
12. 更新 `design/DEVIATIONS.md`（§20 的 **26** 条：F-45…F-52（8）+ S-40…S-50（11）+ V-31…V-37（7））。

**每步的自检问题**：①这个 `pub(crate)` 的消费者在哪？②这个 `pub` 在 §4.1/§4.2 表里吗？③有没有第二个 `impl X { }`？④有没有引入
`round_layout`？

## 附录 B · 与上游文档的交叉引用

| 本文的结论                     | 上游出处                                                                        |
|--------------------------------|---------------------------------------------------------------------------------|
| 节点树即布局树（I5）           | 主设计 §0.2 / §4.1                                                              |
| 冒泡与终止条件                 | 主设计 §3.4；P03 §9.3（本文 §6.2 追加「缓存必须到根」）                         |
| 增量 / 舍入 / 命中测试         | 主设计 §4.4（本文 §7 / §12 具体化）                                             |
| measure 纯函数                 | 主设计 §4.3；本文 §9.3                                                          |
| `LayoutOverride`               | 主设计 §4.3 / §8.4（分栏拖拽）                                                  |
| 逻辑像素 / 不调 `round_layout` | P01 §7（F-17）；本文 §5.3 / §16-7                                               |
| taffy 0.15.0 事实              | P01 附录 A.1（本文 §2 补全 7 个 trait 的 assoc type、OOF 参照系、`Cache` 尺寸） |
| 公开路径 = 定义文件            | P01 §4.5；P03 §4.1（本文 §4.1 续）                                              |
| 模块根模板 / lint 证据         | P01 §4.1–§4.3（本文 §3 + probe P9）                                             |
| `EngineError` 冻结表           | P01 §6.2.2（#9 本阶段落地，#10 不落地）                                         |
| L1 测试清单                    | 主设计 §6.2（本文 §15 的对照表）                                                |
| 滚动 / 虚拟列表 / Overlay      | 主设计 §8.4（本文 §12.2 的 H4 + §11.2 的滚动偏移）                              |
