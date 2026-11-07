fn main() {
    let _ = embed_resource::compile("setup.rc", embed_resource::NONE);
    println!("cargo:rerun-if-changed=setup.rc");
    println!("cargo:rerun-if-changed=setup.manifest");
}
