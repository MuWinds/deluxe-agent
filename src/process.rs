//! Starting helper processes without flashing a console window.
//!
//! A release build is a GUI program — `windows_subsystem = "windows"` in
//! `main.rs` — so this process owns no console. On Windows, a process with no
//! console that starts a console application (`pwsh`, `node`, an MCP server)
//! hands the child a **brand-new console**, and Windows gives that console a
//! visible window. That window pops up over the app for as long as the child
//! runs, which for a dev server is forever.
//!
//! The window is not inherited from the parent, so it cannot be suppressed
//! there: it has to be turned off on every spawn. `CREATE_NO_WINDOW` does that —
//! the child still gets a console (so it can allocate one, and its own children
//! inherit it rather than opening windows of their own), but the console is
//! never shown. It is the same reason Node's `child_process` grew
//! `windowsHide: true`.
//!
//! The flag is meaningless off Windows, so the helper compiles to nothing there
//! and callers stay free of `cfg`.

use tokio::process::Command;

/// `CREATE_NO_WINDOW`: run a console application without a console window.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Marks `command` so a console child it starts will not open a window.
///
/// Call this on the command just before `spawn`; it only touches the process
/// creation flags, so it can sit at the end of a builder chain.
pub(crate) fn hide_console(command: &mut Command) {
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    #[cfg(not(windows))]
    let _ = command;
}
