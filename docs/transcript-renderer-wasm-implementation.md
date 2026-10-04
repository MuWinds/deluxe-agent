# Transcript Renderer：把 Markdown 与工具面板渲染搬进 Wasm

本文档描述 transcript renderer 的目标架构、协议、模块边界与验证标准。它替换
了早期「宿主解析、宿主绘制」的设计稿：现在 **Markdown 与工具面板的解析和排版
全部在 Wasm guest 内完成**，宿主只负责把一个通用 display-list 画成 egui。

实现开始前，应先冻结本文档中的跨 Wasm 边界；如果协议需要变化，应先更新文档和
测试，再修改 Rust 或 guest 源码。

## 0. 落点

| 关注点 | 落点 |
| --- | --- |
| display-list IR + 协议校验 | `src/renderer/protocol.rs` |
| 通用 egui 渲染器 | `src/renderer/present.rs` |
| 插件平台 actor（含可选的 renderer 绑定） | `src/plugins/wasm_runtime.rs` |
| guest（独立 crate，不进根 workspace） | `plugin-src/transcript-renderer/` |
| guest 的 Markdown / 工具解析与排版 | `plugin-src/transcript-renderer/src/markdown.rs`、`code_view.rs` |
| guest 的 display-list 镜像 | `plugin-src/transcript-renderer/src/display.rs` |
| WIT world | `wit/deluxe-harness.wit`（`interface renderer` + world `transcript-renderer` / `renderer-plugin`） |
| 提交进仓库的构建产物 | `builtin-plugins/transcript-renderer/plugin.wasm` |
| 插件清单 | `builtin-plugins/transcript-renderer/plugin.json` |
| GUI 侧缓存 | `src/app/render_cache.rs` |

渲染器是一个**普通内置插件**：它由 `builtin-plugins/transcript-renderer/` 发布，
首次运行经 `ensure_bundled_defaults` 安装并启用，在插件面板可见、可停用。宿主经
插件平台加载并调用它（`ComponentActor`），worker 持有一个全局单例（渲染与项目
无关）。它的 Component 导出 `plugin`（空实现）与 `renderer` 两个接口，且**不
import host**：渲染是纯计算。传输层错误用 `PLUGIN_*`，只有 display-list 自身的
契约错误用 `RENDER_INVALID_OUTPUT`。

**宿主不得出现 Markdown 或 code_view 的解析/排版痕迹。** 任何形如「识别标题」、
「解析 patch」、「按工具类型组装面板」的逻辑都属于 guest。宿主的
`src/app/ui.rs` 只做两件事：把 display-list 交给 `present::draw`，或在 renderer
不可用时画纯文本 fallback。

## 1. 背景与决策

宿主原先有两个模块 `src/markdown.rs` 与 `src/code_view.rs`，各自既解析又绘制，
`src/app/ui.rs` 在绘制过程中同步调用它们。改造的目标是把「输入如何变成一份可
绘制文档」完全移到 Wasm，宿主只保留最终像素布局和交互。

Wasm 不能调用 egui：它拿不到字体、`Context`、`Painter`，也不能测量文本。因此
「在插件里渲染」被实现为一个**通用 display-list**：guest 产出带语义的图元树
（文本 run、行/列栈、缩进、边框、规则线、滚动、折叠、图标、复制按钮、对齐、
网格），宿主用一个与领域无关的渲染器把它画出来。

必须保持的边界：

- Wasm 不获得任何 egui / eframe 类型；
- Wasm 不返回 `Color32`、`FontId`、`LayoutJob`、`Galley`；
- Wasm 不负责实际像素绘制、窗口布局、滚动位置和鼠标事件；
- egui 主线程不加载、实例化或同步调用 Wasm；
- 渲染结果不写入 session 持久化文件；
- Renderer 失败只影响当前消息或工具卡，不影响主窗口、Agent 或其他插件。

## 2. 目标与非目标

### 2.1 目标

- Markdown 的 block、inline span、代码块、引用、列表、表格、链接的解析与排版
  由 guest 提供；
- 工具卡的类型判断、patch 行解析、统计、标题、行号、命令输出的结构化与折叠
  结构由 guest 提供；
- 宿主只按稳定 display-list 绘制，且宿主代码里不再出现 Markdown / code_view；
- 调用可被取消，并对 request / response 与 display-list 规模设限；
- 流式 assistant 文本不会因渲染请求乱序而显示旧结果；
- 旧 session 可在不改存储格式的前提下重新生成渲染结果；
- Renderer 未加载或失败时退回纯文本；
- 现有 UI 的复制、滚动、链接点击、折叠状态与主题仍由宿主控制。

### 2.2 非目标

- 不把 egui 编译进 Wasm；
- 不让 Wasm 返回任意 widget tree 或 egui id；
- 不把现有 `PluginUiDocument` 扩展成富文本协议；
- 不在 Wasm 中执行文件、网络、进程或宿主工具能力；
- 不把渲染结果持久化到 session；
- 不改变 Agent 发给模型的 prompt、tool schema 或 session replay；
- 不替换普通插件的 `deluxe:harness@0.1.0` ABI；
- 第一阶段不支持第三方 Renderer 热插拔。

## 3. Component ABI

### 3.1 WIT world

```wit
package deluxe:harness@0.1.0;

interface renderer {
    render-message: async func(request-json: string) -> result<string, string>;
    render-tool: async func(request-json: string) -> result<string, string>;
}

// 普通插件：空实现，供插件平台加载。
world harness-plugin {
    import host;
    export plugin;
}

// guest 构建的 world：导出 plugin（空实现）+ renderer，不 import host。
world transcript-renderer {
    export plugin;
    export renderer;
}

// 平台侧可选视图：只有实现渲染的组件才导出 renderer。
world renderer-plugin {
    export renderer;
}
```

`renderer` 接口与两个 world 都放在共享的 `wit/deluxe-harness.wit` 里，与其它 guest
（`echo-tool`、`hooks-provider` 等）共用同一个 package：`wit-bindgen` 把单个 `wit/`
目录当作一个 package，所以渲染器的 world 必须和 harness 的 world 同 package，而不
是另开一个 `wit/` 目录。`harness-plugin` 本身不变，现有 mcp/hooks 组件无需重建。

渲染器的 Component 因此导出 `plugin` 与 `renderer` 两个接口。`plugin` 是空实现
（`list-tools`/`list-event-handlers` 返回 `"[]"`，其余返回 `Err`/`()`），存在的唯一
理由是 `ComponentActor` 的 bindgen 固定 `world: "harness-plugin"`，实例化时要求
`plugin` 导出。`renderer` 才是宿主真正调用的部分。

`SCHEMA_VERSION = 2`。请求与响应都用 `serde(rename_all = "camelCase")` 的 JSON，
版本号在 envelope 里，不在 WIT 里。

### 3.2 请求

消息：

```json
{
  "schemaVersion": 2,
  "revision": 7,
  "text": "…原始 Markdown…",
  "metrics": { "availableWidth": 820.0, "charWidth": 8.0, "columnGap": 12.0 }
}
```

工具：

```json
{
  "schemaVersion": 2,
  "revision": 3,
  "metrics": { "…": 0 },
  "tool": {
    "name": "apply_patch",
    "arguments": { "patch": "…" },
    "result": {
      "outcome": "executed",
      "output": "Applied 1 patch operation",
      "hunks": [{ "path": "src/main.rs", "lines": [40, 41, 41] }],
      "durationMs": 12
    }
  }
}
```

`result` 为 `null` 表示工具仍在运行。工具请求使用专门投影：图片永不跨边界，
patch 行号表被压平成面板需要的形状。

`metrics` 是 guest 唯一无法自己取得的信息。guest 没有字体，所以宿主传入内容
宽度和「一个平均字符的宽度」，guest 据此决定表格是 grid、records 还是 source。

### 3.3 响应

```json
{
  "schemaVersion": 2,
  "revision": 7,
  "kind": "message",
  "nodes": [ /* display-list */ ]
}
```

`kind` 必须是 `"message"` 或 `"tool"`，与入口对应。`revision` 原样回显。

## 4. Display-list IR

`src/renderer/protocol.rs` 定义 IR，guest 的 `src/display.rs` 是它的序列化镜像。
IR 是 egui-free、Wasmtime-free 的纯数据契约。

### 4.1 `Node`

```rust
enum Node {
    Text { runs: Vec<Run>, wrap: bool, selectable: bool },
    Column { children: Vec<Node> },
    Row { children: Vec<Node>, gap: f32, align: VerticalAlign },
    Indent { amount: f32, child: Box<Node> },
    Frame { fill: Option<ColorRole>, stroke: Option<ColorRole>, left_bar: Option<Border>,
            radius: CornerSpec, padding: EdgeInsets, stretch: bool, child: Box<Node> },
    Rule,
    Spacer { size: f32 },
    Grow,
    Scroll { max_height: f32, both_axes: bool, child: Box<Node> },
    Collapse { header: Vec<Run>, body: Vec<Node>, default_open: bool, show_background: bool },
    Icon { kind: IconKind, color: ColorRole, size: f32 },
    CopyButton { text: String, hover: String },
    Align { align: HorizontalAlign, width: Option<f32>, child: Box<Node> },
    Grid { columns: Vec<GridColumn>, gap: f32, header: Vec<Vec<Run>>,
           rows: Vec<Vec<Vec<Run>>>, rules: bool },
}
```

- `Grow` 是 `Row` 里的弹性空隙；宿主把 `Grow` 之后的尾巴从右向左布局，所以一个
  尾随按钮会贴到远端。
- `Grid` 的列宽由 guest 用 `charWidth` 估算后给出；宿主按给定宽度精确排布。
- `Collapse` 的 header 是一行 run，body 是节点数组——工具卡就是一棵
  `Collapse`。
- `Icon` 只给语义 `IconKind`，glyph 由宿主从 `icons.rs` 解析。

### 4.2 `Run`

```rust
struct Run {
    text: String,
    font: FontRole,           // Proportional | Monospace
    size: f32,                // 设计单位；宿主再乘主题字号
    color: ColorRole,
    background: Option<ColorRole>,  // 行内代码 chip、diff 底色
    italic: bool,
    underline: bool,
    link: Option<String>,
    icon: Option<IconKind>,   // 在 text 前加一个 glyph
}
```

### 4.3 语义角色

颜色与字体都是**角色**，不是值：guest 从不说「红色」，只说 `Danger`；宿主在
`present::resolve_color` 一处把角色映射到当前 `Palette`。这样主题切换、深浅色
都在宿主完成，guest 无需知道调色板。

```text
ColorRole: Text Muted Dim Accent Success Warning Danger
           DiffAddFg DiffDelFg DiffAddBg DiffDelBg
           CodeBg PanelBg PanelHeaderBg Border
FontRole:  Proportional Monospace
IconKind:  NotePencil Terminal File Folder Image Dots StopCircle Gear
           CheckCircle WarningCircle XCircle Spinner
```

### 4.4 egui id 由宿主派生

guest 从不产出 egui id。`present::draw` 用一个 `path: Vec<usize>` 记录节点在树中
的位置，`Scroll` / `Collapse` 用 `id_salt((base, path))` 取 id，`base` 是调用方
传入的 salt（通常含 session id 与 step index）。这保证滚动位置与折叠状态跨帧
稳定，且不同消息之间不串。

## 5. 宿主协议校验

`src/renderer/protocol.rs` 的 `decode(json, revision, expected)` 在响应进入 UI 前
完成全部校验：

- JSON decode；
- `schemaVersion` / `revision` / `kind` 必须匹配；
- 节点数、深度、run 数、表格行列数、单字段字节数、聚合文本字节数；
- 所有几何量必须有限（拒绝 `NaN` / `inf`）；
- 网格必须矩形（header 与每行的列数与 `columns` 一致）；
- 列宽必须为正；
- link 的长度、控制字符与 scheme（只允许 `http` / `https` / `mailto`，无 scheme
  的相对路径允许）。

边界值（定义在协议模块，测试用常量引用）：

```text
MAX_NODES                 = 16 * 1024
MAX_NODE_DEPTH            = 32
MAX_RUNS_PER_TEXT         = 4096
MAX_GRID_COLUMNS          = 64
MAX_GRID_ROWS             = 4096
MAX_TEXT_FIELD_BYTES      = 16 KiB
MAX_AGGREGATE_TEXT_BYTES  = 512 KiB
MAX_LINK_BYTES            = 4 KiB
```

校验失败时：不写 cache、不更新 UI、记录带 `revision` 与 kind 的 warning、当前项
切到 fallback、不重试完全相同的非法响应。校验失败归 `RENDER_INVALID_OUTPUT`。

## 6. 通用渲染器 `present.rs`

`present::draw(ui, palette, nodes, salt)` 是唯一把 display-list 变成 egui 的地方。
它不认识任何领域概念：

- `draw_nodes` / `draw_node` 分派每个 `Node`；
- `draw_row` 处理 `Grow` 的两段布局；
- `draw_frame` 应用 fill/stroke/圆角/内边距，并用 `painter().vline` 画 `left_bar`；
- `draw_text`：含 link 的文本走 `horizontal_wrapped` + `hyperlink_to`，普通文本
  合成一个 `LayoutJob`（`wrap=false` 时用 `no_max_width`）；
- `draw_grid` 先给每个 cell 取 galley，再按列宽与对齐在固定矩形里绘制；
- `resolve_color` 与 `font_id` 是角色 → 调色板/字体的唯一映射点；
- `icon_glyph` 是语义图标 → glyph 的唯一映射点。

## 7. Renderer runtime

渲染器不再有专属 actor。它由插件平台的 `ComponentActor`
（`src/plugins/wasm_runtime.rs`）加载与调用：实例化时按 `world: "harness-plugin"`
构造 `HarnessPlugin`，再按 `world: "renderer-plugin"` 尝试一次**可选**绑定
`RendererPlugin`——绑定失败即「这个 Component 不是渲染器」。

`Operation` 新增两个变体：

```rust
Operation::RenderMessage(String)   // 消息请求 JSON → display-list JSON
Operation::RenderTool(String)      // 工具请求 JSON → display-list JSON
```

- 有界 mpsc（32）+ `oneshot` 回复；调用可被取消，但没有 wall-clock 超时；
- `CancellationToken` 贯穿请求生命周期；
- 没有执行预算：fuel、内存、table、instance 与 payload 上限均未设置，调用可被取消；
- guest 的 `Err(string)` 是正常控制流，Store 仍可用；trap / 取消会
  poison Store，之后重建 instance；
- 渲染器实例是 **worker 全局单例**（纯计算、无 project 绑定）：启动时从
  `catalogue.global()` 找到 `transcript-renderer@deluxe-defaults` 并
  `ComponentActor::load`；插件被停用/卸载/加载失败时没有实例，发
  `Event::RendererAvailability { available: false }`。

调用流程：

```text
Cmd::RenderMessage / Cmd::RenderTool
    ↓  worker 序列化请求
ComponentActor（bounded queue + timeout + cancellation）
    ↓  Operation::RenderMessage / RenderTool
renderer export call
    ↓  响应字节上限
protocol::decode + validate
    ↓
Event::MessageRendered / Event::ToolRendered / Event::RenderFailed
```

## 8. GUI 与 worker 集成

### 8.1 IPC

```rust
Cmd::RenderMessage { key: RenderKey, revision: u64, text: String, metrics: RenderMetrics }
Cmd::RenderTool    { key: RenderKey, revision: u64, request: ToolRenderRequest }

Event::MessageRendered { key, revision, nodes: Arc<Vec<Node>> }
Event::ToolRendered    { key, revision, nodes: Arc<Vec<Node>> }
Event::RenderFailed    { key, revision, kind: RenderKind, message: String }
Event::RendererAvailability { available: bool }
```

`RendererAvailability` 在 worker 启动时、以及每次 `Cmd::ReloadPlugins` 之后发出。
`false → true`（renderer 恢复可用）会让 GUI 清空 render cache，使之前失败的条目
按新实例重试；`true → false` 只记一条 warning，绘制退回纯文本。

`RenderKey` 稳定标识一个 transcript step：

```text
Assistant / Notice / HostMessage:  session_id + step_index
Tool:                              session_id + call_id
```

不用完整文本作 key：文本在流式过程中变化，长文本还会放大哈希与日志成本。

### 8.2 Render cache

`src/app/render_cache.rs` 只存在于内存，不写 session。条目：

```text
latest_revision
fingerprint
status: Missing | Pending | Ready | Failed
nodes: Option<Arc<Vec<Node>>>
```

**fingerprint 必须包含布局宽度**（按 16 px 取整），因为 guest 用宽度决定表格
形态；只哈希文本会让 resize 后沿用旧布局。工具还额外哈希 name、arguments 与
result。

流式文本：GUI 每收到新 delta 递增 revision 并重新请求；响应按
`RenderKey + revision` 校验，旧 revision 到达时丢弃，不覆盖新内容。

### 8.3 绘制路径

- 消息：`present::draw` 画 `nodes`；`nodes` 未就绪时 `draw_plain_text` 画原始
  正文（selectable、wrapped、无 Markdown）。
- 工具卡：`nodes` 就绪时整张卡（折叠行 + 展开体）都是 display-list；未就绪时
  `draw_tool_fallback` 画「工具名 + 原始输出」的最小折叠。
- 折叠状态、滚动位置、copy 行为、链接点击由宿主与 egui 负责；guest 只描述结构。

## 9. Fallback

Renderer 是增强能力，不能成为 GUI 启动条件。

```text
启动 GUI
    ↓
worker 从 catalogue.global() 加载 transcript-renderer
    ├── 成功：Event::RendererAvailability { available: true } → display-list
    └── 失败：Event::RendererAvailability { available: false } → 全部纯文本
```

加载失败记录 warning，但不阻止 GUI 启动、session 加载、Agent 运行、工具执行或
plugin catalogue 加载。插件被停用/卸载后同样发 `available: false`。

单条消息/工具卡渲染失败时：

```text
消息：原始正文 → 宿主纯文本 Label
工具：工具名 + 原始 output → 宿主 selectable text 折叠
```

fallback 不重试完全相同的请求；只有输入 revision 变化或 instance 成功重建后才
允许再次提交。

## 10. 打包与构建

渲染器是一个**普通内置插件**，与 `hooks`、`mcp` 走同一套打包流程：

- ABI 仍是共享的 `deluxe.harness/plugin@0.1` package：`renderer` 接口与它的
  world 都并入了 `wit/deluxe-harness.wit`；
- 发布目录 `builtin-plugins/transcript-renderer/`，含 `plugin.json` 与
  `plugin.wasm`；
- `build.rs` 扫描 `builtin-plugins/` 下每个子目录并校验清单（因此
  `plugin.json` 的 `runtime.apiVersion` 必须是 `deluxe.harness/plugin@0.1`）；
- `plugin.wasm` 是提交进仓库的构建产物；首次运行由 `ensure_bundled_defaults`
  安装并启用。宿主用字节比对（`is_embedded_plugin`）判断「是否为内置」，
  所以**改了 guest 必须升 `plugin.json.version`**，否则用户机器上缓存的旧副本
  不会被刷新。

改了 guest 源码要重新构建并提交 `plugin.wasm`：

```bash
rustup target add wasm32-unknown-unknown
cargo build --manifest-path plugin-src/transcript-renderer/Cargo.toml \
    --release --target wasm32-unknown-unknown
cargo run --manifest-path plugin-src/componentize/Cargo.toml -- \
    plugin-src/transcript-renderer/target/wasm32-unknown-unknown/release/deluxe_transcript_renderer.wasm \
    builtin-plugins/transcript-renderer/plugin.wasm \
    wit
```

guest 不再自带 `wit/` 目录：它通过 `wit_bindgen::generate!({ path: "../../wit", world: "transcript-renderer" })` 复用仓库根部的共享 package。

## 11. 安全和资源限制

Renderer 虽无 host capability，也不能假设它天然安全。宿主限制：

- component 大小；
- instance memory / table / instances；
- 单次调用时间；
- request / response bytes；
- queue 长度；
- cache 中的对象数量；
- 单个 display-list 的节点/深度/run/grid 数量与聚合文本。

输入也限制：assistant 文本、tool arguments JSON、tool output、patch 文本、link
URL、表格行列数。工具请求使用专门投影，不把未经限制的 `serde_json::Value` 直接
塞进请求。

错误分类沿用插件平台的 `PLUGIN_*`（编译/实例化失败、trap、取消都由
`ComponentActor` 归类），只有 display-list 自身的契约错误由宿主另起一码：

```text
PLUGIN_*               加载、trap、取消（插件平台通用）
RENDER_INVALID_OUTPUT  display-list 协议解析或校验失败（`protocol.rs` 专用）
```

`ComponentActor` 在实例没有 `renderer` 绑定时返回
`Err("Component does not implement the renderer ABI")`，归类为 `PLUGIN_LOAD_FAILED`。

## 12. 测试

- **协议**（`protocol.rs`）：合法响应 decode；revision/schema/kind 不匹配拒绝；
  unknown 字段忽略；超长文本/链接拒绝；深度与节点数上限；非矩形网格拒绝；
  非有限几何拒绝；link scheme 限制。
- **Component**（`golden.rs`）：`render-message` 与 `render-tool` 产出可被协议
  接受的 display-list；消息含标题、强调、行内代码、链接、列表、引用、代码块与
  表格（表格渲染为 `Grid`）；工具卡是一棵 `Collapse`，含文件名、diff 与统计；
  guest error 不破坏 actor。
- **GUI**：renderer 不可用时窗口仍可绘制；乱序响应不覆盖新内容；展开状态不因
  response 重置；worker I/O 不发生在 egui `update` 路径。
- **guest**：`markdown.rs` / `code_view.rs` 内部的纯函数单测留在 guest crate。

## 13. 兼容性与版本升级

- `schemaVersion` 只接受当前版本；未来做兼容应显式 `decode vN -> normalize ->
  current`，不要在校验器里猜版本。
- 升级 renderer：改 WIT 或 JSON schema → 升 `plugin.json.version` → 更新测试 →
  重建并提交 `plugin.wasm` → 跑完整测试 → 确认 fallback 仍可用。
- Renderer 版本不写入 session。session 只存原始文本、工具参数与结果，因此换
  renderer 后可重新生成最新 display-list。
- 本次把 `renderer` 接口与两个 world **新增**进 `wit/deluxe-harness.wit`，但
  `harness-plugin` world 的编码类型未变，因此现有 mcp/hooks 组件无需重建；
  `PluginUiDocument`、`ToolDescriptor`、`ToolSettings`、`HOST_RULES`、
  `SUB_AGENT_RULES` 与普通插件 ABI 均未改动。

## 14. 日志

失败日志只记录长度、`RenderKey`、`revision` 与机器可读错误码，必要时截断 guest
error message；正常路径不输出完整 Markdown、工具参数或结果，避免泄漏用户代码
与命令。
