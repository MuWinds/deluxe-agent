//! The error type shared by the tool layer and the agent loop.

pub mod code {
    pub const INVALID_PARAMS: &str = "invalid_params";
    pub const TOOL_NOT_FOUND: &str = "tool_not_found";
    pub const DENIED: &str = "denied";
    pub const TIMEOUT: &str = "timeout";
    pub const IO: &str = "io";
    pub const INTERNAL: &str = "internal";
    pub const LLM: &str = "llm";
    pub const CANCELLED: &str = "cancelled";
    pub const PLUGIN_LOAD_FAILED: &str = "plugin_load_failed";
    pub const PLUGIN_API_MISMATCH: &str = "plugin_api_mismatch";
    pub const PLUGIN_TRAP: &str = "plugin_trap";
    pub const PLUGIN_TIMEOUT: &str = "plugin_timeout";
    pub const PLUGIN_CANCELLED: &str = "plugin_cancelled";
    pub const PLUGIN_RESOURCE_LIMIT: &str = "plugin_resource_limit";
    pub const PLUGIN_INVALID_OUTPUT: &str = "plugin_invalid_output";
    pub const PLUGIN_PERMISSION_DENIED: &str = "plugin_permission_denied";
}

/// The one error type the tool layer and the loop pass around.
///
/// `code` is the stable, machine-readable half — callers branch on it, so its
/// values live in [`code`] rather than as bare strings — and `message` is the
/// human- and model-facing half, safe to show in a transcript.
#[derive(Debug, Clone)]
pub struct AgentError {
    pub code: &'static str,
    pub message: String,
}

impl AgentError {
    /// Builds an error from an explicit code and message.
    ///
    /// Prefer the `code`-specific constructors below; reach for this only when
    /// a caller genuinely owns the code.
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// A tool call whose arguments were missing or malformed — the model's
    /// fault, and worth reporting back so it can retry.
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(code::INVALID_PARAMS, message)
    }

    /// A call naming a tool that is not registered.
    pub fn tool_not_found(name: &str) -> Self {
        Self::new(code::TOOL_NOT_FOUND, format!("Unknown tool `{name}`"))
    }

    /// A call the host refused to run, as opposed to one that ran and failed.
    pub fn denied(message: impl Into<String>) -> Self {
        Self::new(code::DENIED, message)
    }

    /// A call that outlived its deadline and was stopped.
    pub fn timeout(message: impl Into<String>) -> Self {
        Self::new(code::TIMEOUT, message)
    }

    /// An invariant this program expected to hold did not. Distinct from
    /// [`invalid_params`](Self::invalid_params): the model could not have
    /// avoided it.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(code::INTERNAL, message)
    }

    /// A request to the model endpoint failed — transport, status or parse.
    pub fn llm(message: impl Into<String>) -> Self {
        Self::new(code::LLM, message)
    }

    /// The user stopped the run. Not a failure: the loop ends quietly on it.
    pub fn cancelled() -> Self {
        Self::new(code::CANCELLED, "The run was cancelled")
    }

    /// Wraps an [`std::io::Error`] with the operation that produced it, since
    /// an `io::Error` alone rarely says *what* was being attempted.
    pub fn from_io(context: &str, error: std::io::Error) -> Self {
        Self::new(code::IO, format!("{context}: {error}"))
    }

    /// Whether this error means "the host refused to run it", which the agent
    /// loop reports to the model as a refusal rather than a failure.
    pub fn is_denial(&self) -> bool {
        matches!(self.code, code::DENIED | code::PLUGIN_PERMISSION_DENIED)
    }
}

impl std::fmt::Display for AgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for AgentError {}

pub type Result<T> = std::result::Result<T, AgentError>;
