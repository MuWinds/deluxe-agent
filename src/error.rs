//! The error type shared by the tool layer and the agent loop.

/// Machine-readable error codes.
pub mod code {
    pub const INVALID_PARAMS: &str = "invalid_params";
    pub const TOOL_NOT_FOUND: &str = "tool_not_found";
    pub const DENIED: &str = "denied";
    pub const TIMEOUT: &str = "timeout";
    pub const IO: &str = "io";
    pub const INTERNAL: &str = "internal";
    pub const LLM: &str = "llm";
    pub const CANCELLED: &str = "cancelled";
}

#[derive(Debug, Clone)]
pub struct AgentError {
    pub code: &'static str,
    pub message: String,
}

impl AgentError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(code::INVALID_PARAMS, message)
    }

    pub fn tool_not_found(name: &str) -> Self {
        Self::new(code::TOOL_NOT_FOUND, format!("Unknown tool `{name}`"))
    }

    /// A call the host refused to run, as opposed to one that ran and failed.
    pub fn denied(message: impl Into<String>) -> Self {
        Self::new(code::DENIED, message)
    }

    pub fn timeout(message: impl Into<String>) -> Self {
        Self::new(code::TIMEOUT, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(code::INTERNAL, message)
    }

    pub fn llm(message: impl Into<String>) -> Self {
        Self::new(code::LLM, message)
    }

    pub fn cancelled() -> Self {
        Self::new(code::CANCELLED, "The run was cancelled")
    }

    pub fn from_io(context: &str, error: std::io::Error) -> Self {
        Self::new(code::IO, format!("{context}: {error}"))
    }

    /// Whether this error means "the host refused to run it", which the agent
    /// loop reports to the model as a refusal rather than a failure.
    pub fn is_denial(&self) -> bool {
        matches!(self.code, code::DENIED)
    }
}

impl std::fmt::Display for AgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for AgentError {}

pub type Result<T> = std::result::Result<T, AgentError>;
