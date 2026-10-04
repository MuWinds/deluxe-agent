# UI Module Split Design

## 概述

将 `src/app/ui.rs` (2647 行) 拆分为职责清晰的子模块，提升代码可维护性和审查效率。拆分后保持所有公开接口不变，纯粹是内部重组。

## 目标

1. 降低单文件复杂度，便于 code review
2. 按职责组织代码，提升模块内聚性
3. 为插件系统提供清晰的 UI 组件访问路径
4. 支持并行开发，减少合并冲突

## 非目标

- 不改变任何对外 API
- 不重构业务逻辑
- 不影响现有测试

## 设计决策

### 1. 模块可见性策略

**选择：方案 C - 完全模块化**

所有子模块的主要函数标记为 `pub`，允许：
- 子模块之间相互调用
- 插件通过 `use crate::app::ui::widgets` 访问通用组件
- `app/mod.rs` 直接调用子模块函数

理由：为插件系统提供灵活的 UI 扩展能力。

### 2. 辅助函数归属

**选择：方案 C - 混合方案**

- 共享的工具函数放在 `ui/common.rs`，标记为 `pub`
- 单一模块使用的函数跟随使用者，作为模块私有或 `pub(super)`

理由：平衡职责清晰和代码复用。

### 3. 渲染缓存管理

**选择：方案 B - 提取到 app/render_cache.rs**

将 `collect_rendered()`, `dispatch_render()`, `next_revision()` 从 `impl App` 移到 `impl RenderCache`，保持 `RenderCache` 无状态，通过参数传递 `cmd_tx`。

理由：渲染缓存管理应该内聚，职责更清晰。

## 文件结构

```
src/app/
├── ui/
│   ├── mod.rs              (~300 行)
│   ├── common.rs           (~150 行)
│   ├── widgets.rs          (~200 行)
│   ├── menu.rs             (~200 行)
│   ├── sidebar.rs          (~400 行)
│   ├── composer.rs         (~550 行)
│   ├── transcript.rs       (~650 行)
│   ├── tool_cards.rs       (~350 行)
│   ├── jobs.rs             (~250 行)
│   └── dialogs.rs          (~400 行)
└── render_cache.rs         (增强后 ~350 行)
```

## 模块职责

### ui/mod.rs - UI 层统一入口
- 所有布局常量（`RAIL_WIDTH`, `COMPOSER_MAX_WIDTH`, `BUBBLE_PADDING_X` 等）
- `impl App::ui()` - 主绘制循环
- `impl App::draw_main()` - 中央面板协调器
- `impl App::intake_pasted_images()`, `intake_dropped_files()` - 图片 intake
- 类型定义：`Rendered`, `PendingRender`, `ToolCard<'a>`
- 子模块声明和 re-export

### ui/common.rs - 共享工具函数
- `pub fn format_tokens()` - token 数量格式化（1M/8k）
- `pub fn parse_token_count()` - 解析用户输入的 token 数
- `pub fn project_name()` - 从路径提取项目名
- `pub fn shorten()` - 截断长文本
- `pub fn render_metrics()` - 计算渲染度量
- `pub fn event_run_id()` - 从事件提取 RunId
- `pub fn job_status_text()` - 任务状态文本
- `pub fn format_elapsed()` - 格式化耗时
- `pub fn attachment_caption()` - 附件说明文字

### ui/widgets.rs - 可复用 UI 组件
- `pub fn circle_button()` - 圆形图标按钮
- `pub fn selectable_code()` - 可选择的代码块
- `pub fn remove_chip()` - 附件移除按钮
- `pub fn section_label()` - 分组标签
- `pub fn draw_logo()` - Logo 绘制
- `pub fn draw_empty_state()` - 空状态占位
- `pub fn transcript_thumb()` - 对话记录缩略图
- `pub fn append_run()` - LayoutJob 追加样式化文本段
- `pub fn chip_label()` - 附件 chip 标签文本

### ui/menu.rs - 顶部菜单和导航栏
- `impl App::draw_menu_bar()` - 文件/编辑/视图/帮助菜单
- `impl App::draw_rail()` - 左侧图标导航栏

### ui/sidebar.rs - 会话列表侧边栏
- `impl App::draw_sidebar()` - 项目列表、搜索框、会话历史
- `pub fn session_row()` - 单个会话行渲染
- `pub fn sidebar_row()` - 通用侧边栏行容器
- `pub fn rail_button()` - 导航栏圆形按钮

### ui/composer.rs - 输入区域
- `impl App::draw_composer()` - 输入框、附件条、发送按钮
- `impl App::draw_thinking_picker()` - 思考级别选择器
- `impl App::draw_context_gauge()` - 上下文使用率环形指示器
- `impl App::draw_pending_image_strip()` - 待发送图片预览条
- `pub fn clipboard_image()` - 剪贴板图片读取
- `pub fn clipboard_has_text()` - 剪贴板文本检测

### ui/transcript.rs - 对话记录渲染
- `pub fn draw_step()` - 单个对话步骤（用户消息+agent回复+工具调用）
- `pub fn draw_user_images()` - 用户消息中的图片附件
- `pub fn draw_reasoning_block()` - 思考过程折叠块
- `pub fn draw_agent_message()` - Agent 文本回复
- `pub fn draw_message_body()` - 消息体渲染（通过 renderer 插件或降级纯文本）
- `pub fn draw_plain_text()` - 纯文本降级渲染

### ui/tool_cards.rs - 工具调用卡片
- `pub fn draw_tool_card()` - 工具调用折叠卡片
- `pub fn draw_tool_fallback()` - renderer 不可用时的降级渲染
- `pub fn tool_fingerprint()` - 工具调用去重指纹
- `pub fn tool_result()` - ToolResult 转换为渲染请求
- `pub fn outcome_code()` - 执行结果状态码
- `pub fn outcome_colour()` - 执行结果颜色
- `pub fn outcome_icon()` - 执行结果图标

### ui/jobs.rs - 后台任务列表
- `impl App::draw_jobs()` - 后台任务横条（composer 下方）
- `pub fn draw_job_row()` - 单个任务行
- `pub fn job_button()` - 任务操作按钮
- `pub fn job_colour()` - 任务状态颜色映射
- `pub struct JobRowClick` - 任务行交互事件

### ui/dialogs.rs - 弹出窗口
- `impl App::draw_settings()` - 设置对话框
- `impl App::draw_about()` - 关于对话框
- `impl App::draw_plugins()` - 插件管理对话框
- `impl App::draw_subagent_window()` - 子 agent 对话窗口
- `pub fn draw_plugin_row()` - 插件列表行
- `pub fn draw_plugin_contents()` - 插件详情展开内容

### app/render_cache.rs - 渲染缓存管理（增强）
- 现有：`struct RenderCache` 数据结构
- 新增：`impl RenderCache::collect_rendered()` - 获取已完成的渲染节点
- 新增：`impl RenderCache::dispatch_render()` - 分发渲染请求（接受 `cmd_tx` 参数）
- 新增：`impl RenderCache::next_revision()` - 生成修订版本号

## 迁移策略

### 依赖关系

所有子模块的共同依赖：
```rust
use super::super::App;
use super::common;
use super::widgets;

use eframe::egui;
use egui::{Color32, FontId, RichText, ...};
use crate::theme::{self, Palette};
use crate::ipc::{RunState, JobView, AuditOutcome, ...};
use crate::session::{Session, Step, ToolResult};
```

特定模块额外依赖：
- `transcript.rs`: `crate::renderer::{present, protocol::*}`, `crate::llm::ThinkingLevel`
- `composer.rs`: `crate::config::InputModality`, `crate::attachments::ImageRef`
- `tool_cards.rs`: `serde_json::Value`, `crate::renderer::protocol::*`
- `dialogs.rs`: `crate::plugins::*`, `uuid::Uuid`

### 迁移顺序

按依赖关系从叶子节点向根迁移：

1. **common.rs** - 无依赖的工具函数
2. **widgets.rs** - 只依赖 common 的组件
3. **tool_cards.rs**, **menu.rs**, **sidebar.rs** - 使用 widgets 的模块
4. **jobs.rs** - 独立模块
5. **composer.rs**, **transcript.rs** - 使用多个子模块的复杂组件
6. **dialogs.rs** - 可能调用其他所有模块
7. **mod.rs** - 最后整合，保留 `impl App::ui()` 和 `draw_main()`
8. **render_cache.rs** - 独立增强

### Import 调整示例

**src/app/mod.rs** - 无需改动
```rust
mod ui;  // ui 现在是目录，自动加载 ui/mod.rs
```

**src/app/ui/mod.rs**
```rust
// 声明子模块
mod common;
mod widgets;
mod menu;
mod sidebar;
mod composer;
mod transcript;
mod tool_cards;
mod jobs;
mod dialogs;

// 公开常量
pub(super) const COMPOSER_MAX_WIDTH: f32 = 820.0;
pub(super) const CHAT_MARGIN_X: f32 = 16.0;
pub(super) const BUBBLE_EDGE_GAP: f32 = 14.0;

// impl App 的主入口保留在这里
impl App {
    pub fn ui(&mut self, ui: &mut egui::Ui, ...) { ... }
    fn draw_main(&mut self, ui: &mut egui::Ui, ...) { ... }
}
```

## 验证方案

### 编译验证
每完成一个模块迁移，立即验证：
```bash
cargo build --no-run
cargo clippy --all-targets -- -D warnings
```

### 测试验证
```bash
cargo test
cargo test agent_loop  # 关注 UI 相关测试
```

### 手动验证清单
1. ✓ 启动应用，显示主界面
2. ✓ 左侧边栏显示会话列表
3. ✓ 创建新会话，输入框可用
4. ✓ 发送消息，transcript 正常渲染
5. ✓ 工具调用卡片折叠/展开
6. ✓ 后台任务列表显示
7. ✓ 打开设置对话框
8. ✓ 打开插件管理
9. ✓ 主题切换（Dark/Light）
10. ✓ 图片附件粘贴和显示

### 回归风险点
- Import 路径错误
- 可见性问题（私有函数无法访问）
- 循环依赖
- 常量访问路径变化

## 提交策略

分多个提交完成，每个提交保持可编译：

```
commit 1: Create ui/ directory structure and skeleton files
commit 2: Move common utilities to ui/common.rs
commit 3: Move widgets to ui/widgets.rs
commit 4: Move menu and sidebar rendering
commit 5: Move composer rendering
commit 6: Move transcript rendering
commit 7: Move tool cards rendering
commit 8: Move jobs and dialogs rendering
commit 9: Move render cache methods to app/render_cache.rs
commit 10: Update ui/mod.rs and finalize imports
```

## 风险评估

### 已知风险
1. **编译时间增加** - 模块拆分后增量编译单元增多，但影响应该很小
2. **插件兼容性** - 如果插件访问了内部函数（不太可能），路径会变
3. **IDE 导航体验** - 从单文件跳转变成跨文件，但 rust-analyzer 处理得很好
4. **维护认知负担** - 需要理解 9 个文件而不是 1 个，但职责清晰后更容易定位

### 收益
✅ 代码审查更聚焦  
✅ 支持并行开发  
✅ 测试隔离  
✅ 插件集成友好  
✅ 性能优化更容易定位瓶颈  

### 回退方案
1. **快速回退** - `git revert HEAD~10..HEAD`
2. **部分回退** - 保留有价值的拆分，合并回部分模块
3. **渐进式修复** - 单独修复问题模块

### 合并策略
```bash
git checkout -b refactor/ui-module-split
# 完成所有拆分提交
cargo test && cargo clippy
# 手动测试所有 UI 功能
git checkout main
git merge refactor/ui-module-split
```

## 成功标准

- ✅ 所有测试通过（`cargo test`）
- ✅ Clippy 无警告（`cargo clippy -D warnings`）
- ✅ 手动验证清单全部通过
- ✅ 每个模块文件不超过 700 行
- ✅ 模块职责清晰，无循环依赖
- ✅ 插件可以访问通用 UI 组件

## 未来扩展

拆分后可以考虑的进一步优化：

1. **为每个模块添加单元测试** - 特别是 `common.rs` 和 `widgets.rs`
2. **性能分析** - 识别渲染瓶颈模块
3. **插件 UI 扩展 API** - 基于拆分后的结构设计插件 UI hook 点
4. **主题系统增强** - 将主题相关的渲染逻辑进一步集中
