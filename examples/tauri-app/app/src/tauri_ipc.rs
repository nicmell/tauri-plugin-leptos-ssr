//! Calls into the Tauri app through `window.__TAURI__` (`app.withGlobalTauri`).

#[cfg(feature = "hydrate")]
mod bindings {
    use wasm_bindgen::prelude::*;

    #[wasm_bindgen]
    extern "C" {
        #[wasm_bindgen(js_namespace = ["__TAURI__", "core"], catch)]
        pub async fn invoke(cmd: &str, args: JsValue) -> Result<JsValue, JsValue>;
    }
}

/// The `greet` command of the Tauri app.
#[cfg(feature = "hydrate")]
pub async fn greet(name: &str) -> Result<String, String> {
    // Without the global, the binding throws past `catch` and the caller's
    // task never finishes.
    if !js_sys::Reflect::has(&js_sys::global(), &"__TAURI__".into()).unwrap_or(false) {
        return Err("Tauri commands run in the Tauri app only.".to_owned());
    }
    let args = js_sys::Object::new();
    js_sys::Reflect::set(&args, &"name".into(), &name.into())
        .map_err(|error| format!("{error:?}"))?;
    let reply = bindings::invoke("greet", args.into())
        .await
        .map_err(|error| error.as_string().unwrap_or_else(|| format!("{error:?}")))?;
    reply
        .as_string()
        .ok_or_else(|| "`greet` did not return a string".to_owned())
}

/// The `greet` command of the Tauri app; unreachable outside the browser.
#[cfg(not(feature = "hydrate"))]
pub async fn greet(_name: &str) -> Result<String, String> {
    Err("Tauri commands are called from the browser".to_owned())
}
