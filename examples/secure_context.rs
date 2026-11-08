//! Does the page we serve ourselves get WebCrypto?
//!
//! The link scraper decrypts PrivateBin pastes in the window, which needs
//! `crypto.subtle`, which only exists in a secure context. `with_html` puts
//! the page on `about:blank`, which is not one — so the app serves itself over
//! a custom protocol instead. That holds on WebView2. WKWebView and WebKitGTK
//! are different engines with different rules about custom schemes, and the
//! only honest way to know is to ask them.
//!
//!     cargo run --example secure_context
//!
//! Prints one line of JSON and exits. CI runs it on all three platforms.

use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::window::WindowBuilder;
use wry::WebViewBuilder;

const PAGE: &str = r#"<!doctype html><meta charset="utf-8"><script>
window.ipc.postMessage(JSON.stringify({
  url: location.href,
  secure: window.isSecureContext,
  subtle: !!(window.crypto && window.crypto.subtle),
  decompress: typeof DecompressionStream,
}));
</script>"#;

fn main() {
    let event_loop = EventLoopBuilder::<String>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let window = WindowBuilder::new()
        .with_visible(false)
        .build(&event_loop)
        .expect("create window");

    let _webview = WebViewBuilder::new()
        .with_custom_protocol("idm".into(), |_id, _req| {
            wry::http::Response::builder()
                .header("Content-Type", "text/html")
                .body(std::borrow::Cow::Borrowed(PAGE.as_bytes()))
                .expect("serve the probe page")
        })
        .with_url("idm://localhost/")
        .with_ipc_handler(move |request| {
            let _ = proxy.send_event(request.into_body());
        })
        .build(&window)
        .expect("create webview");

    // Don't hang a CI job forever if the page never reports.
    let watchdog = event_loop.create_proxy();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(30));
        let _ = watchdog.send_event(r#"{"error":"the page never reported"}"#.to_string());
    });

    event_loop.run(move |event, _target, flow| {
        *flow = ControlFlow::Wait;
        if let tao::event::Event::UserEvent(body) = event {
            println!("PROBE {}", body);
            let good = body.contains("\"secure\":true") && body.contains("\"subtle\":true");
            std::process::exit(if good { 0 } else { 1 });
        }
    });
}
