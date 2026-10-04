# Wasmtime 插件编写指南

本文面向 `deluxe-agent` 的插件作者，说明如何编写、构建、安装和调试一个 Wasmtime Component 插件。

本文只描述当前仓库已经实现的协议；**它不是通用的 Wasmtime 教程。要找这个教程就请另请其他 LLM 吧**

下文我们将把 Agent 本体简称为宿主。

当前插件 ABI 是 `deluxe.harness/plugin@0.1`，WIT 定义位于 [`wit/deluxe-harness.wit`](../wit/deluxe-harness.wit)。
Agent 宿主使用 Wasmtime Component Model 加载插件，不提供隐式 WASI 文件系统。插件需要文件、宿主工具、进程或网络能力时，必须通过 WIT 的 `host` import，并在 `plugin.json` 中声明相应权限。

## 1. 插件代码结构

一个可安装的插件根目录至少包含：

```text
example-plugin/
├── plugin.json
└── plugin.wasm
```

源码、测试、构建脚本可以放在根目录之外，或者在导入时一并复制；运行时真正读取的是根目录中的 `plugin.json` 和它声明的 Component 文件。
`runtime.module` 必须是插件根目录内的相对路径，不能使用绝对路径、`..`、Windows 盘符或跳出根目录的符号链接。

推荐的开发目录如下：

```text
example-plugin/
├── Cargo.toml
├── src/lib.rs
├── plugin.json
└── plugin.wasm       # 构建产物
```

插件名称和版本会参与缓存目录名，建议只使用 ASCII 字母、数字、`.`, `_`, `-`。

## 2. 编写 `plugin.json`

完整示例：

```json
{
  "name": "example-plugin",
  "version": "0.1.0",
  "interface": {
    "displayName": "Example Plugin",
    "shortDescription": "Provides a tool and a settings surface"
  },
  "runtime": {
    "module": "plugin.wasm",
    "apiVersion": "deluxe.harness/plugin@0.1",
    "ui": {
      "surfaces": ["settings"],
      "actions": ["save"]
    },
    "permissions": {
      "invokeTools": ["read_file"],
      "processCommands": [],
      "writePluginFiles": false
    }
  }
}
```

字段说明：

| 字段 | 必填 | 说明 |
| --- | --- | --- |
| `name` | 是 | 插件名唯一标识。导入后的本地插件 id 为 `name@deluxe-local`。 |
| `version` | 否 | 插件版本号；未填写时使用 `local`。|
| `interface.displayName` | 否 | UI 和日志中的显示名称。 |
| `interface.shortDescription` | 否 | 摘要。 |
| `runtime.module` | 是 | 根目录内的 Component 路径，通常为 `plugin.wasm`。 |
| `runtime.apiVersion` | 是 | 当前必须是 `deluxe.harness/plugin@0.1`。 |
| `runtime.ui` | 否 | 声明 UI surface 和 action，见第 6 节。 |
| `runtime.permissions` | 否 | 宿主能力 allowlist，见第 7 节。 |

请注意：没有列出的 tool、命令或主机，即使 Component 调用了对应 import，也会收到权限错误。不推荐使用通配符 `"*"`，除非插件确实需要任意命令或任意网络主机；

## 3. 实现 WIT Component

这里我们给出一个示例插件，示例插件的作用很简单：向 Agent 新增一个 Echo 工具，当 LLM 调用这个 Echo 工具时，将 LLM 的调用参数连接成字符串作为 Tool Result 返回。
Cargo.toml 如下所示，使用 wit-bingen 方便处理 Wasm Component 中麻烦的参数编码、导出函数和 ABI 细节。

```toml
[package]
name = "example-plugin"
version = "0.1.0"
edition = "2021"
publish = false

[lib]
crate-type = ["cdylib"]

[dependencies]
serde_json = "1"
wit-bindgen = "0.46"
```

在 `src/lib.rs` 中进行实现：

```rust
wit_bindgen::generate!({
    path: "../../wit",
    world: "harness-plugin",
    async: true,
});

use exports::deluxe::harness::plugin::Guest;

struct ExamplePlugin;

impl Guest for ExamplePlugin {
    async fn configure() -> Result<(), String> {
        Ok(())
    }

    async fn list_tools() -> String { // 在 Agent 里新增工具
        r#"[{"name":"example_echo","summary":"Echo input","description":"Returns input text","guidelines":[],"inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]},"hostValidatesArguments":true,"mutating":false}]"#.into()
    }

    async fn execute_tool(name: String, arguments_json: String) -> Result<String, String> {
        if name != "example_echo" {
            return Err(format!("unknown tool: {name}"));
        }
        let arguments: serde_json::Value =
            serde_json::from_str(&arguments_json).map_err(|error| error.to_string())?;
        let text = arguments
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        Ok(serde_json::json!({
            "content": [{"type": "text", "text": text}],
            "isError": false,
            "truncated": false
        })
        .to_string())
    }

    async fn list_event_handlers() -> String {
        "[]".into()
    }

    async fn handle_event(_handler_id: String, _event_json: String) -> Result<String, String> {
        Err("no event handlers".into())
    }

    async fn open_surface(_request_json: String) -> Result<String, String> {
        Err("no UI surface".into())
    }

    async fn handle_action(_action_json: String) -> Result<String, String> {
        Err("no UI action".into())
    }

    async fn close_surface(_surface_id: String) {}
}

export!(ExamplePlugin);
```

`configure` 在 Component 实例创建后调用一次，适合从宿主配置根读取并建立插件状态。插件状态属于该 Component 实例；**不要假设不同生效范围共享同一份内存。**

### 3.1 WIT 导出的函数

`wit/deluxe-harness.wit` 中的 `plugin` interface 包含：

| 导出 | 返回值 | 用途 |
| --- | --- | --- |
| `configure()` | `result<(), string>` | 初始化插件状态。 |
| `list-tools()` | JSON 字符串 | 返回插件工具描述。无工具返回 `[]`。 |
| `execute-tool(name, arguments-json)` | `result<string, string>` | 执行一个插件工具。 |
| `list-event-handlers()` | JSON 字符串 | 返回事件处理器描述。无处理器返回 `[]`。 |
| `handle-event(handler-id, event-json)` | `result<string, string>` | 处理一次 Agent 宿主事件（当前为 `tool.finished`）。 |
| `open-surface(request-json)` | `result<string, string>` | 打开一个 UI surface，返回快照。 |
| `handle-action(action-json)` | `result<string, string>` | 处理 UI action，返回新快照。 |
| `close-surface(surface-id)` | 无 | 释放 surface 资源。 |

所有 JSON 都是 UTF-8 字符串。未知字段通常会被 Agent 忽略。

### 3.2 可选的 `prompt` interface

除了 `plugin`，Component 还可以导出 `prompt` interface，让宿主把一段文本原样拼进
system prompt：

| 导出 | 返回值 | 用途 |
| --- | --- | --- |
| `prompt-sections()` | `result<string, string>` | 返回本生效范围要贡献的 prompt 文本；返回空串表示不贡献。 |

宿主不解析这段文本，也不规定它的排版；Component 自己决定读什么、怎么措辞。使用它
需要 world 同时 `export prompt`（见 `wit/deluxe-harness.wit` 的 `prompt-provider`
world）。平台按 `prompt-plugin` world 做可选绑定，绑定失败只意味着该 Component 不
贡献 prompt 文本，不影响它作为普通 `plugin` 加载。

## 4. 工具相关的协议

### 4.1 `list-tools`

返回一个 JSON 数组，每个元素对应一个宿主 `ToolDescriptor`。
示例如下：

```json
[
  {
    "name": "example_echo",
    "summary": "Echo input",
    "description": "Returns input text",
    "guidelines": [],
    "inputSchema": {
      "type": "object",
      "properties": {"text": {"type": "string"}},
      "required": ["text"]
    },
    "hostValidatesArguments": true,
    "mutating": false
  }
]
```

工具名必须唯一、稳定且适合放入模型的工具 schema。宿主会检查数量、标识符、JSON大小和重复名称；

### 4.2 `execute-tool`

`arguments-json` 是模型传入的 JSON 对象。成功时返回序列化后的 `ToolOutput`：

```json
{
  "content": [
    {"type": "text", "text": "hello"}
  ],
  "isError": false,
  "truncated": false
}
```

`content` 支持：

```json
{"type":"text","text":"..."}
```

和宿主已有的图片引用格式：

```json
{
  "type": "image",
  "image": {
    "id": "image-id",
    "mediaType": "image/png",
    "bytes": 1234,
    "width": 100,
    "height": 80
  }
}
```

失败结果仍应返回合法 `ToolOutput`，并将 `isError` 设为 `true`；协议错误、超大输出、未知 content block 或非法图片元数据会被宿主拒绝。单次 Component 调用和整个 UI payload 都有大小上限，建议插件在返回前主动截断文本。

## 5. 事件处理器协议

宿主只做通用的插件事件转发，不解释具体事件语义。插件通过`list-event-handlers` 声明订阅哪些事件，宿主把 agent 事件转发给订阅者，由插件自己决定匹配与输出。

### 5.1 `list-event-handlers`

返回数组，每项至少包含 `id`；推荐同时提供 `label` 和 `events`：

```json
[
  {
    "id": "format-after-write",
    "label": "Format after write",
    "events": ["tool.finished"]
  }
]
```

`events` 是订阅的事件 kind 列表，为空表示订阅所有 kind。id 必须稳定且唯一。
Agent 宿主按插件 id 和处理器 id 稳定执行，不要依赖并行执行或隐含的执行顺序。

### 5.2 `handle-event`

`event-json` 当前包含：

```json
{
  "kind": "tool.finished",
  "project": "C:/work/repo",
  "maxOutputChars": 20000,
  "payload": { "tool": "apply_patch", "output": "..." }
}
```

返回：

```json
{
  "output": "formatter output",
  "failed": false,
  "matched": true
}
```

不匹配的事件应返回 `matched: false`，处理器如果需要运行宿主命令，应通过 `host.invoke-tool` 调用已声明且被允许的工具；

## 6. UI Surface 协议

只有 manifest 中声明的 surface 和 action 才会被宿主暴露。`open-surface` 与`handle-action` 都返回 `PluginUiDocument`：

```json
{
  "schemaVersion": 1,
  "pluginId": "example-plugin@deluxe-local",
  "surfaceId": "settings",
  "revision": 1,
  "title": "Example settings",
  "root": {
    "type": "column",
    "children": [
      {
        "type": "button",
        "id": "save",
        "label": "Save",
        "action": "save",
        "enabled": true
      }
    ]
  }
}
```

支持的节点包括 `empty`、`column`、`row`、`section`、`text`、`button`、`textInput`、`checkbox`、`select`、`progress` 和 `divider`。控件 id 必须唯一，action 必须在 manifest 的 `runtime.ui.actions` 中声明。

每次 `handle-action` 都必须返回比上一次更大的 `revision`。宿主会校验：

- `schemaVersion`、`pluginId`、`surfaceId` 与请求一致；
- revision 单调递增且不能为 0；
- 节点数量、树深度、文本大小和总 payload 不超限；
- `select` 的当前值属于声明的选项；
- `progress.value` 是 `0..=1` 的有限数；
- action 只对应当前快照中的启用控件。

因此 action 处理必须视为“快照到快照”的纯协议：不要接受旧快照，也不要相信客户端自行构造的 control id 或 option value。

## 7. Host imports 与权限

WIT 的 `host` interface 提供以下能力：

| import | manifest 权限 | 说明 |
| --- | --- | --- |
| `read-plugin-file(path)` | 无额外开关 | 读取当前生效范围（项目级或全局级）下配置根目录内的文件。 |
| `write-plugin-file(path, contents)` | `writePluginFiles: true` | 写入当前生效范围配置根目录内的文件。 |
| `list-plugin-files(path)` | 无额外开关 | 列出配置根目录下某个相对目录内的普通文件，返回相对根目录、以 `/` 分隔并排序的路径；目录不存在时报 `plugin_file_not_found:`。 |
| `configuration-root()` | 无额外开关 | 返回本实例文件能力绑定的配置根目录绝对路径，供插件向模型展示绝对路径。 |
| `list-tools()` | 仅返回 `invokeTools` 中允许的工具 | 查询可调用的宿主工具描述。 |
| `invoke-tool(name, arguments-json)` | `invokeTools` | 按宿主正常工具运行时执行。 |
| `run-agent(request-json)` | 由宿主按需授予，不在 manifest 里声明 | 跑一次嵌套 Agent：请求带角色的名字、instructions、prompt 和是否后台；前台返回 `{"answer": ...}`，后台返回 `{"jobId": ...}`。宿主拥有模型循环、工具集和事件流。 |
| `spawn-process(...)` | `processCommands` | 启动插件拥有的 transport 进程。 |
| `process-write/read/close` | 对应已创建的句柄 | 进程的有界字节 I/O。 |
| `http-request(...)` | 无额外开关 | 发起 HTTP(S) 请求。请求由宿主发出，插件不持有 socket。 |
| `http-read/close` | 对应已创建的句柄 | 读取和释放 HTTP 响应。 |
| `get-secret(name)` | 无额外开关 | 按名字读取凭据：先查环境变量 `DELUXE_AGENT_SECRET_<NAME>`（名字大写、非字母数字映射为 `_`），再查操作系统凭据库；都没有时返回 `none`。 |

文件能力的根目录不是 `plugin.wasm` 所在目录：
全局级别的插件绑定到用户目录下的 `.deluxe-agents`，项目级别的插件绑定到项目范围的配置目录。
插件配置文件例如`.mcp.json`、`.hooks.json` 不属于插件包，插件应在运行时通过 `read-plugin-file` 读取。宿主不替插件解析这些文件。

路径和传输约束：

- 插件文件路径必须是生效范围内的相对路径；不要使用绝对路径或 `..`；
- `spawn-process` 的工作目录必须位于配置目录的根目录内；
- process 和 HTTP 都是原始字节/响应能力，MCP JSON-RPC、SSE、framing 和协议解析由插件自己负责；
- 宿主不为 process/HTTP 的单次读写设置 payload 上限，插件应自行控制读取量；
- 插件关闭或 reload 时，宿主会终止该实例创建的进程并释放 HTTP 响应。

当前实现不为插件调用设置执行预算：fuel、内存、payload、wall-clock 超时和句柄数量上限均已移除。一次调用能跑多久由调用方是否取消决定；插件仍应在正常路径主动关闭 process 和 HTTP 句柄。

## 8. 构建 Component

### 8.1 Rust guest

先安装 Rust 的 Wasm 目标和项目所需的 `wit-bindgen`。当前宿主不链接 WASI，插件不应依赖隐式的 WASI 文件系统或 WASI 环境；推荐先用 `wasm32-unknown-unknown` 生成 core module，再把它包装成 Component。

典型流程：

```powershell
$repo = "C:\path\to\deluxe-agent"
$plugin = "C:\path\to\example-plugin"
rustup target add wasm32-unknown-unknown
cargo build --manifest-path "$plugin\Cargo.toml" --release --target wasm32-unknown-unknown
# 使用仓库内的 componentizer，将 core module 包装成 Component。
cargo run --manifest-path "$repo\plugin-src\componentize\Cargo.toml" -- `
  "$plugin\target\wasm32-unknown-unknown\release\example_plugin.wasm" `
  "$plugin\plugin.wasm" `
  "$repo\wit"
```

如果你的工具链能直接生成符合本 WIT world 的 Component，也可以跳过 componentize 步骤；无论采用哪种方式，最终的 `plugin.wasm` 必须是 Wasmtime Component，而不是只包含 core Wasm module 的普通 `wasm32` 二进制。可以用 Wasmtime/`wasm-tools` 检查和验证；仓库自身提供了 `plugin-src/componentize` 作为离线的 `ComponentEncoder` 示例。该 fixture 的依赖版本与当前 Wasmtime 解析器保持一致，修改 WIT 或升级工具链后应重新验证。

不要把 `wit/deluxe-harness.wit` 复制后随意修改成私有 ABI。插件应直接引用仓库的 WIT，协议变化时跟随 `apiVersion` 升级。

### 8.2 安装验证

在安装前至少检查：

```powershell
Get-Content .\plugin.json | ConvertFrom-Json
wasm-tools validate .\plugin.wasm
```

随后在 `deluxe-agent` 中导入。宿主还会检查 Component 大小、manifest ABI、入口路径和 Component 是否可编译；失败时不会实例化插件或授予 host capability。

## 9. 安装与作用域

宿主的插件导入操作接受插件目录或其 `plugin.wasm` 文件。选择单个 `.wasm` 时，宿主会向上查找声明该文件的 `plugin.json`；因此最稳妥的方式是选择插件根目录，或选择 `runtime.module` 指向的那个文件。

本地导入会复制整个插件目录到受管理的缓存：

```text
~/.deluxe-agents/plugins/cache/deluxe-local/<name>/<version>/
```

导入后的 id 是 `<name>@deluxe-local`。目标版本已经存在时，宿主会拒绝覆盖；修改代码后请递增 `plugin.json` 的 `version`，或者先卸载旧版本。

插件可以根据具体的生效范围装到不同目录下：

- **全局级别**：对所有项目可见，为 `~/.deluxe-agents`；
- **项目级别**：只对指定项目可见，目录为该项目的 `.deluxe-agents` 目录下。

同一插件 id，项目级别可以覆盖全局级别。安装完成后刷新插件目录，启用开关改变后运行时会重新构建。GUI 主线程不直接加载 Wasm，实例化和 I/O 都在 worker 上完成。

## 10. 常见错误

### `Unsupported plugin API version`

检查 `runtime.apiVersion` 是否精确为 `deluxe.harness/plugin@0.1`。

### `Component entry escapes its plugin root`

`runtime.module` 只能是根目录内的普通相对文件名，例如 `plugin.wasm`。不要使用
`../plugin.wasm`、绝对路径或指向外部文件的符号链接。

### 插件被发现但没有工具或 UI

确认 `list-tools`/`list-event-handlers` 返回的是合法 JSON，并且 manifest 的
`runtime.ui.surfaces`、`runtime.ui.actions` 包含对应声明。工具声明中的 name 也
必须与 `execute-tool` 实际接受的名称一致。

### `This host capability was not granted`

这是预期的权限拒绝。把实际需要的工具名加入 `permissions.invokeTools`，命令名加入 `processCommands`；不要直接扩大到 `*`。

### `UI action targets a stale snapshot`

action 携带的 `revision` 已过期。每次 action 都应基于宿主最后发送的快照，并由
`handle-action` 返回严格递增的新 revision。

### `Plugin ... exceeds the payload/resource limit`

减少单次 JSON、文本、图片元数据、process read 或 HTTP read 的大小；把大结果拆成
多次有界读取，不要在 Component 内缓存无限增长的响应。

## 11. 发布前清单

- [ ] `plugin.json` 的 `name`、`version`、`runtime.module` 和 `apiVersion` 正确。
- [ ] `plugin.wasm` 确实是 Component，并能被 Wasmtime 37 编译加载。
- [ ] 所有 tool/event-handler/surface/action id 稳定、唯一并与导出函数一致。
- [ ] 权限遵循最小化原则，未使用的 capability 不声明。
- [ ] 工具输出、事件处理器输出和 UI snapshot 都有大小边界。
- [ ] 旧 revision、伪造 action、未授权 capability 和非法路径都有负向测试。
- [ ] 插件 reload、取消、超时和关闭路径释放 process/HTTP 资源。
- [ ] 本地导入时递增版本，不覆盖已有缓存目录。
- [ ] 用临时目录运行测试，并通过 `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings` 和 `cargo test`。

宿主实现的更多细节可参阅：
[`docs/wasmtime-plugin-implementation.md`](wasmtime-plugin-implementation.md)、
[`src/plugins/wasm_runtime.rs`](../src/plugins/wasm_runtime.rs)、
[`src/plugins/wasm_manifest.rs`](../src/plugins/wasm_manifest.rs) 和
[`src/plugins/ui_protocol.rs`](../src/plugins/ui_protocol.rs)。
