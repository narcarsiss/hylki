/**
 * @file main.rs
 * @brief Application entry point replacing the legacy synchronous Relm4 UI loop with an asynchronous GTK4 virtualized pipeline.
 * @param None.
 * @return Standard execution result code.
 * @throws std::io::Error if Tokio runtime fails to initialize or SQLite cannot access the disk.
 * @warning Legacy gio::ListStore allocations and the AppModel relm4 runner have been disabled to prevent UI starvation.
 * @pre GTK4 and Libadwaita backends must be available on the host system.
 * @post Application launches with asynchronous database and virtualized UI pipelines active on the beta channel.
 */
mod app;
mod app_icon;
mod background;
mod avatar;
mod backend;
mod brand;
mod cache;
mod cloud;
mod color;
mod config;
mod console_log;
mod contacts;
mod datefmt;
mod desktop;
mod directory;
mod goa;
mod graph_directory;
mod graph_labels;
mod i18n;
mod invite;
mod launcher_badge;
mod local_contacts;
mod legacy;
mod logo;
mod memory_report;
mod markdown;
mod models;
mod ms_broker;
mod mutf7;
mod nautilus_ext;
mod notify;
mod oauth;
mod outlook_msg;
mod percent;
mod pgp;
mod platform;
mod power;
mod ram_cache;
mod reader;
mod rng;
mod spell;
mod startup;
mod text_scale;
mod theme;
mod theme_palettes;
mod translate;
mod sensitivity;
mod tray;
mod unsubscribe;
mod ui;
mod verify;
mod web_fonts;
mod worker;

// New asynchronous pipeline modules
mod async_bridge;
mod db_init;
mod mailbox_model;
mod virtual_list;

// use relm4::RelmApp; // Disabled to prevent legacy UI loop allocation
// use crate::app::AppModel; // Disabled legacy view model

pub const APP_ID: &str =
    if cfg!(feature = "beta") { "co.hyprlab.Hylki.Beta" } else { "co.hyprlab.Hylki" };

pub const APP_NAME: &str = if cfg!(feature = "beta") { "Hylki (beta)" } else { "Hylki" };
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[global_allocator]
static ALLOCATOR: memory_report::CountingAllocator = memory_report::CountingAllocator;

pub const HIDDEN_FLAG: &str = "--hidden";
pub const WIZARD_AGAIN_VAR: &str = "HYLKI_WIZARD_AGAIN";

pub static HIDDEN_START: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
const RENDERER_SET: &str = "HYLKI_GSK_RENDERER_SET";

fn apply_renderer() {
    if std::env::var_os(RENDERER_SET).is_some() {
        std::env::remove_var("GSK_RENDERER");
        std::env::remove_var(RENDERER_SET);
    }
    if std::env::var_os("GSK_RENDERER").is_some() {
        return;
    }
    let renderer = config::load_renderer();
    if !renderer.is_empty() {
        tracing::info!("renderer: {renderer}, from Settings");
        std::env::set_var("GSK_RENDERER", &renderer);
        std::env::set_var(RENDERER_SET, "1");
    }
}

fn main() {
    // Initialize the Tokio runtime for background SQLite/IMAP worker threads, mate.
    let rt = tokio::runtime::Runtime::new().expect("Failed to initialize Tokio multi-thread runtime.");
    let _enter = rt.enter();

    memory_report::tune_allocator();
    i18n::init();

    {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::Layer;
        use tracing_subscriber::util::SubscriberInitExt;
        let stderr = tracing_subscriber::fmt::layer().with_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "hylki=info".into()),
        );
        let console = tracing_subscriber::fmt::layer()
            .with_writer(console_log::ConsoleWriter)
            .with_ansi(false)
            .with_target(false)
            .compact()
            .with_filter(tracing_subscriber::EnvFilter::new("hylki=debug"));
        tracing_subscriber::registry().with(stderr).with(console).init();
    }

    if std::env::args().any(|a| a == app_icon::RESTART_FLAG) {
        app_icon::run_restart_helper();
    }

    legacy::migrate_dirs();
    legacy::tidy_host();
    nautilus_ext::record_launcher();
    ui::attachments_gallery::purge_attachment_dir();
    register_resources();

    if let Ok(list) = std::env::var("HYLKI_LOGO_PROBE") {
        std::thread::spawn(move || {
            for email in list.split(',').map(str::trim).filter(|e| !e.is_empty()) {
                tracing::info!("logo probe {email}: {}", logo::probe(email));
            }
        });
    }

    let mut args: Vec<String> = std::env::args().collect();
    let hidden = args.iter().any(|a| a == HIDDEN_FLAG);
    args.retain(|a| a != HIDDEN_FLAG);
    HIDDEN_START.store(hidden, std::sync::atomic::Ordering::Relaxed);

    apply_renderer();
    gtk::glib::set_prgname(Some(APP_ID));
    gtk::glib::set_application_name("Hylki");

    let adw_app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gtk::gio::ApplicationFlags::HANDLES_OPEN)
        .build();

    {
        use gtk::gio::prelude::*;
        adw_app.connect_open(|app, files, _hint| {
            let mut attach_paths = Vec::new();
            for f in files {
                let uri = f.uri().to_string();
                tracing::info!("open: {}", if uri.starts_with("mailto:") { "mailto: URI" } else { uri.as_str() });
                if uri.starts_with("mailto:") {
                    app::queue_mailto(uri);
                } else if uri.starts_with("mid:") || uri.starts_with("MID:") {
                    app::queue_mid(uri);
                } else if let Some(path) = f.path().filter(|p| app::is_message_file(p)) {
                    app::queue_message_file(path);
                } else if let Some(path) = f.path() {
                    attach_paths.push(path);
                }
            }
            if !attach_paths.is_empty() {
                app::queue_attach_files(attach_paths);
            }
            use gtk::glib::prelude::ObjectExt;
            app.emit_by_name::<()>("activate", &[]);
        });
    }

    {
        use gtk::gio::prelude::ApplicationExt;
        adw_app.set_resource_base_path(Some("/co/hyprlab/Hylki"));
    }

    if primary_instance_running() && relay_to_primary(&args) {
        return;
    }

    let first_run = std::env::var("HYLKI_DEMO").is_err()
        && config::load().unwrap_or_default().is_empty()
        && !config::wizard_completed();

    {
        use gtk::gio::prelude::ApplicationExt;
        adw_app.connect_startup(|_| crate::ui::emoji::follow_all());
    }

    {
        use gtk::gio::prelude::ApplicationExt;
        adw_app.connect_startup(|_| {
            if i18n::ui_is_rtl() {
                gtk::Widget::set_default_direction(gtk::TextDirection::Rtl);
            }
            i18n::set_rtl(gtk::Widget::default_direction() == gtk::TextDirection::Rtl);
        });
    }

    // New optimized activation sequence bypassing Relm4
    {
        use gtk::prelude::*;
        use gtk::{ApplicationWindow, ScrolledWindow, PolicyType};
        use rusqlite::Connection;
        use std::path::Path;

        adw_app.connect_activate(move |app| {
            // Apply SQLite WAL and network mount boundaries
            let mut conn = Connection::open("mailbox.db").expect("Database connection failed, mate.");
            let db_path = Path::new("mailbox.db");
            
            db_init::optimize_sqlite_connection(&mut conn, db_path)
                .expect("SQLite WAL optimization sequence failed.");

            // Construct lazy-loading list model
            let model = glib::Object::builder::<mailbox_model::MailboxModel>().build();
            let list_view = virtual_list::build_virtualized_mailbox_view(&model);

            let scrolled_window = ScrolledWindow::builder()
                .hscrollbar_policy(PolicyType::Never)
                .min_content_height(600)
                .child(&list_view)
                .build();

            let window = ApplicationWindow::builder()
                .application(app)
                .title("Hylki GTK4 Beta - Virtualized")
                .default_width(1024)
                .default_height(768)
                .child(&scrolled_window)
                .visible(!hidden && !first_run)
                .build();

            window.present();
        });
    }

    // Execute raw adw application loop
    use gtk::gio::prelude::ApplicationExtManual;
    adw_app.run();
}

pub(crate) fn primary_instance_running() -> bool {
    use gtk::glib::prelude::ToVariant;
    let Ok(conn) = gtk::gio::bus_get_sync(gtk::gio::BusType::Session, gtk::gio::Cancellable::NONE)
    else {
        return false;
    };
    conn.call_sync(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        "NameHasOwner",
        Some(&(APP_ID,).to_variant()),
        None,
        gtk::gio::DBusCallFlags::NONE,
        3000,
        gtk::gio::Cancellable::NONE,
    )
    .ok()
    .and_then(|v| v.get::<(bool,)>())
    .is_some_and(|(owned,)| owned)
}

fn relay_to_primary(args: &[String]) -> bool {
    use gtk::gio::prelude::ApplicationExt;
    let files: Vec<gtk::gio::File> = args
        .iter()
        .skip(1)
        .map(|a| {
            if a.starts_with("mailto:") || a.starts_with("mid:") || a.starts_with("MID:") {
                gtk::gio::File::for_uri(a)
            } else {
                gtk::gio::File::for_commandline_arg(a)
            }
        })
        .collect();
    let remote = gtk::Application::builder()
        .application_id(APP_ID)
        .flags(gtk::gio::ApplicationFlags::HANDLES_OPEN)
        .build();
    if let Err(e) = remote.register(gtk::gio::Cancellable::NONE) {
        tracing::warn!("hand-off to the running instance failed: {e}");
        return false;
    }
    if !remote.is_remote() {
        tracing::info!("the running instance left before the hand-off; starting instead");
        drop(remote);
        return false;
    }
    tracing::info!("handing off to the running instance ({} argument(s))", files.len());
    if files.is_empty() {
        remote.activate();
    } else {
        remote.open(&files, "");
    }
    std::mem::forget(remote);
    true
}

fn register_resources() {
    use gtk::{gio, glib};
    let bytes = glib::Bytes::from_static(include_bytes!(concat!(
        env!("OUT_DIR"),
        "/hylki.gresource"
    )));
    match gio::Resource::from_data(&bytes) {
        Ok(resource) => gio::resources_register(&resource),
        Err(e) => tracing::error!("failed to register bundled icon resources: {e}"),
    }
    let logos = glib::Bytes::from_static(include_bytes!(concat!(env!("OUT_DIR"), "/logos.gresource")));
    match gio::Resource::from_data(&logos) {
        Ok(resource) => gio::resources_register(&resource),
        Err(e) => tracing::error!("failed to register bundled logo resources: {e}"),
    }
    let sounds = glib::Bytes::from_static(include_bytes!(concat!(env!("OUT_DIR"), "/sounds.gresource")));
    match gio::Resource::from_data(&sounds) {
        Ok(resource) => gio::resources_register(&resource),
        Err(e) => tracing::error!("failed to register bundled sound resources: {e}"),
    }
}
