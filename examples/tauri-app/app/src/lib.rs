use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_meta::{Title, provide_meta_context};
use leptos_router::components::{Route, Router, Routes};
use leptos_router::path;

#[cfg(feature = "ssr")]
pub use server::router;

mod tauri_ipc;

#[component]
pub fn App() -> impl IntoView {
    provide_meta_context();
    view! {
        <Title text="Tauri + Leptos SSR" />
        <Router>
            <main>
                <Routes fallback=|| "Page not found.">
                    <Route path=path!("") view=HomePage />
                </Routes>
            </main>
        </Router>
    }
}

/// The executable that ran the server function, and its OS: the
/// `cargo leptos watch` server in dev, the app itself in release builds.
#[server]
pub async fn whoami() -> Result<String, ServerFnError> {
    let exe = std::env::current_exe().map_err(|error| ServerFnError::new(error.to_string()))?;
    let name = exe
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(format!("{name} ({})", std::env::consts::OS))
}

#[component]
fn HomePage() -> impl IntoView {
    let count = RwSignal::new(0);
    let name = RwSignal::new(String::from("Leptos"));
    let server_reply = RwSignal::new(String::new());
    let command_reply = RwSignal::new(String::new());
    let self_check = RwSignal::new(String::from("Not hydrated yet."));

    // Effects run in the browser only: once, right after hydration.
    Effect::new(move || {
        spawn_local(async move {
            let server = whoami()
                .await
                .unwrap_or_else(|error| format!("error: {error}"));
            let command = tauri_ipc::greet("self-check")
                .await
                .unwrap_or_else(|error| format!("error: {error}"));
            self_check.set(format!(
                "Hydrated. Server function: {server}. Tauri command: {command}"
            ));
        });
    });

    let ask_server = move |_| {
        spawn_local(async move {
            let reply = whoami()
                .await
                .unwrap_or_else(|error| format!("error: {error}"));
            server_reply.set(format!("The server function ran in {reply}."));
        });
    };
    let greet = move |_| {
        spawn_local(async move {
            let reply = tauri_ipc::greet(&name.get_untracked())
                .await
                .unwrap_or_else(|error| format!("error: {error}"));
            command_reply.set(reply);
        });
    };

    view! {
        <h1>"Tauri + Leptos SSR"</h1>
        <div class="logos">
            <img src="/tauri.svg" class="logo" alt="Tauri logo" />
            <img src="/leptos.svg" class="logo" alt="Leptos logo" />
        </div>
        <p id="self-check">{move || self_check.get()}</p>

        <section>
            <button on:click=move |_| *count.write() += 1>"Clicked " {count} " times"</button>
        </section>
        <section>
            <button on:click=ask_server>"Call the server function"</button>
            <p>{move || server_reply.get()}</p>
        </section>
        <section>
            <input bind:value=name />
            <button on:click=greet>"Call the Tauri command"</button>
            <p>{move || command_reply.get()}</p>
        </section>
    }
}

#[cfg(feature = "ssr")]
mod server {
    use axum::Router;
    use leptos::prelude::*;
    use leptos_axum::{LeptosRoutes, generate_route_list};
    use leptos_meta::MetaTags;

    use crate::App;

    /// The SSR document around [`App`].
    fn shell(options: LeptosOptions) -> impl IntoView {
        let stylesheet = format!("/pkg/{}.css", options.output_name);
        view! {
            <!DOCTYPE html>
            <html lang="en">
                <head>
                    <meta charset="utf-8" />
                    <meta name="viewport" content="width=device-width, initial-scale=1" />
                    <link rel="icon" type="image/svg+xml" href="/tauri.svg" />
                    <link rel="stylesheet" href=stylesheet />
                    <AutoReload options=options.clone() />
                    <HydrationScripts options />
                    <MetaTags />
                </head>
                <body>
                    <App />
                </body>
            </html>
        }
    }

    /// The app's pages and server functions, for the standalone server and
    /// for the Tauri plugin.
    pub fn router(options: LeptosOptions) -> Router {
        let routes = generate_route_list(App);
        Router::new()
            .leptos_routes(&options, routes, {
                let options = options.clone();
                move || shell(options.clone())
            })
            .with_state(options)
    }
}
