//! A text-message websocket client: tokio-tungstenite on the desktop, the browser's
//! `WebSocket` on the web.

#[cfg(not(target_arch = "wasm32"))]
pub use native::Socket;
#[cfg(target_arch = "wasm32")]
pub use web::Socket;

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use anyhow::{Context, Result};
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

    pub struct Socket(WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>);

    impl Socket {
        pub async fn connect(url: &str) -> Result<Self> {
            let (ws, _) = tokio_tungstenite::connect_async(url).await.with_context(|| format!("websocket {url}"))?;
            Ok(Self(ws))
        }

        pub async fn send(&mut self, text: String) -> Result<()> {
            Ok(self.0.send(Message::Text(text.into())).await?)
        }

        /// The next text message; `None` once the server closes the connection.
        pub async fn next(&mut self) -> Option<Result<String>> {
            while let Some(msg) = self.0.next().await {
                match msg {
                    Ok(Message::Text(t)) => return Some(Ok(t.as_str().to_owned())),
                    Ok(Message::Close(_)) => return None,
                    Ok(_) => {}
                    Err(e) => return Some(Err(e.into())),
                }
            }
            None
        }
    }
}

#[cfg(target_arch = "wasm32")]
mod web {
    use anyhow::{Result, anyhow, bail};
    use tokio::sync::mpsc;
    use wasm_bindgen::JsCast;
    use wasm_bindgen::prelude::*;
    use web_sys::{CloseEvent, MessageEvent, WebSocket};

    enum Event {
        Open,
        Text(String),
        Error,
        Closed,
    }

    /// Browser websocket; its callbacks feed a channel. Closed when dropped.
    pub struct Socket {
        ws: WebSocket,
        rx: mpsc::UnboundedReceiver<Event>,
        _on_open: Closure<dyn FnMut()>,
        _on_message: Closure<dyn FnMut(MessageEvent)>,
        _on_error: Closure<dyn FnMut()>,
        _on_close: Closure<dyn FnMut(CloseEvent)>,
    }

    impl Socket {
        pub async fn connect(url: &str) -> Result<Self> {
            let ws = WebSocket::new(url).map_err(|e| anyhow!("websocket {url}: {e:?}"))?;
            let (tx, rx) = mpsc::unbounded_channel();
            let t = tx.clone();
            let on_open = Closure::<dyn FnMut()>::new(move || {
                let _ = t.send(Event::Open);
            });
            let t = tx.clone();
            let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
                if let Some(text) = e.data().as_string() {
                    let _ = t.send(Event::Text(text));
                }
            });
            let t = tx.clone();
            let on_error = Closure::<dyn FnMut()>::new(move || {
                let _ = t.send(Event::Error);
            });
            let on_close = Closure::<dyn FnMut(CloseEvent)>::new(move |_| {
                let _ = tx.send(Event::Closed);
            });
            ws.set_onopen(Some(on_open.as_ref().unchecked_ref()));
            ws.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
            ws.set_onerror(Some(on_error.as_ref().unchecked_ref()));
            ws.set_onclose(Some(on_close.as_ref().unchecked_ref()));
            let mut socket =
                Self { ws, rx, _on_open: on_open, _on_message: on_message, _on_error: on_error, _on_close: on_close };
            match socket.rx.recv().await {
                Some(Event::Open) => Ok(socket),
                _ => bail!("websocket {url}: connection failed"),
            }
        }

        pub async fn send(&mut self, text: String) -> Result<()> {
            self.ws.send_with_str(&text).map_err(|e| anyhow!("websocket send: {e:?}"))
        }

        /// The next text message; `None` once the server closes the connection.
        pub async fn next(&mut self) -> Option<Result<String>> {
            match self.rx.recv().await? {
                Event::Text(t) => Some(Ok(t)),
                Event::Error => Some(Err(anyhow!("websocket error"))),
                Event::Open | Event::Closed => None,
            }
        }
    }

    impl Drop for Socket {
        fn drop(&mut self) {
            // Detach the callbacks before their closures are freed.
            self.ws.set_onopen(None);
            self.ws.set_onmessage(None);
            self.ws.set_onerror(None);
            self.ws.set_onclose(None);
            let _ = self.ws.close();
        }
    }
}
