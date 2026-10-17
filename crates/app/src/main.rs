mod app;
mod property_view;
mod settings;

fn main() -> eframe::Result {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("indi-io")
        .build()
        .expect("failed to start tokio runtime");

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("indicontrol")
            .with_inner_size([1280.0, 800.0]),
        ..Default::default()
    };
    // Optional `host[:port]` argument connects on startup.
    let autoconnect = std::env::args().nth(1).map(|arg| match arg.rsplit_once(':') {
        Some((host, port)) if port.parse::<u16>().is_ok() => (host.to_owned(), port.parse().unwrap()),
        _ => (arg, indi_link::DEFAULT_PORT),
    });
    eframe::run_native(
        "indicontrol",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc, runtime, autoconnect)))),
    )
}
