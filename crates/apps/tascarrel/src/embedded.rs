/// Metadata and bytes for the architecture-specific, versioned Tascarrel
/// payload. The compressed tar archive contains the guest system image, its
/// Linux kernel and initrd, their command line, and any additional packaged
/// assets.
#[derive(Clone, Copy, Debug)]
pub struct EmbeddedPayload {
    pub architecture: &'static str,
    pub sha256: &'static str,
    pub size: u64,
    pub compressed: &'static [u8],
}

#[cfg(tascarrel_embedded_payload)]
const COMPRESSED_PAYLOAD_SIZE: usize = parse_usize(env!("TASCARREL_PAYLOAD_SIZE"));

#[cfg(tascarrel_embedded_payload)]
#[allow(unsafe_code)]
unsafe extern "C" {
    // The payload object and its size metadata are produced together by Nix.
    #[link_name = "tascarrel_compressed_payload"]
    safe static COMPRESSED_PAYLOAD: [u8; COMPRESSED_PAYLOAD_SIZE];
}

/// Returns the payload embedded by the distribution build. Ordinary
/// development builds intentionally have no payload.
#[must_use]
pub fn payload() -> Option<EmbeddedPayload> {
    #[cfg(tascarrel_embedded_payload)]
    {
        Some(EmbeddedPayload {
            architecture: env!("TASCARREL_PAYLOAD_ARCHITECTURE"),
            sha256: env!("TASCARREL_PAYLOAD_SHA256"),
            size: COMPRESSED_PAYLOAD_SIZE as u64,
            compressed: &COMPRESSED_PAYLOAD,
        })
    }
    #[cfg(not(tascarrel_embedded_payload))]
    {
        None
    }
}

#[cfg(tascarrel_embedded_payload)]
const fn parse_usize(value: &str) -> usize {
    let bytes = value.as_bytes();
    let mut result = 0_usize;
    let mut index = 0;
    assert!(!bytes.is_empty(), "embedded payload size is empty");
    while index < bytes.len() {
        let byte = bytes[index];
        assert!(
            byte >= b'0' && byte <= b'9',
            "embedded payload size is not numeric"
        );
        result = match result.checked_mul(10) {
            Some(value) => value,
            None => panic!("embedded payload size overflows usize"),
        };
        result = match result.checked_add((byte - b'0') as usize) {
            Some(value) => value,
            None => panic!("embedded payload size overflows usize"),
        };
        index += 1;
    }
    result
}
