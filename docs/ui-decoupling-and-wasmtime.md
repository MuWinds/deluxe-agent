# GUI 解耦与 Wasmtime 插件 UI 设计与实现文档

## 1. 背景

当前 GUI 已经初步拆成状态层和绘制层：

- [src/app/mod.rs](C:/Users/MuWinds/Documents/Coding%20Project/github/deluxe-agent/src/app/mod.rs) 负责应用状态、事件折叠和命令发送。
- [src/app/ui.rs](C:/Users/MuWinds/Documents/Coding%20Project/github/deluxe-agent/src/app/ui.rs) 负责 egui 绘制。
- [src/ipc.rs](C:/Users/MuWinds/Documents/Coding%20Project/github/deluxe-agent/src/ipc.rs) 负责 GUI 与 worker 之间的消息传递。
- [src/plugins/mod.rs](C:/Users/MuWinds/Documents/Coding%20Project/github/deluxe-agent/src/plugins/mod.rs) 负责插件发现、作用域和插件内容加载。

但目前的解耦仍然停留在“文件级拆分”，还没有形成稳定的架构边界：

1. `app/mod.rs` 仍然直接依赖 `egui` 类型，例如 `egui::Context`、`TextureHandle`。
2. `ChannelSink` 同时承担事件发送和 `egui::Context::request_repaint()`，worker 层间接持有 GUI 类型。
3. `ui.rs` 通过 `use super::*` 直接访问和修改大量 `App` 内部状态。
4. 绘制函数中仍然包含配置修改、持久化、主题更新、图片读取、IPC 命令触发等副作用。
5. `Actions` 虽然已经是延迟执行机制，但目前仍然是 GUI 内部私有结构，不是稳定的 UI 意图协议。
6. [src/plugins/manifest.rs](C:/Users/MuWinds/Documents/Coding%20Project/github/deluxe-agent/src/plugins/manifest.rs) 描述 Wasmtime 插件的 manifest、skills、commands、agents 和 UI 能力。

这些问题如果直接延伸到 Wasmtime，最容易出现的错误是让 Wasm 插件直接操作 `egui::Ui` 或 `egui::Painter`。这样会把 egui 的生命周期、线程模型、版本升级和宿主内部布局全部暴露给插件，最终形成另一种更严重的耦合。

---

## 2. 总体决策

### 2.1 Wasm 插件不直接绘制 egui

推荐采用：

> **Wasm 插件产生声明式 UI 描述，宿主负责布局、绘制、主题、事件和安全控制。**

插件只返回类似下面的结构：

```text
Surface
  └── Column
        ├── Text
        ├── Button
        ├── TextInput
        ├── Checkbox
        └── Select
```

宿主将这个结构转换成 egui 控件。

禁止插件直接获得：

- `egui::Context`
- `egui::Ui`
- `egui::Painter`
- `eframe::Frame`
- `TextureHandle`
- `mpsc::Sender<Cmd>`
- 文件系统句柄
- 网络客户端
- 任意 shell 执行能力

这样 Wasmtime 只是插件执行环境，而不是 GUI 的实现环境。

### 2.2 插件 UI 运行在 worker，不运行在 egui 主线程

线程边界固定为：

```text
egui 主线程
    │
    │ Cmd::PluginUiAction
    ▼
tokio worker / Wasmtime runtime
    │
    │ Event::PluginUiUpdated
    ▼
egui 主线程
```

GUI 绘制期间只能读取已经准备好的插件 UI 快照。

用户点击插件控件后：

1. GUI 生成一个纯数据的 UI 意图。
2. 意图通过 `Cmd` 发送到 worker。
3. worker 调用 Wasm 插件。
4. 插件返回新的 UI 状态或 UI patch。
5. worker 通过 `Event` 把结果发回 GUI。
6. GUI 折叠事件并在下一帧绘制。

`App::ui()` 绝不能同步调用 Wasmtime。否则插件执行时间、死循环或资源消耗会直接阻塞窗口。

---

## 3. 目标架构

建议最终形成四层。

```text
┌──────────────────────────────────────┐
│ egui Renderer                        │
│ 只负责控件绘制和输入采集              │
└───────────────────┬──────────────────┘
                    │ UiIntent
┌───────────────────▼──────────────────┐
│ App Controller / View State           │
│ 折叠 Event，生成 Cmd，不持有 egui 资源 │
└───────────────────┬──────────────────┘
                    │ Cmd / Event
┌───────────────────▼──────────────────┐
│ Plugin Runtime Host                  │
│ Wasmtime 实例、能力、超时、状态        │
└───────────────────┬──────────────────┘
                    │ versioned protocol
┌───────────────────▼──────────────────┐
│ Wasm Plugin                          │
│ 只处理状态、事件和宿主能力调用          │
└──────────────────────────────────────┘
```

### 3.1 Domain State

负责：

- 会话和步骤
- 当前选中会话
- agent 运行状态
- 配置
- 插件目录
- 插件运行状态
- 插件 UI 快照

不允许包含：

- `egui::Context`
- `TextureHandle`
- `Color32`
- `Rect`
- `Response`
- `Painter`
- egui layout 类型

### 3.2 Controller

负责：

- 消费 `Event`
- 更新 domain state
- 生成 `Cmd`
- 生成可供绘制的 view model
- 处理 GUI 意图

Controller 不负责：

- 创建 egui 控件
- 计算 egui 布局
- 读取图片文件
- 调用 Wasmtime
- 写配置文件

### 3.3 Renderer

负责：

- 读取 view model
- 调用 egui 绘制
- 收集用户输入
- 生成 `UiIntent`
- 持有 GUI 专属资源，例如纹理缓存

Renderer 可以知道 egui，但不能知道 Wasmtime 的内部结构。

### 3.4 Plugin Runtime

负责：

- 加载和实例化 Wasm
- 检查插件作用域和启用状态
- 执行插件事件处理
- 管理插件状态
- 施加资源限制
- 将插件输出验证为合法 UI 协议
- 通过 IPC 向 GUI 发布快照

---

## 4. 当前代码需要调整的边界

### 4.1 从 `app/mod.rs` 移除 GUI 资源

当前 `app/mod.rs` 中的 GUI 依赖应逐步收敛。

建议将下面这些内容移出状态层：

- `egui::Context`
- `TextureHandle`
- 图片纹理缓存
- repaint 句柄
- 仅用于绘制的尺寸和颜色常量
- egui 事件读取

`App` 可以继续作为应用控制器，但其字段应只保存可测试的业务状态和通道。

建议的后续模块结构：

```text
src/app/
├── mod.rs          # 兼容入口和模块导出
├── state.rs        # AppState、SessionViewState、PluginViewState
├── controller.rs   # Event -> State、UiIntent -> Cmd
├── intents.rs      # GUI 产生的纯数据意图
├── view_model.rs   # 绘制所需的只读投影
├── ui.rs           # egui renderer
├── plugin_ui.rs    # 插件 UI 协议的 egui renderer
└── resources.rs    # TextureCache 等 GUI 专属资源
```

不必一次性重命名所有现有类型。第一步可以保留 `App` 名称，但先把 GUI 专属字段迁移出去。

### 4.2 拆分 `ChannelSink`

当前 `ChannelSink` 同时持有：

```text
mpsc::UnboundedSender<Event>
egui::Context
```

这会让 worker 事件通道依赖 egui。

建议拆成两个概念：

```text
EventSink
    只负责 Event -> GUI channel

RepaintSignal
    只负责通知 GUI 重新绘制
```

worker 侧只依赖：

```rust
trait AgentEventSink {
    fn emit(&self, event: AgentEvent);
}
```

GUI 侧可以在主线程将 channel 接收和 repaint 绑定起来，但这个绑定不应进入插件运行时或 agent worker 的核心接口。

目标是：

```text
worker:
    EventSink::emit(event)

GUI adapter:
    receive event
    fold event
    request_repaint()
```

这样 Wasmtime runtime、agent loop 和测试都不需要知道 egui 存在。

### 4.3 把绘制中的副作用变成意图

目前部分绘制函数会直接修改配置、保存设置或发送命令。后续应统一采用：

```text
绘制阶段：
    widget clicked
        -> UiIntent

绘制结束后：
    apply_intents()
        -> 修改状态
        -> 发送 Cmd
        -> 请求保存
```

例如：

```rust
enum UiIntent {
    NewSession,
    SelectSession(Uuid),
    SendPrompt,
    CancelRun(RunId),
    SetTheme(ThemeChoice),
    SetSidebarVisible(bool),
    SetPluginEnabled {
        id: String,
        enabled: bool,
    },
    PluginUiAction(PluginUiAction),
}
```

这和现有 `Actions` 的思路一致，但应该扩大为稳定的控制器输入，而不是只服务于某一帧的临时绘制。

---

## 5. 插件 UI 协议

## 5.1 顶层结构

插件 UI 文档应当是版本化的纯数据结构：

```rust
struct PluginUiDocument {
    schema_version: u32,
    plugin_id: String,
    surface_id: String,
    revision: u64,
    title: String,
    root: UiNode,
}
```

关键字段：

- `schema_version`：UI 协议版本。
- `plugin_id`：插件身份。
- `surface_id`：插件内部页面或面板身份。
- `revision`：防止旧响应覆盖新状态。
- `root`：声明式控件树。

插件返回的文档必须经过宿主验证，不能直接信任。

## 5.2 控件节点

第一版只支持有限且稳定的控件：

```rust
enum UiNode {
    Empty,

    Column {
        children: Vec<UiNode>,
    },

    Row {
        children: Vec<UiNode>,
    },

    Section {
        id: String,
        title: String,
        children: Vec<UiNode>,
    },

    Text {
        text: String,
        emphasis: TextEmphasis,
    },

    Button {
        id: String,
        label: String,
        action: String,
        enabled: bool,
    },

    TextInput {
        id: String,
        value: String,
        placeholder: Option<String>,
        action: String,
    },

    Checkbox {
        id: String,
        label: String,
        value: bool,
        action: String,
    },

    Select {
        id: String,
        value: String,
        options: Vec<SelectOption>,
        action: String,
    },

    Progress {
        value: f32,
        label: Option<String>,
    },

    Divider,
}
```

第一版不建议支持：

- 任意 egui widget
- 自定义闭包
- 自定义 Rust 代码
- 任意 `Painter` 绘制
- 任意绝对坐标
- 让插件改变窗口主布局
- 让插件覆盖菜单栏、系统对话框或安全确认框

这样可以保持宿主对布局、主题和交互的一致控制。

## 5.3 UI 事件

UI 控件只产生结构化事件：

```rust
struct PluginUiAction {
    plugin_id: String,
    project: PathBuf,
    surface_id: String,
    control_id: String,
    action: String,
    value: Option<UiValue>,
}
```

`UiValue` 只允许有限类型：

```rust
enum UiValue {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Strings(Vec<String>),
}
```

插件不能通过事件携带任意宿主对象、文件句柄或线程对象。

控件 ID 必须在插件作用域内唯一。宿主内部最终使用：

```text
plugin_id / surface_id / control_id
```

避免不同插件之间发生状态或事件串线。

---

## 6. IPC 设计

在 [src/ipc.rs](C:/Users/MuWinds/Documents/Coding%20Project/github/deluxe-agent/src/ipc.rs) 中新增插件 UI 消息时，建议保持“请求”和“结果”分离。

### 6.1 GUI 到 worker

```rust
Cmd::OpenPluginSurface {
    plugin_id: String,
    project: PathBuf,
    surface_id: String,
}

Cmd::PluginUiAction {
    plugin_id: String,
    project: PathBuf,
    surface_id: String,
    control_id: String,
    action: String,
    value: Option<UiValue>,
}

Cmd::ClosePluginSurface {
    plugin_id: String,
    project: PathBuf,
    surface_id: String,
}
```

如果打开 surface 可以由应用自动完成，也可以只保留 `PluginUiAction`，但显式的打开和关闭命令更容易管理插件生命周期。

### 6.2 worker 到 GUI

```rust
Event::PluginUiUpdated {
    plugin_id: String,
    project: PathBuf,
    surface_id: String,
    revision: u64,
    document: Arc<PluginUiDocument>,
}

Event::PluginUiClosed {
    plugin_id: String,
    project: PathBuf,
    surface_id: String,
}

Event::PluginUiFailed {
    plugin_id: String,
    project: PathBuf,
    surface_id: String,
    message: String,
    recoverable: bool,
}
```

GUI 应丢弃以下事件：

- 插件已经被禁用后的旧事件。
- 不属于当前项目的项目级插件事件。
- `revision` 小于当前快照的事件。
- 已关闭 surface 的迟到事件。

插件 UI 事件不能写入主会话 transcript，除非插件明确调用一个受控的宿主通知能力。

---

## 7. 插件作用域和生命周期

现有插件系统已经区分：

- 全局插件
- 项目级插件
- 已禁用插件

这个边界必须继续作为 Wasm UI 的唯一来源。

### 7.1 加载流程

```text
启动时：
    发现 manifest
    读取插件元数据
    不执行 Wasm

worker 初始化项目：
    PluginCatalogue::for_project(project)
    过滤启用插件
    按选中插件的 global/project scope 解析 Wasmtime Component
    仅将当前 global/project configuration root 绑定给 Component；Hooks/MCP
    Component 通过通用 read-plugin-file 自己读取其中的 .hooks.json/.mcp.json
    解析 Component 声明的 UI surface
    创建 Wasmtime 实例
    注册允许的 capability
    获取初始 UI 文档

GUI：
    收到 PluginUiUpdated
    保存快照
    显示插件入口
```

插件页面打开或点击刷新时，GUI 只发送一次 worker discovery 请求。worker 重新读取
当前 global/project scope 的插件目录并替换 catalogue；用户打开具体 surface 后，
worker 再从该插件根目录动态实例化最新的 Wasmtime Component。GUI 不直接读取
`.wasm`，也不在 egui 主线程执行 Wasmtime。

插件页面的“添加到全局”和“添加到当前项目”会打开 Wasmtime Component
（`.wasm`）文件选择器。worker 根据所选 Component 所在目录的
根目录的 `plugin.json` 定位插件根目录，校验 manifest、Wasmtime ABI 和
component entry 后，将整个插件根目录复制到受管理的本地插件缓存，登记
`name@deluxe-local`，再按所选 scope 更新配置和 catalogue。导入失败会回滚本次
新复制的缓存，不覆盖已有同版本组件。

禁用插件不能实例化 Wasm，也不能创建 MCP、hook 或 UI runtime。

### 7.2 项目切换

用户切换项目后：

1. 关闭旧项目的项目级插件 surface。
2. 停止或回收旧项目插件实例。
3. 重新调用 `PluginCatalogue::for_project()`。
4. 创建新项目有效的插件 runtime。
5. 重新请求插件 UI 快照。

不能复用一个项目的插件实例处理另一个项目的 action。

---

## 8. Wasmtime 运行时边界

Wasmtime 只应由 worker 层使用。

建议的 capability 分层：

```text
基础能力：
    读取插件自身配置
    保存插件自身状态
    返回 UI 文档
    接收 UI action

可选能力：
    发送宿主通知
    请求打开文件选择器
    请求读取用户明确选择的文件
    调用宿主注册的安全命令

默认禁止：
    任意文件系统访问
    任意网络访问
    任意进程启动
    任意 shell
    访问 API key
    访问其他插件状态
    访问未授权项目目录
```

运行时需要限制：

- 最大 Wasm 内存
- 最大表大小
- 最大调用时间
- fuel 或 epoch interruption
- 最大 UI 节点数量
- 最大树深度
- 单个文本字段最大长度
- 单次事件返回大小
- 单个插件的并发实例数

UI 文档验证至少应检查：

```text
schema_version 是否支持
plugin_id 是否与当前实例一致
surface_id 是否合法
revision 是否递增
节点数量是否超过上限
树深度是否超过上限
字符串长度是否超过上限
控件 id 是否重复
action 是否符合插件声明
```

插件异常、超时或返回非法 UI 时，只影响该插件 surface，不应导致主窗口退出。

---

## 9. 推荐的实现阶段

### Phase 0：先冻结边界

目标：不引入 Wasmtime，先把宿主 UI 边界确定下来。

工作内容：

1. 明确 `App` 的 domain state 和 GUI resource。
2. 把 `TextureHandle`、`egui::Context` 从状态层迁出。
3. 将 `ChannelSink` 拆成事件发送和 repaint 适配。
4. 将 `Actions` 升级为 `UiIntent`。
5. 将绘制中的配置保存、命令发送等副作用统一移到 intent apply 阶段。

验收标准：

- `app/state.rs` 不依赖 egui。
- controller 单元测试不需要创建 egui context。
- worker 和 agent 测试不需要链接 GUI 资源。
- `ui.rs` 仍然可以正常绘制现有功能。

### Phase 1：引入宿主内部 UI 文档模型

目标：先让桌面内置 UI 也可以使用“状态 -> view model -> renderer”的流程。

工作内容：

1. 新增 `app/view_model.rs`。
2. 将插件 UI 协议的 `UiNode`、`UiValue`、`UiDocument` 定义为宿主纯数据结构。
3. 新增 `app/plugin_ui.rs`，实现 egui renderer。
4. 加入节点数、深度、文本长度和 ID 校验。
5. 用一个内存构造的测试文档验证渲染和事件生成。

验收标准：

- renderer 只读取 `PluginUiDocument`。
- renderer 不知道 Wasmtime。
- renderer 只能返回 `PluginUiAction`。
- 非法文档不会导致 GUI panic。

### Phase 2：接入 IPC

目标：使插件 UI 可以通过 worker 与 GUI 异步通信，但暂时使用 fake runtime。

工作内容：

1. 在 `Cmd` / `Event` 中加入插件 UI 消息。
2. 在 worker 中增加 `PluginUiExecutor` trait。
3. 测试环境使用 fake executor。
4. 实现 revision、项目作用域和插件禁用状态检查。
5. 加入 action 超时和失败事件。

推荐接口：

```rust
#[async_trait::async_trait]
trait PluginUiExecutor: Send {
    async fn open_surface(
        &mut self,
        request: OpenSurfaceRequest,
    ) -> Result<PluginUiDocument>;

    async fn handle_action(
        &mut self,
        action: PluginUiAction,
    ) -> Result<PluginUiUpdate>;
}
```

先使用 trait 隔离运行时，后续再由 Wasmtime 实现该 trait。

### Phase 3：引入 Wasmtime adapter

目标：只增加 Wasmtime 适配层，不改变 GUI renderer 和 IPC 协议。

建议新增：

```text
src/plugins/runtime.rs
src/plugins/wasm.rs
src/plugins/capabilities.rs
src/plugins/ui_protocol.rs
```

其中：

- `runtime.rs`：插件实例生命周期和项目作用域。
- `wasm.rs`：Wasmtime engine、store、instance。
- `capabilities.rs`：宿主能力注册和权限检查。
- `ui_protocol.rs`：Wasm 边界上的编码、解码和版本控制。

GUI 层不应该出现 `wasmtime::Store`、`wasmtime::Instance` 或任何 Wasm 类型。

### Phase 4：扩展 Wasmtime 插件 manifest

在 manifest 中增加可选 UI 声明，例如：

```json
{
  "name": "example-plugin",
  "version": "1.0.0",
  "interface": {
    "displayName": "Example Plugin",
    "shortDescription": "..."
  },
  "runtime": {
    "module": "./runtime/plugin.wasm",
    "ui": {
      "surfaces": ["main", "settings"]
    }
  }
}
```

规则：

- 每个插件必须声明合法的 Wasmtime `runtime`；缺失或非法时跳过整个插件。
- 未知 runtime 类型不会回退到旧插件路径。
- UI surface 列表用于宿主展示入口，但实际权限仍由运行时校验。

---

## 10. 测试策略

### 协议测试

覆盖：

- UI 文档序列化和反序列化。
- 未知字段忽略。
- 不支持的 schema version 拒绝。
- 节点深度、数量、文本长度限制。
- 重复控件 ID 拒绝。
- 非法 action 拒绝。
- revision 递增和旧事件丢弃。

### renderer 测试

覆盖：

- `Button` 生成正确的 `PluginUiAction`。
- `TextInput` 返回最新文本。
- `Checkbox` 返回布尔值。
- `Select` 只允许声明过的选项。
- disabled 控件不产生 action。
- 空节点和错误节点不会破坏宿主布局。

### IPC 测试

覆盖：

- action 从 GUI 到 worker 的完整流转。
- worker 更新快照后 GUI 正确折叠。
- 项目切换后旧项目事件被丢弃。
- 禁用插件无法打开 surface。
- 关闭 surface 后迟到事件被丢弃。
- 插件错误只影响单个 surface。

### runtime 测试

覆盖：

- Wasm 无限循环会被终止。
- 超出内存或 fuel 限制时返回错误。
- 未授权能力调用被拒绝。
- 插件不能访问其他插件的状态。
- 插件不能绕过项目作用域访问其他项目。
- Wasm panic 或 trap 不会带走主窗口。

测试中不应调用真实网络、真实用户配置或真实插件目录。Wasmtime 测试可以使用仓库内最小 fixture，或者先使用 fake `PluginUiExecutor` 覆盖 IPC 和 GUI 行为。

---

## 11. 不建议采用的方案

### 11.1 直接把 `egui::Ui` 暴露给 Wasm

问题：

- egui 不是稳定插件 ABI。
- egui 类型无法直接跨 Wasm 边界传递。
- 生命周期和借用关系复杂。
- 插件代码会依赖宿主版本。
- 插件可以影响宿主布局和交互。
- 很难实施超时和资源限制。
- 运行时只能在 GUI 主线程执行。

### 11.2 让插件每帧返回完整绘制命令

问题：

- UI 更新频率不可控。
- 文本和布局数据反复传输。
- 插件执行时间直接影响帧率。
- 事件和状态很难关联。
- 容易演变为另一套不受约束的 immediate-mode GUI。

更合适的方式是事件驱动的快照或 patch：

```text
插件状态改变
    -> 返回新的 UI snapshot / patch
    -> GUI 在下一帧绘制
```

### 11.3 让插件直接发送 `Cmd`

问题：

- 绕过宿主权限检查。
- 插件可以伪造任意会话或配置操作。
- 插件作用域难以审计。
- 以后增加新的 Cmd 会自动扩大插件权限。

插件只能发送声明过的 action，由 worker 根据 capability 映射为宿主操作。

---

## 12. 最终验收标准

完成这套改造后，应满足：

1. `app` 状态层不依赖 egui。
2. Wasmtime 只存在于 worker/plugin runtime 层。
3. GUI 绘制不会同步调用 Wasm。
4. 插件不能直接获得 `egui::Ui` 或 `egui::Painter`。
5. 插件 UI 只通过版本化声明式协议描述。
6. 所有插件 UI 事件都经过 IPC 和 capability 校验。
7. 全局插件和项目级插件的作用域保持不变。
8. 禁用插件不会启动 runtime。
9. 插件崩溃、超时或返回非法 UI 不会影响主窗口。
10. 没有合法 Wasmtime runtime 的目录不会进入插件 catalogue。
11. 宿主可以更换 egui 版本而不要求重新编译插件。
12. 后续增加新的宿主控件时，只需要扩展协议和 renderer，不需要修改插件 ABI。

## 结论

当前最重要的不是立即加入 Wasmtime，而是先完成 **GUI 状态、控制器、renderer、IPC、插件 runtime** 之间的边界冻结。

推荐执行顺序是：

```text
移除 app 状态层的 egui 依赖
    -> 将 Actions 升级为 UiIntent
    -> 定义声明式 PluginUiDocument
    -> 接入 Cmd/Event
    -> 用 fake runtime 打通流程
    -> 最后接入 Wasmtime
```

这样后续 Wasmtime 插件修改 UI 时，插件修改的是稳定的 UI 协议，而不是宿主当前版本的 egui 实现。
