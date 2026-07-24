use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

fn main() {
    println!("cargo:rustc-check-cfg=cfg(tascarrel_embedded_payload)");
    println!("cargo:rerun-if-env-changed=TASCARREL_EMBEDDED_PAYLOAD_DIR");
    println!("cargo:rerun-if-changed=embedded-payload.S");

    let Some(directory) = env::var_os("TASCARREL_EMBEDDED_PAYLOAD_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
    else {
        return;
    };
    let payload = directory.join("payload.tar.xz");
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
    let payload_object = compile_payload_object(&payload);
    println!(
        "cargo:rustc-link-arg-bin=tascarrel={}",
        payload_object.display()
    );
    println!("cargo:rustc-cfg=tascarrel_embedded_payload");
}

/// Wraps the portable payload archive in an object for Cargo's current target.
fn compile_payload_object(payload: &Path) -> PathBuf {
    let output_directory = PathBuf::from(
        env::var_os("OUT_DIR").expect("Cargo must provide an output directory to build scripts"),
    );
    let assembly_path = output_directory.join("embedded-payload.S");
    let payload_path = payload
        .to_str()
        .expect("embedded payload path must be valid UTF-8");
    let escaped_payload_path = payload_path.replace('\\', "\\\\").replace('"', "\\\"");
    let assembly =
        include_str!("embedded-payload.S").replace("@payloadPath@", &escaped_payload_path);
    fs::write(&assembly_path, assembly).unwrap_or_else(|error| {
        panic!(
            "write embedded payload assembly {}: {error}",
            assembly_path.display()
        )
    });

    let objects = cc::Build::new()
        .cargo_metadata(false)
        .file(&assembly_path)
        .compile_intermediates();
    assert_eq!(
        objects.len(),
        1,
        "embedded payload assembly must produce exactly one object"
    );
    objects
        .into_iter()
        .next()
        .expect("embedded payload object must exist")
}
