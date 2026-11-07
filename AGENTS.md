# AGENTS.md: FUGUE 核心架构公理与工程开发宪章

`PROTOCOL_VERSION`: `2.4.0-STRICT`  
`CORE_ECOSYSTEM`: `Rust (Edition 2021+)`  
`SUPREME_DIRECTIVE`: `极致性能 + 轻量 (Zero-Cost Abstraction & Minimal Overhead)`

---

## 1. 核心架构公理 (Architectural Axioms)

任何参与本项目开发的人类工程师或 AI Agent 必须无条件遵守以下 6 项公理，不得违背：

### AXIOM[01]: 动态复杂度自适应 (Dynamic Range Invariance)
- **L0 任务（单行微调/一句话问答）**：编排开销必须为 0，单实例直通，毫秒级响应，物理上严禁引入中间管理节点。
- **L1/L2 任务（多阶段复杂工程）**：拓扑结构动态展开为网格（Dynamic Mesh），实例依托对等分身（Avatars）在共享内存中协作，严禁单线程死锁阻塞。

### AXIOM[02]: 唯一对外门面垄断 (Sole User-Facing Monopoly)
- 面向外部用户交互的唯一实体必须且仅能是 **Omni**（`Entities(ExternalInterface) ≡ { Omni }`）。
- 内部所有任务节点（TaskNode）与执行工作单元（ExecutionWorker）对用户完全不可见，其输出只能定向汇聚至节点活体记忆（NodeMemory）。

### AXIOM[03]: Omni 物理工具纯洁性 (Omni Tool Purity)
- **工具清单唯一约束**：Omni 的物理可用工具仅有且必须仅有 `new_task`。
- **零直接副作用**：Omni 物理禁止挂载文件读写、Shell 执行或网络抓取工具；Omni 的本质是**用户意图编译与沙箱赋权工厂**。

### AXIOM[04]: 节点即对等集群 (Node as Peer Cluster)
- 任何节点（OmniNode / TaskNode）皆非孤立单体进程，而是**对等实例集群（Peer Instances） ⊗ 活体节点记忆（LivingNodeMemory）**。
- 集群内所有对等实例无从属层级，具有严格平等的地位，统一通过 CAS 乐观锁原子读写活体节点记忆。

### AXIOM[05]: 白板即用即毁 (Ephemeral Blank Slate)
- 实例启动时遵循完全白板原则：零写死预制提示词、零预烘焙上下文，全部上下文均由上游调用者注水（Injected by Upstream）或从活体记忆切片拉取。
- 实例生命周期：完成单次运行闭环即刻销毁进程并 100% 释放资源，连续性完全由活体记忆维护。

### AXIOM[06]: 任务节点思执并发 (Concurrent Avatars)
- TaskNode 内部打破传统 IDE/Agent 的串行等待壁垒：
  - **思维分身（Planning Avatar）**：负责推测推理、活体记忆看护与产物门禁。
  - **执行分身（Action Avatar）**：负责沙箱内的原子执行。
- 两者生命周期与时间轴并发重叠，执行分身通过结构化 `SEMANTIC_ACTION_v2` 遥测日志自底向上驱动与唤醒思维分身自检。

---

## 2. 严禁事项与架构铁律 (Critical Prohibitions)

> [!CAUTION]
> 1. **严禁复辟中心化调度器**：绝对禁止编写带有 `while True`、`loop { std::thread::sleep(...) }` 的中心常驻轮询/守护调度线程。必须严格遵循“调用即触发、上游送入上下文、执行即销毁”的事件与调用驱动范式。
> 2. **严禁扩展 Omni 的物理工具集**：任何试图给 Omni 挂载文件编辑、命令执行等实装工具的代码提交均属于违背公理，在 CI / 单元测试中必须直接判定失败。
> 3. **保持 Mermaid 纯净**：所有架构图严禁使用 `<===>`、严禁直接连线至 `subgraph` ID、严禁使用 `<b>` 或 `<br/>` 等 HTML 标签，换行必须使用标准 `\n`。

---

## 3. Rust 生态技术规范与极致性能军规

本项目首选语言生态为 **Rust**，一切设计围绕**极致性能与超轻量**展开：

### 3.1 内存与零成本抽象
- **优先栈分配与切片引用**：在上下文流转中尽量使用 `&str`、`Cow<'_, str>` 或 `bytes::Bytes`，杜绝不必要的堆内存拷贝与克隆。
- **并发控制**：在活体节点记忆中，采用无锁原子量或轻量 CAS（如 `std::sync::atomic` / `parking_lot::RwLock`），杜绝粗粒度重型全局锁。
- **严控外部依赖膨胀**：禁止为了简单功能引入重型第三方库。任何依赖必须评估其编译时间与最终二进制大小贡献。

### 3.2 强类型编译期契约 (Type State Pattern)
- 利用 Rust 丰富的类型系统在编译期表达状态机（如任务流转 `Unclaimed -> Leased -> Running -> Review -> Committed`）。
- 非法状态流转必须在编译期报错，而不是在运行时抛异常。

### 3.3 异步与事件驱动
- 使用轻量异步通道（如 `tokio::sync::mpsc` / `tokio::sync::broadcast`）进行事件分发。
- 绝不开启空转轮询，事件监听必须基于 `async`/`await` 就绪通知。

---

## 4. Cargo Workspace 物理边界分工

```
Fugue/
├── Cargo.toml                  # Workspace 根配置与优化 Profile
├── AGENTS.md                   # 本架构宪章
├── crates/
│   ├── fugue-core/             # 领域核心模型、协议 Schema (SEMANTIC_ACTION_v2)、错误定义
│   ├── fugue-kernel/           # LivingNodeMemory 活体记忆、CAS 租约锁、事件广播总线
│   ├── fugue-omni/             # Omni 白板运行时、new_task 元工具校验与上下文切片注水器
│   └── fugue-ui/               # 基于 Slint 的原生声明式 GUI，极致轻量交互界面
```

### 依赖单向拓扑：
- `fugue-core`：基础层，零内部依赖。
- `fugue-kernel`：依赖 `fugue-core`。
- `fugue-omni`：依赖 `fugue-core`、`fugue-kernel`。
- `fugue-ui`：依赖 `fugue-core`（可挂载 `fugue-omni`/`fugue-kernel` 用于端到端交互展示）。

---

## 5. 提交前自检清单 (Pre-flight Checklist)

- [ ] `cargo check --workspace` 编译通过。
- [ ] `cargo test --workspace` 单元测试全部通过。
- [ ] `cargo clippy --workspace -- -D warnings` 零警告。
- [ ] 确认 Omni 仅暴露 `new_task`。
- [ ] 确认未引入任何常驻中心化调度循环。

---

## 6. 代码索引地图 (Code Index Map)

> 用途：新会话先读本节，按符号名/路径直接定位，避免全仓扫描。
> 行号为仓库当前快照（2026-11），漂移时以符号名 Grep 为准，不要依赖行号硬编码。

### 6.1 启动与构建入口

| 操作 | 位置 / 命令 |
|---|---|
| 启动整个软件（唯一可执行入口） | 仓库根执行 `cargo run`，即 `crates/fugue-ui` |
| 默认成员（使根目录 `cargo run` 生效） | [Cargo.toml](Cargo.toml) L8 `default-members = ["crates/fugue-ui"]` |
| Workspace 成员与 release Profile | [Cargo.toml](Cargo.toml) L1-L9 / L17-L22（lto、panic=abort、strip） |
| Slint 编译钩子 | [build.rs](crates/fugue-ui/build.rs) L1-L3 `slint_build::compile("ui/appwindow.slint")` |
| Slint 宏展开与进程入口 | [main.rs](crates/fugue-ui/src/main.rs) L4 `slint::include_modules!()` / L6-L17 `MainWindow::new()` + 语言初始化 + `run()` |

### 6.2 模块符号索引

**`fugue-core`（领域模型，零内部依赖）— [src/lib.rs](crates/fugue-core/src/lib.rs)**

| 符号 | 行 | 职责 |
|---|---|---|
| `TaskComplexity` (L0/L1/L2) | L8-L16 | AXIOM[01] 复杂度分层 |
| `TaskStatus` (Unclaimed→…→Committed/Failed) | L18-L27 | 任务生命周期状态机 |
| `ActionCategory` | L29-L35 | SEMANTIC_ACTION_v2 动作类别 |
| `SemanticActionFeedback` | L37-L46 | SEMANTIC_ACTION_v2 遥测结构体 |
| `FugueError` | L49-L65 | 全局错误枚举（`CasConflict`/`ForbiddenTool` 等） |
| `Result<T>` 别名 | L67 | 统一返回类型 |
| `UuidV7` | L77-L195 | RFC 9562 无锁严格单调递增 128-bit 会话与消息唯一标识符 |
| `SessionMetadata` / `StoredMessageRecord` | L218-L265 | 会话与消息持久化元数据结构 |
| `ClientControlCommand` / `client_control_tool_schemas` | L321-L618 | 9 项客户端预留大模型控制接口强类型协议与 OpenAI/DeepSeek Tool JSON Schema |

**`fugue-kernel`（活体记忆、CAS 租约与多会话沙箱存储）— [src/lib.rs](crates/fugue-kernel/src/lib.rs) & [src/session_store.rs](crates/fugue-kernel/src/session_store.rs)**

| 符号 | 行 | 职责 |
|---|---|---|
| `MemoryEvent` | L14-L20 | 无轮询事件广播（`broadcast` 通道） |
| `LeasedTask` | L22-L30 | TaskBoard 中的任务记录（lease_owner / version） |
| `LivingNodeMemory` 结构体 | L33-L47 | `intent_store` / `task_board` / `symbol_registry` / `telemetry_log` / `version_counter` / `event_tx` |
| `LivingNodeMemory::new` | L55-L66 | 初始化各分区与广播通道（容量 1024） |
| `subscribe` | L68-L71 | 获取事件接收端 |
| `add_task` | L73-L85 | 建任务并广播 `TaskCreated` |
| `acquire_lease` | L87-L120 | CAS 乐观租约；冲突返回 `FugueError::CasConflict` |
| `append_telemetry` | L122-L127 | 追加遥测并广播 `TelemetryAppended` |
| `SharedMemory` | L130 | `Arc<LivingNodeMemory>` 共享句柄 |
| `resolve_user_fugue_root` | [session_store.rs](crates/fugue-kernel/src/session_store.rs) L197-L217 | 解析唯一用户可变数据根目录 `C:\Users\<Username>\.fugue` |
| `RawMessageIndexEntry` / `NgramSignature` | 同上 L44-L194 | 32B 定长消息偏移索引与 64B Cache-Line 对齐 512-bit N-Gram 位图签名 |
| `SessionSandboxStore` | 同上 L498-L1450 | 独立文件系统沙箱管理、`memmap2` 零拷贝读写、100% 独立会话克隆、双层 SIMD 搜索与 ID 查阅 |

**`fugue-omni`（唯一对外门面 + new_task）— [src/lib.rs](crates/fugue-omni/src/lib.rs)**

| 符号 | 行 | 职责 |
|---|---|---|
| `OMNI_SYSTEM_PROMPT` | [system_prompt.rs](crates/fugue-omni/src/system_prompt.rs) | 对标 Claude Haiku 5.5 风格的权威系统提示词规约 |
| `AgentRole` | L9-L13 | TaskAgent / ExecutionAgent |
| `PromptSpec` | L15-L20 | 白板注水的提示词切片描述 |
| `SandboxConfig` | L22-L27 | 工作区根、只读源、产物目录 |
| `PermissionScope` | L29-L34 | token 预算、子派发开关、超时 |
| `NewTaskArgs` | L36-L45 | `new_task` 完整参数（六个字段） |
| `OmniToolManifest` | L47-L77 | 工具白名单；`new` L53-L58、`register_tool` L60-L72（非 `new_task` 直接拒绝）、`tools` L74-L76 |
| `OmniNode` | L79-L83 | 门面运行时（manifest + SharedMemory） |
| `OmniNode::new` | L85-L91 | 构造 |
| `compile_and_dispatch` | L93-L102 | 当前为最小实现：写入 TaskBoard + IntentStore |

**`fugue-ui`（Slint GUI）**

| 位置 | 行 | 职责 |
|---|---|---|
| [ui/appwindow.slint](crates/fugue-ui/ui/appwindow.slint) `ThemeMode` / `BlockType` | L4-L28 | 主题模式枚举与 16 种块级渲染类型枚举 |
| 同上 `AppState` | L95-L133 | 全局状态：主题、语言、Fenwick 视口上下 Spacer、实测高度回写回调、块选区与单例右键菜单回调 |
| 同上 `Theme` | L136-L198 | 分层配色：内容区 `#FAF9F6`/`#1E1F22`、选区高亮、行内代码胶囊、引用导轨、表格斑马纹配色 |
| 同上 `RichLineView` / `RichParagraphView` | L514-L579 | 行内富文本渲染（`**粗体**`、`_斜体_`、`` `行内代码` ``、`~~删除线~~`、`$行内公式$`、可点击链接与图片胶囊） |
| 同上 `BlockquoteView` / `ListBlockView` / `TableBlockView` / `DiagramBlockView` | L581-L1007 | 引用块、任务复选框列表、GFM 对齐表格（含 MD/TSV 复制栏）、Mermaid 流程图矢量卡片 |
| 同上 `CodeBlockView` | L1009-L1185 | 代码块视图（行号列 + 自动折行开关 + 横向滚动条 + 跨行状态高亮 + 4 空格 Tab 对齐） |
| 同上 `SingletonContextMenu` / `SingletonSessionContextMenu` | L1330-L1535 | 顶层全局唯一静态消息右键菜单与会话右键菜单（O(1) 弹出、置顶/重命名/克隆/归档/删除） |
| 同上 `SessionSidebar` | L1910-L2340 | 可折叠多会话沙箱侧边栏（`Ctrl+B`、`Ctrl+N`、实时 SIMD 搜索命中列表、活跃/归档切换、内联重命名） |
| 同上 `MainWindow` | L2510-L3163 | `chat-flick`（Lenis 平滑滚动、智能粘性贴底、边缘自动滚屏选区、实测高度回传）、Fenwick 可视区挂载 |
| [src/smooth_scroll.rs](crates/fugue-ui/src/smooth_scroll.rs) | L1-L275 | `LenisScrollManager`：全局 `duration=1.2, lerp=0.1` 帧率无关指数阻尼平滑滚动物理引擎 |
| [src/client_control.rs](crates/fugue-ui/src/client_control.rs) | L1-L273 | `ClientControlGateway`：9 项大模型预留客户端控制接口路由器与 UI 同步钩子 |
| [src/viewport_culler.rs](crates/fugue-ui/src/viewport_culler.rs) | L1-L306 | `FenwickViewportIndex`：树状数组 + 树上倍增二分查找（$O(\log N)$ 计算可视块区间与实测高度原位更新） |
| [src/markdown_parser.rs](crates/fugue-ui/src/markdown_parser.rs) | L1-L1425 | 零回溯块分类器、单遍 $O(L)$ 行内分词器、词边界折行、GFM 列对齐表格/TSV 转换器、Mermaid 解析器 |
| [src/stream_automaton.rs](crates/fugue-ui/src/stream_automaton.rs) | L1-L1407 | 前缀单调增量行扫描状态机（零拷贝 `advance`、嵌套围栏、64KB 围栏上下文保持、级联拆卸） |
| [src/highlighter.rs](crates/fugue-ui/src/highlighter.rs) | L1-L406 | 跨行状态（块注释/三引号/模板字符串）+ 4 空格 Tab 对齐展开 + 连续同色 Token 合并高亮分词器 |
| [src/latex_renderer.rs](crates/fugue-ui/src/latex_renderer.rs) | L1-L390 | `OnceLock<MathFont>` 字体单例 + `Arc<[u8]>` 零拷贝上限缓存 + 复杂行内公式 PNG 渲染 + 递归花括号 Unicode 降级 |
| [src/main.rs](crates/fugue-ui/src/main.rs) | L1-L2429 | 多会话沙箱持久化接线、Lenis 按需帧定时器、16ms 流式批处理器、可视区惰性编译与剪贴板序列化 |
| [Cargo.toml](crates/fugue-ui/Cargo.toml) | L8-L35 | 依赖 core/kernel/omni + slint / sys-locale + slint-build |

### 6.3 按修改意图定位（高频场景）

| 我要改… | 直接去 |
|---|---|
| 任务状态、复杂度分层、遥测协议字段 | [fugue-core/src/lib.rs](crates/fugue-core/src/lib.rs) 对应 enum/struct |
| 新增错误类型或错误文案 | `FugueError`（core L49-L65） |
| 记忆分区、租约时长/冲突逻辑、事件类型 | [fugue-kernel/src/lib.rs](crates/fugue-kernel/src/lib.rs) `LivingNodeMemory` 及 `MemoryEvent` |
| `new_task` 入参结构 | `NewTaskArgs`（omni L36-L45） |
| 工具白名单/防越权校验 | `OmniToolManifest`（omni L47-L77，铁律：仅 `new_task`） |
| 任务派发最小闭环（写记忆/写意图） | `compile_and_dispatch`（omni L93-L102） |
| 窗口标题、尺寸、背景、主题色 | [ui/appwindow.slint](crates/fugue-ui/ui/appwindow.slint) `MainWindow` / `Theme` |
| 标题栏布局、拖拽区域、窗口控制按钮 | 同上 `TitleBar`，拖拽由 `WindowMoveArea`（L185）原生处理 |
| 设置面板尺寸、展开/收起动效、遮罩 | 同上 `SettingsPanel` / `MainWindow` 中的遮罩与 `key-handler`（ESC） |
| 语言切换文案、主题模式选项 | 同上 `AppState` / `OptionChip` 实例 |
| 主界面 AI 输入框样式、占位文案、尺寸 | 同上 `AiInputBox` 及 `MainWindow` 内 L502 实例（宽 1/2、高 1/5、底部居中，底距 12px） |
| AI 输入框聚焦/滚动/光标跟随 | 同上 `AiInputBox`：`input.has-focus`、`Flickable.content-y`、`cursor-position-changed` |
| Rust 与 Slint 接线、启动流程 | [ui/src/main.rs](crates/fugue-ui/src/main.rs) |

### 6.4 依赖与版本事实

- `crates/fugue-ui/Cargo.toml` 写 `slint = "1.8"`，按语义版本解析到 `1.18.1`（见 [Cargo.lock](Cargo.lock)）。
- Slint 默认 widget style 为 `fluent`（build.rs 未调用 `with_style`）；`Palette` 从 `"std-widgets.slint"` 导入，编译器仅加载 `style-base.slint`，不会引入完整控件库。
- `sys-locale = "0.3"` 为 `fugue-ui` 直接依赖（0.3.2，原本已随 Slint 传递锁定），仅用于系统语言检测。
- 根 [Cargo.toml](Cargo.toml) L24-L26 为 `[profile.dev.package."*"] opt-level = 2`：开发构建仅优化依赖，避免 Slint 在 debug 下的渲染/文本布局卡顿；首次构建会重编依赖。
- 依赖拓扑唯一方向：`core ← kernel ← omni`，`ui` 依赖三者；禁止反向依赖。

### 6.5 测试分布与已知空缺

| 模块 | 测试位置 |
|---|---|
| core | [lib.rs](crates/fugue-core/src/lib.rs) L69-L87（遥测序列化往返） |
| kernel | [lib.rs](crates/fugue-kernel/src/lib.rs) L132-L182（CAS 冲突 + 事件广播） |
| omni | [lib.rs](crates/fugue-omni/src/lib.rs) L104-L131（AXIOM[03] 工具纯洁性） |
| ui | [stream_automaton.rs](crates/fugue-ui/src/stream_automaton.rs), [markdown_parser.rs](crates/fugue-ui/src/markdown_parser.rs), [viewport_culler.rs](crates/fugue-ui/src/viewport_culler.rs), [latex_renderer.rs](crates/fugue-ui/src/latex_renderer.rs), [highlighter.rs](crates/fugue-ui/src/highlighter.rs)（共 24 项单测） |

已知空缺（未来开发优先关注）：
1. `LivingNodeMemory` 尚无 `Running/Review/Committed` 状态流转方法，仅有 `add_task` / `acquire_lease` / `append_telemetry`。
2. `.env`（仓库根，勿提交）仅含 `DEEPSEEK_API_KEY`、`DEEPSEEK_BASE_URL`；读取密钥时不得回显或写入文档。
3. `target/` 为构建产物，禁止手改。
