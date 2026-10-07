use serde::{Serialize, ser::Serializer};

pub type Result<T> = std::result::Result<T, Error>;

/// The error that the app's router closure returns.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("the app's router failed: {0}")]
    Router(BoxError),
    #[error("`build.devUrl` is not set: point it at the `cargo leptos watch` server")]
    DevUrlUnset,
    #[error("`build.devUrl` must be an http:// URL, got `{0}`")]
    DevUrlNotHttp(String),
    #[error(
        "LEPTOS_OUTPUT_NAME was not set when the app was compiled: set it under `[env]` \
         in `.cargo/config.toml` to the cargo-leptos project `name`"
    )]
    OutputNameUnset,
    #[error(
        "`build.frontendDist` has no `{0}`: point it at the cargo-leptos `site-root` and build \
         the site with `cargo leptos build --release` in `build.beforeBuildCommand`"
    )]
    MissingAsset(String),
    #[error("the plugin only serves its own origin, got `{0}`")]
    ForeignUrl(String),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("no open response body {0}")]
    UnknownStream(u64),
    #[error("the response body was cancelled")]
    StreamCancelled,
    #[error("the page that asked for the response is gone")]
    StreamClosed,
    #[error("the response body failed: {0}")]
    Stream(String),
    #[error("no open websocket {0}")]
    UnknownSocket(u64),
    #[error("the page that opened the websocket is gone")]
    SocketClosed,
    #[error("the websocket failed: {0}")]
    Socket(String),
    #[error(transparent)]
    Url(#[from] url::ParseError),
}

impl Serialize for Error {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.to_string().as_ref())
    }
}
