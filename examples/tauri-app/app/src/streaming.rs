//! The streaming demo: a server function that streams text, and server-sent
//! events from `/events`.

use futures::StreamExt;
use leptos::prelude::*;
use leptos::server_fn::codec::{StreamingText, TextStream};

/// Counts down to liftoff, one chunk every 500 ms.
#[server(output = StreamingText)]
pub async fn countdown() -> Result<TextStream, ServerFnError> {
    use std::time::Duration;

    let steps = futures::stream::iter(["3", "2", "1", "liftoff"]).then(|step| async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        Ok::<_, ServerFnError>(format!("{step} "))
    });
    Ok(TextStream::new(steps))
}

/// Appends each chunk of [`countdown`] to `text` as it arrives.
pub async fn run_countdown(text: RwSignal<String>) {
    let stream = match countdown().await {
        Ok(stream) => stream,
        Err(error) => return text.set(format!("error: {error}")),
    };
    let mut chunks = Box::pin(stream.into_inner());
    while let Some(chunk) = chunks.next().await {
        match chunk {
            Ok(chunk) => text.update(|text| text.push_str(&chunk)),
            Err(error) => return text.set(format!("error: {error}")),
        }
    }
}

/// Shows the latest `/events` message in `latest` while the page lives.
#[cfg(feature = "hydrate")]
pub async fn follow_events(latest: RwSignal<String>) {
    use gloo_net::eventsource::futures::EventSource;

    let mut source = match EventSource::new("/events") {
        Ok(source) => source,
        Err(error) => return latest.set(format!("error: {error:?}")),
    };
    let mut messages = match source.subscribe("message") {
        Ok(messages) => messages,
        Err(error) => return latest.set(format!("error: {error:?}")),
    };
    while let Some(message) = messages.next().await {
        match message {
            Ok((_, event)) => latest.set(event.data().as_string().unwrap_or_default()),
            Err(error) => latest.set(format!("error: {error}")),
        }
    }
    // Dropping the source closes it, so it lives until the stream ends.
    drop(source);
}

/// Shows the latest `/events` message in `latest` while the page lives.
#[cfg(not(feature = "hydrate"))]
pub async fn follow_events(_latest: RwSignal<String>) {}

/// `/events`: a tick every second.
#[cfg(feature = "ssr")]
pub async fn events() -> axum::response::sse::Sse<
    impl futures::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
> {
    use std::time::Duration;

    use axum::response::sse::{Event, KeepAlive, Sse};

    let ticks = futures::stream::unfold(1u64, |tick| async move {
        if tick > 1 {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        Some((Ok(Event::default().data(format!("tick {tick}"))), tick + 1))
    });
    Sse::new(ticks).keep_alive(KeepAlive::default())
}
