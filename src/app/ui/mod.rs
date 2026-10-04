//! UI 层的统一入口。
//!
//! 这个目录正在逐步从单文件 ui.rs 拆分而来。

mod common;
mod jobs;
mod sidebar;
mod widgets;

// 主实现文件，包含 impl App 的所有 UI 方法
#[path = "impl.rs"]
mod ui_impl;

// 重新导出主实现中的所有内容
pub use ui_impl::*;
