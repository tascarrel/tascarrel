//! Hand-written behavior attached to generated API values.

mod changes;
mod chats;
mod files;
mod host_operations;
mod network;
mod processes;
mod repositories;
mod workspaces;

pub use chats::ChatCostCenterIdParseError;
pub use chats::MAX_CHAT_COST_CENTER_ID_BYTES;
pub use chats::is_valid_chat_cost_center_id;
pub use files::MAX_RELATIVE_PATH_BYTES;
pub use network::PortMappingError;
pub use processes::ProcessTerminalData;
pub use processes::ProcessTerminalDataDecodeError;
