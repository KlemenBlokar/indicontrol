mod app;
mod guider_view;
mod imaging_view;
mod link;
mod mount_view;
mod property_view;
mod sequence_view;

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use clap::Parser;

    use crate::app::{App, EngineChoice, StartOptions};

    /// indicontrol: astrophotography control for INDI equipment.
    ///
    /// By default an engine runs inside this app. Use --engine to control a
    /// standalone `indicontrol-engine` (e.g. on the computer at the telescope).
    #[derive(Parser)]
    #[command(version)]
    struct Args {
        /// Remote engine to use (HOST[:PORT] or ws:// URL).
        #[arg(long, value_name = "HOST[:PORT]")]
        engine: Option<String>,
        /// Use the built-in local engine (overrides a saved remote choice).
        #[arg(long, conflicts_with = "engine")]
        local: bool,
        /// Local engine only: connect to this INDI server on startup.
        #[arg(long, value_name = "HOST[:PORT]")]
        indi: Option<String>,
    }

    pub fn main() -> eframe::Result {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "info".into()),
            )
            .init();
        let args = Args::parse();
        let options = StartOptions {
            engine: match (args.engine, args.local) {
                (Some(addr), _) => Some(EngineChoice::Remote(addr)),
                (None, true) => Some(EngineChoice::Local),
                (None, false) => None,
            },
            local_indi: args
                .indi
                .map(|s| protocol::parse_host_port(&s, indi_link::DEFAULT_PORT)),
        };
        let native_options = eframe::NativeOptions {
            viewport: eframe::egui::ViewportBuilder::default()
                .with_title("indicontrol")
                .with_inner_size([1280.0, 800.0]),
            ..Default::default()
        };
        eframe::run_native(
            "indicontrol",
            native_options,
            Box::new(|cc| Ok(Box::new(App::new(cc, options)))),
        )
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn main() -> eframe::Result {
    native::main()
}

#[cfg(target_arch = "wasm32")]
fn main() {
    use eframe::wasm_bindgen::JsCast as _;

    eframe::WebLogger::init(log::LevelFilter::Info).ok();
    wasm_bindgen_futures::spawn_local(async {
        let canvas = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.get_element_by_id("indicontrol_canvas"))
            .and_then(|e| e.dyn_into::<web_sys::HtmlCanvasElement>().ok())
            .expect("missing <canvas id=\"indicontrol_canvas\">");
        let result = eframe::WebRunner::new()
            .start(
                canvas,
                eframe::WebOptions::default(),
                Box::new(|cc| Ok(Box::new(app::App::new(cc, Default::default())))),
            )
            .await;
        if let Err(e) = result {
            log::error!("failed to start indicontrol: {e:?}");
        }
    });
}
