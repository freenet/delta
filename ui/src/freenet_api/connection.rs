use dioxus::prelude::*;

#[cfg(target_arch = "wasm32")]
use freenet_stdlib::client_api::WebApi;

/// Global WebApi connection (only meaningful on WASM).
#[cfg(target_arch = "wasm32")]
pub static WEB_API: GlobalSignal<Option<WebApi>> = GlobalSignal::new(|| None);

/// Connection status.
pub static CONNECTION_STATUS: GlobalSignal<ConnectionStatus> =
    GlobalSignal::new(|| ConnectionStatus::Disconnected);

#[derive(Clone, Debug, PartialEq)]
pub enum ConnectionStatus {
    Disconnected,
    Connecting,
    Connected,
    Error(String),
}

/// Connect to the Freenet node's WebSocket API.
pub fn connect_to_freenet() {
    #[cfg(target_arch = "wasm32")]
    {
        *CONNECTION_STATUS.write() = ConnectionStatus::Connecting;

        let ws_url = get_websocket_url();
        web_sys::console::log_1(&format!("Delta: connecting to {ws_url}").into());

        // Check if we're likely on a Freenet gateway (URL contains /v1/contract/)
        // On a dev server, WebSocket will fail and crash the WASM runtime
        let is_gateway = web_sys::window()
            .and_then(|w| w.location().pathname().ok())
            .map(|p| p.contains("/v1/contract/"))
            .unwrap_or(false);

        if !is_gateway {
            web_sys::console::log_1(&"Delta: not on gateway, skipping WebSocket connection".into());
            *CONNECTION_STATUS.write() = ConnectionStatus::Disconnected;
            return;
        }

        let websocket = match web_sys::WebSocket::new(&ws_url) {
            Ok(ws) => ws,
            Err(e) => {
                let msg = format!("WebSocket creation failed: {e:?}");
                web_sys::console::error_1(&msg.clone().into());
                *CONNECTION_STATUS.write() = ConnectionStatus::Error(msg);
                return;
            }
        };

        let web_api = WebApi::start(
            websocket,
            move |result| match result {
                Ok(response) => super::operations::handle_response(response),
                Err(e) => {
                    let msg = format!("Delta: API error: {e:?}");
                    let truncated = if msg.len() > 200 {
                        format!("{}...", &msg[..200])
                    } else {
                        msg
                    };
                    web_sys::console::error_1(&truncated.into());
                }
            },
            move |error| {
                let msg = error.to_string();
                let truncated = if msg.len() > 200 {
                    format!("Delta: connection error: {}...", &msg[..200])
                } else {
                    format!("Delta: connection error: {msg}")
                };
                web_sys::console::error_1(&truncated.into());
                *CONNECTION_STATUS.write() =
                    ConnectionStatus::Error("Connection failed".to_string());
            },
            move || {
                web_sys::console::log_1(&"Delta: connected to Freenet".into());
                *CONNECTION_STATUS.write() = ConnectionStatus::Connected;
                // Now that we're connected, register the delegate
                super::delegate::register_delegate();
                // Replay any hash navigation that arrived before connection
                crate::components::replay_pending_hash();
            },
        );

        *WEB_API.write() = Some(web_api);
    }
}

#[cfg(target_arch = "wasm32")]
fn get_websocket_url() -> String {
    let window = web_sys::window().expect("no window");
    let location = window.location();

    let protocol = location.protocol().unwrap_or_else(|_| "http:".into());
    let host = location.host().unwrap_or_else(|_| "127.0.0.1:7509".into());

    let ws_protocol = if protocol == "https:" { "wss:" } else { "ws:" };

    let search = location.search().unwrap_or_default();
    let auth_param = search
        .split('&')
        .find(|p| p.contains("authToken="))
        .map(|p| format!("&{}", p.trim_start_matches('?').trim_start_matches('&')))
        .unwrap_or_default();

    format!("{ws_protocol}//{host}/v1/contract/command?encodingProtocol=native{auth_param}")
}
