fn main() {
    println!("cargo:rerun-if-changed=assets/icon/olympus.ico");
    embed_resource::compile("assets/icon/olympus.rc", embed_resource::NONE).manifest_optional().expect("embedding the icon");
}
