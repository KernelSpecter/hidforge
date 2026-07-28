// No console window in release; keep one in debug for panics and logs.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod bench;
mod clock;
mod engine;
mod inject;
mod macros;
mod rawinput;
mod theme;
mod tray;

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--bench") {
        bench::run();
        return Ok(());
    }
    if args.iter().any(|a| a == "--selftest") {
        bench::selftest();
        return Ok(());
    }
    if let Some(i) = args.iter().position(|a| a == "--probe") {
        let secs = args
            .get(i + 1)
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(20.0);
        bench::probe(secs);
        return Ok(());
    }

    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1000.0, 660.0])
            .with_min_inner_size([880.0, 600.0])
            .with_title("HidForge"),
        ..Default::default()
    };
    eframe::run_native(
        "HidForge",
        opts,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    )
}
