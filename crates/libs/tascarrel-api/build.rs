fn main() {
    sidex_build_rs::configure()
        .with_bundle(".")
        .generate()
        .expect("generate Tascarrel API types from Sidex");
}
