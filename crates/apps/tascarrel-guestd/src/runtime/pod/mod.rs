//! Guest-owned Btrfs image storage and native pod runtime.
//!
//! The store keeps immutable image generations and gives each pod four
//! independent writable subvolumes: a snapshot of its image root, an empty
//! workspace, an empty Docker data root, and quota-limited temporary storage.
//! All multi-step mutations are recorded in durable transaction manifests,
//! committed with bounded Btrfs transaction waits, and recovered when the
//! store is reopened. Per-resource locks keep an unrelated pod usable while
//! another mutation waits. Pods are executed through runc with a mandatory
//! outer user namespace, idmapped Btrfs mounts, private namespaces, and
//! workspace-policy capabilities and devices.

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]
#![forbid(unsafe_code)]

#[cfg(not(target_os = "linux"))]
compile_error!("Tascarrel's pod runtime requires Linux and Btrfs");

mod command;
mod id;
mod manifest;
mod runtime;
mod store;

pub use command::CommandOutput;
pub use command::CommandRunner;
pub use command::ProcessCommandRunner;
pub use id::ImageId;
pub use id::PodId;
pub use runtime::ContainerStatus;
pub use runtime::CreatedPod;
pub use runtime::ID_MAP_SIZE;
pub use runtime::POD_DEVICE_SOURCE_ROOT;
pub use runtime::POD_ID_MAP_SIZE;
pub use runtime::PodDevice;
pub use runtime::PodDeviceKind;
pub use runtime::PodMounts;
pub use runtime::PodPolicy;
pub use runtime::PodPrograms;
pub use runtime::PodRuntime;
pub use runtime::PodShare;
pub use runtime::RuntimeConfig;
pub use runtime::RuntimeError;
pub use store::BtrfsStore;
pub use store::ImageConfig;
pub use store::ImageGeneration;
pub use store::ImageUser;
pub use store::PodStorage;
pub use store::StoreError;
