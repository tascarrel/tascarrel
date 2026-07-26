//! Provider-neutral HTTP model transports.

mod openai_chat;

pub use openai_chat::HttpAuthorization;
pub use openai_chat::OpenAiChatBackend;
