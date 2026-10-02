# 代码解耦实施文档

本文档描述 `deluxe-agent` 当前代码如何分阶段解耦。

这项工作是 Wasmtime 插件化之前的基础工程。当前仓库已经有比较清晰的功能模块，
但模块之间大量传递具体实现类型，导致 Agent、GUI、IPC、工具、插件和持久化相互
牵制。若直接在现状上加入 Wasmtime，插件运行时会被迫知道 GUI 事件、工具注册表、
具体 LLM client 和 session 文件格式，最终只会把现有耦合搬到新的抽象层下面。

本计划的原则是：

- 先稳定领域数据和调用方向，再引入动态插件；
- 宿主内部可以保留文件、进程和 HTTP 的 native capability implementation，但插件
  本体统一是 Wasmtime Component，所有插件可执行能力都以 Component 为边界；
- 一个阶段只改变一类依赖；
- 每个阶段保持现有行为、prompt 和工具协议不变；
- 不为了抽象而抽象，只有跨模块变化点才建立 trait；
- 所有 I/O 继续运行在 tokio worker，不把业务操作放回 egui 主线程。

本项目的插件格式唯一固定为 Wasmtime Component，不兼容旧的可执行插件格式。
每个插件都必须声明 Wasmtime Component；Component 可以同时提供 skills、commands、agents、tool
和声明式 UI。Hooks 与 MCP 的实现全部位于各自的 Wasmtime Component 中。Component
需要配置时，通过通用 `read-plugin-file` 请求宿主读取当前 global/project
configuration root 下的 `.hooks.json` 或 `.mcp.json`；宿主不在 discovery 阶段
读取或解析这些文件。project configuration root 是当前 project root；global
configuration root 是插件配置目录 `~/.deluxe-agents`，**不是整个 home 目录**——把全局
root 绑到 home 会把 `.ssh`、`.aws`、API key 等与插件无关的文件一并暴露给通用
读取能力。configuration root 也不是 `plugin.wasm` 所在的插件包目录。

相关目标设计见：

- `docs/wasmtime-plugin-implementation.md`

## 1. 当前耦合问题

### 1.1 `Agent` 依赖所有具体机制

`src/agent.rs` 中的 `Agent` 同时负责：

- 组织 system prompt；
- 读取项目上下文；
- 调用具体的 `LlmClient`；
- 读取 `ToolRegistry`；
- 校验工具参数；
- 管理工具超时和取消；
- 执行 `PostToolUse` hook；
- 管理 context compaction；
- 读取 `JobRegistry` 通知；
- 向 GUI 发送 `ipc::Event`；
- 解释 `LoadedPlugin`、`Skill`、`AgentRole` 和 `Hook`。

关键入口：

- `src/agent.rs` 的 `Agent`；
- `src/agent.rs` 的 `Agent::new`；
- `src/agent.rs` 的 `Agent::run`；
- `src/agent.rs` 的 `Agent::dispatch`；
- `src/agent.rs` 的 `Agent::run_hooks`。

当前构造函数直接接收：

```text
LlmClient
Arc<ToolRegistry>
Arc<RwLock<ToolSettings>>
PathBuf
ContextSettings
&[&LoadedPlugin]
```

这使得 Agent 无法在没有具体 HTTP client、插件目录和工具注册表的情况下单独测试
或复用。

### 1.2 `Task` 反向依赖 `Agent`

`src/tools/task.rs` 的 `Task` 是一个工具，但它直接持有：

```text
LlmClient
ToolRegistry
ToolSettings
ContextSettings
EventSink
```

执行时又直接调用 `Agent::for_role` 和 `Agent::run`。

依赖关系实际是：

```text
Agent
  -> ToolRegistry
     -> Task
        -> Agent
```

这条环会阻止工具层独立演进，也会让 Wasm tool 直接触碰 Agent 生命周期。

### 1.3 Session 依赖 IPC、LLM 和 Tool 类型

`src/session.rs` 当前同时包含：

- session 领域模型；
- transcript step 操作；
- message replay；
- JSON 文件持久化；
- `AuditOutcome` 和 `RunState`；
- `HunkLines`；
- `llm::Message`、`ToolCall` 和 `Usage` 的转换。

具体问题：

```text
session -> ipc
session -> llm
session -> tools
session -> context
```

持久化层一旦改动，GUI IPC、LLM wire format 和工具输出都会被牵连。

### 1.4 IPC 成为全局类型汇聚点

`src/ipc.rs` 的 `Cmd` 和 `Event` 同时携带：

- GUI 命令；
- Agent 运行事件；
- LLM 配置；
- 工具配置；
- PluginCatalogue；
- JobSnapshot；
- ToolResult 相关字段；
- 图片附件；
- session replay 所需的 message。

`ipc.rs` 不是纯通信协议，而是整个应用的共享领域模型。这会让任何新运行时都
必须依赖 `ipc.rs`。

### 1.5 `main.rs` 承担过多业务编排

`src/main.rs` 当前同时负责：

- 创建 LLM client；
- 创建 ToolRegistry；
- 判断是否注册 `read_image`；
- 连接 MCP server；
- 注册 MCP tools；
- 收集 AgentRole；
- 创建 Task；
- 创建 Agent；
- 缓存 project Agent；
- 管理 run cancellation。

这使得增加新的 Wasmtime Component 或 Agent runtime 都需要修改入口文件。

### 1.6 GUI 直接执行插件和配置 I/O

`src/app/mod.rs` 当前包含：

- 配置写入；
- session 写入；
- plugin discovery；
- plugin cache 删除；
- plugin catalogue 替换。

特别是：

- `reload_plugins` 在 GUI 对象中直接执行 discovery；
- `uninstall_plugin` 在 GUI 对象中直接 `remove_dir_all`；
- `save_config` 直接写文件；
- `flush` 直接保存 session。

这让 GUI 既是状态管理器，又是持久化服务、插件管理器和文件操作器。

## 2. 解耦后的依赖方向

目标依赖图：

```text
domain
  ├── run identifiers
  ├── messages and tool calls
  ├── tool descriptor/output
  ├── agent events
  ├── session model
  └── error codes

ports
  ├── LlmProvider
  ├── ToolRuntime
  ├── PromptProvider
  ├── PluginEventRuntime
  ├── JobRuntime
  ├── SubagentRunner
  ├── SessionStore
  └── PluginManager

agent-runtime
  └── only depends on domain + ports

adapters
  ├── native LLM
  ├── host capability implementations
  ├── Wasmtime Component plugins
  ├── JSON session store
  └── Wasmtime components

frontends
  ├── ipc
  └── app

composition
  └── main / worker
```

目标约束：

```text
domain 不依赖 app、ipc、plugins、mcp、llm、tools
agent-runtime 不依赖具体 LlmClient
tools 不依赖 Agent
session 不依赖 ipc
app 不执行 plugin discovery 和 plugin filesystem mutation
main 只负责组装和生命周期
```

不是所有模块都需要 trait。建议只在以下边界使用 trait：

- Agent 调用外部能力；
- 一个能力有两个以上实现；
- 测试需要替换外部依赖；
- Agent runtime 需要隔离多个宿主或 Wasmtime Component 实现。

单一、纯函数、没有替代实现的代码继续使用普通 struct 和函数。

## 3. 目标目录结构

第一阶段不拆成多个 Cargo crate，仍然保持当前 binary crate 结构：

```text
src/
├── domain/
│   ├── mod.rs
│   ├── ids.rs
│   ├── messages.rs
│   ├── tools.rs
│   └── events.rs
├── ports/
│   ├── mod.rs
│   ├── llm.rs
│   ├── tools.rs
│   ├── prompt.rs
│   ├── hooks.rs
│   ├── jobs.rs
│   ├── subagents.rs
│   └── session.rs
├── runtime/
│   ├── mod.rs
│   ├── agent.rs
│   ├── project.rs
│   └── worker.rs
├── adapters/
│   ├── mod.rs
│   ├── native_llm.rs
│   ├── native_tools.rs
│   ├── native_plugins.rs
│   ├── json_session.rs
│   └── wasm.rs
├── agent.rs
├── app/
├── ipc.rs
├── llm.rs
├── plugins/
├── tools/
└── session/
```

第一阶段可以先使用以下较小目录，避免一次性移动所有文件：

```text
src/harness/
├── mod.rs
├── types.rs
├── ports.rs
├── events.rs
└── services.rs
```

等接口稳定后再把 `harness` 拆成 `domain`、`ports` 和 `runtime`。

## 4. 第一阶段：建立中立领域类型

### 4.1 新增中立类型

新增 `src/harness/types.rs`，先放不会依赖具体实现的类型：

```rust
pub type RunId = u64;
pub type CallId = String;
pub type PluginId = String;
pub type ToolName = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditOutcome {
    Executed,
    Denied,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    Running,
    Finished,
    Failed,
}
```

以下类型也应逐步移动或在 harness 中提供中立版本：

- `HunkLines`；
- `ToolResult`；
- `ToolCallEnvelope`；
- `RunRequest`；
- `ProjectContext`；
- `PromptSection`；
- `ToolStarted`；
- `ToolFinished`。

### 4.2 稳定内部类型

这些类型属于宿主内部和应用数据协议，不是旧插件 ABI。可以按依赖关系逐步移动，
但不需要为旧可执行插件保留转换层：

1. 在 harness 中定义新类型；
2. 让新 runtime 内部使用 harness 类型；
3. 由 `ipc` adapter 转换为 GUI 所需的 `Event`；
4. 删除已经没有生产调用方的旧定义。

转换只保留一个方向，避免两个类型互相转换后产生循环依赖：

```text
harness/domain -> ipc adapter -> legacy Event
```

不要让 `domain` 反向引用 `ipc`。

### 4.3 验收

- prompt 文本不变；
- session JSON 字段不变；
- GUI 显示文字不变；
- 现有 `AuditOutcome`、`RunState` 测试继续通过；
- `domain`/`harness` 不导入 `app`、`ipc`、`plugins`、`mcp`。

## 5. 第二阶段：抽取服务端口

新增 `src/harness/ports.rs`。

### 5.1 `LlmProvider`

```rust
#[async_trait::async_trait]
pub trait LlmProvider: Send + Sync {
    async fn stream_turn(
        &self,
        messages: &[Message],
        tools: &Value,
        thinking: Option<ThinkingLevel>,
        cancel: &CancellationToken,
        sink: &mut dyn LlmStreamSink,
    ) -> Result<AssistantTurn>;

    async fn complete_turn(
        &self,
        messages: &[Message],
        cancel: &CancellationToken,
    ) -> Result<AssistantTurn>;
}
```

`LlmClient` 作为第一版 native implementation：

```rust
pub struct NativeLlmProvider {
    client: LlmClient,
}
```

需要特别处理 streaming callback。不要让 `LlmProvider` 依赖 GUI 的 `Event`，使用
专门的流事件：

```rust
pub enum LlmStreamEvent {
    Reset,
    Reasoning(String),
    Content(String),
}

pub trait LlmStreamSink: Send {
    fn push(&mut self, event: LlmStreamEvent);
}
```

### 5.2 `ToolRuntime`

```rust
#[async_trait::async_trait]
pub trait ToolRuntime: Send + Sync {
    fn descriptors(&self) -> Vec<ToolDescriptor>;

    async fn execute(
        &self,
        call: &ToolCall,
        context: &ToolContext,
        cancel: &CancellationToken,
    ) -> Result<ToolExecution>;
}
```

```rust
pub struct ToolContext {
    pub project: PathBuf,
    pub working_directory: PathBuf,
    pub timeout: Duration,
    pub max_output_chars: usize,
    pub block_destructive_commands: bool,
}

pub struct ToolExecution {
    pub output: ToolOutput,
    pub outcome: AuditOutcome,
}
```

当前 `ToolRegistry` 不立即删除，新增 adapter：

```rust
pub struct RegistryToolRuntime {
    registry: Arc<ToolRegistry>,
}
```

以下逻辑必须从 `Agent::dispatch` 移入 adapter：

- tool lookup；
- `host_validates_arguments` 判断；
- schema validation；
- ToolSettings clone；
- project working directory 注入；
- host timeout；
- cancellation；
- output truncation；
- `AuditOutcome` 计算。

### 5.3 `PromptProvider`

```rust
pub trait PromptProvider: Send + Sync {
    fn build_system_prompt(&self, context: &PromptContext) -> Result<String>;
    fn tool_schema(&self) -> Value;
}
```

`PromptContext`：

```rust
pub struct PromptContext {
    pub project: PathBuf,
    pub project_files: Vec<ProjectInstruction>,
    pub tools: Vec<ToolDescriptor>,
    pub skills: Vec<SkillSummary>,
    pub roles: Vec<RoleSummary>,
}
```

当前 `build_system_prompt` 的固定顺序必须保持：

```text
identity
tools
skills
agents
rules
project context
```

拆出 PromptProvider 后，排序和 prompt 文本要有快照测试。

### 5.4 `PluginEventRuntime`

```rust
#[async_trait::async_trait]
pub trait PluginEventRuntime: Send + Sync {
    async fn dispatch(
        &self,
        event: &PluginEvent,
        cancel: &CancellationToken,
    ) -> Result<String>;
}
```

`PluginEventRuntime` 不能依赖 GUI `Event`。事件订阅、匹配语义和执行全部由
Component export（`list-event-handlers`/`handle-event`）；宿主只负责调用
Component、传递事件、限制输出并处理取消。

行为不变：

- refused call 不触发；
- 顺序执行；
- 输出追加到 tool result；
- 处理器失败作为 tool result 的可见错误；
- 处理器如需执行命令，必须调用 provider 被授予的 `invoke-tool` 或 raw process
  capability；
- 继承 host timeout 和 destructive command guard。

### 5.5 `JobRuntime`

```rust
#[async_trait::async_trait]
pub trait JobRuntime: Send + Sync {
    fn list(&self) -> Vec<JobSnapshot>;
    async fn kill(&self, id: &str, reason: Option<&str>) -> Result<()>;
    fn drain_notifications(&self) -> Vec<String>;
}
```

当前 `JobRegistry` 包装成 `NativeJobRuntime`。

`ToolRegistry` 可以暂时继续拥有 `Arc<JobRegistry>`，但 Agent 不再直接依赖
`JobRegistry`，而是依赖 `Arc<dyn JobRuntime>`。

### 5.6 `SessionStore`

```rust
#[async_trait::async_trait]
pub trait SessionStore: Send + Sync {
    async fn load(&self) -> Result<Vec<Session>>;
    async fn save(&self, sessions: &[Session]) -> Result<()>;
}
```

当前 `session::load/save` 包装成 `JsonSessionStore`。

session model 和 session store 要分开：

```text
Session / Step / ToolResult
  -> domain model

JSON path / temp file / rename
  -> JsonSessionStore
```

## 6. 第三阶段：重构 Agent

### 6.1 引入 `AgentServices`

```rust
pub struct AgentServices {
    pub llm: Arc<dyn LlmProvider>,
    pub tools: Arc<dyn ToolRuntime>,
    pub prompts: Arc<dyn PromptProvider>,
    pub events: Arc<dyn PluginEventRuntime>,
    pub jobs: Arc<dyn JobRuntime>,
    pub context: Arc<dyn ContextPolicy>,
}
```

Agent 改成：

```rust
pub struct Agent {
    services: Arc<AgentServices>,
    working_directory: PathBuf,
    system_prompt: String,
    tools_schema: Value,
    context_settings: ContextSettings,
}
```

Agent 不再保存：

- `LlmClient`；
- `ToolRegistry`；
- `Vec<Hook>`；
- `LoadedPlugin`；
- `Skill`；
- `AgentRole`。

这些都在 runtime factory 中解析成 service。

### 6.2 Agent 的职责边界

重构后 Agent 只负责：

1. 组装消息；
2. 判断是否需要 compaction；
3. 从 `LlmProvider` 获取一轮 assistant turn；
4. 发布 Agent domain events；
5. 把 tool call 交给 `ToolRuntime`；
6. 将 tool result 继续放入消息；
7. 处理取消、完成和失败；
8. 消费 `JobRuntime` 的 notice。

Agent 不负责：

- 工具 map；
- 插件 discovery；
- MCP handshake、JSON-RPC 和 transport framing；
- `.hooks.json` matcher、command 解析和 command 执行；
- skill 文件扫描；
- JSON session save；
- GUI channel；
- Wasmtime Store。

### 6.3 context compaction

当前 `context::summarize` 直接接受 `&LlmClient`。改成：

```rust
#[async_trait::async_trait]
pub trait ContextCompactor: Send + Sync {
    async fn summarize(
        &self,
        history: &[Message],
        cancel: &CancellationToken,
    ) -> Result<Option<String>>;
}
```

默认实现：

```rust
pub struct LlmContextCompactor {
    llm: Arc<dyn LlmProvider>,
}
```

`ContextWindow` 本身继续保持普通 struct，因为它是纯状态机，不需要 trait：

```text
ContextWindow = settings + latest measurement
ContextCompactor = external LLM operation
```

这两个概念必须分开。

### 6.4 Agent event sink

当前 `EventSink` 定义在 `agent.rs`，GUI 的 `ChannelSink` 反过来实现它。这让
`app` 依赖 Agent 模块。

移动到中立模块：

```rust
pub trait AgentEventSink: Send + Sync {
    fn emit(&self, event: AgentEvent);
}
```

GUI adapter：

```rust
pub struct UiEventSink {
    tx: mpsc::UnboundedSender<Event>,
    ctx: egui::Context,
}
```

依赖变为：

```text
AgentRuntime -> AgentEventSink
UiEventSink -> AgentEvent + ipc::Event
```

而不是：

```text
app -> agent::EventSink
```

## 7. 第四阶段：解耦 Task 和 Agent

### 7.1 定义 `SubagentRunner`

```rust
#[async_trait::async_trait]
pub trait SubagentRunner: Send + Sync {
    async fn run_role(
        &self,
        role: &AgentRole,
        prompt: String,
        context: SubagentContext,
        sink: Arc<dyn AgentEventSink>,
        cancel: CancellationToken,
    ) -> Result<String>;
}
```

```rust
pub struct SubagentContext {
    pub project: PathBuf,
    pub context_settings: ContextSettings,
}
```

### 7.2 `Task` 只依赖 runner

改造前：

```rust
pub struct Task {
    llm: LlmClient,
    roles: Vec<AgentRole>,
    registry: Arc<ToolRegistry>,
    settings: Arc<RwLock<ToolSettings>>,
    working_directory: PathBuf,
    context_settings: ContextSettings,
    forwarder: Arc<dyn EventSink>,
}
```

改造后：

```rust
pub struct Task {
    roles: Vec<AgentRole>,
    runner: Arc<dyn SubagentRunner>,
    jobs: Arc<dyn JobRuntime>,
}
```

`Task` 只做：

1. 解析 `agent`、`prompt` 和 `runInBackground`；
2. 校验 role 是否存在；
3. 构造 `SubagentContext`；
4. 调用 runner；
5. 将 foreground/background 结果转换成 `ToolOutput`。

它不再调用 `Agent::for_role`。

### 7.3 默认 runner

新增：

```rust
pub struct NativeSubagentRunner {
    factory: Arc<AgentFactory>,
}
```

`NativeSubagentRunner` 才负责：

- 创建 sub-agent registry；
- 创建 role prompt；
- 创建 `Agent`；
- 运行 `Agent::run`；
- 收集最终回答；
- 将事件包装成 sub-agent job event。

未来 Wasm agent plugin 可以实现另一个 `SubagentRunner`，而不需要修改 `Task`。

### 7.4 防止递归 delegation

当前通过注册 `task` 前 clone registry 来禁止 sub-agent 再调用 `task`。解耦后保留
同一语义，但移到 `AgentFactory`：

```rust
pub enum AgentKind {
    Root,
    Subagent,
}

impl AgentFactory {
    fn services_for(&self, kind: AgentKind) -> AgentServices {
        // Root includes SubagentRunner.
        // Subagent omits the delegation capability.
    }
}
```

不要让 `Task` 通过检查 tool name 自己决定是否递归。

## 8. 第五阶段：拆分 Worker 和插件组装

### 8.1 `ProjectRuntimeFactory`

新增：

```rust
pub struct ProjectRuntimeFactory {
    llm_factory: Arc<dyn LlmProviderFactory>,
    plugin_manager: Arc<PluginManager>,
    settings: Arc<RwLock<ToolSettings>>,
    sink: Arc<dyn AgentEventSink>,
}
```

```rust
impl ProjectRuntimeFactory {
    pub async fn build(
        &self,
        project: &Path,
        model: &LlmSettings,
    ) -> Result<ProjectRuntime>;
}
```

`build` 内部负责：

1. 创建 `LlmProvider`；
2. 创建 builtin ToolRegistry；
3. 根据输入 modality 注册 `read_image`；
4. 通过 PluginManager 获取当前项目 plugins；
5. 按 global/project scope 解析 Wasmtime Component；
6. 为当前 global/project Component 实例绑定 configuration root，并提供通用的
   `read-plugin-file` host import；
8. 加载 bundled 和 project-scoped Wasmtime Components，并只绑定当前实例对应的
   global/project configuration root；
9. 为 Component 创建 project-scoped `CapabilityHub`；
10. 通过 Component 的通用 export 加载 tools 和 hooks；MCP Component 自己把
    MCP tools 投影为普通 tools；
11. 创建 native prompt provider；
12. 创建 hook runtime；
13. 创建 subagent runner；
14. 创建 AgentServices；
15. 创建 project Agent。

MCP Component 自己实现 `initialize`、`initialized`、`tools/list`、`tools/call`、
JSON-RPC、stdio framing 和 HTTP/event-stream framing。宿主只向它提供声明约束下的
raw process/HTTP bytes。

### 8.2 `ProjectRuntime`

```rust
pub struct ProjectRuntime {
    pub project: PathBuf,
    pub agent: Arc<Agent>,
    pub jobs: Arc<dyn JobRuntime>,
    pub plugins: PluginSnapshot,
}
```

Worker 不再自己拼装 Agent，只缓存：

```rust
HashMap<PathBuf, Arc<ProjectRuntime>>
```

### 8.3 Worker 的职责

Worker 只负责：

- 接收 Cmd；
- 维护当前设置；
- 按 project 获取 ProjectRuntime；
- 维护 active run cancellation；
- 启动 run task；
- 将 runtime event 转发到 GUI；
- 在设置或插件变化时让 runtime 失效。

Worker 不负责：

- MCP 具体握手；
- plugin descriptor 解析；
- ToolRegistry 具体注册顺序；
- Agent 构造参数拼接。

## 9. 第六阶段：拆分 PluginCatalogue 和 PluginRuntime

### 9.1 当前问题

`LoadedPlugin` 同时包含：

- manifest；
- root；
- scope；
- skills；
- commands；
- agents；
- Wasm runtime module 和 permissions；
- 由 Component 主动读取的 global/project configuration root 级 `.hooks.json` /
  `.mcp.json` 原始字节；
- 宿主通用的 `processCommands` / `networkHosts` capability allowlist。

它既是发现结果，又是 GUI 展示模型，又是运行时输入。

### 9.2 分成三种模型

```rust
pub struct PluginDescriptor {
    pub id: PluginId,
    pub root: PathBuf,
    pub scope: PluginScope,
    pub display_name: String,
    pub version: Option<String>,
    pub source: PluginSource,
}

pub struct PluginSnapshot {
    pub enabled: Vec<PluginDescriptor>,
    pub disabled: Vec<PluginDescriptor>,
}

pub struct ProjectCapabilities {
    pub tools: Vec<Arc<dyn Tool>>,
    pub prompts: Vec<Arc<dyn PromptProvider>>,
    pub roles: Vec<AgentRole>,
    pub commands: Vec<Command>,
    pub wasm_providers: Vec<PluginRuntimeDescriptor>,
}
```

关系：

```text
Plugin discovery
  -> PluginSnapshot

PluginSnapshot + project
  -> PluginResolver

PluginResolver
  -> ProjectCapabilities

ProjectCapabilities
  -> ProjectRuntimeFactory
```

GUI 只需要 `PluginSnapshot` 和展示 DTO，不应该持有带有 MCP client、Wasm Store 或
运行时资源的对象。

### 9.3 PluginManager

新增：

```rust
#[async_trait::async_trait]
pub trait PluginManager: Send + Sync {
    async fn snapshot(&self) -> Result<PluginSnapshot>;
    async fn resolve_project(&self, project: &Path) -> Result<ProjectCapabilities>;
    async fn enable(&self, id: &PluginId) -> Result<PluginSnapshot>;
    async fn disable(&self, id: &PluginId) -> Result<PluginSnapshot>;
    async fn uninstall(&self, id: &PluginId) -> Result<PluginSnapshot>;
}
```

当前 `plugins::discover` 作为 native implementation。

插件管理器负责：

- discovery；
- scope 合并；
- disabled 过滤；
- shadowing；
- Wasmtime Component 初始化和生命周期；
- provider 生命周期。

GUI 不再直接调用 `plugins::discover`。

## 10. 第七阶段：GUI 和 Worker 边界

### 10.1 Cmd 只表达意图

保留当前 channel 结构，但重新定义 Cmd 的语义：

```rust
pub enum Cmd {
    Run(RunRequest),
    Cancel { run_id: RunId },
    SetToolSettings(ToolSettings),
    SetLlmSettings(LlmSettings),
    ReloadPlugins,
    SetPluginEnabled {
        id: PluginId,
        enabled: bool,
    },
    UninstallPlugin {
        id: PluginId,
    },
    ListJobs {
        project: PathBuf,
    },
    KillJob {
        project: PathBuf,
        job_id: String,
    },
}
```

GUI 不再发送 `Arc<PluginCatalogue>` 给 Worker。Worker 自己通过 PluginManager 取得
新的 snapshot。

### 10.2 Event 分层

Worker 内部使用：

```rust
AgentEvent
RuntimeEvent
PluginEvent
```

跨线程发送给 GUI 时再转换成：

```rust
UiEvent
```

推荐：

```rust
pub enum UiEvent {
    Agent(AgentEvent),
    Jobs {
        project: PathBuf,
        jobs: Vec<JobView>,
    },
    PluginsUpdated(PluginSnapshotView),
    PluginOperationFailed {
        id: Option<PluginId>,
        message: String,
    },
}
```

### 10.3 插件 reload

流程应改成：

```text
GUI 修改开关
  -> Cmd::SetPluginEnabled
  -> Worker / PluginManager 保存设置
  -> Worker / PluginManager discovery
  -> 清理受影响 project runtime
  -> Event::PluginsUpdated
  -> GUI 更新 snapshot view
```

旧 Agent 必须先停止或标记为 stale，再释放 MCP/Wasm/plugin resources。

### 10.4 文件 I/O

以下操作移出 GUI：

- plugin discovery；
- plugin cache 删除；
- config save；
- session save；
- plugin manifest 读取；
- Wasmtime Component 加载及其 raw transport capability；
- Wasm component 加载。

GUI 可以保留：

- 文件夹选择对话框；
- 图片选择对话框；
- 用户输入和本地 transient state。

## 11. 第八阶段：Session 解耦

### 11.1 拆分目录

可以先保持 `src/session.rs`，等依赖清理后再拆文件：

```text
src/session/
├── mod.rs
├── model.rs
├── replay.rs
├── transcript.rs
└── store.rs
```

### 11.2 model

`model.rs` 只包含：

- `Session`；
- `Step`；
- `ToolResult`；
- session 状态；
- transcript append helpers。

不导入：

- `ipc`；
- `ToolRegistry`；
- `LlmClient`；
- filesystem implementation。

### 11.3 replay

`replay.rs` 只负责：

```rust
pub fn to_messages(session: &Session) -> Vec<Message>;
pub fn summary_turn(summary: &str) -> Message;
```

如果 `Message` 最终也要成为 domain 类型，增加一个 `llm` wire adapter，不让
session model 直接知道 provider 请求 JSON。

### 11.4 store

`store.rs` 实现：

```rust
pub struct JsonSessionStore {
    path: PathBuf,
}
```

并实现 `SessionStore`。文件写入、临时文件、rename 和版本检查都留在 store，
不能散落到 App 或 Worker。

## 12. 第九阶段：Prompt 管线解耦

### 12.1 当前问题

当前 prompt 由 `Agent::new` 直接从：

- `ToolRegistry`；
- `Skill`；
- `AgentRole`；
- `Hook`；
- project files；

组装出来。

这样新增 Wasm prompt provider 就必须继续修改 Agent。

### 12.2 Prompt pipeline

```rust
pub struct PromptPipeline {
    contributors: Vec<Arc<dyn PromptContributor>>,
}

pub trait PromptContributor: Send + Sync {
    fn id(&self) -> &str;
    fn sections(&self, context: &PromptContext) -> Result<Vec<PromptSection>>;
}
```

固定的 pipeline 阶段：

```text
Identity
Tools
Skills
Agents
Rules
ProjectInstructions
```

每个阶段只能由对应 contributor 写入，禁止任意插件直接拼接完整 system prompt。

### 12.3 稳定排序

最终输出按照：

```text
stage index
contributor id
section priority
section id
```

排序。

必须保持：

- ToolRegistry 的 BTreeMap 顺序；
- skills 的名称顺序；
- agents 的名称顺序；
- plugin id 顺序；
- hook 不进入 prompt 的既有行为。

增加 prompt snapshot tests，防止解耦后无意中改变 provider cache prefix。

## 13. 第十阶段：为 Wasmtime 留出 adapter 边界

当前 ports 负责宿主内的 Agent 编排；Wasmtime provider 通过独立 ABI 接入：

```text
WasmComponent
  -> generic tool/event exports
  -> ToolRuntime / PluginEventRuntime adapters
  -> Agent
```

Wasm 插件不能直接依赖：

- `Agent`；
- `ToolRegistry`；
- `ipc::Event`；
- `LoadedPlugin`；
- `ToolSettings`；
- `LlmClient`。

它只通过 WIT 获得：

- tool metadata；
- execute tool；
- hook metadata 和 event；
- `read-plugin-file`，由 Component 自己读取当前 global/project configuration root
  下的配置；
- 受权限控制的 host tool invocation；
- raw process bytes；
- raw HTTP request/response bytes；
- 声明式 UI surface/action。

这时 Wasm runtime 的依赖方向为：

```text
Wasm adapter -> ports/domain
Wasmtime implementation -> WIT runtime
Agent runtime -> ports
```

而不是：

```text
Agent -> Wasmtime
Tool -> Agent
Plugin -> GUI IPC
```

## 14. 推荐提交顺序

每次提交只完成一类解耦，并保证可以编译和测试。

### Commit 1

```text
Add neutral harness types and event conversions
```

内容：

- 新增 `src/harness/types.rs`；
- 增加 legacy IPC 转换；
- 保持现有 `Cmd/Event`；
- 不改变 Agent 逻辑。

### Commit 2

```text
Add service ports for LLM, tools, hooks, jobs, and sessions
```

内容：

- 新增 trait；
- 为宿主实现增加 adapters；
- adapter 测试；
- 暂不改变 Agent 构造。

### Commit 3

```text
Move context summarization behind LlmProvider
```

内容：

- `ContextCompactor`；
- `LlmClient` adapter；
- fake provider 测试；
- 保持 compaction 行为。

### Commit 4

```text
Move agent tool dispatch behind ToolRuntime
```

内容：

- 把 validate/timeout/truncate/cancel 移入 `RegistryToolRuntime`；
- Agent 只调用 port；
- 保留 `Tool` 和 `ToolRegistry`。

### Commit 5

```text
Decouple task delegation from Agent construction
```

内容：

- `SubagentRunner`；
- `NativeSubagentRunner`；
- Task 不再导入 `Agent`；
- 保持 foreground/background delegation 行为。

### Commit 6

```text
Move prompt and hook assembly into runtime services
```

内容：

- PromptPipeline；
- PluginEventRuntime；
- prompt snapshot；
- event handler integration tests。

### Commit 7

```text
Extract project runtime construction from main
```

内容：

- `ProjectRuntimeFactory`；
- `ProjectRuntime`；
- provider loading 从 `main.rs` 移出；
- Worker 只管理 runtime cache。

### Commit 8

```text
Move plugin lifecycle operations into the worker
```

内容：

- reload/enable/disable/uninstall Cmd；
- plugin operation events；
- GUI 不再直接 discovery 和删除 plugin；
- plugin lifecycle tests。

### Commit 9

```text
Separate session model from persistence and IPC
```

内容：

- session model/replay/store；
- JSON store adapter；
- UI session folding 改用 domain event；
- session round-trip tests。

### Commit 10

```text
Add Wasmtime adapters on top of harness ports
```

只有前九个提交稳定后再做 Wasmtime。

## 15. 测试策略

### 15.1 Agent 测试

Agent 测试不再创建真实 `LlmClient`，改用：

```rust
struct FakeLlmProvider {
    turns: Mutex<Vec<AssistantTurn>>,
}
```

覆盖：

- 普通回答；
- tool call；
- 多轮 tool call；
- stream reset；
- cancel；
- retry；
- compaction；
- tool failure；
- tool denial。

### 15.2 ToolRuntime 测试

测试独立覆盖：

- tool not found；
- schema validation；
- host validation off；
- timeout；
- cancellation；
- truncation；
- destructive command denial；
- output images；
- hunk metadata。

这些测试不需要启动完整 Agent。

### 15.3 SubagentRunner 测试

测试：

- role lookup；
- foreground delegation；
- background delegation；
- final answer extraction；
- sub-agent event forwarding；
- sub-agent 不拥有 `task`；
- job kill；
- parent cancel。

### 15.4 Plugin lifecycle 测试

测试：

- global/project merge；
- project shadowing；
- disabled plugin；
- reload；
- stale runtime 清理；
- MCP server shutdown；
- plugin cache 删除范围；
- 一个坏插件不影响其他插件。

### 15.5 主线程 I/O 测试

不依赖时间的单元测试很难直接证明没有主线程 I/O，因此至少应通过设计保证：

- `App` 不导入 `std::fs` 的 plugin 操作；
- `App` 不调用 `plugins::discover`；
- `App` 不调用 `McpClient::connect`；
- `App` 不加载 Wasm；
- worker command 负责所有 runtime I/O。

可以用 `rg` 作为提交前检查：

```text
rg "plugins::discover|remove_dir_all|McpClient::connect|Wasmtime" src/app
```

理想结果是没有生产代码命中。

## 16. 兼容性要求

解耦期间必须保持：

### 工具

- `ToolDescriptor` 字段不变；
- tool name 不变；
- schema 不变；
- builtin tool 注册顺序不变；
- MCP tool 命名不变；
- `ToolSettings::block_destructive_commands` 默认仍为 `true`。

### Prompt

- `HOST_RULES` 不改；
- `SUB_AGENT_RULES` 不改；
- identity、tools、skills、agents、rules、project context 顺序不改；
- prompt 中的插件排序保持稳定；
- provider cache prefix 不因 adapter 改写而变化。

### Session

- JSON version 继续兼容；
- 旧字段仍能读取；
- `context_measurement`、thinking、images、hunks 不丢失；
- 旧 session 能继续 replay 成相同消息。

### GUI

- 用户操作不变；
- stop 只取消对应 run；
- background job 继续可查看和停止；
- sub-agent transcript 继续按 job 展示；
- plugin 开关和卸载行为不变。

## 17. 不建议的做法

### 17.1 不要先把所有内容放进 `PluginContext`

这种设计：

```rust
pub struct PluginContext {
    pub agent: Agent,
    pub app: App,
    pub registry: ToolRegistry,
    pub config: Config,
    pub session: Session,
}
```

表面上减少了参数，实际上把所有模块重新绑在一起，并且无法转成 Wasm ABI。

### 17.2 不要让 Wasm plugin 直接实现 Rust `Tool`

Wasm 不能直接实现：

```rust
impl Tool for WasmComponent { ... }
```

正确方式是：

```text
WIT component
  -> WasmPluginInstance
  -> WasmTool adapter
  -> ToolRuntime
```

### 17.3 不要把 `ipc::Event` 当成领域事件

GUI 需要的字段不等于 Agent runtime 需要的字段。领域事件应先表达发生了什么，
UI 再决定如何展示。

### 17.4 不要把所有 trait 都放在 `agent.rs`

这会让 `agent.rs` 变成新的 god module。trait 应放在依赖方向更中立的
`harness/ports.rs` 或后续的 `ports/` 模块。

### 17.5 不要一次性拆成 workspace 多 crate

当前仓库是 binary crate，没有 lib target。第一阶段拆成多个 crate 会同时引入：

- public API；
- crate visibility；
- test fixture；
- build dependency；
- Wasmtime ABI；

风险太高。先在单 crate 内稳定模块方向，之后再判断是否需要 workspace。

## 18. 完成标准

解耦阶段完成的判断标准：

- [ ] `Agent` 不再直接依赖 `LlmClient`；
- [ ] `Agent` 不再直接依赖 `LoadedPlugin`、`Skill`、`AgentRole` 和 `Hook`；
- [ ] `Agent` 的工具执行通过 `ToolRuntime`；
- [ ] `Task` 不再导入或构造 `Agent`；
- [ ] `context::summarize` 不再接收具体 `LlmClient`；
- [ ] `session` 不再依赖 `ipc`；
- [ ] `ipc` 不再作为所有领域类型的定义位置；
- [x] `main.rs` 不再实现 native MCP tool registration；
- [ ] `main.rs` 主要负责组合和 Worker 生命周期；
- [ ] `App` 不再做 plugin discovery；
- [ ] `App` 不再删除 plugin cache；
- [ ] plugin reload 在 worker 中完成；
- [ ] Wasmtime Component 的 tool、hook、MCP loading 和 transport 行为保持原有配置
  语义；
- [ ] prompt snapshot 测试通过；
- [ ] Agent loop、MCP、hooks、task、image、jobs、session 测试通过；
- [ ] 所有 I/O 仍在 tokio worker；
- [ ] `cargo fmt --check` 通过；
- [ ] `cargo clippy --all-targets -- -D warnings` 通过；
- [ ] `cargo test` 通过。

## 19. 建议的实际执行顺序

不要从目录重命名开始。最小风险顺序是：

```text
1. 定义中立 event/type
2. 定义 ports
3. 给现有实现加 adapters
4. 改 context
5. 改 Agent dispatch
6. 改 Task delegation
7. 改 prompt/hooks
8. 抽 ProjectRuntimeFactory
9. 抽 PluginManager
10. 把 GUI plugin I/O 移到 worker
11. 拆 Session model/store/replay
12. 接 Wasmtime
```

每一步都先让旧测试继续通过，再进行下一步。这样即使 Wasmtime ABI 后续调整，
native Agent runtime 也已经具备独立的服务边界，不需要再回头重构 GUI、工具和
session。
