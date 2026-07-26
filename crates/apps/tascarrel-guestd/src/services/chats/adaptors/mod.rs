//! Concrete adaptors for supported coding harnesses.
//!
//! Provider wire types are intentionally minimal: each adaptor models only
//! the fields it consumes, and Serde ignores the rest. They should not grow
//! merely to mirror a provider's complete protocol.

mod claude_code;
mod codex;
mod tasci;

pub use claude_code::ClaudeCodeAdaptor;
pub use codex::CodexAdaptor;
pub use tasci::TasciAdaptor;
pub use tasci::TasciConfigurationStore;
