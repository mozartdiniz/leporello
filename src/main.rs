mod config;
mod editor;
mod notifier;
mod secrets;
mod window;

use adw::prelude::*;
use gtk::glib;

/// Tokio runtime for network I/O; GTK owns the main thread.
static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();

pub fn runtime() -> &'static tokio::runtime::Runtime {
    RUNTIME.get().expect("tokio runtime not initialised")
}

/// Runs `fut` on tokio and awaits it from any executor (GTK's included).
pub async fn bg<F>(fut: F) -> F::Output
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    runtime().spawn(fut).await.expect("background task panicked")
}

fn main() -> glib::ExitCode {
    pretty_env_logger::init();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");
    assert!(RUNTIME.set(rt).is_ok(), "runtime set once");

    glib::set_application_name("Leporello");
    let app = adw::Application::builder()
        .application_id(config::APP_ID)
        .flags(gtk::gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();
    app.connect_startup(|app| {
        app.set_accels_for_action("win.new-reminder", &["<Control>n"]);
        app.set_accels_for_action("win.refresh", &["<Control>r"]);
        app.set_accels_for_action("win.search", &["<Control>f"]);
        app.set_accels_for_action("window.close", &["<Control>w"]);
        app.set_accels_for_action("app.quit", &["<Control>q"]);
        let quit = gtk::gio::SimpleAction::new("quit", None);
        let a = app.clone();
        quit.connect_activate(move |_, _| a.quit());
        app.add_action(&quit);
        // Alerts must keep firing with the window closed, so the app stays
        // alive until Quit (Ctrl+Q). `--background` starts without a window
        // (autostart).
        std::mem::forget(app.hold());
    });
    app.connect_command_line(|app, cmd| {
        let background = cmd.arguments().iter().any(|a| a.to_str() == Some("--background"));
        let first = app.active_window().is_none() && window::controller().is_none();
        if first {
            window::start(app);
        }
        if !background || !first {
            window::present(app);
        }
        if let Some(path) = std::env::var_os("LEPORELLO_SNAPSHOT") {
            if let Some(win) = app.active_window() {
                snapshot_later(win, path.into());
            }
        }
        glib::ExitCode::SUCCESS
    });
    app.connect_activate(window::present);
    app.run()
}

/// Dev aid: `LEPORELLO_SNAPSHOT=out.png` renders the window to a PNG after
/// a few seconds (for checking layouts where screen capture isn't available).
/// `LEPORELLO_SNAPSHOT_SCRIPT` runs window actions first, comma-separated.
fn snapshot_later(win: gtk::Window, path: std::path::PathBuf) {
    glib::timeout_add_seconds_local_once(4, move || {
        if let Ok(script) = std::env::var("LEPORELLO_SNAPSHOT_SCRIPT") {
            for action in script.split(',').filter(|a| !a.is_empty()) {
                let _ = win.activate_action(action, None);
            }
        }
        glib::timeout_add_seconds_local_once(1, move || {
            // A dialog, if one is open, is drawn inside the window.
            let (w, h) = (win.width(), win.height());
            let paintable = gtk::WidgetPaintable::new(Some(&win));
            let snapshot = gtk::Snapshot::new();
            paintable.snapshot(&snapshot, w as f64, h as f64);
            let (Some(node), Some(renderer)) = (snapshot.to_node(), win.renderer()) else { return };
            let texture = renderer.render_texture(node, None);
            match texture.save_to_png(&path) {
                Ok(()) => log::info!("snapshot written to {}", path.display()),
                Err(e) => log::warn!("snapshot failed: {e}"),
            }
        });
    });
}
