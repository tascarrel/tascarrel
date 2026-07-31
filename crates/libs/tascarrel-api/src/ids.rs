//! Stripe-style identifiers shared across the Tascarrel API.

use rand::Rng as _;

crate::ids::define_ids! {
    (
        ClientId,
        "client_",
        "Identifier for one client session attached to the host daemon."
    ),
    (
        GuestInstanceId,
        "guest_instance_",
        "Identifier for one running incarnation of a workspace VM."
    ),
    (
        HostInstanceId,
        "host_instance_",
        "Identifier for one running host daemon instance."
    ),
    (
        BrowserSessionId,
        "browser_session_",
        "Identifier for one durable authenticated browser session."
    ),
    (
        ConfigInstanceId,
        "config_instance_",
        "Identifier for one host read of workspace configuration inputs."
    ),
    (PodId, "pod_", "Stable pod identifier."),
    (
        ImageId,
        "image_",
        "Identifier for one image within a workspace guest daemon's inventory."
    ),
    (
        ProcessId,
        "process_",
        "Identifier for one process within a workspace guest daemon's supervision scope."
    ),
    (ChatId, "chat_", "Stable durable chat identifier."),
    (
        ChatBindingId,
        "chat_binding_",
        "Identifier for one chat binding attempt."
    ),
    (ChatTurnId, "chat_turn_", "Stable chat turn identifier."),
    (ChatItemId, "chat_item_", "Stable chat item identifier."),
    (
        ChatRequestId,
        "chat_request_",
        "Stable chat structured-request identifier."
    ),
    (
        ChatQuestionId,
        "chat_question_",
        "Stable chat question identifier."
    ),
    (
        ChatActivityId,
        "chat_activity_",
        "Stable chat activity identifier."
    ),
    (
        ChatAttachmentId,
        "chat_attachment_",
        "Stable chat attachment identifier."
    ),
    (
        ChatQueuedPromptId,
        "chat_queued_prompt_",
        "Identifier for one runtime queued chat prompt."
    ),
    (
        PortForwardId,
        "port_forward_",
        "Identifier for one dynamic port forward owned by the host daemon."
    ),
    (
        PodHostForwardId,
        "pod_host_forward_",
        "Identifier for one pod-scoped forward to a host-loopback port."
    ),
    (
        HttpRouteId,
        "http_route_",
        "Identifier for one host-issued HTTP route."
    ),
    (
        TcpFlowId,
        "tcp_flow_",
        "Identifier for one TCP flow handled by the host network service."
    ),
    (
        CodeSessionId,
        "code_session_",
        "Identifier for one host-owned Code session."
    ),
    (
        RepositoryCacheId,
        "repository_cache_",
        "Identifier for one workspace-isolated host repository cache."
    ),
    (
        RepositoryApprovalId,
        "repository_approval_",
        "Identifier for one host-owned repository publication approval request."
    ),
    (
        RepositoryPushId,
        "repository_push_",
        "Identifier for one host-owned repository push operation."
    ),
    (
        ShareOverlayApprovalId,
        "share_overlay_approval_",
        "Identifier for one host-owned overlay share approval request."
    ),
    (
        HostOperationId,
        "host_operation_",
        "Identifier for one durable approval-gated process executed by the host daemon."
    ),
    (
        AutomationExecutionId,
        "automation_execution_",
        "Identifier for one durable automation execution."
    ),
    (
        TraceId,
        "trace_",
        "Identifier for one distributed trace."
    ),
    (
        InvocationId,
        "invocation_",
        "Identifier for one link-local RPC invocation."
    ),
    (
        SubscriptionId,
        "subscription_",
        "Identifier for one link-local subscription."
    ),
}

/// Default number of Base58 characters generated after an identifier prefix.
pub const DEFAULT_LENGTH: usize = 22;

/// Reason a string could not be parsed as a Tascarrel identifier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParseIdError {
    /// The string does not begin with the identifier type's prefix.
    Prefix {
        /// Prefix required by the identifier type.
        expected: &'static str,
    },
    /// The Base58 suffix does not have the identifier type's configured length.
    Length {
        /// Number of suffix characters required by the identifier type.
        expected: usize,
        /// Number of suffix bytes found in the input.
        actual: usize,
    },
    /// The suffix contains a character outside the Base58 alphabet.
    Alphabet,
}

impl std::fmt::Display for ParseIdError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Prefix { expected } => {
                write!(formatter, "identifier must start with {expected}")
            }
            Self::Length { expected, actual } => write!(
                formatter,
                "identifier suffix must be {expected} characters, found {actual}"
            ),
            Self::Alphabet => formatter.write_str("identifier suffix must use the Base58 alphabet"),
        }
    }
}

impl std::error::Error for ParseIdError {}

const BASE58_ALPHABET: &[u8; 58] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
const ACCEPTANCE_BOUND: u8 = 232;
const RANDOM_BUFFER_LENGTH: usize = 32;

#[must_use]
fn is_base58(byte: u8) -> bool {
    BASE58_ALPHABET.contains(&byte)
}

fn append_generated_suffix(value: &mut crate::ArcStr, length: usize) {
    let target_length = value.len() + length;
    let mut random = [0_u8; RANDOM_BUFFER_LENGTH];
    let mut rng = rand::rng();

    while value.len() < target_length {
        rng.fill(&mut random);
        append_accepted(value, &random, target_length);
    }
}

fn append_accepted(value: &mut crate::ArcStr, random: &[u8], target_length: usize) {
    for &byte in random {
        if byte < ACCEPTANCE_BOUND {
            value.push(char::from(
                BASE58_ALPHABET[usize::from(byte) % BASE58_ALPHABET.len()],
            ));
            if value.len() == target_length {
                return;
            }
        }
    }
}

macro_rules! define_ids {
    (@length $length:expr) => {
        $length
    };
    (@length) => {
        $crate::ids::DEFAULT_LENGTH
    };
    ($(($name:ident, $prefix:literal, $doc:literal $(, $length:expr)?)),* $(,)?) => {
        $(
            #[doc = $doc]
            #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
            pub struct $name(pub $crate::ArcStr);

            impl $name {
                /// Prefix required for values of this identifier type.
                pub const PREFIX: &'static str = $prefix;

                /// Number of generated Base58 characters following the prefix.
                pub const LENGTH: usize =
                    $crate::ids::define_ids!(@length $($length)?);

                /// Generates a new identifier using operating-system-seeded randomness.
                #[must_use]
                pub fn generate() -> Self {
                    let mut value = $crate::ArcStr::with_capacity(
                        Self::PREFIX.len() + Self::LENGTH,
                    );
                    value.push_str(Self::PREFIX);
                    $crate::ids::append_generated_suffix(&mut value, Self::LENGTH);
                    Self(value)
                }
            }

            impl std::str::FromStr for $name {
                type Err = $crate::ids::ParseIdError;

                fn from_str(value: &str) -> Result<Self, Self::Err> {
                    let Some(suffix) = value.strip_prefix(Self::PREFIX) else {
                        return Err($crate::ids::ParseIdError::Prefix {
                            expected: Self::PREFIX,
                        });
                    };
                    if suffix.len() != Self::LENGTH {
                        return Err($crate::ids::ParseIdError::Length {
                            expected: Self::LENGTH,
                            actual: suffix.len(),
                        });
                    }
                    if !suffix.bytes().all($crate::ids::is_base58) {
                        return Err($crate::ids::ParseIdError::Alphabet);
                    }
                    Ok(Self(value.into()))
                }
            }

            impl serde::Serialize for $name {
                fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
                where
                    S: serde::Serializer,
                {
                    serializer.serialize_str(&self.0)
                }
            }

            impl<'de> serde::Deserialize<'de> for $name {
                fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
                where
                    D: serde::Deserializer<'de>,
                {
                    let value = <String as serde::Deserialize>::deserialize(deserializer)?;
                    <Self as std::str::FromStr>::from_str(&value)
                        .map_err(serde::de::Error::custom)
                }
            }
        )*
    };
}

pub(crate) use define_ids;
