# AGENTS.md

`deluxe-agent` 的协作约定。本文件会被 agent 自己当作项目指令读取（`src/agent.rs`
的 `PROJECT_CONTEXT_FILES`：`AGENTS.md` → `CLAUDE.md` → `README.md`，每个文件上限
16 KB），所以这里的规则对人和对模型同样生效。

技术栈：Rust 2021（`rust-version = "1.88"`）、eframe/egui 0.36 桌面界面、
tokio 多线程运行时。没有 `lib` target —— 全部代码都是 `src/main.rs` 这个二进制
crate 的模块。

---

## Commands

```bash
# 构建
cargo build                       # debug：保留控制台，能看到 tracing 输出
cargo build --release             # release：GUI 子系统，Windows 下无控制台窗口

# 运行
cargo run

# 测试
cargo test                        # 全部
cargo test apply_patch            # 按名字过滤
cargo test --no-run               # 只编译测试，快速确认类型是否通过

# 格式与静态检查（提交前必过）
cargo fmt
cargo fmt --check
cargo clippy --all-targets -- -D warnings

# 日志级别（仅 debug 构建可见）
# PowerShell:  $env:RUST_LOG = "debug"; cargo run
# bash:        RUST_LOG=debug cargo run
```

长耗时命令放到后台跑（`exec` 的 `runInBackground`，用 `job_output` 读输出、
`job_kill` 结束），不要空转轮询。

---

## Testing

本仓库是**单元测试为主的原地测试**：没有 `tests/` 目录（因为没有 `lib` target），
测试与被测代码在同一个文件。

| 位置 | 形态 | 用途 |
| --- | --- | --- |
| 各模块文件底部 | `#[cfg(test)] mod tests { use super::*; ... }` | 单元测试，可测私有函数 |
| `src/agent_loop_tests.rs` | 由 `main.rs` 的 `#[cfg(test)] mod agent_loop_tests;` 挂载 | agent 循环端到端测试 |


规则：

- **不碰用户的真实状态**。测试必须把状态重定向到临时目录：

  ```rust
  let config_dir = tempfile::tempdir().expect("a temp directory is available");
  std::env::set_var(crate::config::CONFIG_DIR_ENV, config_dir.path());
  ```

  同理，不要读写真实的 `~/.deluxe-agents` 或系统 keyring。
- **不依赖网络**。`agent_loop_tests.rs` 用 `FakeServer` 绑定 `127.0.0.1:0` 并返回
  预设的 SSE 片段；新写这类测试照抄该模式，不要打真实模型接口。
- 需要文件系统时用 `tempfile`（已在 `[dev-dependencies]`），不要往仓库里落临时文件。
- 断言带上下文消息：`assert!(cond, "expected `{command}` to be refused")`，失败时能定位。
- 异步测试用 `#[tokio::test]`，不要自己搭 runtime。
- `unwrap()` / `expect()` 只允许出现在测试里（见 Boundaries）。

---

## Project Structure

```
deluxe-agent/
├── Cargo.toml             # 依赖、release profile、对 egui-winit 的 [patch.crates-io]
├── Cargo.lock             # 提交进仓库（二进制 crate）
├── AGENTS.md              # 本文件
├── vendor/egui-winit/     # 0.36.2 的本地补丁副本（见 Boundaries，勿删）
└── src/
    ├── main.rs            # 入口：tracing、config/session/plugin 预加载、tokio runtime、eframe
    ├── app/               # 桌面外壳：视图状态、事件折叠与 egui 绘制
    │   ├── mod.rs         # 视图状态与状态机（Cmd 下行 / Event 上行）
    │   ├── render_cache.rs# 渲染 IR 的内存缓存
    │   └── ui.rs          # 全部 egui 绘制：布局、调色板取用与自由函数
    ├── agent.rs           # agent 循环：system prompt 组装、工具分发、流式回合
    ├── agent_loop_tests.rs# 循环层的端到端测试
    ├── llm.rs             # 供应商无关的会话类型：Message / AssistantTurn / Usage / ThinkingLevel
    ├── config.rs          # config.toml 读写、重试策略、通用 secret 解析（get-secret 的后端）
    ├── session.rs         # 会话与步骤的持久化
    ├── context.rs         # 上下文窗口与压缩（compaction）
    ├── ipc.rs             # 主线程 ↔ worker 的 Cmd / Event 消息
    ├── error.rs           # AgentError + code 常量 + Result 别名
    ├── attachments.rs     # 图片附件存储（ImageRef）
    ├── image_ops.rs       # 图片缩放 / 重编码（png、zune-jpeg）
    ├── renderer/          # 内置渲染器插件：display-list 协议 + 宿主通用渲染
    │   ├── protocol.rs    # display-list IR、请求投影与 decode/validate（RENDER_* 错误码）
    │   └── present.rs     # 通用 display-list → egui 渲染器
    ├── theme.rs           # 调色板与主题
    ├── fonts.rs  icons.rs
    ├── process.rs         # hide_console：Windows 下子进程不开控制台窗口
    ├── tools/             # 工具层：read_file、list_dir、exec、apply_patch、job_*、read_image
    │   ├── mod.rs         # Tool trait、ToolRegistry、ObjectSchema、参数校验
    │   ├── fs.rs  shell.rs  patch.rs  jobs.rs  image.rs  settings.rs
    ├── mcp/               # MCP 客户端与传输（stdio / HTTP）
    └── plugins/           # wasmtime 插件运行时
        ├── llm.rs         # LlmProvider：宿主侧 socket + 组件 codec 的适配器
        └── descriptor.rs  # PluginDescriptor：plugin.describe 的通用投影
```

线程模型（`main.rs` 顶部注释，全仓库依赖此约定）：

1. egui 窗口占用主线程，**永不阻塞**；
2. 多线程 tokio runtime 跑在 worker 线程，承担全部 I/O；
3. 两者用两个 channel 通信 —— `Cmd` 下行、`Event` 上行；agent 通过
   `Context::request_repaint` 唤醒窗口。

新增一个工具的三处落点：在 `src/tools/<name>.rs` 实现 `Tool`、在
`ToolRegistry::with_builtins` 中 `register`、`ToolDescriptor` 会把它自动带进
prompt 的 `<tools>` / `<rules>` 段落（不需要手抄一份工具清单）。

Transcript renderer（内置默认插件）：

- guest 在 `plugin-src/transcript-renderer/`（独立 crate，不进根 workspace）：
  `src/lib.rs` 是 `wit-bindgen` 导出，`src/markdown.rs` / `src/code_view.rs` 是
  Markdown 与工具面板的**全部**解析与排版（只产出通用 display-list），
  `src/display.rs` 是 display-list 的 guest 镜像；
- 它的 ABI 在共享的 `wit/deluxe-harness.wit` 里：`interface renderer`
  （`render-message` / `render-tool`），以及两个 world —— `transcript-renderer`
  （导出 `plugin` + `renderer`，不 import host）和 `renderer-plugin`
  （只导出 `renderer`，供平台做可选绑定）；
- 它以**普通内置插件**的形式发布：`builtin-plugins/transcript-renderer/`
  （`plugin.json` + `plugin.wasm`），首次运行由 `ensure_bundled_defaults` 安装并
  启用，在插件面板可见、可停用。`plugin.wasm` 是提交进仓库的构建产物（CI 不构建
  Component）。改了 guest 源码要重新构建并提交它（并升 `plugin.json.version`，
  否则缓存副本不会被刷新）：

  ```bash
  rustup target add wasm32-unknown-unknown
  cargo build --manifest-path plugin-src/transcript-renderer/Cargo.toml \
      --release --target wasm32-unknown-unknown
  cargo run --manifest-path plugin-src/componentize/Cargo.toml -- \
      plugin-src/transcript-renderer/target/wasm32-unknown-unknown/release/deluxe_transcript_renderer.wasm \
      builtin-plugins/transcript-renderer/plugin.wasm \
      wit
  ```
- 宿主通过插件平台调用它：`ComponentActor` 实例化时按 `renderer-plugin` world 做一次
  **可选**绑定（失败即「不是渲染器」），`Operation::RenderMessage` / `RenderTool`
  分派到 `render-message` / `render-tool`；worker 启动时从 `catalogue.global()` 找到
  `transcript-renderer@deluxe-defaults` 并加载一个全局单例（渲染与项目无关）。插件
  被停用/卸载/加载失败时发 `Event::RendererAvailability { available: false }`，
  GUI 退回纯文本；
- 宿主**不认识 Markdown 或工具语义**：`src/renderer/protocol.rs` 定义通用的
  display-list（`Node` / `Run` / 语义 `ColorRole` / `FontRole`），
  `src/renderer/present.rs` 是唯一把它画成 egui 的地方。宿主代码里不得再出现
  markdown / code_view 的解析或排版痕迹；
- renderer 不可用时**退回纯文本**：消息直接画原始正文，工具卡退化为
  「工具名 + 原始输出」的最小折叠。`src/renderer/golden.rs` 的测试断言 guest
  产出的 display-list 形状与内容。

LLM provider（内置默认插件，模型访问的唯一实现）：

- guest 在 `plugin-src/llm-provider/`（独立 crate，不进根 workspace）：
  `src/lib.rs` 是 `wit-bindgen` 导出，`src/config.rs` 拥有**整份**供应商配置
  （档案、当前选中、内联密钥），`src/codec/` 是三种协议的全部请求整形与流式解码
  —— `openai_chat.rs` / `openai_responses.rs` / `anthropic.rs`，共用
  `canonical.rs` 的供应商无关类型与 `sse.rs` 的行缓冲。`codec/` 不引用
  wit-bindgen，因此能在宿主 target 上直接 `cargo test`；
- 它的 ABI 在共享的 `wit/deluxe-harness.wit` 里：`interface llm`
  （`build-request` / `parse-stream` / `close-stream` / `parse-complete`）与 world
  `llm-provider`（导出 `plugin` + `llm`，import host 的 `read-plugin-file` /
  `write-plugin-file` / `get-secret`）。**自描述 `describe` 属于通用 `plugin`
  接口**（每个 Component 都实现它），不属于 `llm`；
- **宿主不拥有 endpoint、模型名与密钥**：`src/config.rs` 只剩重试策略，
  `LlmSettings` 只剩 `context` / `retry_count` / `retry_forever`。宿主通过
  `plugin.describe()` 拿到一份通用 JSON，只读其中的 `ready` /
  `capabilities.imageInput` / `limits.contextTokens`（见
  `src/plugins/descriptor.rs` 的 `PluginDescriptor`），据此决定是否注册
  `read_image`、如何播种压缩预算；
- **socket 归宿主、codec 归组件**：`ComponentActor` 严格串行且没有 guest→host
  回调，若由组件持 socket 会阻塞整个 actor。所以 `src/plugins/llm.rs` 的
  `LlmProvider` 拥有 `reqwest::Client`：`build_request` 拿到组件整形的
  `{method,url,headers,body}`，逐块 `parse_stream` 回灌，再把解出的
  `events` / `turn` 转成 `LlmStreamEvent` 推给 sink。重试、退避与取消都在这里，
  与旧原生客户端一致；宿主里不再有 `LlmProvider` trait，只有这一个结构体；
- 它以**普通内置插件**的形式发布：`builtin-plugins/llm-provider/`
  （`plugin.json` + `plugin.wasm`），`ensure_bundled_defaults` 安装并启用，在
  插件面板可见、可停用。`plugin.wasm` 是提交进仓库的构建产物（CI 不构建
  Component）。改了 guest 源码要重新构建并提交它（并升 `plugin.json.version`，
  否则缓存副本不会被刷新）：

  ```bash
  rustup target add wasm32-unknown-unknown
  cargo build --manifest-path plugin-src/llm-provider/Cargo.toml \
      --release --target wasm32-unknown-unknown
  cargo run --manifest-path plugin-src/componentize/Cargo.toml -- \
      plugin-src/llm-provider/target/wasm32-unknown-unknown/release/deluxe_llm_provider.wasm \
      builtin-plugins/llm-provider/plugin.wasm \
      wit
  ```
- worker 启动时 `load_llm_provider` 从 `catalogue.global()` 找到
  `llm-provider@deluxe-defaults` 加载一个全局单例；通用的
  `Event::PluginAvailability` 与 `Event::PluginDescriptor` 报告可用性与
  `describe` 的投影，插件重载后重发。不可用时每次运行都失败并给出明确提示，
  GUI 也不会放行发送；
- 供应商配置由 guest 的 `providers` surface 编辑（`src/surface.rs`），宿主只把
  声明式 `PluginUiDocument` 画出来；`get-secret` 由 `src/config.rs::resolve_secret`
  支撑（环境变量 `DELUXE_AGENT_SECRET_<NAME>` 或 OS keyring，服务名
  `deluxe-agent`）；
- **输入行的模型选择器是 composer surface，不是宿主 IPC**：manifest 用
  `ui.composer = true` 声明（见 `src/plugins/wasm_manifest.rs`），worker 为它开一个
  `surfaceId = COMPOSER_SURFACE_ID`（`"composer"`）的 surface 并绑定全局配置根；
  GUI 按 `surface_id` 把它画进输入行（`src/app/ui/composer.rs`），不弹窗。guest 的
  `surface.rs` 按 `surfaceId` 分派出 `composer_document()`（一个 `select` +
  `select_model` 动作）。composer 的模型切换后，worker 在下次 run 失效全部缓存
  runtime 并重新 `describe`，宿主不认识任何供应商字段；
- 端到端测试在 `src/agent_loop_tests.rs`：`provider_for` 把内置组件指向
  `FakeServer` 的地址加载起来，从而走的是**应用真正发布的同一份 codec**。

---

## Code Style

遵循 <https://doc.rust-lang.org/style-guide/>，并按 **rustfmt 默认配置**格式化
（仓库没有 `rustfmt.toml`）：4 空格缩进、行宽 100、尾随逗号、`use` 分组排序。
注释按 80 列手工折行。提交前 `cargo fmt` 与 `cargo clippy -D warnings` 必须干净。

要点：

- **命名**：类型 / trait `UpperCamelCase`，函数与变量 `snake_case`，常量与 `static`
  `SCREAMING_SNAKE_CASE`；布尔量用肯定式短语（`mutating`、
  `host_validates_arguments`）。
- **文档注释**：模块顶部 `//!`，条目 `///`。写**为什么**，不要复述代码做了什么。
- **错误**：统一用 `crate::error::{AgentError, Result}`；`code` 常量是机器可读的，
  `message` 面向模型可读。
- **序列化**：对外结构体一律 `#[serde(rename_all = "camelCase")]`；可选字段配
  `#[serde(default, skip_serializing_if = ...)]`。
- **异步**：工具实现 `#[async_trait::async_trait] impl Tool`；可取消的耗时路径传入
  `CancellationToken`。
- **用户没有明确说明的情况下不要考虑历史兼容**
- 注释与文档里的标识符用反引号包裹（rustdoc 风格）。

### 注释规范

**只有下面四种用途才值得写**，其余情况一律不写

1. **非常难理解的代码段**：算法、位运算、协议字节序、`unsafe` 的安全前提、
   平台差异的绕行。读者盯着看三遍还不确定的，才配一段注释。
2. **分类用**：一段字段、一列分支或一份常量表太长时，用注释标出每一组是什么，
   让读者能跳读。
3. **特殊情况**：必须交代的前提、约束、已知坑，或"为什么不能写成更直观的那种写法"。
4. **数字代表枚举的地方必须写注释**，写明每个取值的含义：

   ```rust
   const CREATE_NO_WINDOW: u32 = 0x0800_0000; // 以无窗口的控制台启动子进程
   let tier = 100; // 100 = 钻石
   ```

另外：**通用工具方法类的函数必须写 `///` 文档注释**，说明它做什么、参数、返回值、
以及在什么条件下返回 `Err`（会 panic 的要专门写 `# Panics`）。这是公共 API 的契约，
不属于上面"可以不写"的范畴。

**严禁**

- 整行的 `// ====`、`// ----`、`// ****`、`// ~~~~` 之类的分隔注释 —— 零信息量。
- `// 增加计数`、`// 返回结果`、`// 循环遍历列表` 这类复述代码的注释 —— 看一眼代码
  就知道，纯噪声。
- 注释掉的死代码。删掉它，版本历史会记得。
- 注释与代码不一致。改了代码就同步改注释，宁可删掉注释。

模块文档、错误处理与工具实现的骨架（照这个写）：

```rust
//! `apply_patch` — create, update, delete, and move files with a patch.
//!
//! The patch parser is self-contained on purpose: patch semantics should not
//! depend on shell quoting or on a platform-specific `patch` binary.

use std::time::Instant;

use serde_json::{json, Value};

use super::settings::ToolSettings;
use super::{required_str, ContentBlock, ObjectSchema, Tool, ToolDescriptor, ToolOutput};
use crate::error::{AgentError, Result};

pub struct ApplyPatch;

#[async_trait::async_trait]
impl Tool for ApplyPatch {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "apply_patch".into(),
            summary: "Create, update, delete, or move files with a patch".into(),
            // `guidelines` 是跨工具的行为规则，会被注入 system prompt 的 <rules> 段。
            guidelines: vec![
                "Edit files with `apply_patch`, never by scripting `exec` around \
                 `sed`, `echo`, or shell redirections."
                    .into(),
            ],
            host_validates_arguments: true,
            mutating: true,
            input_schema: ObjectSchema {
                schema_type: "object".into(),
                properties: serde_json::from_value(json!({
                    "patch": { "type": "string" },
                }))
                .expect("schema must be an object"),
                required: vec!["patch".into()],
            },
        }
    }

    async fn execute(&self, arguments: Value, settings: &ToolSettings) -> Result<ToolOutput> {
        let started = Instant::now();
        let patch = required_str(&arguments, "patch")?;
        let changes = parse_patch(&patch)?;
        if changes.is_empty() {
            return Err(AgentError::invalid_params("Patch contains no file operations"));
        }

        Ok(ToolOutput {
            content: vec![ContentBlock::text("Applied 1 patch operation")],
            is_error: false,
            truncated: false,
            original_bytes: None,
            duration_ms: Some(started.elapsed().as_millis() as u64),
            hunks: Vec::new(),
        })
    }
}
```

测试的写法：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// 这条断言为什么重要，一句话说清；必要时补一段背景说明。
    #[test]
    fn turning_the_guard_off_lets_everything_through() {
        let settings = ToolSettings {
            block_destructive_commands: false,
            ..Default::default()
        };
        assert!(settings.destructive_reason("rm -rf /").is_none());
    }
}
```

---

## Git Workflow

默认分支 `main`。`.gitignore` 已经就位，至少忽略：

```
/target
/build.log
/build_err.log
```

`.gitattributes` 固定 `* text=auto eol=lf`：仓库内所有文件都以 LF 存储，因为
`apply_patch` 会把 CRLF 文件按 CRLF 写回，一次静默的换行转换就会让所有 hunk 失配。

> `Cargo.lock` 要提交（二进制 crate）；`vendor/egui-winit` 也要提交，它是
> `[patch.crates-io]` 指向的依赖源。

提交规范：

- **一次提交只做一件事**，且能编译、能过测试；不要混入无关的格式化改动。
- 首行是**祈使句、英文、不超过 72 字符**，正文写动机与取舍：

  ```
  Add job_kill so a background command can be stopped

  Why the job registry hands out cancellation handles, and what a kill does
  to a process already past its timeout.
  ```

- 行为变更要在同一提交里补上或更新对应测试。
- `main` 始终可构建；功能分支用 `feat/`、`fix/`、`chore/`、`docs/` 前缀。
- 推送前本地跑：`cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`。
- 不提交密钥、keyring 内容或用户配置（`~/.deluxe-agents`、本机 config 目录）。

---

## Boundaries

**必须遵守**

- 改文件用 `apply_patch`；不要用 `sed`、`echo`、shell 重定向去“编辑”源码
  （这同时也是工具层 `apply_patch` 的 guideline）。
- 改文件前先读文件，让 patch 的上下文与磁盘字节一致。
- 同一处或相邻的改动放进**一个** patch，而不是两个。
- 上下文尽量小但唯一，不要用大段未改动内容填充 hunk。
- 工具调用前用一句话说明要做什么；任务结束时给简短总结，不要仅为确认而再调工具。
- 工具报错就调整策略，不要原样重试；调用被拒绝说明宿主拦截了它，不要绕开。

**禁止**

- **不要删除 `Cargo.toml` 的 `[patch.crates-io]` 与 `vendor/egui-winit`**。它是
  0.36.2 上移除一处 `return` 的补丁，让 Ctrl+V 粘贴图片的按键事件能到达应用；
  只有上游 <https://github.com/emilk/egui/pull/8472> 落地后才能一并移除。
- **不要给 release profile 加 `panic = "abort"`**。tokio worker 里的 panic 会带走整个
  进程，而这个进程持有窗口。
- 非测试代码不要 `unwrap()` / `expect()` / `panic!()`，用 `AgentError` 返回错误。
- 不要削弱 `ToolSettings::block_destructive_commands` 的默认值（默认 `true`），也不要
  缩小 `DESTRUCTIVE_PATTERNS` 的覆盖面，除非任务明确要求完全无限制。
- 不要在 egui 主线程（`update`、事件处理）上做阻塞 I/O；I/O 一律进 worker。
- 不要为了跑测试而修改用户的真实配置、keyring 或 home 目录下的插件。
- 不要提交 `target/`、`build.log`、`build_err.log`。

**先确认再动**

- 升降 `eframe` / `egui` 版本（会牵动 `vendor/egui-winit` 补丁与 `egui-phosphor`
  的版本对齐）。
- 改动 `ToolSettings` 的公开字段或 `ToolDescriptor` 的形状（会改变模型看到的 schema
  与 system prompt，破坏 provider 的 prompt cache 前缀稳定性）。
- 改动 `HOST_RULES` / `SUB_AGENT_RULES` 的文本（同上，前缀要求字节稳定）。
- 放宽 `crate::plugins` 的信任边界 —— 插件 hook 是任意 shell 命令。

**已知坑**

- 路径解析没有沙箱：`ToolSettings::resolve` 只做 `~` 展开与相对路径拼接，绝对路径
  原样通过，`..` 交给操作系统。不要假设这里有包含性检查。
- `read_image` 只在模型声明支持图像输入时注册（`ToolRegistry::with_image_input`）；
  文本模型看不到该工具，相关测试依赖这一门控。
- Windows 下 release 是 GUI 子系统，启动任何控制台子进程都要经过
  `crate::process::hide_console`，否则会弹出控制台窗口。
