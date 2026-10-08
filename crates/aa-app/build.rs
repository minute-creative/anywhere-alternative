//! Gives anywhere.exe its icon on Windows (compiled into the program).
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let _ = embed_resource::compile("app.rc", embed_resource::NONE).manifest_optional();
    }
}
