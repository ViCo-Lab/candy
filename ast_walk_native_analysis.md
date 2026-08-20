# `ast_walk.rs` 原生化（复用 Typst crate）可行性评估

## 背景与文件职责

`rust/src/parser/ast_walk.rs`（配合 `expr.rs`、`directives.rs`）是 `.tyx` 解析器的核心，其职责是在 **Typst 编译之前** 对源码做静态 AST 遍历，把标准 Typst 文档转成 Candy 私有的 `Scene` 动画时间线结构（`slides`、`scopes`、`counter_events`、`scene` 时序等）。渲染器随后把这个 `Scene` 切成逐帧，每帧用 `sys.inputs` 重写后再调用 `typst::compile` 重编译。

关键点：这个文件处理的不是"渲染 Typst"，而是"从 Typst 源码里提取 Candy 的动画编排语义"。后者是 Candy 自己发明的，Typst 完全没有对应概念。

## 核心判断

整个 `walk` 的**语义提取层**（slides/counters/scene 时序）必须手搓，Typst 无法替代。但"手搓"与"对 Typst 有误解"是两回事。下面逐项核对真正涉及 Typst 行为的部分，标注其原生化可行性。

## 逐项分析

| 实现点 | 当前做法 | Typst 原生替代 | 是否误解风险 | 可行性 |
| --- | --- | --- | --- | --- |
| 遍历 `typst_syntax::LinkedNode` | 递归 `walk` 匹配 `FuncCall`/`SetRule`/`ShowRule`/`LetBinding`/`ModuleInclude`/`CodeBlock` 等节点 | 已是 Typst 语法层 API（`typst_syntax`），本身即原生 | 无 | 已原生 |
| `is_valid_typst_ident` | 委托 `typst_syntax::is_ident` | 已正确复用（与 Typst lexer 同源） | 无 | 已原生（示范） |
| `unit_to_cm` 长度换算 | 手搓系数 `cm=1, mm*0.1, pt/PT_PER_CM, in*2.54` | 复用 `typst_library::layout::{Abs, Length}` 的 `to_pt()` | 核对系数无误（`PT_PER_CM=28.346456…` 即 `72/2.54`，与 Typst 一致） | 可原生，低风险 |
| `extract_page_size` 从 `set page` 取宽高 | 只接受 `Expr::Numeric` 且单位已知（cm/mm/pt/in） | 用 Typst 的 page 字段解析或真实 eval `set page` 的 args | **有**：会静默漏掉 `auto`/`50%`/`1fr`/`calc(...)`，且正值假设会丢弃负号宽度 | 建议原生，中高风险 |
| `call_symbol` import/别名解析 | 手搓 `#import "candy": *`、`as X`、`_`↔`-` 归一化的符号表 | Typst 真正的 name resolution 在编译期 evaluator 内，`typst_syntax` 的 `LinkedNode` 不提供"ident 来自哪个 import" | 中：覆盖常见 case，但边缘 import 形态可能误判 | 难原生（破坏"编译前提取"架构） |
| `expr_to_*` 常量求值（f64/bool/angle/ratio） | 手搓递归 AST 求值，含 `Unary(Neg/Pos)` 包裹处理 | `typst::eval` 对纯字面量求值（需最小 world） | 低：注释已说明为何绕过编译期校验 | 可评估，高改动成本 |

## 结论

1. **不能整体用 Typst 原生替换**：`ast_walk.rs` 提取的动画时间线是 Candy 的领域创新，Typst 没有任何 API 提供"第 500ms 显示什么"。用 `typst::compile` 反而无法在编译前拿到时间线，会破坏"逐帧 text-splice 重编译"的渲染架构。

2. **已实现的部分是对的**：遍历用 `typst_syntax`、`is_valid_typst_ident` 委托 `typst_syntax::is_ident`、长度换算系数与 Typst 完全一致——这些不是"误解 Typst"，是可保留的正确原生用法。

3. **真正值得改的"手搓/可能误解"点**：
   - **A（高收益）**：`extract_page_size` 的 `set page` 尺寸提取只认已知长度单位。应复用 Typst 的 `Page` 解析路径或真实 eval，至少覆盖 `auto`/`fr`/`%`/`calc`，并去掉"宽高必须为正"的硬编码假设（Typst 允许负向/相对写法）。这是当前最可能导致"与 Typst 行为不符"的bug。
   - **B（中收益）**：`unit_to_cm` 改为调用 `typst_library::layout::Abs::cm(val).to_pt()` 等原生转换，消除"手搓系数未来与 Typst 脱节"的维护风险。
   - **C（谨慎）**：`call_symbol` 的 import 解析若要做到 100% 正确，需要引入编译期 name resolution，但这与"编译前静态提取"的架构冲突。建议保持手搓但补单测覆盖各种 import 形态，而非强行原生化。

4. **不建议原生化的**：`expr_to_f64`/`expr_to_angle`/`expr_to_ratio` 等常量求值。它们刻意绕过编译期校验以支持 legacy `.tyx`，且当前覆盖已够。改走 `typst::eval` 需构造最小 world、引入编译路径，成本高风险低收益。

## 重构建议清单（按优先级）

1. `extract_page_size`：替换手搓 named-arg 提取，改为解析/复用 Typst 的 `page` 设置字段，覆盖非数值单位与相对写法。
2. `unit_to_cm`/`expr_length_cm`/`expr_to_f64`：长度/角度归一到 `typst_library::layout` 的类型方法。
3. 保持：`walk` 遍历骨架、`call_symbol` 静态符号表、`expr_to_*` 常量求值（补测试即可）。

## 限制

本评估基于静态代码阅读，未运行现有测试。建议下一步用 `examples/` 与 `rust/src/parser/ast_walk.rs` 末尾的单测（如 `extract_page_size` 的 `20cm×10cm` 断言）验证 A/B 项改动不回归。

## 参考

1. [typst - crates.io](https://crates.io/crates/typst) — `typst::compile` / `World` / `Library` 编译 API
2. [typst-syntax - crates.io](https://crates.io/crates/typst-syntax) — `LinkedNode` / `ast` / `is_ident` 语法层 API
3. [typst-library - crates.io](https://crates.io/crates/typst-library) — `layout::{Abs, Length, Ratio}` 单位类型与 `to_pt()`
