use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    println!("cargo:rustc-check-cfg=cfg(tascarrel_embedded_payload)");
    println!("cargo:rerun-if-env-changed=TASCARREL_EMBEDDED_PAYLOAD_DIR");

    let Some(directory) = env::var_os("TASCARREL_EMBEDDED_PAYLOAD_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
    else {
        return;
    };
    let payload = directory.join("payload.o");
    let fields = [
        ("TASCARREL_PAYLOAD_ARCHITECTURE", "architecture"),
        ("TASCARREL_PAYLOAD_SHA256", "payload.sha256"),
        ("TASCARREL_PAYLOAD_SIZE", "payload.size"),
    ];

    assert!(
        payload.is_file(),
        "embedded payload is missing: {}",
        payload.display()
    );
    println!("cargo:rerun-if-changed={}", payload.display());
    println!("cargo:rustc-link-arg-bin=tascarrel={}", payload.display());
    for (environment, file) in fields {
        let path = directory.join(file);
        let value = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        let value = value.trim();
        assert!(
            !value.is_empty(),
            "embedded payload metadata is empty: {}",
            path.display()
        );
        println!("cargo:rerun-if-changed={}", path.display());
        println!("cargo:rustc-env={environment}={value}");
    }
    println!("cargo:rustc-cfg=tascarrel_embedded_payload");
}
