# Wasmtime 插件化实现文档

本文档描述 `deluxe-agent` 当前如何由宿主内核统一编排、由 Wasmtime Component
插件提供可执行能力的 Harness 结构。

当前插件格式唯一固定为 Wasmtime Component，不兼容旧的可执行插件格式。每个插件
都必须是 Wasmtime Component；tool、hook、MCP loading、MCP tool adapter 以及
MCP transport 都由对应 Component 提供。宿主只提供显式授权的安全能力和 Agent
编排。Component 仍可提供 skills、commands、agents 等声明式内容，这些内容仍
属于同一个 Wasmtime 插件。

`.mcp.json` 和 `.hooks.json` 不是插件包资源，也不属于 `plugin.json`。Plugin
discovery 只读取插件根目录的 `plugin.json` 和 `plugin.wasm`，并根据插件的
global/project scope 为 Component 实例绑定配置根目录。MCP 或 Hooks Component
需要配置时，主动调用通用的 `read-plugin-file` host import 读取当前 scope 根目录
下的文件。宿主返回原始字节，不做 UTF-8、JSON、MCP 或 Hooks 解析；MCP transport、
Hooks matcher 和 command 执行都由对应 Component 自己负责。

## 1. 目标与非目标

### 1.1 目标

最终系统应具备以下能力：

- 插件可以通过 Wasmtime Component 贡献工具、提示词片段、技能、命令、子 Agent
  角色和生命周期 hook；
- 插件按 global/project scope 加载，project 插件不能泄漏到其他项目；
- 所有插件都是 Wasm Component，不存在非 Wasm 插件；MCP 和 Hooks 由对应
  Component 实现，不是宿主识别的插件类型；
- bundled built-in tools 也通过内置 Wasmtime Component 暴露，宿主不把 native tool
  registry 直接交给 Agent；
- hooks 和 MCP server/tool discovery、连接、协议握手、framing、结果解析均在
  provider 内完成；
- Agent loop 不关心工具的实现来源，只通过统一的宿主服务接口调用；
- Wasm 插件拥有独立的实例状态、超时、取消、内存和执行预算；
- 插件崩溃、trap、超时、非法返回值只影响当前插件调用，不拖垮 GUI 和其他 Agent；
- 插件 ABI 通过 WIT 版本化，不把 Rust 私有类型直接暴露给 Wasm；
- 宿主仍然掌握文件、进程、网络、LLM、会话和事件能力，但插件只能通过
  `CapabilityHub` 获得它们；
- 现有 `Tool`、`ToolDescriptor`、`ToolSettings` 继续作为宿主内部执行契约；
- bundled Wasmtime Component 缺失或加载失败时，宿主仍可启动并记录明确 warning。

### 1.2 非目标

第一阶段不做以下事情：

- 不把 `Agent` 的完整主循环搬到 Wasm；
- 不允许 Wasm 插件直接访问完整 WASI 文件系统；
- 不把 Rust 的 `async_trait`、`Arc<dyn Trait>`、`PathBuf` 或 `serde_json::Value`
  直接当作跨 Wasm ABI；
- 不改变当前 `ToolDescriptor` 的字段形状；
- 不改变 `HOST_RULES`、`SUB_AGENT_RULES` 和现有 prompt 前缀；
- 不在宿主实现 MCP JSON-RPC、handshake、framing 或 transport parser；
- 不做远程插件下载、签名市场和自动更新；
- 不把 GUI 的 `ipc::Event` 直接暴露给插件；
- 不允许插件绕过 `ToolSettings::block_destructive_commands`；
- 不在主线程运行 Wasm 或执行插件 I/O。

## 2. 当前实现盘点

### 2.1 当前调用链

当前程序的关键路径是：

```text
main
  -> Worker
     -> 按 project 缓存 Agent
        -> LlmClient
        -> host capability registry
           -> native safe implementations
        -> bundled Wasmtime Component
           -> built-in tools
        -> project Wasmtime Components
           -> tools / hooks / MCP tools
        -> Task Tool
        -> system prompt
           -> tools
           -> skills
           -> agents
           -> host rules
           -> project context
        -> Agent::run
           -> LLM streaming
           -> context compaction
           -> tool dispatch
           -> PostToolUse hooks
           -> job notices
           -> EventSink
  -> GUI 通过 Cmd/Event 与 Worker 通信
```

线程模型不能改变：

```text
egui 主线程
  <== Cmd/Event channel ==> 
tokio worker 线程
  -> Agent / Wasmtime Components / 文件、进程和网络 I/O
```

Wasm 插件只能在 worker 侧运行。GUI 不得直接加载、实例化或调用 Wasm。

### 2.2 当前模块与目标抽象的对应关系

| 当前代码 | 当前职责 | 目标抽象 |
| --- | --- | --- |
| `src/agent.rs::Agent` | Agent loop、prompt、工具 dispatch、hook、事件 | `AgentDriver` + `ToolRuntime` + `PromptPipeline` + `HookRuntime` |
| `src/llm.rs::LlmClient` | OpenAI 兼容请求、流式解析、重试 | `LlmProvider` |
| `src/tools/mod.rs::Tool` | 单个工具定义与执行 | 保留，作为宿主内部工具接口 |
| `src/tools/mod.rs::ToolRegistry` | 工具注册、schema、共享 JobRegistry | `ToolRuntime` 的默认实现 |
| `src/tools/settings.rs::ToolSettings` | 工具工作目录、超时、输出上限和破坏性命令保护 | `ToolContext` 的宿主策略来源 |
| `src/context.rs::ContextWindow` | 上下文测量与 compaction 判断 | `ContextPolicy` |
| `src/context.rs::summarize` | 使用 LLM 生成摘要 | `ContextCompactor` 或 `ContextPolicy` 的默认实现 |
| `src/session.rs` | 会话模型、transcript、JSON 持久化 | `SessionStore` |
| `src/tools/jobs.rs::JobRegistry` | 后台任务、取消、通知 | `JobRuntime` |
| `src/plugins/mod.rs` | Codex 插件发现、scope、启停 | `PluginManager` |
| `src/plugins/providers.rs` | Wasm tool、hook adapter | `ToolProvider`、`HookRuntime` |
| `src/plugins/capabilities.rs` | 显式授权的 tool/process/HTTP raw capability | `CapabilityHub` |
| `wit/deluxe-harness.wit` | Wasm host capability 与 provider ABI | 版本化 Component ABI |
| `src/ipc.rs::Event` | Agent 到 GUI 的传输协议 | 拆成 `AgentEvent` 和 `UiEvent` |

### 2.3 目前最需要拆开的耦合

当前 `Agent` 同时知道以下实现细节：

1. LLM 请求如何发出；
2. 工具如何查找、校验、执行和截断；
3. tool output 如何变成 transcript；
4. hook 如何通过 Component export 执行；
5. context compaction 如何请求摘要；
6. JobRegistry 如何产生 notice；
7. 插件的 skill、agent role 和 Wasmtime metadata 如何进入 prompt；
8. GUI 需要哪些 `Event`。

这使得 `Agent` 成为所有机制的耦合中心。插件化的第一步不是 Wasmtime，而是把
这些依赖变成可注入的宿主服务。

## 3. 目标架构

### 3.1 分层

```text
┌─────────────────────────────────────────────────────────────┐
│ GUI / eframe                                                │
│ App state, rendering, user commands                         │
└──────────────────────────────┬──────────────────────────────┘
                               │ Cmd / UiEvent
┌──────────────────────────────▼──────────────────────────────┐
│ Worker / HarnessHost                                         │
│ project scope, plugin lifecycle, agent cache, cancellation   │
└──────────────┬───────────────────────┬──────────────────────┘
               │                       │
┌──────────────▼──────────────┐ ┌──────▼───────────────────────┐
│ AgentRuntime                │ │ PluginManager                │
│ loop, context, prompt       │ │ discovery, scope, lifecycle   │
└──────┬───────────┬───────────┘ └─────────┬────────────────────┘
       │           │                       │
┌──────▼─────┐ ┌───▼──────────────┐ ┌──────▼────────────────────┐
│ LlmProvider│ │ CapabilityHub    │ │ PluginInstance             │
│ native/http│ │ tools/jobs/events│ │ native / MCP / Wasm        │
└────────────┘ └───┬──────────────┘ └──────────┬─────────────────┘
                   │                           │
        ┌──────────▼──────────┐     ┌──────────▼──────────┐
        │ Host capabilities   │     │ Wasm Component       │
        │ safe built-in impls │     │ tool/hook/MCP logic   │
        │ raw process bytes   │     │ JSON-RPC + framing    │
        │ raw HTTP responses  │     │ isolated Store        │
        └─────────────────────┘     └───────────────────────┘
```

### 3.2 核心原则

#### Rust trait 只用于宿主内部

Rust trait 适合隔离 `Agent` 和宿主实现：

```rust
#[async_trait::async_trait]
pub trait LlmProvider: Send + Sync {
    async fn stream_turn(
        &self,
        request: LlmRequest<'_>,
        sink: &mut dyn LlmStreamSink,
        cancel: &CancellationToken,
    ) -> Result<AssistantTurn>;
}
```

但这个 trait 不能直接成为 Wasm ABI。原因是：

- Rust trait object 没有稳定 ABI；
- `async_trait` 的 future 类型不是跨组件协议；
- Rust 的布局、所有权和 panic 语义不适合插件边界；
- `serde_json::Value`、`PathBuf` 等类型不应直接穿过组件边界。

#### WIT 只描述稳定的领域协议

Wasmtime 边界只使用 WIT 可表达的类型，例如：

- `string`
- `u32`、`u64`、`bool`
- `list<T>`
- `record`
- `variant`
- `result<T, E>`
- `future<T>` 或 `async func`

复杂 JSON Schema、工具参数和工具结果在第一版使用 UTF-8 JSON 字符串承载。
这样可以复用当前 OpenAI/MCP 工具协议，并避免第一版 WIT 被 JSON Schema 的完整
语义绑死。

#### 宿主拥有权限

Wasm 插件请求文件、shell、网络、LLM、任务和事件能力时，必须通过宿主导入接口。
插件本身不获得隐式的文件系统权限，也不获得宿主内部对象的引用。

## 4. 目录与模块规划

第一阶段新增以下宿主模块：

```text
src/harness/
├── mod.rs
├── types.rs
├── services.rs
├── events.rs
├── prompt.rs
├── runtime.rs
└── error.rs
```

Wasmtime 接入后新增：

```text
src/plugins/
├── wasm.rs
├── wasm_runtime.rs
└── wasm_manifest.rs

wit/
└── deluxe-harness.wit

plugin-fixtures/
└── echo-tool/
    ├── Cargo.toml
    ├── src/lib.rs
    └── plugin.json
```

建议的职责：

- `harness/types.rs`：`PluginId`、`PluginScope`、`ToolCall`、`ToolContext`、
  `PromptSection`、`AgentEvent` 等领域类型；
- `harness/services.rs`：宿主内部 trait；
- `harness/events.rs`：Agent 领域事件和订阅；
- `harness/prompt.rs`：tools、skills、agents、rules 的贡献合并；
- `harness/runtime.rs`：Agent 运行所需服务集合；
- `plugins/wasm_manifest.rs`：Wasm 插件 manifest 解析；
- `plugins/wasm_runtime.rs`：Wasmtime `Engine`、`Component`、`Linker`、`Store`
  和资源限制；
- `plugins/wasm.rs`：将一个 Wasm Component 包装为宿主 `Tool`、prompt provider
  或 hook provider；
- `wit/deluxe-harness.wit`：版本化 ABI。

## 5. 宿主内部接口设计

### 5.1 基础类型

第一阶段的领域类型可以定义为：

```rust
pub type PluginId = String;
pub type ToolName = String;
pub type CallId = String;
pub type RunId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginScope {
    Global,
    Project,
}

#[derive(Debug, Clone)]
pub struct ToolContext {
    pub plugin_id: Option<PluginId>,
    pub working_directory: PathBuf,
    pub timeout: Duration,
    pub max_output_chars: usize,
    pub block_destructive_commands: bool,
}

#[derive(Debug, Clone)]
pub struct PromptSection {
    pub id: String,
    pub priority: i32,
    pub content: String,
}
```

`ToolContext` 必须由宿主创建。插件不能自行修改 `working_directory`、超时或
破坏性命令策略。

### 5.2 LlmProvider

当前 `LlmClient` 先实现默认版本：

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

第一阶段只做 native adapter：

```rust
pub struct NativeLlmProvider {
    client: LlmClient,
}
```

不建议在第一版把 LLM provider 放进 Wasm。LLM 流式响应、HTTP retry、图片引用、
取消和 provider 特有字段都需要较多 ABI 设计，放在宿主更容易保持现有行为。

### 5.3 ToolRuntime

当前 `ToolRegistry` 的查找、schema 校验、超时、输出截断和取消逻辑应逐渐集中到
`ToolRuntime`：

```rust
#[async_trait::async_trait]
pub trait ToolRuntime: Send + Sync {
    fn descriptors(&self) -> Vec<ToolDescriptor>;

    async fn execute(
        &self,
        call: &ToolCall,
        context: &ToolContext,
        cancel: &CancellationToken,
    ) -> Result<ToolOutput>;
}
```

建议保留当前 `Tool` trait，不要直接删除。迁移方式：

```rust
pub struct RegistryToolRuntime {
    registry: Arc<ToolRegistry>,
}
```

`RegistryToolRuntime` 复用当前 `Agent::dispatch` 中的逻辑：

1. 根据工具名查找；
2. 根据 `host_validates_arguments` 决定是否校验；
3. 生成带 project working directory 的 `ToolContext`；
4. 应用 host timeout；
5. 监听 `CancellationToken`；
6. 执行工具；
7. 截断输出；
8. 返回 `ToolOutput` 和 `AuditOutcome`。

Wasm 工具最终也只需要实现 `ToolRuntime` 的 adapter，而不需要进入 Agent loop。

### 5.4 PromptContributor

当前 `build_system_prompt` 直接知道 skill、agent role 和 rules 的具体来源。应抽成：

```rust
#[async_trait::async_trait]
pub trait PromptContributor: Send + Sync {
    fn id(&self) -> &str;

    fn contribute(
        &self,
        context: &PromptContext,
    ) -> Result<Vec<PromptSection>>;
}
```

`PromptContext` 至少包含：

```rust
pub struct PromptContext {
    pub project: PathBuf,
    pub registered_tools: Vec<ToolDescriptor>,
    pub delegated_agents_enabled: bool,
}
```

当前实现映射为：

- `BuiltinPromptContributor`：内置工具说明和 host rules；
- `NativePluginPromptContributor`：skills、agents、commands 的 prompt 相关部分；
- `WasmPromptContributor`：Wasm 插件导出的 prompt sections。

所有 contributor 必须返回稳定排序后的内容。排序规则：

1. contributor id；
2. section priority；
3. section id。

这样可以保持 provider prompt cache 所依赖的前缀稳定性。

### 5.5 AgentHook

当前 hook 已经由 Wasmtime Component export，宿主只负责传递事件和校验输出：

```rust
#[async_trait::async_trait]
pub trait AgentHook: Send + Sync {
    fn matches(&self, event: &AgentEvent) -> bool;

    async fn on_event(
        &self,
        event: &AgentEvent,
        context: &HookContext,
        cancel: &CancellationToken,
    ) -> Result<HookResult>;
}
```

`WasmHookRuntime` 只接受 `list-hooks` 和 `invoke-hook` 的 JSON metadata。hook
如需执行命令，必须由 provider 自己实现 command 语义并调用显式的
`invoke-tool` 或 raw process capability；宿主不会把 hook 隐式升级成 `exec` 调用。

### 5.6 SessionStore

`session.rs` 当前同时包含领域模型和持久化实现。第一阶段只抽持久化边界：

```rust
#[async_trait::async_trait]
pub trait SessionStore: Send + Sync {
    async fn load_all(&self) -> Result<Vec<Session>>;
    async fn save_all(&self, sessions: &[Session]) -> Result<()>;
}
```

现有 JSON 文件实现为 `JsonSessionStore`。不要第一阶段允许插件替换 session
格式，因为 session 是 GUI、transcript、context carry-over 和恢复逻辑共同依赖的
数据结构。

### 5.7 JobRuntime

`JobRegistry` 已经是一个独立的后台任务运行时，应先包一层：

```rust
#[async_trait::async_trait]
pub trait JobRuntime: Send + Sync {
    fn list(&self) -> Vec<JobSnapshot>;
    async fn kill(&self, id: &str, reason: Option<&str>) -> Result<()>;
    fn drain_notifications(&self) -> Vec<String>;
}
```

Wasm 插件可以创建的 job 必须由宿主登记。插件退出时，PluginManager 负责取消该
插件创建的所有 job。

## 6. AgentRuntime 重构

### 6.1 新的依赖集合

当前 `Agent` 的构造参数过多且直接绑定具体实现。建议引入：

```rust
pub struct AgentServices {
    pub llm: Arc<dyn LlmProvider>,
    pub tools: Arc<dyn ToolRuntime>,
    pub jobs: Arc<dyn JobRuntime>,
    pub prompts: Arc<dyn PromptPipeline>,
    pub hooks: Arc<dyn HookRuntime>,
    pub context: Arc<dyn ContextPolicy>,
}
```

`Agent` 变为：

```rust
pub struct Agent {
    services: Arc<AgentServices>,
    settings: Arc<RwLock<ToolSettings>>,
    working_directory: PathBuf,
    system_prompt: String,
    context_settings: ContextSettings,
}
```

这里的 `Agent` 仍然是 native loop，不是 Wasm plugin。它依然负责：

- 组织 system/history/user messages；
- 处理 assistant stream；
- 根据 tool calls 继续循环；
- 把工具结果加入消息；
- 处理 cancellation；
- 发送 `AgentEvent`。

它不再直接知道：

- `LlmClient` 的 HTTP 细节；
- 工具 registry 的内部 map；
- MCP JSON-RPC 或 transport framing；
- provider 的 hook/MCP 声明格式；
- skills 的目录；
- JobRegistry 的实现。

### 6.2 dispatch 迁移

当前 `Agent::dispatch` 中以下逻辑移动到 `RegistryToolRuntime`：

- `registry.require(name)`；
- `descriptor()`；
- schema validation；
- `ToolSettings` clone；
- project working directory 注入；
- host timeout；
- cancellation；
- output truncation；
- `AuditOutcome` 计算。

Agent 只保留：

```rust
let result = self
    .services
    .tools
    .execute(call, &tool_context, cancel)
    .await;
```

这样 Wasm tool、MCP tool 和 builtin tool 对 Agent 都是同一种调用。

### 6.3 hook 迁移

当前 `run_hooks` 从 Agent 移到 `HookRuntime`：

```rust
pub struct CompositeHookRuntime {
    native: Vec<Arc<dyn AgentHook>>,
    wasm: Vec<Arc<dyn AgentHook>>,
}
```

执行顺序必须固定：

1. Wasm hooks 按 plugin id、hook id 排序；
3. 同一个 hook 内不并行；
4. 被拒绝的工具调用不触发 PostToolUse；
5. hook 输出继续附加到 tool output；
6. hook 失败不让整个 Agent run 失败，但必须写入 tool output 和 tracing。

## 7. Wasm Component ABI

### 7.1 WIT package 与版本

建议第一版使用：

```wit
package deluxe:harness@0.1.0;
```

ABI 版本不跟随应用版本自动变化。只有 WIT 的兼容边界变化时才升级 package
版本。manifest 中同时记录：

```json
{
  "apiVersion": "deluxe.harness/plugin@0.1",
  "minHostVersion": "0.1.0"
}
```

### 7.2 当前 MVP WIT

仓库中的实际 ABI 位于 `wit/deluxe-harness.wit`，当前 world 是：

```wit
package deluxe:harness@0.1.0;

interface host {
    read-plugin-file: async func(path: string) -> result<list<u8>, string>;
    write-plugin-file: async func(path: string, contents: list<u8>)
        -> result<_, string>;
    list-tools: async func() -> string;
    invoke-tool: async func(name: string, arguments-json: string)
        -> result<string, string>;
    spawn-process: async func(
        command: string,
        arguments-json: string,
        cwd: string,
        environment-json: string
    ) -> result<u64, string>;
    process-write: async func(handle: u64, data: list<u8>)
        -> result<_, string>;
    process-read: async func(handle: u64, max-bytes: u32)
        -> result<list<u8>, string>;
    process-close: async func(handle: u64);
    http-request: async func(
        method: string,
        url: string,
        headers-json: string,
        body: string
    ) -> result<string, string>;
    http-read: async func(handle: u64, max-bytes: u32)
        -> result<list<u8>, string>;
    http-close: async func(handle: u64);
}

interface plugin {
    configure: async func() -> result<_, string>;
    list-tools: async func() -> string;
    execute-tool: async func(name: string, arguments-json: string)
        -> result<string, string>;
    list-hooks: async func() -> string;
    invoke-hook: async func(hook-id: string, event-json: string)
        -> result<string, string>;
    open-surface: async func(request-json: string)
        -> result<string, string>;
    handle-action: async func(action-json: string)
        -> result<string, string>;
    close-surface: async func(surface-id: string);
}

world harness-plugin {
    import host;
    export plugin;
}
```

第一版故意把工具描述、工具输出和 UI snapshot 放在 UTF-8 JSON 字符串中。这样
WIT 负责稳定的函数、方向和异步语义，而宿主可以复用现有的
`ToolDescriptor`、`ToolOutput` 和 MCP/OpenAI JSON 形状。

当前导出约定：

- `list-tools` 返回 `Vec<ToolDescriptor>` 的 JSON；
- `execute-tool` 返回 `ToolOutput` 的 JSON；
- `open-surface` 和 `handle-action` 返回 `PluginUiDocument` 的 JSON；
- `close-surface` 没有返回 payload；
- guest 错误先作为 WIT 的 `result` error 返回，再映射为稳定的
  `AgentError`，不会直接进入 GUI 或触发宿主 panic。

当前导入约定：

- `host.read-plugin-file` 只接受当前 global/project configuration root 下的相对
  路径，并原样返回文件字节。这里的 configuration root 不是
  `plugin.wasm` 所在的插件包目录。project root 是当前项目目录，global root 是
  插件配置目录 `~/.agents` 而非整个 home，避免把 `.ssh`、`.aws` 等无关文件
  暴露给通用读取能力；
- `host.write-plugin-file` 用同一套相对路径规则替换 scope root 内的文件，且必须在
  manifest 里声明 `permissions.writePluginFiles = true`，否则返回 permission denied。
  写入先落到同目录的临时文件再 rename，读方不会看到半截配置；路径检查在
  canonicalize 父目录之后进行，父目录符号链接跳出 scope 时会被拒绝。这是 MCP
  Component 卸载 server 时改写 `.mcp.json` 的途径——Wasm 没有 WASI，写配置只能经
  过宿主；
- `host.list-tools` 和 `host.invoke-tool` 用于受 allowlist 约束的宿主工具能力；
- `host.spawn-process`、`host.process-*` 提供 provider-owned stdio 的原始字节；
- `host.http-*` 提供 provider-owned HTTP response 的原始字节；
- 宿主在每次调用时检查 manifest 的 `permissions.invokeTools`；
- 调用会经过正常的 `ToolRuntime` dispatch、schema 校验、工作目录、超时、
  cancellation 和输出限制；
- process/HTTP 请求只检查 manifest 中通用的 `processCommands`/`networkHosts`
  capability allowlist、句柄数量和 payload 上限；宿主不读取或解析 MCP 配置；
- Wasm 没有 WASI import，因此不能直接访问文件系统、网络、进程、环境变量或
  keyring。

当前 Component export 包括通用的 `list-hooks`/`invoke-hook` 和
`list-tools`/`execute-tool`。MCP Component 把 `tools/list` 的结果转换成普通
Wasmtime tool descriptor，并在 `execute-tool` 内完成 `tools/call`。
MCP `initialize`、`initialized`、`tools/list`、`tools/call`、JSON-RPC、
stdio line framing 以及 HTTP/event-stream framing 全部由 MCP Component 自己实现；
宿主只搬运 raw bytes 和执行通用 capability 边界。

宿主还会验证 JSON 的 identity、revision、节点数、深度、文本大小、control id、
声明过的 action 和 select option。guest 不能伪造宿主拥有的图片引用或 patch
hunk。

### 7.3 为什么第一版使用 JSON 字符串

当前工具协议已经使用 JSON：

- OpenAI tool arguments 是 JSON 字符串；
- `ToolOutput` 包含文本、错误、截断和审计相关字段；
- MCP input schema 是完整 JSON Schema；
- 当前 `ObjectSchema` 只表达根级别的一小部分结构。

因此 JSON payload 能复用现有工具协议，避免把 JSON Schema 和 UI 细节过早冻结
在 WIT 中。代价是所有边界都必须有明确上限和解析校验：

1. 参数必须是有效 JSON；
2. descriptor 必须是合法的 object schema，工具名和数量有上限；
3. `ToolOutput` 只能包含宿主当前允许的文本 block；
4. UI document 必须符合声明的 surface/action 和 revision 规则；
5. response、文本和节点树都必须受 payload/resource limit 约束。

### 7.4 后续 ABI 扩展

当前 WIT 尚未提供 `emit-event`、prompt provider 或 agent driver。它们不是当前
ABI 的隐含能力，也不能通过字符串 payload 绕过宿主权限。后续应在单独的、版本化
world 中增加，例如：

```wit
world prompt-plugin {
    export prompt-provider;
}

world hook-plugin {
    import host;
    export hook-provider;
}

world agent-plugin {
    import host;
    export agent-driver;
}
```

按能力拆 world 可以让插件只请求自己需要的 imports，且不会让 GUI、session、
LLM 或 agent loop 变成第一版 ABI 的隐式依赖。

## 8. Wasm 插件 Manifest

Wasmtime 插件使用根目录的 `plugin.json` 声明其 Component：

```text
example-plugin/
├── plugin.json
├── plugin.wasm
├── README.md
└── assets/
```

`plugin.json`：

```json
{
  "name": "example-echo",
  "version": "0.1.0",
  "interface": {
    "displayName": "Example Echo",
    "shortDescription": "A small Component plugin"
  },
  "runtime": {
    "type": "wasm",
    "module": "plugin.wasm",
    "apiVersion": "deluxe.harness/plugin@0.1",
    "ui": {
      "surfaces": ["main"],
      "actions": ["echo"]
    },
    "permissions": {
      "invokeTools": ["read_file"]
    }
  }
}
```

字段规则：

- `name`：插件的短 id；完整运行时 id 仍是
  `name@marketplace`；
- `version`：插件自己的 SemVer；
- `runtime.type`：当前必须是 `wasm`；
- `runtime.module`：相对插件 root 的 Component 文件；
- `runtime.apiVersion`：WIT ABI 版本；
- `runtime.ui`：允许暴露给宿主 UI 的 surface/action 名称；
- `runtime.permissions.invokeTools`：宿主 tool capability allowlist；
- `runtime.permissions.processCommands`：允许 Component 启动的进程命令；
- `runtime.permissions.networkHosts`：允许 Component 访问的网络主机；
- `runtime.permissions.writePluginFiles`：是否允许 Component 替换 scope root 内的
  配置文件；未声明时只能读。MCP Component 声明它为 `true`，因为它要卸载 server。

MCP server 的 `command`、`args`、`cwd`、`env`、`url` 和其他字段由 MCP
Component 通过 `read-plugin-file(".mcp.json")` 从当前 configuration root
读取。Hooks matcher、command 和 command 参数由 Hooks Component 通过
`read-plugin-file(".hooks.json")` 从当前 configuration root 读取。这些文件是
global/project scope 的宿主配置资源，不属于插件包，也不需要重复写入 manifest。

scope 仍由 marketplace 和 `PluginSettings` 决定，不由 Wasm manifest 自己声明。
global marketplace 的插件会进入所有适用项目的 catalogue；project-scoped
marketplace 的插件只会进入对应 project 的 catalogue。`PluginCatalogue::for_project`
先合并 global plugins，再合并该项目的 plugins，并按 id 让项目级插件覆盖同 id 的
global plugin。宿主把 global Component 绑定到全局 configuration root（`~/.agents`），
把 project Component 绑定到项目 configuration root；disabled
plugin 不会进入 runtime。项目级插件覆盖同 id 的全局插件时，使用项目实例对应的
项目 scope 配置。

manifest 解析采用当前 `src/plugins/manifest.rs` 的容错原则：

- 未知字段忽略；
- 一个插件的 manifest 错误不阻止其他插件加载；
- path 必须相对插件 root 解析；
- entry 不得跳出插件 root；
- 缺少 `name` 或合法的 `runtime` 字段时直接跳过整个插件，不进入其他加载路径；
- `runtime.module` 只能解析为插件 root 内的真实文件，symlink/path escape 会被
  拒绝；
- 插件加载失败必须有 tracing warning。

## 9. PluginManager

### 9.1 PluginManager 的职责

`PluginManager` 负责：

1. 发现已启用插件；
2. 根据 global/project scope 解析插件；
3. 为每个 Component 实例绑定 global 或 project scope 根目录；
4. 提供通用的受限 `read-plugin-file` host import；
5. 校验 manifest；
6. 检查 WIT API 版本；
7. 建立依赖顺序；
8. 加载 `Component`；
9. 为每个插件创建 runtime instance；
10. 处理启用、禁用、卸载和 reload；
11. 关闭插件时取消它创建的 job 和请求。

PluginManager 不负责：

- Agent loop；
- GUI 绘制；
- 将插件输出直接写入 session；
- 替插件决定工具参数是否合法；
- 绕过宿主安全策略。

### 9.2 运行时对象

建议使用以下层次：

```rust
pub struct PluginManager {
    engine: Arc<Engine>,
    catalog: Arc<PluginCatalogue>,
    components: BTreeMap<PluginId, Arc<Component>>,
}

pub struct PluginInstance {
    id: PluginId,
    scope: PluginScope,
    component: Arc<Component>,
    state: Arc<PluginStateHandle>,
}

pub struct PluginState {
    pub id: PluginId,
    pub project: PathBuf,
    pub permissions: Permissions,
    pub jobs: Arc<dyn JobRuntime>,
    pub events: Arc<dyn EventSink>,
}
```

`Component` 可以缓存。`Store` 不应跨不同 project 复用，因为 Store 中会包含：

- 当前 project；
- capability 权限；
- 插件实例资源；
- cancellation 状态；
- job ownership。

### 9.3 Store 与异步调用

每个 PluginInstance 应由一个独立 actor 或受控 mutex 管理，避免多个 async 调用
同时借用同一个 Wasmtime Store。

推荐模型：

```text
WasmTool.execute
  -> 发送 Execute 消息到 PluginActor
     -> actor 独占 Store
     -> 调用 component export
     -> host import 回调 CapabilityHub
     -> 返回 ToolOutput
```

不要在 `WasmTool.execute` 中直接拿一个长期持有的 `Mutex<Store>`，然后在持锁期间
等待 host import 的异步 I/O。host import 回调需要再次访问宿主服务时，容易产生：

- Store 锁与 capability 锁互相等待；
- 插件调用宿主工具时死锁；
- cancellation 无法及时传播。

第一版可以采用单插件串行 actor。一个插件的多次调用排队，但不同插件之间可以
并行。

## 10. Wasmtime 资源和安全模型

### 10.1 Engine 配置

实现时需要配置：

- Component Model；
- async support；
- epoch interruption；
- fuel consumption；
- 序列化/反序列化策略；
- debug/release 的日志差异。

具体 Wasmtime crate 版本应与 Rust `1.88` 和项目锁文件一起确定，不在文档中固定
一个可能过期的版本号。加入依赖前先确认：

```toml
wasmtime = { version = "...", features = ["component-model", "async"] }
wasmtime-wasi = "..."
```

如果第一版不需要 WASI，则不要引入完整 WASI capability，只使用 component runtime
和自定义 host imports。

### 10.2 超时、取消和 CPU 限制

一次插件调用至少有三层限制：

1. Tokio host timeout；
2. `CancellationToken`；
3. Wasmtime epoch/fuel。

三者职责不同：

- Tokio timeout 限制宿主异步 I/O 总时长；
- cancellation 响应用户取消和插件卸载；
- epoch/fuel 终止纯 Wasm CPU 死循环。

epoch/fuel 不能替代 Tokio timeout。插件调用宿主 `invoke-tool` 后，真正阻塞的可能
是文件、进程、MCP 或 HTTP I/O，必须由宿主服务自己处理超时。

取消路径：

```text
用户点击停止
  -> Cmd::Cancel
  -> run CancellationToken.cancel()
  -> AgentRuntime 停止当前循环
  -> ToolRuntime 停止等待
  -> PluginActor 收到 cancel
  -> epoch/future 返回 cancelled
  -> 清理插件 job
```

### 10.3 内存和实例上限

每个插件实例设置：

- 最大 Wasm linear memory；
- 最大 table elements；
- 最大 concurrent instances；
- 最大返回 payload；
- 最大 event payload；
- 最大 plugin-created jobs；
- 最大 plugin log message。

超过限制统一返回 `plugin_resource_limit`，不允许转成宿主 panic。

### 10.4 WASI 权限

默认配置：

```text
filesystem = none
network = none
process = none
environment = filtered
stdin/stdout = not exposed
```

如未来确实需要 WASI：

- 只预开放插件 root 的只读目录；
- 写入必须经过宿主文件服务；
- 不开放任意绝对路径；
- 不开放 `Command::spawn`；
- 不把 API key 放入插件环境变量；
- 不把完整宿主环境变量导入 Wasm。

当前项目的 `ToolSettings::resolve` 没有 containment check，因此不能直接拿它当
Wasm sandbox。Wasm 插件文件访问必须增加独立的 capability 检查，不能仅依赖
`ToolSettings::resolve`。

### 10.5 错误和 panic

插件侧的 trap、invalid return、out of fuel、超时和资源限制全部映射为稳定错误码：

```text
plugin_load_failed
plugin_api_mismatch
plugin_trap
plugin_timeout
plugin_cancelled
plugin_resource_limit
plugin_invalid_output
plugin_permission_denied
```

宿主非测试代码继续禁止 `unwrap()`、`expect()` 和 `panic!()`。Wasm 调用返回的
任何数据都必须经过显式解析和错误映射。

## 11. CapabilityHub

Wasm 插件不直接依赖 `ToolRegistry`，而依赖按权限过滤后的 `CapabilityHub`：

```rust
pub struct CapabilityHub {
    pub project: PathBuf,
    pub permissions: Permissions,
    pub tools: Arc<dyn ToolRuntime>,
}
```

`invoke-tool` 调用流程：

1. 解析工具名和 JSON 参数；
2. 检查插件 manifest 的 allowlist；
3. 对宿主工具重新执行正常的 schema validation；
4. 注入当前 project 的 `ToolContext`；
5. 应用当前 host timeout 和 cancellation；
6. 执行工具；
7. 将结果转换成 `ToolOutput` JSON；
8. 限制返回大小；
9. 返回插件。

插件不能通过 `invoke-tool` 调用不存在的工具，也不能通过字符串拼接绕过
`block_destructive_commands`。权限检查发生在每次调用，而不是只在插件加载时检查。

## 12. ToolRegistry 集成方案

### 12.1 WasmTool adapter

新增：

```rust
pub struct WasmTool {
    plugin_id: PluginId,
    descriptor: ToolDescriptor,
    instance: Arc<PluginInstance>,
}
```

实现：

```rust
#[async_trait::async_trait]
impl Tool for WasmTool {
    fn descriptor(&self) -> ToolDescriptor {
        self.descriptor.clone()
    }

    async fn execute(
        &self,
        arguments: Value,
        settings: &ToolSettings,
    ) -> Result<ToolOutput> {
        self.instance
            .execute_tool(&self.descriptor.name, arguments, settings)
            .await
    }
}
```

`WasmTool` 负责 ABI 转换和插件调用，但不重复实现：

- schema validation；
- host timeout；
- output truncation；
- audit outcome；
- destructive command guard。

这些仍由 `ToolRuntime`/Agent dispatch 统一处理。

### 12.2 注册顺序

当前 Worker 创建 Agent 时的注册顺序是：

```text
host_registry = native built-in implementations
  -> 根据 model modality加入 read_image
registry = empty_with_jobs(host_registry.jobs())
  -> 当前 project 的 Wasmtime Components list-tools/execute-tool
  -> 注册 task
  -> 冻结 ToolRuntime 和 prompt snapshot
  -> 创建 Agent
```

冲突处理：

- bundled provider 先注册，保证 host built-in tool 名称稳定；
- 其他 Component 和 MCP tool 保持“第一个注册者胜出”；
- Wasm 工具不能覆盖已有工具；
- 冲突写 warning；
- prompt 只列出最终注册成功的工具。

### 12.3 Agent role 与 task

当前不让 Wasm 插件直接创建任意 `Agent`。Wasm 插件如果需要子 Agent：

- 先导出静态 role metadata；
- 由宿主现有 `task` 工具调度；
- role 的工具 registry 仍由宿主创建；
- 子 Agent 不继承 plugin hooks，沿用当前防止 hook 重复执行的规则。

等 `AgentDriver` ABI 稳定后，再增加独立的 `agent-plugin` world。

## 13. Wasm Provider 统一模型

当前 `LoadedPlugin` 承载资源发现结果、scope 和插件 root。可执行能力统一由
Wasmtime Component 产生：

```rust
pub struct PluginDescriptor {
    pub id: PluginId,
    pub display_name: String,
    pub version: Option<String>,
    pub scope: PluginScope,
    pub root: PathBuf,
    pub source: PluginSource,
    pub capabilities: Vec<CapabilityKind>,
}

pub enum PluginRuntime {
    WasmtimeComponent,
}
```

然后由 provider 产生 capability：

```text
Agent
  -> ToolRuntime adapter
     -> ComponentActor
        -> Wasmtime tool export
        -> MCP Component 的通用 tool export
           -> JSON-RPC / initialize / tools/list / tools/call
           -> stdio byte framing or HTTP/event-stream framing
              -> CapabilityHub raw process/HTTP
```

宿主没有 `McpClient`、`Transport`、`McpServerConfig` 或 native MCP tool
registration。MCP Component 自己通过 `read-plugin-file(".mcp.json")` 读取并解析
server declaration，自己拥有协议、握手、消息 framing、transport 状态和生命周期。

```text
Wasmtime plugin
  -> skills / commands / agents
  -> Component tools
  -> Hooks Component + read-plugin-file(".hooks.json")
  -> MCP Component + read-plugin-file(".mcp.json")
  -> MCP JSON-RPC and handshake
  -> stdio / HTTP transport framing
  -> declarative UI surface
```

没有合法 Wasmtime runtime 的目录不会进入 catalogue。只有 Component 自己请求时才
会读取 `.hooks.json` 或 `.mcp.json`；宿主不根据文件名或 Component 职责预先读取它们。
global Component 从全局 configuration root 读取，project Component 从项目
configuration root 读取；项目级同 id 覆盖 global plugin 时，使用项目实例对应的
项目 configuration root。

## 14. 分阶段实施计划

### Phase 0：文档和 fixture

目标：

- 固定本设计；
- 建立一个不依赖网络的 Wasm echo fixture；
- 不改变生产行为。

交付：

```text
docs/wasmtime-plugin-implementation.md
wit/deluxe-harness.wit
plugin-fixtures/echo-tool/
```

验收：

- fixture 能编译为 Component；
- manifest 能被解析；
- 当前 `cargo test` 无行为变化。

### Phase 1：抽宿主 trait

新增：

```text
src/harness/types.rs
src/harness/services.rs
src/harness/events.rs
src/harness/prompt.rs
src/harness/runtime.rs
```

工作：

1. `LlmClient` 包装成 `NativeLlmProvider`；
2. `ToolRegistry` 包装成 `RegistryToolRuntime`；
3. `JobRegistry` 包装成 `NativeJobRuntime`；
4. 当前 session JSON 实现包装成 `JsonSessionStore`；
5. `Agent` 改为依赖 `AgentServices`；
6. `build_system_prompt` 改为依赖 `PromptPipeline`；
7. `Agent::dispatch` 只调用 `ToolRuntime`；
8. `Agent::run_hooks` 移到 `HookRuntime`。

验收：

- 现有 Agent loop 测试全部通过；
- 现有 hook、MCP、task、image 和 compaction 测试全部通过；
- 生成的 system prompt 字节保持不变；
- 工具 schema 顺序保持不变；
- 不增加 GUI 主线程工作。

### Phase 2：PluginManager 和统一 provider

工作：

1. 保留现有 `PluginCatalogue` 发现逻辑；
2. 增加 `PluginDescriptor`；
3. 将 Wasm component plugin 接入通用 Component runtime；
4. 为 Component 绑定当前 global/project configuration root，并暴露通用
   `read-plugin-file`；
6. Worker 只向 PluginManager 请求 project scope 的 capabilities；
7. `SetPlugins` 后清理旧 Agent 和旧 plugin instances。

验收：

- global/project shadowing 行为保持；
- disabled plugin 永远不进入运行时；
- project plugin 不泄漏；
- MCP server 生命周期与 Agent 生命周期一致；
- reload 不遗留旧进程或旧 job。

### Phase 3：Wasmtime tool、hook 和 MCP Component

工作：

1. 加入 Wasmtime Component 依赖；
2. 实现 `WasmRuntime`；
3. 实现 WIT bindings；
4. 实现 `WasmPluginLoader`；
5. 实现 `WasmPlugin::list_tools`；
6. 实现 `WasmTool` adapter；
7. 实现 `host.invoke-tool`；
8. 实现 timeout、cancel、fuel/epoch、memory limit；
9. 将 Wasm tools 注册进现有 `ToolRegistry`；
10. 由 Hooks Component 解析 `.hooks.json` matcher 并执行 hook command；
11. 由 MCP Component 解析 `.mcp.json`，实现 MCP loading、initialize、JSON-RPC、
    stdio/HTTP transport framing 和 MCP tool adapter。

MVP 当前允许：

- `list-tools`；
- `execute-tool`；
- 受权限控制的 `host.invoke-tool`；
- project-scoped 的 declarative UI surface；
- UI action 的 revision、control、enabled 和 option 校验。

当前不包含 `emit-event`、Wasm prompt、Wasm-created job 或完整 WASI。Hooks 和
MCP 已属于当前 Component ABI，不再恢复宿主内置的 Hooks/MCP executor。

验收：

- echo tool 可以被模型调用；
- Wasm tool 的参数校验、超时、截断和审计沿用 native `ToolRuntime`；
- 插件无法读写未授权路径；
- 插件无法调用未授权工具；
- 插件不会在 GUI 主线程运行；
- UI snapshot 只能作为 validated immutable document 进入 renderer。

fuel、memory/table limits、trap、cancel 和重新实例化已经接入 runtime；对应的
压力和故障 fixture 仍是后续测试工作。

### Phase 4：Wasm prompt 和更广泛的 provider 能力

工作：

1. 扩展 WIT prompt provider；
2. 加入 prompt section id、priority 和稳定排序；
3. 扩展 provider-owned event、job 或其他明确版本化能力；
4. 统一 provider 输出和错误展示。

验收：

- prompt 前缀稳定；
- provider 输出和错误展示保持稳定；
- provider-owned event、job 等新增能力具有独立的 ABI 版本和权限边界。

### Phase 5：可替换 context/session/agent driver

只有前面阶段稳定后才做：

- `ContextPolicy`；
- `ContextCompactor`；
- `SessionStore` provider；
- `AgentDriver` provider；
- plugin-created sub-agent。

这一步风险最大，必须单独提交，不能与 Wasm tool MVP 混在一起。

## 15. 测试计划

### 15.1 单元测试

新增测试位置：

```text
src/harness/*.rs 底部
src/plugins/wasm.rs 底部
src/plugins/wasm_runtime.rs 底部
```

必须覆盖：

- WIT JSON payload 编解码；
- manifest 默认值和未知字段；
- entry 路径不能跳出插件 root；
- API version 不兼容；
- tool descriptor 转换；
- output 截断；
- unknown output block；
- plugin error 到 `AgentError` 的映射；
- permission allowlist；
- tool name 冲突；
- plugin scope 合并；
- disabled plugin 不进入 runtime；
- plugin reload 释放旧实例。

### 15.2 Wasm fixture 测试

`plugin-fixtures/echo-tool` 至少提供：

1. `echo`：原样返回参数；
2. `surface`：打开、处理 action 并关闭一个 UI snapshot；
3. `invoke-read-file`：请求宿主调用允许的 `read_file`；
4. `invoke-denied-tool`：请求未授权工具；
5. `large-output`：返回超过上限的内容；
6. `trap`：主动 trap；
7. `infinite-loop`：触发 fuel 限制。

当前仓库已提交离线 `echo` Component fixture；其 componentizer 与 guest 使用
和 Wasmtime 37 兼容的 `wasmparser/wit-* 0.239` 工具链，避免生成运行时无法
解析的新 Component type 编码。

构建产物也可以携带默认 Wasmtime 插件包。主程序通过 `include_bytes!` 嵌入
Component 及其 manifest，首次启动时将它们写入普通的
`.codex/plugins/cache/<marketplace>/<plugin>/<version>/` 缓存目录，再沿用同一套
discovery、scope、启停、UI 和卸载逻辑。当前预置的 MCP Component id 是
`mcp@deluxe-defaults`；它不携带 `.mcp.json`，运行时从当前 global/project scope
根目录主动请求配置。
用户明确将该插件设为 `enabled = false` 后，后续启动不会重新安装或强制启用它。

测试不能访问真实 home、keyring 或网络。fixture 的文件读取使用临时目录。

### 15.3 Agent loop 集成测试

复用 `src/agent_loop_tests.rs` 的 `FakeServer`，新增场景：

- 模型调用 Wasm tool 后得到 tool result；
- Wasm tool 失败后模型收到 error output 并继续；
- Wasm tool 的 hook 输出加入同一 tool result；
- plugin tool 与 builtin tool 同一回合调用；
- plugin tool timeout 后 Agent 继续运行；
- run cancel 终止正在等待的 plugin tool；
- UI surface 的 snapshot 和 action 经过 IPC，但不会写入 agent transcript；
- plugin-created job 和 plugin event 尚未暴露。

### 15.4 安全测试

必须有负向测试：

- plugin manifest 的 `entry = "../outside.wasm"` 被拒绝；
- plugin 不得读取宿主 API key；
- plugin 不得调用未授权 `exec`；
- plugin 不得通过 `invoke-tool` 绕过 destructive guard；
- plugin 不得把超大 JSON 返回给 GUI；
- plugin trap 不得让 tokio worker 退出；
- plugin infinite loop 不得卡住其他 plugin；
- project plugin 不得出现在另一个项目的 registry；
- disabled plugin 不得注册 tool、hook 或 prompt。

## 16. 提交拆分建议

每个提交只完成一个可验证行为：

```text
1. Add harness domain types and service traits
2. Adapt the native LLM client to LlmProvider
3. Move tool dispatch behind ToolRuntime
4. Move hooks behind HookRuntime
5. Add PluginManager capability resolution
6. Add the harness WIT package and echo fixture
7. Add Wasmtime component loading
8. Register Wasm tools in the tool registry
9. Add Wasm cancellation and resource limits
10. Add Wasm plugin integration tests
11. Add Wasm prompt and hook providers
```

每个提交都应满足：

```text
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

不得把 `Cargo.lock`、`vendor/egui-winit`、GUI 重构和 Wasm ABI 设计混在一个提交中。

## 17. 迁移期间的兼容规则

### 17.1 工具协议

当前的 `Tool` 和 `ToolDescriptor` 继续作为宿主内部兼容接口。Wasm descriptor
通过 adapter 转换，不直接改变当前字段。

### 17.2 Prompt

当前 prompt 的固定顺序必须保留：

```text
identity
tools
skills
agents
rules
project context
```

新 provider 只能在明确的插入点贡献内容，不能自行把内容插入 identity 或 rules
的中间位置。除非有专门的 prompt version migration，否则同一组插件生成的 prompt
必须保持稳定。

### 17.3 Event

GUI 暂时继续接收当前 `ipc::Event`。新增 `AgentEvent` 后由 worker 做 adapter：

```text
AgentEvent -> UiEvent/Event -> Cmd/Event channel -> App
```

Wasm plugin event 不得直接构造 GUI 内部 `Event`。插件事件先进入 Agent event bus，
再由 UI adapter 决定是否展示。

### 17.4 错误

保留 `AgentError` 作为宿主统一错误类型。Wasm 错误只增加新的稳定 `code` 常量，
不暴露 Wasmtime 的长错误堆栈给模型。详细 trap 信息写 tracing，模型收到短、可行动
的错误消息。

## 18. 已知风险与处理方式

### 18.1 Wasmtime 依赖体积

Wasmtime 会明显增大 release binary。处理方式：

- 只启用 Component Model 和需要的 async feature；
- 不默认启用完整 WASI；
- 评估 release `opt-level = "z"` 下的体积；
- 如果未来支持无插件构建，再考虑 Cargo feature；
- 不能为了体积删除当前 `vendor/egui-winit` patch。

### 18.2 Store 与 async 借用

这是最容易产生死锁和不可取消等待的地方。必须优先使用 PluginActor 模型，
而不是在多处共享 `Mutex<Store>`。

### 18.3 插件 ABI 过早冻结

第一版 WIT 只覆盖 tool plugin 的最小能力。不要一次加入 session、LLM、GUI、
filesystem 和 agent loop，否则每个细节都会变成 ABI 兼容负担。

### 18.4 插件权限被 native path 绕过

插件所有 I/O 都必须经过 CapabilityHub。不能把 plugin root 直接映射成完整 WASI
目录后，再假设插件会遵守 manifest 权限。

### 18.5 Prompt cache 失效

插件排序、工具排序、section 排序和序列化格式都必须稳定。任何依赖 hash map
无序遍历的输出都必须改成 `BTreeMap` 或显式排序。

### 18.6 Component 职责与 Wasmtime manifest

所有插件都通过根目录的 `plugin.json` 声明 Wasmtime Component。manifest 不声明
`general`、`hooks` 或 `mcp` 之类的宿主插件类型。Hooks Component 和 MCP
Component 通过同一个通用 ABI 暴露 hooks/tools，并主动调用 `read-plugin-file`
读取当前 global/project configuration root 的配置文件。插件页面统一展示 Wasmtime Component，宿主不根据
配置文件存在与否推断插件职责。

## 19. 第一版完成标准

当前已经完成的 MVP 验收项：

- [x] 文档中的当前 ABI 与 `wit/deluxe-harness.wit` 一致；
- [x] `plugin-fixtures/echo-tool` 可在离线测试中加载、注册并执行；
- [x] project-scoped catalogue 能隔离 Wasm plugin；
- [x] Wasm tool 能显示在 `<tools>` 和 OpenAI tools schema 中；
- [x] Wasm tool 能执行并返回受校验的 `ToolOutput`；
- [x] host tool invocation 经过权限和正常 tool dispatch；
- [x] plugin 不访问完整 WASI；
- [x] plugin 不会在 GUI 主线程运行；
- [x] UI surface 使用 IPC 和 validated immutable snapshot，不把 Wasmtime 带进
  renderer；
- [x] 插件页面打开或刷新时可以重新 discovery，并在打开 surface 时动态实例化
  当前插件根目录中的 Wasmtime Component；
- [x] 插件页面可以将选中的 Wasmtime 插件根目录导入全局或当前项目，并通过
  `name@deluxe-local` 登记后动态加载；
- [x] Wasmtime Component tool、MCP transport、hooks、task、image、compaction 的既有
  测试通过。

仍需继续补齐的运行时压力测试和扩展：

- [ ] timeout、cancel、fuel 和 memory/table limit 的专用 Wasm fixture；
- [ ] trap 后重新实例化、reload cleanup 和并发插件隔离的专用测试；
- [ ] prompt、event、plugin-created job ABI；
- [x] `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings` 和
  `cargo test` 在最终提交前全部通过；
- [x] 没有修改 `HOST_RULES`、`SUB_AGENT_RULES`、`ToolDescriptor` 的既有形状；
- [x] 没有删除 `Cargo.toml` 的 `egui-winit` patch 或 `vendor/egui-winit`。

## 20. 建议的下一步

当前 Phase 1 的 harness 解耦、声明式 UI surface 和 Wasmtime tool/hook/MCP/UI
Component MVP 已经落地。
后续工作按风险从运行时验证到 ABI 扩展推进：

1. 增加专用 Wasm fixture，覆盖 timeout、cancel、fuel、memory/table limit、
   trap 后重新实例化和超大输出；
2. 增加并发调用、插件 reload、项目切换和 runtime shutdown 测试，确认不同项目
   的 component actor、权限和 job 生命周期彼此隔离；
3. 将 UI surface 的临时 actor 接入 project runtime 的统一 component cache，避免
   同一个插件同时拥有 tool actor 和 surface actor；
4. 在 `agent_loop_tests.rs` 中加入 Wasm tool 的调用、失败恢复、取消，以及 UI
   action 不写入 agent transcript 的集成场景；
5. 在单独的 ABI 设计阶段加入 Wasm prompt 和 event provider，并为每个新
   world 保留版本号、稳定排序和明确的权限边界；
6. 最后评估 plugin-created job、context/session provider 等更大范围的扩展；
   这些能力不能与当前 Wasm tool MVP 共享未经验证的 ABI。

每一步都应保持资源 discovery、Wasmtime Component、agent loop 和 GUI 测试通过，并在完成后
重新运行 `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings` 和
`cargo test`。
