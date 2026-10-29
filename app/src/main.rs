//! terminal-rs: a keyboard-driven, Bloomberg-style market terminal with GPU charts and
//! congressional (STOCK Act) trade tracking.

mod anim;
mod app;
mod chart_view;
mod command;
mod congress_view;
mod dock;
mod layout;
mod layouts;
mod markets_view;
mod palette;
mod prefs;
mod storage;
mod theme;
mod themes;
#[cfg(target_arch = "wasm32")]
mod web;
mod widgets;
mod worker;

use app::TerminalApp;
use web_time::Instant;

/// Build the terminal once eframe has a GPU: register the chart renderer, start the worker
/// and restore the saved workspace. Shared by the desktop and browser entry points.
fn create_app(
    cc: &eframe::CreationContext<'_>,
    provider: tr_data::Provider,
    msaa: u32,
    t0: Instant,
) -> Result<TerminalApp, Box<dyn std::error::Error + Send + Sync>> {
    let rs = cc.wgpu_render_state.as_ref().ok_or("terminal-rs requires the wgpu renderer")?;
    // Charts read their series from storage buffers in the vertex shader. WebGL2 has none,
    // so there chart panes keep their axes and labels but draw no series.
    let charts = rs.adapter.get_downlevel_capabilities().flags.contains(eframe::wgpu::DownlevelFlags::VERTEX_STORAGE);
    if charts {
        rs.renderer.write().callback_resources.insert(tr_chart::gpu::ChartRenderer::new(
            &rs.device,
            rs.target_format,
            msaa,
        ));
    }
    let name = provider.name().to_string();
    let worker = worker::Worker::spawn(cc.egui_ctx.clone(), provider)?;
    let info = rs.adapter.get_info();
    tracing::info!("window + GPU ready in {:?} (adapter: {}, backend: {:?})", t0.elapsed(), info.name, info.backend);
    let mut app = TerminalApp::new(&cc.egui_ctx, worker, name);
    if !charts {
        tracing::warn!("this GPU backend has no vertex storage buffers; charts are off");
        app.set_status("Charts need WebGPU, which this browser doesn't offer (it fell back to WebGL2).".into());
    }
    Ok(app)
}

#[cfg(target_arch = "wasm32")]
fn main() {
    web::start();
}

#[cfg(not(target_arch = "wasm32"))]
fn main() -> eframe::Result {
    let t0 = Instant::now();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,wgpu_core=warn,wgpu_hal=warn,naga=warn".into()),
        )
        .init();

    // MSAA smooths chart lines and drawings; 4x by default on macOS (TR_MSAA overrides).
    let msaa: u16 = std::env::var("TR_MSAA")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| matches!(n, 1 | 2 | 4 | 8))
        .unwrap_or(if cfg!(target_os = "macos") { 4 } else { 1 });
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Terminal")
        .with_app_id("terminal-rs")
        .with_icon(eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")).expect("app icon"))
        .with_inner_size([1600.0, 960.0])
        .with_min_inner_size([900.0, 560.0]);
    if cfg!(target_os = "macos") {
        // Content extends under a transparent title bar; tr-macos adds the unified
        // toolbar (inset traffic lights) and sidebar vibrancy on the first frame.
        viewport = viewport
            .with_transparent(true)
            .with_fullsize_content_view(true)
            .with_title_shown(false)
            .with_titlebar_shown(false);
    }
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        viewport,
        multisampling: if msaa > 1 { msaa } else { 0 },
        ..Default::default()
    };
    eframe::run_native(
        "terminal-rs",
        options,
        Box::new(move |cc| Ok(Box::new(create_app(cc, tr_data::from_env(), u32::from(msaa), t0)?))),
    )
}
