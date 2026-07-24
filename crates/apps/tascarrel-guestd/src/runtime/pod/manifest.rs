//! Durable manifests for recoverable Btrfs store mutations.

use serde::Deserialize;
use serde::Serialize;

use super::ImageConfig;
use super::ImageId;
use super::PodId;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoreManifest {
    pub format_version: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ImageManifest {
    pub format_version: u32,
    pub id: ImageId,
    pub config: ImageConfig,
    pub created_at_unix_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PodManifest {
    pub format_version: u32,
    pub id: PodId,
    pub image: ImageId,
    pub created_at_unix_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TransactionManifest {
    pub format_version: u32,
    pub transaction_id: String,
    pub operation: TransactionOperation,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum TransactionOperation {
    StageImage,
    PublishImage { image: ImageId },
    CreatePod { pod: PodId, image: ImageId },
    DeletePod { pod: PodId },
    DeleteImage { image: ImageId },
}
