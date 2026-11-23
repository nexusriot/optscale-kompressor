//! The websocket relay, over real sockets on both sides.
//!
//! The other suites drive the router in process with `oneshot`, which cannot
//! reach this code at all: an upgrade is completed by hyper *after* the
//! response is returned, so a test that never binds a socket sees the 101 and
//! none of the frames that are the actual behaviour. So this one binds two —
//! an echo server standing in for the provider, and the proxy in front of it.
//!
//! What is asserted is what a client would observe: frames survive the round
//! trip intact in both directions, a close is carried rather than swallowed,
//! and an upstream that cannot be reached ends the session instead of hanging.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

use kompressor_core::AppCore;
use kompressor_core::config::{Config, Upstream};
use kompressor_proxy::ProxyState;
use kompressor_store::memory::MemoryRefStore;

/// An echo websocket server. Returns the address it bound.
///
/// Echoes text and binary, and answers a close with a close, which is what
/// lets the shutdown path be asserted rather than inferred from a timeout.
async fn echo_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
                    return;
                };
                while let Some(Ok(message)) = socket.next().await {
                    match message {
                        Message::Text(_) | Message::Binary(_) => {
                            if socket.send(message).await.is_err() {
                                break;
                            }
                        }
                        Message::Close(_) => {
                            let _ = socket.send(Message::Close(None)).await;
                            break;
                        }
                        _ => {}
                    }
                }
            });
        }
    });
    addr
}

/// A proxy in front of `upstream`, on a real socket. Returns its address.
/// Like [`proxy_in_front_of`], but keeps the recorded events so a test can
/// assert what the relay filed.
async fn proxy_recording_in_front_of(
    upstream: &str,
) -> (String, kompressor_stats::writer::CapturedEvents) {
    let mut config = Config::default();
    config.stats.enabled = false;
    config.proxy.websocket_relay = true;
    config.proxy.upstreams = vec![Upstream {
        name: "provider".to_string(),
        path_prefix: String::new(),
        base_url: format!("http://{upstream}"),
        api_family: "openai".to_string(),
        forward_auth: true,
        ..Default::default()
    }];

    let (sink, events) = kompressor_stats::writer::capturing_sink(256);
    let core = Arc::new(AppCore::new(config, Arc::new(MemoryRefStore::new()), sink));
    let router = ProxyState::new(core, reqwest::Client::new()).into_router();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (addr, events)
}

async fn proxy_with(upstream: &str, tweak: impl FnOnce(&mut Config)) -> String {
    let mut config = Config::default();
    config.stats.enabled = false;
    config.proxy.websocket_relay = true;
    config.proxy.upstreams = vec![Upstream {
        name: "provider".to_string(),
        path_prefix: String::new(),
        base_url: format!("http://{upstream}"),
        api_family: "openai".to_string(),
        forward_auth: true,
        ..Default::default()
    }];
    tweak(&mut config);

    let (sink, _events) = kompressor_stats::writer::capturing_sink(256);
    let core = Arc::new(AppCore::new(config, Arc::new(MemoryRefStore::new()), sink));
    let router = ProxyState::new(core, reqwest::Client::new()).into_router();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    addr
}

/// `proxy_with`, keeping the stats sink's receiving end.
///
/// `capturing_sink` is enabled whatever `config.stats.enabled` says — the sink
/// is what `is_enabled` asks — so a test can read back exactly the events the
/// relay filed, without a database.
async fn proxy_recording(
    upstream: &str,
    tweak: impl FnOnce(&mut Config),
) -> (String, kompressor_stats::writer::CapturedEvents) {
    let mut config = Config::default();
    config.proxy.websocket_relay = true;
    config.proxy.upstreams = vec![Upstream {
        name: "provider".to_string(),
        path_prefix: String::new(),
        base_url: format!("http://{upstream}"),
        api_family: "openai".to_string(),
        forward_auth: true,
        ..Default::default()
    }];
    tweak(&mut config);

    let (sink, events) = kompressor_stats::writer::capturing_sink(256);
    let core = Arc::new(AppCore::new(config, Arc::new(MemoryRefStore::new()), sink));
    let router = ProxyState::new(core, reqwest::Client::new()).into_router();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (addr, events)
}

async fn proxy_in_front_of(upstream: &str) -> String {
    let mut config = Config::default();
    config.stats.enabled = false;
    // Off by default, because relaying means not compressing. These tests are
    // about whether the relay is correct when asked for, not whether it should
    // be on — `the_relay_is_off_unless_asked_for` covers the default.
    config.proxy.websocket_relay = true;
    config.proxy.upstreams = vec![Upstream {
        name: "provider".to_string(),
        path_prefix: String::new(),
        base_url: format!("http://{upstream}"),
        api_family: "openai".to_string(),
        forward_auth: true,
        ..Default::default()
    }];

    let (sink, _events) = kompressor_stats::writer::capturing_sink(256);
    let core = Arc::new(AppCore::new(config, Arc::new(MemoryRefStore::new()), sink));
    let router = ProxyState::new(core, reqwest::Client::new()).into_router();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    addr
}

async fn connect(
    addr: &str,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/v1/responses"))
        .await
        .expect("the proxy should accept a websocket upgrade");
    socket
}

#[tokio::test]
async fn an_upgrade_is_relayed_rather_than_answered_with_405() {
    let upstream = echo_server().await;
    let proxy = proxy_in_front_of(&upstream).await;

    // The whole point: before the relay existed this handshake fell through to
    // the forward path as a plain GET and the upstream answered 405.
    let mut socket = connect(&proxy).await;

    socket
        .send(Message::Text("{\"type\":\"response.create\"}".into()))
        .await
        .unwrap();
    let echoed = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("the relay should answer well inside the timeout")
        .expect("the stream should still be open")
        .unwrap();

    assert_eq!(
        echoed,
        Message::Text("{\"type\":\"response.create\"}".into()),
        "a text frame must reach the upstream and come back byte for byte"
    );
}

#[tokio::test]
async fn binary_frames_survive_the_round_trip_unchanged() {
    let upstream = echo_server().await;
    let proxy = proxy_in_front_of(&upstream).await;
    let mut socket = connect(&proxy).await;

    // Deliberately not valid UTF-8: the relay must not be quietly treating
    // frames as text, because a protobuf or compressed payload would be
    // corrupted by a round trip through `String`.
    let payload: Vec<u8> = vec![0x00, 0xff, 0xfe, 0x42, 0x00, 0x80];
    socket
        .send(Message::Binary(payload.clone().into()))
        .await
        .unwrap();

    let echoed = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("the relay should answer well inside the timeout")
        .expect("the stream should still be open")
        .unwrap();

    assert_eq!(echoed, Message::Binary(payload.into()));
}

#[tokio::test]
async fn many_frames_keep_their_order() {
    let upstream = echo_server().await;
    let proxy = proxy_in_front_of(&upstream).await;
    let mut socket = connect(&proxy).await;

    // A response turn is a stream of events, so a relay that reordered them
    // would corrupt the turn while every single-frame test still passed.
    for i in 0..25 {
        socket
            .send(Message::Text(format!("event {i}").into()))
            .await
            .unwrap();
    }
    for i in 0..25 {
        let echoed = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .expect("no stall mid-stream")
            .expect("the stream should still be open")
            .unwrap();
        assert_eq!(echoed, Message::Text(format!("event {i}").into()));
    }
}

#[tokio::test]
async fn a_close_from_the_client_reaches_the_upstream_and_comes_back() {
    let upstream = echo_server().await;
    let proxy = proxy_in_front_of(&upstream).await;
    let mut socket = connect(&proxy).await;

    socket.send(Message::Close(None)).await.unwrap();

    // The echo server answers a close with a close. Receiving it proves the
    // frame was carried rather than swallowed by the relay, and that the
    // session ends rather than hanging until a timeout.
    let ended = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(message) = socket.next().await {
            match message {
                Ok(Message::Close(_)) | Err(_) => return true,
                Ok(_) => continue,
            }
        }
        true
    })
    .await;

    assert!(ended.is_ok(), "the relay must close rather than hang");
}

#[tokio::test]
async fn the_relay_ships_on_and_can_still_be_switched_off() {
    // Both halves of the default, because each has its own way of going wrong.
    //
    // The default moved to *on* once the relay learned to compress: it is no
    // longer the choice between compressing and not, only between compressing
    // a socket whose tokens the provider never reports and breaking that
    // socket so the client retries over a transport that does report them.
    // Pinned here rather than left to `Config::default` because flipping it
    // back would not look like a bug — upgrades would start failing, codex
    // would fall back, and the only visible symptom would be the eight-second
    // handshake nobody times.
    //
    // The off path is still real behaviour and still tested: it is the escape
    // hatch for an operator who wants provider usage on this traffic, and it
    // has to keep falling through rather than, say, refusing the request.
    assert!(
        Config::default().proxy.websocket_relay,
        "the relay ships on; off means a websocket client is only compressed \
         if it chooses to retry over HTTPS, which is not ours to rely on"
    );

    let upstream = echo_server().await;
    let addr = proxy_with(&upstream, |config| config.proxy.websocket_relay = false).await;

    // With the relay off the upgrade falls through to the ordinary forward
    // path, exactly as it did before this module existed, so the handshake
    // does not complete.
    let connected = tokio_tungstenite::connect_async(format!("ws://{addr}/v1/responses")).await;
    assert!(
        connected.is_err(),
        "with the relay off an upgrade must not be completed; it has to fall \
         through so the client drops back to a transport we can compress"
    );
}

#[tokio::test]
async fn an_upstream_that_cannot_be_reached_ends_the_session() {
    // Nothing is listening here. The handshake with the client has already been
    // answered by the time the upstream dial fails, so there is no status code
    // left to send — the session has to end, and the failure belongs in the log.
    // Hanging instead would leave the client waiting on a socket nobody serves.
    let proxy = proxy_in_front_of("127.0.0.1:1").await;

    let Ok((mut socket, _)) =
        tokio_tungstenite::connect_async(format!("ws://{proxy}/v1/responses")).await
    else {
        // Refusing the handshake outright is also a correct answer here.
        return;
    };

    let ended = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(message) = socket.next().await {
            if message.is_err() {
                return;
            }
        }
    })
    .await;

    assert!(
        ended.is_ok(),
        "a dead upstream must end the session rather than hang the client"
    );
}

#[tokio::test]
async fn a_response_create_frame_is_compressed_before_it_goes_upstream() {
    // The shape measured on a live codex sweep: one whole `response.create`
    // per frame, with the mass in `custom_tool_call_output` — 85.7% of request
    // bytes in that capture. The echo server sends back exactly what it
    // received, so what arrives here *is* what the upstream was given.
    let upstream = echo_server().await;
    let proxy = proxy_in_front_of(&upstream).await;
    let mut socket = connect(&proxy).await;

    let bulky = (0..400)
        .map(|i| format!("2026-09-26T10:00:{:02}Z DEBUG [req-{i:05}] resolved route /api/v2/items/{} in {}ms", i % 60, i % 7, i % 40))
        .collect::<Vec<_>>()
        .join("\n");
    let frame = serde_json::json!({
        "type": "response.create",
        "model": "gpt-5-codex",
        "input": [
            {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "summarise the log"}]},
            {"type": "custom_tool_call_output", "call_id": "call_a1", "output": bulky},
        ],
        "stream": true,
    });
    let sent = serde_json::to_string(&frame).unwrap();

    socket
        .send(Message::Text(sent.clone().into()))
        .await
        .unwrap();
    let echoed = tokio::time::timeout(Duration::from_secs(10), socket.next())
        .await
        .expect("the relay should answer well inside the timeout")
        .expect("the stream should still be open")
        .unwrap();

    let Message::Text(received) = echoed else {
        panic!("expected a text frame back");
    };

    assert!(
        received.len() < sent.len(),
        "the upstream should have received a smaller frame: sent {} bytes, got {}",
        sent.len(),
        received.len()
    );

    // Shrinking is not enough on its own: the frame has to still be the frame.
    // A relay that mangled the envelope would pass a size assertion and break
    // every session.
    let parsed: serde_json::Value = serde_json::from_str(&received).unwrap();
    assert_eq!(parsed["type"], "response.create");
    assert_eq!(parsed["model"], "gpt-5-codex");
    assert_eq!(parsed["stream"], true);
    assert_eq!(parsed["input"][1]["call_id"], "call_a1");
    assert_eq!(parsed["input"][1]["type"], "custom_tool_call_output");
    // The user turn is a protected role and must arrive untouched.
    assert_eq!(
        parsed["input"][0]["content"][0]["text"],
        "summarise the log"
    );
}

#[tokio::test]
async fn a_frame_that_is_not_a_request_is_relayed_untouched() {
    // Everything else on this socket — cancellations, whatever a future client
    // adds — is not a shape the walker knows, and guessing at one would corrupt
    // a session. Declining means forwarding exactly what arrived.
    let upstream = echo_server().await;
    let proxy = proxy_in_front_of(&upstream).await;
    let mut socket = connect(&proxy).await;

    let sent = r#"{"type":"response.cancel","response_id":"resp_123"}"#;
    socket.send(Message::Text(sent.into())).await.unwrap();
    let echoed = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("no stall")
        .expect("still open")
        .unwrap();

    assert_eq!(
        echoed,
        Message::Text(sent.into()),
        "an unrecognised frame must reach the upstream byte for byte"
    );
}

/// A relayed session has to leave a `proxy_requests` row behind.
///
/// Without one it is invisible to every per-request report — `kompressor stats
/// summary`, the HTML report and the Harbor bench all read that table — so a
/// relayed session contributed compression events and nothing else: no
/// duration, no tenant, and nowhere for a relay failure to land except a log
/// line nobody is tailing.
#[tokio::test]
async fn a_relayed_session_is_recorded_as_a_request() {
    let upstream = echo_server().await;
    let (proxy, events) = proxy_recording_in_front_of(&upstream).await;
    let mut socket = connect(&proxy).await;
    socket.send(Message::Text("hello".into())).await.unwrap();
    socket.send(Message::Close(None)).await.unwrap();
    // Drain until the far end hangs up, so the relay has finished.
    while let Some(Ok(message)) = socket.next().await {
        if matches!(message, Message::Close(_)) {
            break;
        }
    }
    drop(socket);

    let row = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            for event in events.drain() {
                if let kompressor_stats::events::Event::ProxyRequest(row) = event {
                    return row;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the relay must file a proxy_requests row when the session ends");

    assert_eq!(
        row.status,
        Some(101),
        "the handshake succeeded, and 101 keeps relayed sessions separable from forwarded 200s"
    );
    assert!(row.stream, "a websocket is a stream by construction");
    assert!(row.error.is_none(), "this session ended cleanly");
    assert!(
        row.total_ms.is_some(),
        "the duration is the one cost figure this transport can honestly report"
    );
    // Null, not zero: the relay never reads a `response.completed` frame, so it
    // does not know what the provider billed. Zero would read as "this cost
    // nothing" and quietly deflate every cost report.
    assert!(row.prompt_tokens.is_none());
    assert!(row.completion_tokens.is_none());
    assert!(row.cache_read_tokens.is_none());
    // The retrieve tool is deliberately not offered here, so these are
    // genuinely zero rather than unknown.
    assert_eq!(row.retrieve_rounds, 0);
}

/// And a session that fails leaves the reason in the row.
///
/// This is the half that matters: before the relay recorded anything, a dead
/// upstream produced one `tracing::warn!` and no trace in the database at all.
/// The comment on the failure path called the log "the only place this can
/// surface" — it no longer has to be.
#[tokio::test]
async fn a_relay_that_fails_records_why() {
    // Nothing is listening on this port.
    let (proxy, events) = proxy_recording_in_front_of("127.0.0.1:1").await;
    let Ok((mut socket, _)) =
        tokio_tungstenite::connect_async(format!("ws://{proxy}/v1/responses")).await
    else {
        // Refusing the handshake outright is also correct, and files no row
        // because no session ever began.
        return;
    };
    while let Some(message) = socket.next().await {
        if message.is_err() {
            break;
        }
    }
    drop(socket);

    let row = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            for event in events.drain() {
                if let kompressor_stats::events::Event::ProxyRequest(row) = event {
                    return row;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("a failed relay must still file a row");

    assert!(
        row.error.is_some(),
        "the reason the session ended has to reach the database, not just the log"
    );
}

/// An upstream that echoes every frame and then answers a handle with a delta
/// naming it — the only way a model can ask for an elided fragment on this
/// transport, since the tool tree it is offered executes inside the client.
async fn handle_echoing_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
                    return;
                };
                while let Some(Ok(message)) = socket.next().await {
                    let Message::Text(text) = &message else {
                        if matches!(message, Message::Close(_)) {
                            break;
                        }
                        continue;
                    };
                    // Echoed first, so the test can read what the upstream was
                    // actually given.
                    let seen = text.to_string();
                    if socket.send(message.clone()).await.is_err() {
                        break;
                    }
                    if let Some(at) = seen.find("ref:k_") {
                        // "ref:" + "k_" + the 12-character body.
                        let handle = &seen[at..at + 18];
                        let delta = serde_json::json!({
                            "type": "response.output_text.delta",
                            "delta": format!("I need the contents of {handle} to answer."),
                        });
                        if socket
                            .send(Message::Text(delta.to_string().into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            });
        }
    });
    addr
}

async fn next_text(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> String {
    let message = tokio::time::timeout(Duration::from_secs(10), socket.next())
        .await
        .expect("the relay should answer well inside the timeout")
        .expect("the stream should still be open")
        .unwrap();
    match message {
        Message::Text(text) => text.to_string(),
        other => panic!("expected a text frame, got {other:?}"),
    }
}

/// 400 uniform log lines — the shape the crusher is built to collapse.
fn bulky_log() -> String {
    (0..400)
        .map(|i| {
            format!(
                "2026-09-26T10:00:{:02}Z DEBUG [req-{i:05}] resolved /api/v2/items/{} in {}ms",
                i % 60,
                i % 7,
                i % 40
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A line that is in `original` but not in `sent`.
///
/// Asking the frame which lines went missing beats naming one in advance: the
/// crusher keeps a sample from each end and whichever rows look unlike their
/// neighbours, so a token planted at a chosen offset may well be one of the
/// rows it decides to keep.
fn a_line_that_was_elided(original: &str, sent: &str) -> String {
    original
        .lines()
        .find(|line| !sent.contains(*line))
        .expect("recoverable mode must have removed something")
        .to_string()
}

#[tokio::test]
async fn a_handle_the_model_names_is_restored_on_the_next_frame() {
    // The websocket retrieve loop end to end. There is no tool call to make
    // here and no round the proxy can run on its own, so the whole mechanism is
    // this: the relay remembers what it elided, sees the handle come back in
    // the model's output, and appends the original to the client's next frame.
    let upstream = handle_echoing_server().await;
    let proxy = proxy_in_front_of(&upstream).await;
    let mut socket = connect(&proxy).await;

    let bulky = bulky_log();

    let first = serde_json::json!({
        "type": "response.create",
        "model": "gpt-5-codex",
        "input": [
            {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "summarise the log"}]},
            {"type": "custom_tool_call_output", "call_id": "call_a1", "output": bulky},
        ],
        "stream": true,
    });
    socket
        .send(Message::Text(first.to_string().into()))
        .await
        .unwrap();

    let went_up = next_text(&mut socket).await;
    assert!(
        went_up.contains("ref:k_"),
        "recoverable mode should have left a handle behind: {}",
        &went_up[..went_up.len().min(400)]
    );
    let elided = a_line_that_was_elided(&bulky, &went_up);

    // The model's answer, naming the handle. Relayed to the client untouched —
    // the relay only reads it.
    let delta = next_text(&mut socket).await;
    assert!(delta.contains("response.output_text.delta"));

    // The next frame the client sends is the one that carries the fragment
    // back. It is small and would never have been compressed on its own, which
    // is the point: restoring is not conditional on there being work to do.
    let second = serde_json::json!({
        "type": "response.create",
        "model": "gpt-5-codex",
        "previous_response_id": "resp_abc",
        "input": [
            {"type": "custom_tool_call_output", "call_id": "call_a2", "output": "ok"},
        ],
        "stream": true,
    });
    socket
        .send(Message::Text(second.to_string().into()))
        .await
        .unwrap();

    let restored = next_text(&mut socket).await;
    assert!(
        restored.contains("<kompressor_restored>"),
        "the fragment should have been appended: {restored}"
    );
    assert!(
        restored.contains(&elided),
        "and it should be the original text, not a note about it; \
         line missing upstream and still missing here: {elided}"
    );

    let parsed: serde_json::Value = serde_json::from_str(&restored).unwrap();
    // The envelope is untouched and the client's own item still leads.
    assert_eq!(parsed["type"], "response.create");
    assert_eq!(parsed["previous_response_id"], "resp_abc");
    assert_eq!(parsed["input"][0]["call_id"], "call_a2");
    let appended = &parsed["input"][1];
    assert_eq!(appended["role"], "developer");
    assert_eq!(appended["content"][0]["type"], "input_text");
}

#[tokio::test]
async fn a_restored_fragment_is_not_sent_twice() {
    // A model that keeps repeating a handle is not asking again, and answering
    // every time would pay for the fragment on every frame for the rest of the
    // session — the exact failure the HTTP side was fixed for.
    let upstream = handle_echoing_server().await;
    let proxy = proxy_in_front_of(&upstream).await;
    let mut socket = connect(&proxy).await;

    let bulky = bulky_log();
    let create = |payload: serde_json::Value| {
        serde_json::json!({
            "type": "response.create",
            "model": "gpt-5-codex",
            "input": [payload],
            "stream": true,
        })
    };

    socket
        .send(Message::Text(
            create(serde_json::json!({
                "type": "custom_tool_call_output", "call_id": "call_a1", "output": bulky
            }))
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
    let went_up = next_text(&mut socket).await;
    let elided = a_line_that_was_elided(&bulky, &went_up);
    let _asked = next_text(&mut socket).await;

    // Second frame: restored, and the upstream's echo names the handle again.
    socket
        .send(Message::Text(
            create(serde_json::json!({
                "type": "custom_tool_call_output", "call_id": "call_a2", "output": "ok"
            }))
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
    let restored = next_text(&mut socket).await;
    assert!(restored.contains(&elided), "the first ask is answered");
    let _asked_again = next_text(&mut socket).await;

    // Third frame: the handle was named again, and must not be answered again.
    socket
        .send(Message::Text(
            create(serde_json::json!({
                "type": "custom_tool_call_output", "call_id": "call_a3", "output": "ok"
            }))
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
    let again = next_text(&mut socket).await;
    assert!(
        !again.contains("<kompressor_restored>"),
        "the same fragment was sent a second time: {again}"
    );
}

#[tokio::test]
async fn a_restored_fragment_is_not_elided_again_on_the_frame_that_carries_it() {
    // The frame that carries a fragment back can be a big one — the client has
    // its own payload on it. If the restore were weighed up by the engine it
    // would be elided straight back out, under a fresh handle at that, since
    // the span minted around the wrapper text is not the one `keep_inline`
    // protects. The model would then be holding a new handle it cannot resolve
    // in place of the content it asked for.
    let upstream = handle_echoing_server().await;
    let proxy = proxy_in_front_of(&upstream).await;
    let mut socket = connect(&proxy).await;

    let bulky = bulky_log();
    let frame = |payload: &str, call: &str| {
        serde_json::json!({
            "type": "response.create",
            "model": "gpt-5-codex",
            "input": [{"type": "custom_tool_call_output", "call_id": call, "output": payload}],
            "stream": true,
        })
        .to_string()
    };

    socket
        .send(Message::Text(frame(&bulky, "call_a1").into()))
        .await
        .unwrap();
    let went_up = next_text(&mut socket).await;
    let elided = a_line_that_was_elided(&bulky, &went_up);
    let _asked = next_text(&mut socket).await;

    // A second frame every bit as bulky as the first, so the engine certainly
    // has work to do on it.
    let more = (0..400)
        .map(|i| {
            format!(
                "2026-09-26T11:00:{:02}Z INFO [job-{i:05}] committed batch {}",
                i % 60,
                i % 9
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    socket
        .send(Message::Text(frame(&more, "call_a2").into()))
        .await
        .unwrap();
    let restored = next_text(&mut socket).await;

    assert!(
        restored.contains("<kompressor_restored>"),
        "the fragment should still have been appended: {}",
        &restored[..restored.len().min(400)]
    );
    assert!(
        restored.contains(&elided),
        "the restored text was elided again on its way out: {}",
        &restored[..restored.len().min(600)]
    );
    // And the frame's own payload was still compressed: restoring is not a way
    // of opting the turn out of the engine.
    assert!(
        !restored.contains(more.lines().nth(200).unwrap()),
        "the client's own payload should still have been compressed"
    );
}

#[tokio::test]
async fn the_frame_that_elides_a_fragment_does_not_restore_it() {
    // A compressed log still carries the samples the crusher kept, and those
    // have every word in common with the fragment behind them. Weigh the turn's
    // text against handles minted on the same frame and every one of them looks
    // wanted, so the relay restores what it has just elided and the frame goes
    // out larger than it came in — compression undone, in the name of
    // retrieval.
    let upstream = echo_server().await;
    let proxy = proxy_in_front_of(&upstream).await;
    let mut socket = connect(&proxy).await;

    let bulky = bulky_log();
    let frame = serde_json::json!({
        "type": "response.create",
        "model": "gpt-5-codex",
        "input": [
            {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "summarise the resolved routes in the log"}]},
            {"type": "custom_tool_call_output", "call_id": "call_a1", "output": bulky},
        ],
        "stream": true,
    });
    let sent = frame.to_string();
    socket
        .send(Message::Text(sent.clone().into()))
        .await
        .unwrap();
    let went_up = next_text(&mut socket).await;

    assert!(
        !went_up.contains("<kompressor_restored>"),
        "a fragment was restored onto the frame that elided it"
    );
    assert!(
        went_up.len() < sent.len(),
        "sent {} bytes, upstream got {}",
        sent.len(),
        went_up.len()
    );
}

#[tokio::test]
async fn the_relay_stops_restoring_once_the_connection_has_spent_its_budget() {
    // `proxy.retrieve_max_tokens_per_request` bounds the HTTP path through
    // `Budget`. A relayed connection is what this transport has instead of a
    // request, so the same number bounds it here — without this the relay will
    // hand back whatever the model appears to ask for, and one connection in a
    // measured sweep returned 60,086 tokens against a default of 16,384.
    let upstream = handle_echoing_server().await;
    let proxy = proxy_with(&upstream, |config| {
        // Small enough that the first fragment fits and the next does not.
        config.proxy.retrieve_max_tokens_per_request = 400;
    })
    .await;
    let mut socket = connect(&proxy).await;

    let bulky = bulky_log();
    let frame = |payload: &str, call: &str| {
        serde_json::json!({
            "type": "response.create",
            "model": "gpt-5-codex",
            "input": [{"type": "custom_tool_call_output", "call_id": call, "output": payload}],
            "stream": true,
        })
        .to_string()
    };

    socket
        .send(Message::Text(frame(&bulky, "call_a1").into()))
        .await
        .unwrap();
    let _elided = next_text(&mut socket).await;
    let _asked = next_text(&mut socket).await;

    let mut restored_bytes = 0usize;
    for call in ["call_a2", "call_a3", "call_a4"] {
        socket
            .send(Message::Text(frame("ok", call).into()))
            .await
            .unwrap();
        let out = next_text(&mut socket).await;
        if let Some(at) = out.find("<kompressor_restored>") {
            restored_bytes += out[at..].len();
        }
        // The upstream only answers with a delta when the frame it saw carried
        // a handle, and a frame the budget emptied carries none — so this is
        // read with a short timeout rather than awaited.
        let _ = tokio::time::timeout(Duration::from_millis(250), socket.next()).await;
    }

    // Four characters to the token is the engine's own estimate; the budget is
    // 400 tokens, so a run that respects it cannot have shipped many thousands
    // of characters of fragment.
    assert!(
        restored_bytes < 4 * 400 * 2,
        "the budget did not bind: {restored_bytes} bytes of fragment went out \
         against a 400-token budget"
    );
}

#[tokio::test]
async fn a_fragment_the_budget_withholds_is_recorded_as_withheld() {
    // The budget bound correctly and said so only in a log line. Every rate in
    // `kompressor stats` is computed from `retrieval_events`, and a refusal
    // filed no row, so a connection refused eleven times reported `hits 7
    // (100.0%), 0 missed` — measured on a relayed sweep, 2026-10-09.
    //
    // Recorded as `withheld`, never as a miss: `found = 0` is the store not
    // having it, and blaming the store for a budget the operator set is how a
    // tightened budget would read as a regression.
    let upstream = handle_echoing_server().await;
    let (proxy, events) = proxy_recording(&upstream, |config| {
        // Small enough that the first fragment fits and the next does not.
        config.proxy.retrieve_max_tokens_per_request = 400;
    })
    .await;
    let mut socket = connect(&proxy).await;

    let bulky = bulky_log();
    let frame = |payload: &str, call: &str| {
        serde_json::json!({
            "type": "response.create",
            "model": "gpt-5-codex",
            "input": [{"type": "custom_tool_call_output", "call_id": call, "output": payload}],
            "stream": true,
        })
        .to_string()
    };

    socket
        .send(Message::Text(frame(&bulky, "call_a1").into()))
        .await
        .unwrap();
    let _elided = next_text(&mut socket).await;
    let _asked = next_text(&mut socket).await;
    for call in ["call_a2", "call_a3", "call_a4"] {
        socket
            .send(Message::Text(frame("ok", call).into()))
            .await
            .unwrap();
        let _ = next_text(&mut socket).await;
        let _ = tokio::time::timeout(Duration::from_millis(250), socket.next()).await;
    }

    let retrievals: Vec<_> = events
        .drain()
        .into_iter()
        .filter_map(|event| match event {
            kompressor_stats::events::Event::Retrieval(e) => Some(e),
            _ => None,
        })
        .collect();
    let withheld: Vec<_> = retrievals.iter().filter(|e| e.withheld).collect();
    assert!(
        !withheld.is_empty(),
        "the budget refused fragments and filed nothing; {} retrieval event(s) in all",
        retrievals.len()
    );
    for event in &withheld {
        assert!(!event.found, "a withheld row claims the store answered it");
        assert_eq!(
            event.returned_tokens, 0,
            "a withheld row debits the saving by content the model never got"
        );
        assert!(
            event.reference.starts_with("k_"),
            "a refusal with no handle tells an operator nothing: {:?}",
            event.reference
        );
    }
}

#[test]
fn the_budget_is_priced_in_the_same_unit_the_statistics_record() {
    // The budget bounds what goes back to the model; `retrieval_events`
    // records what went back. If the two use different units the budget is
    // silently the wrong size, and it is wrong by exactly the estimator's
    // prose discount — which is how a 16,384-token budget admitted 20,844.
    //
    // `AppCore::retrieve` files `estimate_tokens(returned, "")`. Anything the
    // relay checks against that number has to ask the same question.
    let fragment =
        "2026-09-26T10:00:01Z DEBUG [req-00042] resolved /api/v2/items/3 in 7ms\n".repeat(200);
    let as_recorded = kompressor_engine::estimate_tokens(&fragment, "");
    let as_authored = kompressor_engine::estimate_tokens(&fragment, "developer");

    assert!(
        as_authored < as_recorded,
        "the discount this guards against has gone away; if the estimator no \
         longer scales authored roles, this test and the comment in \
         restore_fragments are both stale"
    );
    // The margin is the whole bug: pricing a fragment as prose understates it
    // by about a fifth, so the budget admits about a fifth more than it says.
    let overshoot = as_recorded as f64 / as_authored as f64;
    assert!(
        overshoot > 1.2,
        "expected the prose discount to be material, got {overshoot:.3}x"
    );
}

#[tokio::test]
async fn the_relay_dials_its_upstream_through_the_socks5_proxy() {
    use kompressor_core::network::testing::SocksServer;
    let upstream = echo_server().await;
    let port = upstream.rsplit(':').next().unwrap().to_string();
    let server = SocksServer::with_password("alice", "s3cret");
    // `ws.test` is a name only the SOCKS5 proxy resolves, so a frame that
    // comes back at all came back through it.
    let proxy = proxy_with(&format!("ws.test:{port}"), |config| {
        config.proxy.allow_insecure_upstream = true;
        config.network.socks5 = server.url();
        config.network.socks5_username = "alice".into();
        config.network.socks5_password = "s3cret".into();
    })
    .await;

    let mut socket = connect(&proxy).await;
    socket
        .send(Message::Text("{\"type\":\"response.create\"}".into()))
        .await
        .unwrap();
    let echoed = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("the relay should answer well inside the timeout")
        .expect("the stream should still be open")
        .unwrap();
    assert_eq!(
        echoed,
        Message::Text("{\"type\":\"response.create\"}".into())
    );
    assert_eq!(server.connected(), [format!("ws.test:{port}")]);
    assert_eq!(
        server.seen()[0].credentials,
        Some(("alice".to_string(), "s3cret".to_string()))
    );
}

#[tokio::test]
async fn a_rejected_socks5_password_ends_the_relay_session() {
    use kompressor_core::network::testing::{Outcome, SocksServer};
    let upstream = echo_server().await;
    let port = upstream.rsplit(':').next().unwrap().to_string();
    let server = SocksServer::with_password("alice", "s3cret");
    let proxy = proxy_with(&format!("ws.test:{port}"), |config| {
        config.proxy.allow_insecure_upstream = true;
        config.network.socks5 = format!("socks5h://alice:wrong@{}", server.address());
    })
    .await;

    let Ok((mut socket, _)) =
        tokio_tungstenite::connect_async(format!("ws://{proxy}/v1/responses")).await
    else {
        return;
    };
    let ended = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(message) = socket.next().await {
            if message.is_err() {
                return;
            }
        }
    })
    .await;
    assert!(
        ended.is_ok(),
        "a refused proxy must end the session, not hang it"
    );
    assert_eq!(server.seen()[0].outcome, Outcome::AuthRejected);
}

#[tokio::test]
async fn a_loopback_relay_upstream_does_not_use_the_socks5_proxy() {
    use kompressor_core::network::testing::SocksServer;
    let upstream = echo_server().await;
    let server = SocksServer::open();
    let proxy = proxy_with(&upstream, |config| {
        config.network.socks5 = server.url();
    })
    .await;
    let mut socket = connect(&proxy).await;
    socket.send(Message::Text("ping".into())).await.unwrap();
    let echoed = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("answered")
        .expect("open")
        .unwrap();
    assert_eq!(echoed, Message::Text("ping".into()));
    assert!(server.seen().is_empty(), "{:?}", server.seen());
}
