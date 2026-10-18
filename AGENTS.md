# AGENTS.md

## 1. 核心原则

### 1.1 不维护过时设计

* 不保留向后兼容。
* 过时的 API、代码、配置和架构直接删除。
* 不添加兼容层、migration、fallback 或过渡实现。
* 不为了避免重构而继续堆叠在已经不适合当前需求的架构上。

### 1.2 保持简单

* 优先选择能够直接满足当前需求的最简单实现。
* 不做预防性抽象，不增加当前没有实际需求的配置层、扩展点或间接层。
* 严禁为了“未来可能需要”而增加复杂度。
* 遇到现有架构无法合理满足需求时，优先重构，而不是继续堆叠补丁。

### 1.3 保持模块化

* 保持合理的模块边界和关注点分离。
* 一个模块只负责清晰、相关的一组职责。
* 避免出现包含数千甚至上万行代码的巨型文件。
* 当文件过大时，按功能拆分模块，并让生产代码与测试代码保持清晰的模块结构，但是严禁拆分多个impl到不同的文件中。
* 不使用 `#[path = "..."]` 引入模块，使用 Rust 标准模块布局。

### 1.4 优先复用成熟方案

* 优先使用项目中已有的依赖和能力。
* 在添加新依赖或自行实现之前，先检查现有依赖是否已经能够满足需求。
* 优先使用成熟且持续维护的库；除非有明确理由，否则不要重复实现已有能力。
* 在进行架构设计或技术选型前，优先查阅官方文档和成熟项目的实际实现。
* 不基于猜测设计 API、库行为或框架机制；缺乏事实依据时先查证。

### 1.5 架构决策一次做到位

* 架构设计应面向长期维护。
* 不接受“先这样，以后再换”的临时架构。
* 不为了降低当前开发成本而引入明确需要后续重构的设计。

---

## 2. Rust 数据结构与序列化

### 2.1 Serde

生产代码中：

* 禁止使用 `serde_json::Value`。
* 禁止使用 `serde_json::RawValue`。
* 禁止使用 `serde_json::json!`。
* JSON 序列化和反序列化必须使用强类型结构体或枚举。
* 一次性的 JSON 数据交换也必须定义明确的 DTO，不使用动态 JSON 结构。
* 禁止使用 `#[serde(deny_unknown_fields)]`。
* 禁止使用 `serde_ignored`。
* 禁止使用 `#[serde(rename_all = "...")]`。
* 字段名称转换必须显式写在具体字段上，例如：
  `#[serde(rename = "someField")]`。
* 生产代码中的数据结构不得定义未使用的字段。
* 不得通过增加无实际用途的字段来“兼容”未知数据。

以下情况可以例外：

* 测试代码可以使用动态 JSON API。
* Dicebear 相关代码可以使用动态 JSON API。
* GPUI Actions 可以使用动态 JSON API。

---

### 2.2 SmolStr

生产代码中：

* 短字符串优先使用`SmolStr`替代`String`。
* 使用`SmolStr`时，如果需要格式化拼接字符串，应该使用`format_smolstr!`宏。严禁使用`format!`构造为`String`后使用
  `SmolStr::from`或者`String::into`的方式转换到`SmolStr`。
* 通过短静态字符串构造`SmolStr`时，应该使用`SmolStr::new_static("static_string")`构造。严禁使用`String`作为转换中介。
* 对于可以预期的较短的文本的在进行数据序列化与反序列化时，优先考虑使用`SmolStr`。

---

### 2.3 集合类型

* `HashMap`、`HashSet` 等哈希容器优先使用 `ahash` 提供的实现，或显式使用 AHash 的 `RandomState`。
* 初始化 `HashMap`、`HashSet`、`Vec` 等集合时，根据已知上下文尽可能提供合理的初始容量。
* 处理集合时，优先使用 Rust 标准库提供的迭代器方法。
* 不要为了简单的集合操作编写不必要的手动循环或辅助抽象。

---

### 2.4 字节数据

* `Vec<u8>` 在适合作为不可变字节缓冲区时，应优先使用 `bytes::Bytes`。
* 需要可变字节缓冲区时，应优先使用 `bytes::BytesMut`。
* 只有在确实需要 `Vec<u8>` 的所有权语义或特定 API 兼容性时才使用 `Vec<u8>`。

---

## 3. 错误处理

* 优先使用强类型错误，而不是字符串错误或无结构的通用错误。
* 错误类型应表达明确的业务语义。
* 应根据错误来源和处理策略进行分类，例如：

    * 网络错误
    * 业务错误
    * 数据解析错误
    * 配置错误
    * 权限错误
* 上层调用者应能够根据错误类型决定具体处理策略。
* GPUI 上下文尤其必须使用强类型错误，以便：

    * 向用户提供准确的错误信息；
    * 支持多语言；
    * 根据错误类型执行不同的 UI 或业务处理。

---

## 4. Warning 与无用代码

* 禁止在生产代码中使用 `#[expect(...)]` 绕过 Warning。
* Warning 应通过正确的代码重构解决，而不是隐藏。
* 禁止在生产代码中使用 `#[allow(dead_code)]`。
* 不再使用的代码、类型、函数、字段和依赖应直接删除。
* 不通过 `#[allow(...)]`、`#[expect(...)]` 等属性隐藏本应解决的问题。

Dicebear 相关代码可以例外。

---

## 5. 测试组织

遵循 Rust 官方对 unit test 和 integration test 的职责划分。

### 5.1 Unit Tests

* Unit test 放在 `src/` 中，并与被测试的生产代码保持在同一模块内。
* 使用 `#[cfg(test)] mod tests` 组织 unit test。
* Unit test 可以访问被测试模块的私有字段、私有函数和内部实现。
* 测试单个模块或内部实现时，使用 unit test。
* 测试代码较多时，可以按照项目统一的模块布局拆分为多个文件，但生产代码和对应测试仍应保持在同一模块边界内。
* 不创建 crate 级别和 module 级别的独立 `tests.rs` 集中存放 unit test。
* 如果确实有需要在多个测试之间共享的基础设置，则在同一模块下引入 `test_support.rs`。

### 5.2 Integration Tests

* Integration test 放在 crate 根目录的 `tests/` 下。
* `tests/` 下的每个顶层 `.rs` 文件都是独立的 integration-test target。
* Integration test 只能通过 crate 的公开 API 访问被测试代码。
* Integration test 不使用 `#[cfg(test)]`。
* 当测试多个模块之间的协作、完整功能流程或 crate 的公共 API 时，使用 integration test。
* 不要为了访问私有实现而将 unit test 改成 integration test。

### 5.3 测试代码组织

* 禁止使用 `#[path = "..."]` 引入测试模块。
* Integration test 应保持独立和清晰，避免为了少量共享 helper 引入额外抽象。
* 确实需要跨多个 integration test 共享复杂测试基础设施时，应采用符合项目模块规范的明确测试支持方案。

### 5.4 Binary Crate

* 如果 crate 是 binary crate，应将主要业务逻辑放入 `src/` 下独立的模块中。
* `src/main.rs` 尽量只负责程序启动、参数解析和顶层组装。
* 如果需要通过 integration test 测试业务逻辑，可以提供 `src/lib.rs` 作为 library crate 的入口，但 `lib.rs` 只负责模块声明和公开
  API 导出，不直接实现业务逻辑。
* 不要将大量业务逻辑直接堆在 `src/main.rs`，也不要为了 integration test 而将业务逻辑堆积到 `src/lib.rs`。

### 5.5 Library Crate

* `src/lib.rs` 只负责统一管理 crate 的模块声明和导入。
* `src/lib.rs` 不编写生产代码。
* `src/lib.rs` 不编写测试代码。
* 不在 `lib.rs` 中定义结构体、枚举、trait、函数、常量、类型别名或其他业务实现。
* `lib.rs` 中仅允许存在必要的 `mod`、`pub mod` 两种模块组织代码，具体实现必须放在对应的模块文件中。
* Unit test 必须放在实际被测试的模块中，而不是 `lib.rs`。
* 如果 `lib.rs` 中的模块声明较多，也不要为了减少 `lib.rs` 内容而增加额外的聚合层；保持 `lib.rs` 作为 crate 的模块入口即可。
* 对于 binary crate，即使需要提供 integration test 使用的 library crate，也必须遵守本规则；业务逻辑应放入 `src/` 下独立的模块中，由
  `lib.rs` 统一导出。

例如：

```text
src/
├── lib.rs
├── main.rs
├── config.rs
├── service.rs
├── database.rs
├── trading.rs
└── trading/
    ├── order.rs
    └── position.rs
```

其中 `lib.rs` 只负责模块组织：

```rust
mod config;
mod database;
mod service;
pub mod trading;
```

而不应该直接在 `lib.rs` 中实现 `Service`、测试函数或其他业务逻辑。


---

## 6. 依赖管理

* 所有依赖必须在 workspace 根目录的 `Cargo.toml` 中声明。
* 各个 crate 通过 workspace dependency 使用这些依赖。
* 不允许在子 crate 的 `Cargo.toml` 中重复独立声明 workspace 已管理的依赖版本。
* 添加新依赖前，必须先检查 workspace 中已有依赖是否能够满足需求。
* 没有明确理由不要引入新的 crate。

---

## 7. 小型函数与无意义抽象

* 禁止为了“封装”而封装超小型函数。
* 特别禁止创建只返回一个或多个变量、常量或简单表达式的函数。
* 如果一个操作只在一个地方使用，且抽取函数不会明显改善可读性、职责边界或测试性，直接保留在调用位置。
* 抽象必须解决实际的复杂度、复用或模块边界问题，而不是单纯增加间接层。

---

## 8. 实现前检查清单

在实现新的功能或修改架构之前：

1. 检查当前项目已有的模块、依赖和实现。
2. 检查已有依赖是否已经提供所需能力。
3. 查阅相关库或框架的官方文档。
4. 优先参考成熟项目已经验证的实现模式。
5. 选择能够满足当前需求的最简单设计。
6. 如果当前架构已经不合理，直接重构，不增加临时兼容层。
7. 确保新增代码符合本文件中的模块、错误处理、数据结构和测试组织规则。
8. 删除因重构而不再使用的代码、依赖和抽象。

---

## 9. 验收

1. `cargo test` 通过
2. `cargo +nightly clippy -- -D warnings` 通过
3. `cargo +nightly fmt` 格式化