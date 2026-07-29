//! Hand-written behavior attached to generated API values.

mod changes;
mod files;
mod host_operations;
mod network;
mod processes;
mod repositories;
mod workspaces;

pub use files::MAX_RELATIVE_PATH_BYTES;
pub use network::PortMappingError;
pub use processes::ProcessTerminalData;
pub use processes::ProcessTerminalDataDecodeError;
