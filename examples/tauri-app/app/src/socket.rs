//! The websocket demo: `/ws` greets with where it runs, then echoes.

/// `/ws`: greets with the executable and the OS that serve it, then answers
/// each text with `echo: <text>`.
#[cfg(feature = "ssr")]
pub async fn echo(upgrade: axum::extract::ws::WebSocketUpgrade) -> axum::response::Response {
    use axum::extract::ws::Message;

    upgrade.on_upgrade(|mut socket| async move {
        let host = crate::served_by().unwrap_or_else(|error| error.to_string());
        if socket
            .send(Message::Text(format!("hello from {host}").into()))
            .await
            .is_err()
        {
            return;
        }
        while let Some(Ok(message)) = socket.recv().await {
            if let Message::Text(text) = message {
                let echo = format!("echo: {}", text.as_str());
                if socket.send(Message::Text(echo.into())).await.is_err() {
                    return;
                }
            }
        }
    })
}

/// Opens `/ws`, sends `text`, and returns the greeting and the echo.
#[cfg(feature = "hydrate")]
pub async fn talk(text: String) -> Result<String, String> {
    use futures::SinkExt;
    use gloo_net::websocket::Message;
    use gloo_net::websocket::futures::WebSocket;

    let mut socket = WebSocket::open("/ws").map_err(|error| error.to_string())?;
    let greeting = next_text(&mut socket).await?;
    socket
        .send(Message::Text(text))
        .await
        .map_err(|error| error.to_string())?;
    let echo = next_text(&mut socket).await?;
    let _ = socket.close(None, None);
    Ok(format!("{greeting}, {echo}"))
}

#[cfg(feature = "hydrate")]
async fn next_text(socket: &mut gloo_net::websocket::futures::WebSocket) -> Result<String, String> {
    use futures::StreamExt;
    use gloo_net::websocket::Message;

    match socket.next().await {
        Some(Ok(Message::Text(text))) => Ok(text),
        Some(Ok(Message::Bytes(_))) => Err("a binary message".to_owned()),
        Some(Err(error)) => Err(error.to_string()),
        None => Err("the websocket closed".to_owned()),
    }
}

/// Opens `/ws`, sends `text`, and returns the greeting and the echo.
#[cfg(not(feature = "hydrate"))]
pub async fn talk(_text: String) -> Result<String, String> {
    Err("websockets run in the browser".to_owned())
}
