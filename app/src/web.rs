//! Browser entry point and page glue: start eframe's web runner on the page's canvas, log to
//! the console, and publish frame timings on `window.terminalPerf` for measurement.
//!
//! Query flags: `?sample` serves offline sample data only (like `TR_PROVIDER=sample`), and
//! `?fps` shows the frame-time readout and repaints continuously (like `TR_FPS`).
//!
//! Crypto markets come from the Terminal server that `TR_SERVER_URL` named when the page was built
//! (scripts/build-web.sh). `?server=off` turns it off, and `?server=ws://localhost:8787/ws` uses a
//! server on this machine instead (see `tr_data::web::server_url`).

use std::cell::Cell;

use tracing::{Level, Metadata};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use wasm_bindgen::{JsCast, JsValue};
use web_time::Instant;

pub fn start() {
    init_tracing();
    let t0 = Instant::now();
    let query = web_sys::window().and_then(|w| w.location().search().ok()).unwrap_or_default();
    let offline = has_flag(&query, "sample");
    let perf = has_flag(&query, "fps");
    let requested = web_sys::UrlSearchParams::new_with_str(&query).ok().and_then(|p| p.get("server"));
    let server = tr_data::web::server_url(option_env!("TR_SERVER_URL"), requested.as_deref());
    if let Some(url) = server.as_deref().filter(|_| !offline) {
        tracing::info!(
            "crypto markets come from the Terminal server at {url}, or Binance and Coinbase while it's down"
        );
    }
    wasm_bindgen_futures::spawn_local(async move {
        let Some(canvas) = canvas() else { return fail("the page has no #terminal canvas") };
        let started = eframe::WebRunner::new()
            .start(
                canvas,
                eframe::WebOptions::default(),
                Box::new(move |cc| {
                    // The first frame needs fonts; set them before it (set_fonts applies from the next pass).
                    cc.egui_ctx.set_fonts(crate::theme::base_fonts());
                    wasm_bindgen_futures::spawn_local(load_emoji_fonts(cc.egui_ctx.clone()));
                    let mut app = crate::create_app(cc, tr_data::for_browser(offline, server.as_deref()), 1, t0)?;
                    if perf {
                        app.show_perf();
                    }
                    Ok(Box::new(app))
                }),
            )
            .await;
        if let Err(e) = started {
            fail(&format!("{e:?}"));
        }
    });
}

/// Fetch egui's emoji fonts, which scripts/build-web.sh puts in `fonts/` next to the page, and
/// add them as fallbacks. Until they arrive, emoji and some symbols draw as boxes.
async fn load_emoji_fonts(ctx: egui::Context) {
    let fonts = [("NotoEmoji-Regular", 0.81), ("emoji-icon-font", 0.90)];
    let fetched = futures::future::join_all(fonts.iter().map(|(name, _)| fetch(format!("fonts/{name}.ttf")))).await;
    for ((name, scale), bytes) in fonts.into_iter().zip(fetched) {
        match bytes {
            Ok(bytes) => {
                let tweak = egui::FontTweak { scale, ..Default::default() };
                crate::theme::add_late_font(&ctx, name, egui::FontData::from_owned(bytes).tweak(tweak));
            }
            Err(e) => tracing::warn!("font {name}: {e}"),
        }
    }
    ctx.request_repaint();
}

async fn fetch(url: String) -> Result<Vec<u8>, String> {
    use wasm_bindgen_futures::JsFuture;
    let window = web_sys::window().ok_or("no window")?;
    let resp: web_sys::Response = JsFuture::from(window.fetch_with_str(&url))
        .await
        .map_err(|e| format!("{e:?}"))?
        .dyn_into()
        .map_err(|_| "not a response")?;
    if !resp.ok() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let buf = JsFuture::from(resp.array_buffer().map_err(|e| format!("{e:?}"))?).await.map_err(|e| format!("{e:?}"))?;
    Ok(js_sys::Uint8Array::new(&buf).to_vec())
}

fn has_flag(query: &str, flag: &str) -> bool {
    query.trim_start_matches('?').split('&').any(|p| p.split('=').next() == Some(flag))
}

fn canvas() -> Option<web_sys::HtmlCanvasElement> {
    web_sys::window()?.document()?.get_element_by_id("terminal")?.dyn_into().ok()
}

/// Show why the app couldn't start in place of the loading message.
fn fail(why: &str) {
    tracing::error!("terminal failed to start: {why}");
    if let Some(el) = web_sys::window().and_then(|w| w.document()).and_then(|d| d.get_element_by_id("status")) {
        el.set_text_content(Some(&format!("Terminal couldn't start: {why}")));
    }
}

/// Per-frame numbers from `TerminalApp::ui` (milliseconds).
pub struct FrameStats {
    /// Smoothed CPU time inside `ui()`.
    pub ui_ms: f64,
    /// Smoothed time between frames.
    pub interval_ms: f64,
    /// eframe's CPU time for the whole previous frame (UI, tessellation and painting).
    pub last_cpu_ms: f64,
    pub frames: u64,
}

thread_local! {
    static FIRST_FRAME: Cell<bool> = const { Cell::new(true) };
}

pub fn record_frame(s: &FrameStats) {
    let Some(w) = web_sys::window() else { return };
    let perf = match js_sys::Reflect::get(&w, &"terminalPerf".into()) {
        Ok(v) if v.is_object() => v,
        _ => {
            let v: JsValue = js_sys::Object::new().into();
            let _ = js_sys::Reflect::set(&w, &"terminalPerf".into(), &v);
            v
        }
    };
    let set = |k: &str, v: f64| {
        let _ = js_sys::Reflect::set(&perf, &k.into(), &v.into());
    };
    if FIRST_FRAME.replace(false) {
        let at = w.performance().map_or(0.0, |p| p.now());
        tracing::info!("first frame built {at:.0} ms after navigation start");
        set("firstFrameMs", at);
        if let Some(el) = w.document().and_then(|d| d.get_element_by_id("status")) {
            el.remove();
        }
    }
    set("uiMs", s.ui_ms);
    set("intervalMs", s.interval_ms);
    set("cpuMs", s.last_cpu_ms);
    set("frames", s.frames as f64);
}

fn init_tracing() {
    let filter = tracing_subscriber::filter::Targets::new()
        .with_default(Level::INFO)
        .with_target("wgpu_core", Level::WARN)
        .with_target("wgpu_hal", Level::WARN)
        .with_target("naga", Level::WARN);
    let fmt = tracing_subscriber::fmt::layer().with_writer(Console).without_time().with_ansi(false);
    let _ = tracing_subscriber::registry().with(fmt).with(filter).try_init();
}

/// Sends each formatted log line to the browser console at its level.
struct Console;

struct Line {
    level: Level,
    buf: Vec<u8>,
}

impl std::io::Write for Line {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for Line {
    fn drop(&mut self) {
        let text = JsValue::from_str(String::from_utf8_lossy(&self.buf).trim_end());
        match self.level {
            Level::ERROR => web_sys::console::error_1(&text),
            Level::WARN => web_sys::console::warn_1(&text),
            _ => web_sys::console::log_1(&text),
        }
    }
}

impl<'a> MakeWriter<'a> for Console {
    type Writer = Line;

    fn make_writer(&'a self) -> Line {
        Line { level: Level::INFO, buf: Vec::new() }
    }

    fn make_writer_for(&'a self, meta: &Metadata<'_>) -> Line {
        Line { level: *meta.level(), buf: Vec::new() }
    }
}
