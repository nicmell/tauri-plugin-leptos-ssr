//! The facts `tauri.conf.json` and the cargo-leptos metadata repeat.

use std::path::Path;

#[test]
fn tauri_and_cargo_leptos_agree() {
    let workspace: toml::Table = std::fs::read_to_string("../Cargo.toml")
        .expect("read the workspace manifest")
        .parse()
        .expect("parse the workspace manifest");
    let leptos = &workspace["workspace"]["metadata"]["leptos"][0];
    let leptos = |key: &str| leptos[key].as_str().expect(key).to_owned();

    let tauri: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string("tauri.conf.json").expect("read tauri.conf.json"),
    )
    .expect("parse tauri.conf.json");
    let build = &tauri["build"];

    assert_eq!(build["devUrl"], format!("http://{}", leptos("site-addr")));
    assert_eq!(
        Path::new(build["frontendDist"].as_str().expect("frontendDist")),
        Path::new("..").join(leptos("site-root"))
    );
    assert_eq!(env!("LEPTOS_OUTPUT_NAME"), leptos("name"));
}
