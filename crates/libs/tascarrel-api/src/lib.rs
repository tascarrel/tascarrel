//! Sidex-defined API types, operation registries, and configuration values
//! shared by Tascarrel components.

mod collections;
mod configuration;
mod extensions;
mod operations;

pub use collections::ArcStr;
pub use collections::ArcVec;
pub use configuration::BinarySizeError;
pub use configuration::parse_memory_mib;
pub use configuration::parse_size_bytes;
pub use extensions::ChatCostCenterIdParseError;
pub use extensions::MAX_CHAT_COST_CENTER_ID_BYTES;
pub use extensions::MAX_RELATIVE_PATH_BYTES;
pub use extensions::PortMappingError;
pub use extensions::ProcessTerminalData;
pub use extensions::ProcessTerminalDataDecodeError;
pub use extensions::is_valid_chat_cost_center_id;
pub use operations::Action;
pub use operations::GuestAction;
pub use operations::GuestSubscription;
pub use operations::HostAction;
pub use operations::HostSubscription;
pub use operations::Subscription;

/// Maximum accepted byte length of one workspace configuration input file.
pub const MAX_WORKSPACE_CONFIG_BYTES: u64 = 4 * 1024 * 1024;

/// Maximum accepted byte length of the host-wide server configuration file.
pub const MAX_SERVER_CONFIG_BYTES: u64 = 64 * 1024;

pub mod ids;

pub mod types {
    sidex::include_bundle! {
        #[doc(hidden)]
        tascarrel_api as generated
    }

    pub use generated::*;
}
