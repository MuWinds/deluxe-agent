//! UI 层的统一入口。

mod common;
mod composer;
mod jobs;
mod messages;
mod panels;
mod primitives;
mod tools;
mod transcript;
mod widgets;
mod windows;

// 主实现文件，包含 impl App 的核心 UI 方法
mod impl_ui;

// 重新导出测试需要的类型和常量
#[allow(unused_imports)]
pub use composer::COMPOSER_MAX_WIDTH;
#[allow(unused_imports)]
pub use messages::{draw_bubble, Message, BUBBLE_EDGE_GAP, CHAT_MARGIN_X};
