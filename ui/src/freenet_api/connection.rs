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

/// Whether a reconnect attempt is already scheduled. Prevents the
/// error callback from queueing multiple parallel reconnects when
/// it fires more than once during a teardown.
#[cfg(target_arch = "wasm32")]
static RECONNECT_PENDING: GlobalSignal<bool> = GlobalSignal::new(|| false);

/// Schedule a `connect_to_freenet()` retry after a 2 s delay. Called
/// from the error callback when the WebSocket closes — without this,
/// the iframe stays in `Error` state until the user refreshes,
/// because nothing in the codebase reopens a closed WebSocket.
#[cfg(target_arch = "wasm32")]
fn schedule_reconnect() {
    if *RECONNECT_PENDING.read() {
        return;
    }
    *RECONNECT_PENDING.write() = true;

    use wasm_bindgen::prelude::*;
    let cb = Closure::<dyn Fn()>::new(|| {
        *RECONNECT_PENDING.write() = false;
        // Skip if the user has already manually reconnected (e.g.
        // by triggering a hash navigation that fires
        // connect_to_freenet) — `Connecting` and `Connected` both
        // mean we don't want to clobber a fresh socket.
        if matches!(
            &*CONNECTION_STATUS.read(),
            ConnectionStatus::Connected | ConnectionStatus::Connecting
        ) {
            return;
        }
        web_sys::console::log_1(&"Delta: attempting WebSocket reconnect".into());
        connect_to_freenet();
    });
    if let Some(window) = web_sys::window() {
        let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(
            cb.as_ref().unchecked_ref(),
            2_000,
        );
    }
    cb.forget();
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
                    let truncated = truncate_for_log(&msg, 200);
                    web_sys::console::error_1(&truncated.as_ref().into());
                    // If a GET timed out, try restoring from delegate backup
                    if msg.contains("GET operation timed out") {
                        super::operations::handle_get_timeout(&msg);
                    }
                }
            },
            move |error| {
                let msg = error.to_string();
                let truncated = format!("Delta: connection error: {}", truncate_for_log(&msg, 200));
                web_sys::console::error_1(&truncated.into());
                *CONNECTION_STATUS.write() =
                    ConnectionStatus::Error("Connection failed".to_string());
                // Schedule a reconnect attempt. Without this, the
                // WebSocket stays dead until the user refreshes the
                // iframe — which is what surfaced as Ivvor's "red
                // Error indicator" 2026-05-03 12:20 ("WebSocket
                // connection closed" + "{error:'connection closed'}"
                // in the console). Backed off to 2 s so a flapping
                // node doesn't trigger a tight retry loop.
                schedule_reconnect();
            },
            move || {
                web_sys::console::log_1(&"Delta: connected to Freenet".into());
                *CONNECTION_STATUS.write() = ConnectionStatus::Connected;
                // Register delegate — hash replay happens after known sites load
                super::delegate::register_delegate();
            },
        );

        *WEB_API.write() = Some(web_api);
    }
}

/// Truncate `msg` to at most `max_bytes` UTF-8 bytes, appending `…`
/// if any bytes were dropped. Always cuts on a char boundary so a
/// long error message containing non-ASCII bytes (which can sit
/// across the cut point) does not panic with `&str[..n]` slicing.
pub(crate) fn truncate_for_log(msg: &str, max_bytes: usize) -> std::borrow::Cow<'_, str> {
    if msg.len() <= max_bytes {
        return std::borrow::Cow::Borrowed(msg);
    }
    let mut cut = max_bytes;
    while cut > 0 && !msg.is_char_boundary(cut) {
        cut -= 1;
    }
    std::borrow::Cow::Owned(format!("{}…", &msg[..cut]))
}

#[cfg(test)]
mod tests {
    use super::truncate_for_log;

    #[test]
    fn short_message_passes_through() {
        assert_eq!(truncate_for_log("hello", 200).as_ref(), "hello");
    }

    #[test]
    fn long_ascii_message_truncates_with_ellipsis() {
        let s = "x".repeat(250);
        let out = truncate_for_log(&s, 200);
        assert!(out.ends_with('…'));
        // 200 'x' bytes plus the 3-byte ellipsis.
        assert_eq!(out.as_ref().len(), 200 + '…'.len_utf8());
    }

    #[test]
    fn non_ascii_at_boundary_does_not_panic() {
        // "·" is 2 bytes; place it so naive `&s[..3]` would land
        // mid-char.
        let s = format!("ab·{}", "x".repeat(300));
        let out = truncate_for_log(&s, 3);
        // We cut at 2 (start of '·' fails boundary at 3, so we walk
        // back to 2). Output is "ab" + ellipsis.
        assert_eq!(out.as_ref(), "ab…");
    }

    #[test]
    fn equal_length_returns_borrowed() {
        let s = "exactly twenty char!";
        assert_eq!(s.len(), 20);
        let out = truncate_for_log(s, 20);
        assert!(matches!(out, std::borrow::Cow::Borrowed(_)));
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
