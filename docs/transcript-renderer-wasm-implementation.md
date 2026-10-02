# Markdown 与 code_view Wasm 插件化实现文档

本文档描述如何把当前宿主内的 `markdown` 和 `code_view` 逻辑迁移为
Wasmtime Component 插件。

本文档只定义目标架构、协议、模块边界、迁移顺序和验证标准，不包含本轮代码
实现。实现开始前，应先冻结本文档中的跨 Wasm 边界；如果协议需要变化，应先
更新文档和 fixture，再修改 Rust 或插件源码。

## 1. 背景

当前消息和工具结果的绘制路径仍然由宿主直接实现：

```text
src/app/ui.rs
    ├── markdown::draw_text
    │       ├── markdown::parse
    │       └── egui widgets / LayoutJob / table layout
    │
    └── code_view::draw
            ├── tool_panel
            ├── patch_lines
            └── egui panel / diff / scroll / copy
```

其中，解析和数据整理逻辑与 egui 绘制逻辑已经部分分离，但还在同一个宿主
crate 中：

- `src/markdown.rs` 同时包含 Markdown parser 和 egui renderer；
- `src/code_view.rs` 同时包含 patch/tool-result parser 和 egui renderer；
- `src/app/ui.rs` 负责选择工具面板形状，并直接调用 `code_view`；
- `src/app/ui.rs` 在绘制过程中同步调用 Markdown parser；
- 当前 Wasm UI 协议 `PluginUiDocument` 面向设置面板控件，不适合表示富文本、
  diff、表格和代码块。

本次改造的目标不是让 Wasm 直接绘制 egui，而是把“输入文本或工具结果如何
变成语义文档”的实现移到 Wasm。宿主仍然负责最终的 egui 布局和交互。

## 2. 总体决策

采用一个专用的内置 Wasm Component：

```text
transcript-renderer.wasm
    ├── render-markdown
    └── render-tool
```

两种输入共用一个 renderer Component，但使用两个版本化请求入口：

- `render-markdown`：把 Markdown 文本转换成 Markdown 语义块；
- `render-tool`：把工具名称、参数、结果和 patch 行号转换成工具面板语义。

目标数据流：

```text
egui 主线程
    │
    │ Cmd::RenderMarkdown / Cmd::RenderTool
    ▼
tokio worker
    │
    │ RendererActor
    ▼
transcript-renderer.wasm
    │
    │ versioned JSON IR
    ▼
tokio worker
    │
    │ Event::MarkdownRendered / Event::ToolRendered
    ▼
egui 主线程
    │
    ├── markdown_ui.rs
    └── code_view_ui.rs
```

必须保持以下边界：

- Wasm 不获得 `egui::Context`、`egui::Ui`、`egui::Painter` 或 `eframe::Frame`；
- Wasm 不返回 `Color32`、`FontId`、`LayoutJob`、`Galley` 等 egui 类型；
- Wasm 不负责实际像素绘制、窗口布局、滚动位置和鼠标事件；
- egui 主线程不加载、实例化或同步调用 Wasm；
- 渲染结果不写入 session 持久化文件；
- Renderer 失败只影响当前消息或工具面板，不影响主窗口、Agent 或其他插件；
- 第一阶段 Renderer 作为受信任的内置 Component，不开放为普通用户插件替换。

## 3. 目标与非目标

### 3.1 目标

完成后应满足：

- Markdown 的 block、inline span、代码块、引用、列表、表格和链接解析由
  Wasm 提供；
- `code_view` 的工具类型判断、patch 行解析、统计、标题、行号和命令输出
  结构化由 Wasm 提供；
- 宿主只根据稳定的语义 IR 进行 egui 绘制；
- Wasm 调用具有独立的 timeout、fuel、内存和 payload 限制；
- 流式 assistant 文本不会因渲染请求乱序而显示旧结果；
- 旧 session 可以在不修改存储格式的情况下重新生成渲染结果；
- Renderer 未加载或失败时，宿主仍可用纯文本 fallback；
- 协议可以在不暴露 Rust 私有类型的情况下被 Rust、C、Go 或其他语言实现；
- 现有 UI 的复制、滚动、链接点击、折叠状态和主题仍由宿主控制。

### 3.2 非目标

第一阶段不做：

- 不把 egui 编译进 Wasm；
- 不让 Wasm 返回任意 widget tree；
- 不把现有 `PluginUiDocument` 扩展成富文本协议；
- 不在 Wasm 中执行文件、网络、进程或宿主工具能力；
- 不把 Markdown 渲染结果持久化到 session；
- 不改变 Agent 发给模型的 prompt、tool schema 或 session replay；
- 不替换现有普通插件的 `deluxe:harness@0.1.0` ABI；
- 不在第一阶段支持第三方 Renderer 热插拔；
- 不顺手重写当前 Markdown 语法或 UI 样式。

## 4. 现有代码到目标代码的映射

### 4.1 Markdown

迁移到 Renderer Wasm 的纯逻辑：

| 当前位置 | 逻辑 |
| --- | --- |
| `markdown::parse` | Markdown 顶层解析入口 |
| `parse_blocks` | block 识别和顺序 |
| `fence` / `fence_marker` / `closes_fence` | fenced code |
| `heading` | ATX heading |
| `quote_line` / `gather_quote` | block quote |
| `list_item` / `item_spans` | list item |
| `paragraph` / `split_break` | paragraph 和 hard break |
| `parse_inline` | inline span |
| `emphasis` / `find_run` | bold、italic |
| `link` | inline link |
| `table_delimiter` / `parse_delimiter` | table header/delimiter |
| `split_row` / `read_table` / `fit_row` | table source parsing |

保留在宿主的逻辑：

| 当前位置 | 逻辑 |
| --- | --- |
| `draw_blocks` | block 的 egui 顺序布局 |
| `draw_block` | block 到 widget 的映射 |
| `draw_spans` | `LayoutJob` 和 selectable label |
| `draw_linked_spans` | hyperlink widget |
| `draw_item` | list gutter 和布局 |
| `draw_quote` | quote bar 和嵌套布局 |
| `draw_table` | 依据当前宽度决定 grid/record/source |
| `measure_columns` 等 | 依赖 egui 字体的宽度测量 |
| `draw_source` | table fallback 的实际绘制 |

### 4.2 code_view

迁移到 Renderer Wasm 的纯逻辑：

| 当前位置 | 逻辑 |
| --- | --- |
| `LineKind` / `Line` | 行语义和行号 |
| `tool_label` | 工具摘要动词 |
| `is_known` | 是否有专用工具面板 |
| `tool_icon` | 语义图标类型，不再返回 glyph |
| `patch_lines` | patch envelope、hunk、行类型和行号 |
| `patch_stats` | 增删统计 |
| `patch_title` | 文件标题 |
| `text_lines` / `meta_lines` | 普通文本和元数据 |
| `command_lines` | shell prompt 和输出 |
| `tool_panel` | 按工具类型组装面板输入 |

保留在宿主的逻辑：

| 当前位置 | 逻辑 |
| --- | --- |
| `draw` | 工具 panel frame 和 title bar |
| `draw_title_bar` | 标题、状态、copy button |
| `draw_body` | running 状态和 body 分支 |
| `draw_slab` | Markdown code fence 的无标题代码 slab |
| `draw_body_lines` | ScrollArea、尺寸测量和 selectable galley |
| `body_job` | 行颜色、gutter、字体和 `LayoutJob` |
| `draw_tool_card` | CollapsingHeader 和工具行状态 |
| `header_job` | 工具行的 egui 文本布局 |

`tool_icon` 在 Wasm 中只能返回稳定的语义枚举，例如 `Terminal` 或
`NotePencil`。宿主根据当前图标字体映射成 `icons::TERMINAL_WINDOW` 等 glyph。

## 5. Component ABI

### 5.1 新增独立 WIT world

新增：

```text
wit/deluxe-renderer.wit
```

建议内容：

```wit
package deluxe:renderer@0.1.0;

interface renderer {
    render-markdown: func(request-json: string) -> result<string, string>;
    render-tool: func(request-json: string) -> result<string, string>;
}

world transcript-renderer {
    export renderer;
}
```

Renderer 不导入 `host` interface。第一阶段它是纯计算 Component，不需要
文件、网络、进程、工具或 UI capability。

不要把两个导出函数直接加入 `wit/deluxe-harness.wit`。当前 harness world
服务普通插件的 tool、hook、MCP 和 settings UI；修改该 world 会增加所有普通
插件的 ABI 迁移成本，也会让渲染器不必要地拥有通用插件能力。

### 5.2 WIT 版本与 JSON schema 版本

版本分两层：

1. WIT package/world 版本表示 Component ABI；
2. JSON 的 `schemaVersion` 表示请求和响应结构。

请求和响应都必须带：

```json
{
  "schemaVersion": 1,
  "revision": 1
}
```

含义：

- `schemaVersion`：结构版本，不允许宿主静默接受不兼容版本；
- `revision`：一次渲染请求的单调编号，用于丢弃过期结果；
- `revision` 不代表 session revision，也不写入 session 文件。

### 5.3 `render-markdown` 请求

```json
{
  "schemaVersion": 1,
  "revision": 42,
  "text": "# Hello\n\nSome **text**.",
  "mode": "transcript"
}
```

字段：

| 字段 | 类型 | 约束 |
| --- | --- | --- |
| `schemaVersion` | number | 必须为当前支持版本 |
| `revision` | number | 大于 0，由宿主生成 |
| `text` | string | 不超过请求文本上限 |
| `mode` | string | 第一阶段固定为 `transcript` |

`mode` 保留是为了将来区分 transcript、notice、compact summary 等语义，
但第一阶段不能因为 mode 不同而改变现有 Markdown 语法。

### 5.4 `render-markdown` 响应

```json
{
  "schemaVersion": 1,
  "revision": 42,
  "kind": "markdown",
  "blocks": [
    {
      "type": "heading",
      "level": 1,
      "spans": [
        {
          "text": "Hello",
          "style": {
            "bold": false,
            "italic": false,
            "code": false
          },
          "link": null
        }
      ]
    },
    {
      "type": "paragraph",
      "spans": [
        {
          "text": "Some ",
          "style": {
            "bold": false,
            "italic": false,
            "code": false
          },
          "link": null
        },
        {
          "text": "text",
          "style": {
            "bold": true,
            "italic": false,
            "code": false
          },
          "link": null
        }
      ]
    }
  ]
}
```

语义类型：

```text
RenderSpan {
    text: string,
    style: SpanStyle,
    link: optional<string>
}

SpanStyle {
    bold: bool,
    italic: bool,
    code: bool
}
```

Markdown block：

```text
Heading {
    level: u8,
    spans: list<RenderSpan>
}

Paragraph {
    spans: list<RenderSpan>
}

Item {
    depth: u32,
    marker: Marker,
    spans: list<RenderSpan>
}

Code {
    lang: string,
    text: string
}

Quote {
    blocks: list<Block>
}

Rule

Table {
    align: list<ColumnAlign>,
    header: list<list<RenderSpan>>,
    rows: list<list<list<RenderSpan>>>,
    source: string
}
```

JSON 使用 internally tagged enum：

```json
{
  "type": "item",
  "depth": 1,
  "marker": {
    "type": "ordered",
    "value": 3
  },
  "spans": []
}
```

`Table.source` 必须保留原始表格文本。宿主在当前窗口太窄、无法排成可读
表格时，使用它进行 monospace fallback。

### 5.5 `render-tool` 请求

请求不直接传输宿主的 `ToolResult` Rust 类型，而是传输专门的稳定投影：

```json
{
  "schemaVersion": 1,
  "revision": 12,
  "tool": {
    "name": "apply_patch",
    "arguments": {
      "patch": "*** Begin Patch\n*** Update File: src/main.rs\n@@\n-old\n+new\n*** End Patch"
    },
    "result": {
      "outcome": "executed",
      "output": "Applied 1 patch operation",
      "hunks": [
        {
          "path": "src/main.rs",
          "lines": [40, 41, 41]
        }
      ]
    }
  }
}
```

工具仍在运行时：

```json
{
  "schemaVersion": 1,
  "revision": 13,
  "tool": {
    "name": "exec",
    "arguments": {
      "command": "cargo test"
    },
    "result": null
  }
}
```

请求投影字段：

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `tool.name` | string | 工具名 |
| `tool.arguments` | object | 已解析的参数对象 |
| `tool.result` | object/null | 运行结果；运行中为 `null` |
| `result.outcome` | string | `executed`、`denied` 或 `failed` |
| `result.output` | string | 文本输出 |
| `result.hunks` | array | apply_patch 的行号表 |

图片引用不进入 Renderer 协议。图片仍由宿主的 attachment 逻辑负责读取、
缓存和创建 `TextureHandle`。

### 5.6 `render-tool` 响应

```json
{
  "schemaVersion": 1,
  "revision": 12,
  "kind": "tool",
  "known": true,
  "title": "src/main.rs",
  "icon": "notePencil",
  "added": 1,
  "removed": 1,
  "copyText": "*** Begin Patch\n*** Update File: src/main.rs\n...",
  "lines": [
    {
      "kind": "meta",
      "text": "*** Update File: src/main.rs",
      "number": null
    },
    {
      "kind": "hunk",
      "text": "@@",
      "number": null
    },
    {
      "kind": "del",
      "text": "old",
      "number": 40
    },
    {
      "kind": "add",
      "text": "new",
      "number": 40
    }
  ]
}
```

语义枚举：

```text
LineKind = plain | add | del | hunk | meta

IconKind =
    notePencil
    | terminal
    | file
    | folder
    | image
    | dots
    | stopCircle
    | gear
```

宿主不能信任 `known` 来决定安全性，它只影响显示分支。即使 Wasm 返回
`known: false`，宿主仍要按照自己的协议验证标题、行数、文本长度和 copy
内容。

## 6. 宿主协议校验

新增：

```text
src/plugins/render_protocol.rs
```

该模块不依赖 egui，也不依赖 Wasmtime，负责：

- JSON decode；
- `schemaVersion` 校验；
- `revision` 校验；
- response `kind` 校验；
- payload 大小限制；
- block、span、row、line 数量限制；
- 每个文本字段长度限制；
- 聚合文本字节数限制；
- `heading.level` 范围校验；
- `ColumnAlign`、`LineKind`、`IconKind` 的未知值处理；
- link URL 的长度和允许 scheme 校验；
- `copyText` 长度校验；
- patch 行号是否为非零有限整数；
- table 行宽是否与 align/header 结构匹配。

建议第一阶段的边界值：

```text
MAX_RENDER_REQUEST_BYTES  = 256 KiB
MAX_RENDER_RESPONSE_BYTES = 512 KiB
MAX_MARKDOWN_BLOCKS       = 512
MAX_MARKDOWN_DEPTH        = 24
MAX_SPANS_PER_BLOCK       = 1024
MAX_TOOL_LINES            = 8192
MAX_TEXT_FIELD_BYTES      = 16 KiB
MAX_AGGREGATE_TEXT_BYTES  = 256 KiB
```

这些值应定义在协议模块中，并在测试中用常量引用。不要让 GUI renderer
自己重复做一套不同的预算。

响应校验失败时：

1. 不写入 render cache；
2. 不更新当前 UI；
3. 记录带 `revision` 和渲染类型的 warning；
4. 当前项切换到 fallback；
5. 不重试同一个完全相同的非法响应。

## 7. Renderer runtime

### 7.1 模块

新增：

```text
src/plugins/renderer_runtime.rs
```

第一阶段建议实现独立的 `RendererActor`，而不是立即把
`ComponentActor` 泛化成任意 WIT world。原因是当前
`src/plugins/wasm_runtime.rs` 的绑定类型固定为
`deluxe:harness/plugin`，直接泛化会同时扩大普通插件、MCP 和 UI
runtime 的变更面。

`RendererActor` 应复用当前 Component runtime 的安全策略：

- Wasm Component 编译；
- component 大小限制；
- store memory/table/instance 限制；
- fuel；
- `CALL_TIMEOUT` 类似的单次调用超时；
- `CancellationToken`；
- trap/resource exhaustion 后丢弃 instance；
- 有界 request queue；
- `Arc<RendererActor>` 共享只读 renderer 实例。

Renderer 不需要 `CapabilityHub`。如果实现上需要 `StoreState`，应使用最小
的 renderer 专用 state，而不是把普通插件 capability 接口接进来。

### 7.2 调用模型

建议 API：

```rust
pub struct RendererActor {
    // Component instance queue and lifetime token.
}

impl RendererActor {
    pub async fn load_bytes(bytes: &[u8]) -> Result<Arc<Self>>;

    pub async fn render_markdown(
        &self,
        request_json: String,
    ) -> Result<String>;

    pub async fn render_tool(
        &self,
        request_json: String,
    ) -> Result<String>;

    pub fn shutdown(&self);
}
```

调用流程：

```text
RenderRequest
    ↓
serialize request
    ↓
bounded actor queue
    ↓
timeout + cancellation
    ↓
Component export call
    ↓
response byte limit
    ↓
decode + validate render protocol
    ↓
RenderEvent
```

Wasm guest 返回的 `Err(string)` 是正常的插件输入或处理错误；trap、超时、
fuel 耗尽、内存限制和取消属于 runtime error。两者都不能导致窗口线程
panic。

### 7.3 Component 生命周期

Renderer Component 可以按进程复用，因为它是无 host capability 的纯计算
组件。但必须保留重建能力：

- 首次启动时异步加载；
- 编译或实例化失败时进入 fallback；
- trap 或 resource limit 后重建 instance；
- 应用关闭时取消 actor；
- 不把 renderer 的状态绑定到某个 project；
- 不因切换 project 重新加载同一个内置 renderer。

如果未来 Renderer 允许用户替换，再把生命周期扩展成
`project + renderer_id` 作用域，并重新评估信任边界。

## 8. GUI 与 worker 集成

### 8.1 IPC 命令和事件

在 `src/ipc.rs` 中新增纯数据消息：

```rust
Cmd::RenderMarkdown {
    key: RenderKey,
    revision: u64,
    text: String,
}

Cmd::RenderTool {
    key: RenderKey,
    revision: u64,
    request: ToolRenderRequest,
}

Event::MarkdownRendered {
    key: RenderKey,
    revision: u64,
    document: Arc<RenderedMarkdown>,
}

Event::ToolRendered {
    key: RenderKey,
    revision: u64,
    panel: Arc<RenderedToolPanel>,
}

Event::RenderFailed {
    key: RenderKey,
    revision: u64,
    kind: RenderKind,
    message: String,
}
```

`RenderKey` 需要稳定标识一个 transcript step：

```text
Assistant / Notice / HostMessage:
    session_id + step_index

Tool:
    session_id + call_id
```

不要用完整文本作为唯一 key。文本会在流式过程中变化，并且长文本会增加
哈希和日志处理成本。

### 8.2 Render cache

新增：

```text
src/app/render_cache.rs
```

缓存条目至少包含：

```text
RenderKey
latest_revision
input_fingerprint
status
fallback_text
markdown_document / tool_panel
```

缓存只存在内存中，不进入 session JSON。

输入 fingerprint 用于避免重复发送相同内容。建议使用标准 hash，不要把
完整文本写入 cache key。

状态：

```text
Missing
Pending
Ready
Failed
Fallback
```

GUI 只读取 `Ready` 的 IR。`Pending`、`Failed` 或 Renderer 尚未加载时使用
fallback。

### 8.3 流式 assistant 文本

当前 assistant 文本会通过 `Step::Assistant { text }` 持续增长。改造后：

1. GUI 仍然显示 session 中的原始文本；
2. App 收到新的 assistant delta 后增加 revision；
3. worker 只处理该 step 的最新请求；
4. 旧 response 回来后比较 `RenderKey + revision`；
5. revision 小于当前值的 response 丢弃；
6. 最新 response 到达后替换 IR；
7. `request_repaint` 唤醒窗口。

为避免 Wasm 调用频率过高，应在 worker 侧合并请求：

- 同一个 `RenderKey` 只保留一个 pending request；
- 新文本到达时替换尚未开始的旧 request；
- 对正在执行的 Wasm 调用可以让其完成，但结果必须按 revision 校验；
- 可选地对流式 delta 做 30 到 60 ms debounce；
- 最终 assistant delta 到达后必须立即提交一次最终渲染请求。

第一阶段可以不做复杂的可取消 guest call，只要旧结果严格丢弃；后续再用
`CancellationToken` 取消已经过时的请求。

### 8.4 工具卡片

工具卡片的折叠状态和 Renderer 分离：

- collapsed row 不需要 `render-tool`；
- 用户展开时，如果 cache 没有结果，异步提交 `render-tool`；
- 结果未返回时显示当前工具名、running 状态或纯文本 fallback；
- `CollapsingHeader` 的 `id_salt(call_id)` 保持不变；
- tool result 到达后只替换面板内容，不重置展开状态；
- copy button 由宿主直接使用 `copyText`；
- 参数 fallback 仍由宿主用 `pretty(arguments)` 绘制。

工具标题行使用宿主自己的状态信息，不能依赖 Wasm 返回结果：

- outcome 颜色；
- running 状态；
- duration；
- tool call id；
- tool name。

Renderer 只负责返回展开后的语义 panel。

## 9. egui renderer 拆分

建议把当前 `markdown.rs` 和 `code_view.rs` 中的 egui 部分移到：

```text
src/app/markdown_ui.rs
src/app/code_view_ui.rs
```

### 9.1 `markdown_ui.rs`

输入：

```rust
&RenderedMarkdown
```

保留：

- heading size；
- body size；
- inline span `TextFormat`；
- link widget；
- list indentation；
- quote bar；
- table measure；
- grid/records/source fallback；
- code block 调用 `code_view_ui::draw_slab`；
- scroll salt。

注意：Renderer 的 `Code` block 只包含 `lang` 和原文。代码块的具体 egui
显示继续由宿主完成，以便 Markdown code fence 与工具输出使用同一个
`draw_slab` 实现。

### 9.2 `code_view_ui.rs`

输入：

```rust
&RenderedToolPanel
```

保留：

- panel frame；
- title bar；
- copy button；
- running body；
- line gutter；
- `LayoutJob`；
- diff colors；
- horizontal/vertical ScrollArea；
- panel maximum height；
- code slab；
- semantic icon 到当前 icon glyph 的映射。

`RenderedToolPanel` 不应包含任何 `egui` 类型。宿主可以在进入 renderer 前
把 `LineKind` 映射为本地颜色，但更推荐保留语义枚举到绘制层再映射。

## 10. Fallback 策略

Renderer 是增强能力，不能成为 GUI 启动条件。

### 10.1 Renderer 加载失败

启动流程：

```text
启动 GUI
    ↓
异步加载 transcript-renderer
    ├── 成功：后续消息使用 Wasm IR
    └── 失败：标记 RendererUnavailable，全部走 fallback
```

加载失败应记录 warning，但不阻止：

- GUI 启动；
- session 加载；
- Agent 运行；
- 工具执行；
- plugin catalogue 加载。

### 10.2 单条消息渲染失败

Markdown fallback：

```text
原始 Markdown
    ↓
宿主纯文本 Label
```

工具 panel fallback：

```text
工具名 + 原始 output
    ↓
宿主普通 selectable text panel
```

fallback 不应重新调用失败的相同请求。只有输入 revision 变化，或者 Renderer
instance 成功重建后，才允许再次提交。

### 10.3 迁移期间双实现

迁移阶段保留 native implementation，但只能作为：

- fallback；
- golden comparison；
- 开发期调试开关。

不能让 native parser 和 Wasm parser 同时决定最终 UI，否则会产生两个行为源。
正常路径必须明确为：

```text
Wasm IR -> host egui renderer
```

而不是：

```text
native parser / Wasm parser -> randomly selected renderer
```

## 11. 插件包和构建集成

建议新增：

```text
builtin-plugins/transcript-renderer/
├── plugin.json
└── plugin.wasm
```

但它与当前普通插件包有一个重要区别：它不是普通 `PluginCatalogue` 中的
Agent tool/provider 插件，也不出现在插件设置列表中。

建议初版由 `build.rs` 单独嵌入：

```rust
include_bytes!("../builtin-plugins/transcript-renderer/plugin.wasm")
```

或者在现有 bundled plugin 生成逻辑中增加明确的 `kind`：

```json
{
  "name": "transcript-renderer",
  "version": "0.1.0",
  "runtime": {
    "kind": "renderer",
    "module": "plugin.wasm",
    "apiVersion": "deluxe.renderer/renderer@0.1"
  }
}
```

推荐第一种方式，原因是：

- 不让 renderer 进入普通插件启停、scope 和 project override 流程；
- 不向 renderer 暴露普通插件 capability；
- 不改变 `PluginManifest::wasm_runtime()` 对现有插件的语义；
- 可以在未来协议稳定后再决定是否开放可替换 renderer。

如果复用 `builtin-plugins/` 的自动扫描，需要同时修改构建校验，允许
renderer manifest 使用独立 API version，并防止它被普通插件 discovery
误收录。

## 12. 安全和资源限制

Renderer 虽然没有 host capability，也不能假设它天然安全。

宿主必须限制：

- component 文件大小；
- instance memory；
- table elements；
- fuel；
- 单次调用时间；
- response bytes；
- queue 长度；
- cache 中的对象数量；
- 单个 document 的 block/line/span 数量。

输入也必须限制：

- assistant 文本长度；
- tool arguments JSON 长度；
- tool output 长度；
- patch 文本长度；
- link URL 长度；
- table 行列数。

不要把未经限制的完整 `serde_json::Value` 直接序列化到 Renderer 请求中。
`render-tool` 应使用专门投影，只传递 code_view 需要的字段。

错误分类建议沿用现有插件错误码：

```text
PLUGIN_LOAD_FAILED
PLUGIN_TIMEOUT
PLUGIN_TRAP
PLUGIN_RESOURCE_LIMIT
PLUGIN_INVALID_OUTPUT
```

协议解析错误属于 `PLUGIN_INVALID_OUTPUT`；Wasm guest trap 和 fuel/memory
问题由 runtime 分类；用户输入过大在提交给 Wasm 前由宿主拒绝。

## 13. 迁移阶段

### Phase 0：冻结行为

- 为 `markdown::parse` 和 code_view 纯函数建立 JSON golden fixtures；
- 记录当前 parser 的已知非 CommonMark 行为；
- 记录当前 Markdown 和工具 panel 的视觉回归样本；
- 明确当前 session 不存储渲染结果；
- 确认所有改造都不改变 prompt 和 session replay。

完成标准：

- 每个现有 parser 测试都有对应 fixture 或明确保留在宿主侧；
- fixture 能描述未闭合 fence、空文本和异常 patch。

### Phase 1：抽取跨边界类型

- 新增 `src/plugins/render_protocol.rs`；
- 从 `markdown.rs` 提取协议类型；
- 从 `code_view.rs` 提取协议类型；
- 类型全部使用 `serde(rename_all = "camelCase")`；
- 删除跨边界类型中的 egui 字段；
- 为协议增加 decode/validate 测试。

此阶段仍然可以继续使用 native parser 和 native renderer。

### Phase 2：实现 Wasm parser

- 新增 `plugin-src/transcript-renderer`；
- 迁移 Markdown parser；
- 迁移 patch/tool panel parser；
- 使用 `wit-bindgen` 生成独立 renderer world；
- 输出与 golden fixture 完全一致；
- 构建 `builtin-plugins/transcript-renderer/plugin.wasm`。

此阶段不改 GUI 调用路径，只验证 Component 输出。

### Phase 3：实现 RendererActor

- 新增 `src/plugins/renderer_runtime.rs`；
- 编译、实例化并调用 renderer Component；
- 加入 timeout、fuel、memory 和 payload 限制；
- 加入 trap 后重建；
- 加入 actor 单元测试和 Component 集成测试；
- 启动失败时允许应用继续进入 fallback。

### Phase 4：接入 Markdown

- 新增 `RenderKey`、render command/event；
- 新增 `render_cache`；
- 将 assistant/notice/host message 的 Markdown 请求发送到 worker；
- 新增 `markdown_ui.rs`；
- 由 IR 驱动宿主 egui renderer；
- 保留 native parser 作为 fallback；
- 验证流式 revision 丢弃和 session 重载。

### Phase 5：接入 code_view

- 把 `tool_panel` 改成构造 `ToolRenderRequest`；
- 工具展开时异步请求 Renderer；
- 新增 `code_view_ui.rs`；
- 保留 title bar、copy、scroll、fold 和 outcome 状态在宿主；
- 为 unknown tool 保留参数 fallback；
- 验证 patch hunks、line number 和 running 状态。

### Phase 6：清理和固化

- 删除 native Markdown parser 的正常路径；
- 删除 native code_view parser 的正常路径；
- 删除 `src/markdown.rs` 和 `src/code_view.rs` 中已经迁移的逻辑；
- 保留宿主 IR renderer；
- 将 Renderer 作为独立内置 Component 文档化；
- 关闭开发期双实现开关；
- 更新 `builtin-plugins/README.md` 和插件构建说明。

## 14. 测试计划

### 14.1 协议测试

`src/plugins/render_protocol.rs` 应测试：

- request/response round trip；
- unknown JSON fields 被忽略；
- schema version 不匹配被拒绝；
- response kind 不匹配被拒绝；
- 超过 payload 上限被拒绝；
- 超过 block/line/span/depth 上限被拒绝；
- invalid heading level 被拒绝；
- invalid table shape 被拒绝；
- invalid line number 被拒绝；
- invalid URL scheme 被拒绝；
- duplicate 或无效语义 enum 被拒绝。

### 14.2 Wasm Component 测试

使用仓库现有的 `wat`、`tempfile` 和 Component fixture 模式，测试：

- renderer Component 能被编译；
- `render-markdown` 返回合法 JSON；
- `render-tool` 返回合法 JSON；
- guest error 不会破坏 actor；
- trap 后 actor 可以重建；
- timeout 不会阻塞后续 GUI 命令；
- response 大于宿主上限时被拒绝；
- Renderer 不需要任何 host capability。

### 14.3 Markdown golden 测试

至少覆盖：

- plain paragraph；
- heading 1 到 6；
- bold、italic、nested emphasis；
- inline code；
- inline link；
- escaped punctuation；
- hard break；
- unordered list；
- ordered list；
- nested list；
- quote；
- fenced code；
- 未闭合 fenced code；
- tilde fence；
- table alignment；
- ragged table；
- empty input；
- Unicode 和 CJK 文本。

### 14.4 code_view golden 测试

至少覆盖：

- `apply_patch` 单文件；
- `apply_patch` 多文件；
- add/update/delete/move；
- blank hunk line；
- hunk line number table；
- 缺少 hunk line number table；
- partial line number table；
- `exec` command and output；
- plain text tool output；
- unknown tool；
- empty output；
- running tool；
- denied/failed result；
- long path and long output。

### 14.5 GUI 集成测试

验证：

- Renderer 尚未加载时窗口仍可绘制；
- Markdown response 乱序时旧 revision 不覆盖新内容；
- 流式文本最终稳定；
- code block 和 tool panel 的 ScrollArea salt 不冲突；
- copy button 返回原始 `copyText`；
- 展开状态不因 renderer response 重置；
- link 仍由宿主 clickable widget 绘制；
- 表格在窄窗口下仍能切换到 records/source fallback；
- Renderer 失败时 UI 不 panic；
- worker I/O 不发生在 egui `update` 路径。

## 15. 兼容性和版本升级

### 15.1 协议兼容

`schemaVersion` 只接受当前版本。未来如果做向后兼容，应显式实现：

```text
decode v1 -> normalize -> current IR
decode v2 -> normalize -> current IR
```

不要在校验器里根据字段缺失猜测版本。

### 15.2 Renderer Component 升级

升级 Renderer 时：

1. 修改 `wit/deluxe-renderer.wit` 或 JSON schema；
2. 更新 golden fixtures；
3. 更新 `plugin.wasm`；
4. 增加 Component version；
5. 运行完整 renderer 和 GUI 测试；
6. 确认 fallback 仍然可用。

Renderer 版本不应写入 session。session 只保存原始文本、工具参数和工具
结果，因此更换 Renderer 后可以重新生成最新 IR。

### 15.3 普通插件兼容

本改造不改变：

- `wit/deluxe-harness.wit`；
- `PluginUiDocument`；
- `PluginUiAction`；
- `ToolDescriptor`；
- `ToolSettings`；
- `HOST_RULES`；
- `SUB_AGENT_RULES`；
- 普通插件的 `plugin.json` runtime ABI。

Renderer 是内部 Component，不应被普通 plugin discovery 当成 agent plugin。

## 16. 日志和可观测性

每次异步渲染至少记录：

```text
render_kind
render_key
revision
input_bytes
output_bytes
duration_ms
cache_hit
fallback
error_code
```

正常成功路径不应输出完整 Markdown、工具参数或工具结果，避免日志泄漏用户
代码和命令。失败日志只记录长度、key、revision 和机器可读错误码，必要时
截断 guest error message。

建议增加以下 tracing span：

```text
renderer.load
renderer.render_markdown
renderer.render_tool
renderer.validate_response
renderer.cache
```

## 17. 完成标准

只有满足以下条件，才可以删除 native parser 的正常路径：

- `markdown` 和 `code_view` 所有原有行为测试都有 Wasm 对应覆盖；
- Wasm 输出通过宿主统一协议校验；
- Renderer trap/timeout/非法输出不会影响 GUI；
- 流式 assistant 文本没有旧 revision 覆盖问题；
- session 重启后能重新生成 IR；
- code block、tool panel 的滚动和 copy 行为保持；
- 主题变化不需要重新调用 Wasm；
- Markdown 表格在不同窗口宽度下仍由宿主正确布局；
- fallback 在 Renderer 缺失时可用；
- `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings` 和
  `cargo test` 全部通过；
- 新增的 Component fixture 已纳入构建和测试，但没有将 `target/`、临时
  fixture 或用户配置写入仓库。

最终的职责划分应稳定为：

```text
transcript-renderer.wasm
    Markdown / patch / tool-result -> semantic IR

deluxe-agent host
    semantic IR -> egui layout, theme, interaction, scrolling, copy
```

