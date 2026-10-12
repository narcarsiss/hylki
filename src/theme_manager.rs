//! # HYLKI GTK4 DYNAMIC THEME ENGINE
//!
//! G'day mate! This module handles loading, extracting, and applying external .theme bundles
//! (standard ZIP archives containing theme.toml, gtk.css, reader.css, and assets)
//! from ~/.local/share/hylki/themes/ with live GTK4 CSS injection, window transparency,
//! WebKit email body styling, and a fail-safe safety rollback mechanism.
//! She'll be right, no main-thread freezes or broken styles allowed!

use std::cell::RefCell;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use gtk::prelude::*;
use gtk::glib;
use adw::prelude::*;
use tracing::{debug, error, info, warn};

/// Manifest schema defined inside `theme.toml` of each .theme bundle.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ThemeManifest {
    pub theme: ThemeMeta,
    #[serde(default)]
    pub window: WindowConfig,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ThemeMeta {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub style: Option<String>,
}

#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
pub struct WindowConfig {
    #[serde(default)]
    pub transparent: bool,
    #[serde(default)]
    pub opacity: Option<f64>,
    #[serde(default)]
    pub blur: bool,
}

/// Metadata representation of a discovered theme bundle.
#[derive(Debug, Clone)]
pub struct ThemeInfo {
    pub id: String,
    pub name: String,
    pub bundle_path: PathBuf,
    pub extracted_dir: PathBuf,
    pub manifest: ThemeManifest,
}

/// ThemeManager backend engine responsible for scanning ~/.local/share/hylki/themes/
/// for .theme (ZIP) archives and unpacked theme bundles, and unpacking them to cache.
#[derive(Debug, Clone)]
pub struct ThemeManager {
    pub themes_dir: PathBuf,
    pub cache_dir: PathBuf,
}

impl Default for ThemeManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ThemeManager {
    /// Constructs a new ThemeManager pointing to standard XDG user directories.
    pub fn new() -> Self {
        Self {
            themes_dir: themes_dir(),
            cache_dir: cache_dir(),
        }
    }

    /// Discovers all available .theme bundles and unpacked theme directories.
    pub fn discover(&self) -> Vec<ThemeInfo> {
        discover_themes()
    }

    /// Unpacks a .theme (ZIP) archive to `~/.cache/hylki/themes/<id>/`.
    pub fn unpack(&self, zip_path: &Path, id: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
        let dest = self.cache_dir.join(id);
        extract_bundle(zip_path, &dest)?;
        Ok(dest)
    }

    /// Parses the manifest `theme.toml` located inside an extracted theme folder.
    pub fn parse_manifest(&self, dir: &Path) -> Result<ThemeManifest, Box<dyn std::error::Error>> {
        let manifest_path = dir.join("theme.toml");
        let content = fs::read_to_string(&manifest_path)?;
        let mut manifest: ThemeManifest = toml::from_str(&content)?;
        // Strict guardrail: Window transparency and blur are strictly isolated to glass-vibrancy.
        // All other themes remain 100% opaque at the system window level, fair dinkum!
        if manifest.theme.id != "glass-vibrancy" {
            manifest.window.transparent = false;
            manifest.window.blur = false;
            manifest.window.opacity = 1.0;
        }
        Ok(manifest)
    }
}

thread_local! {
    /// Active custom GTK CSS provider at STYLE_PROVIDER_PRIORITY_USER
    static ACTIVE_PROVIDER: RefCell<Option<gtk::CssProvider>> = const { RefCell::new(None) };

    /// Saved backup provider for the 20-second safety rollback
    static SAVED_PROVIDER: RefCell<Option<gtk::CssProvider>> = const { RefCell::new(None) };

    /// Currently applied theme ID (None = Stock GNOME / Built-in theme)
    static ACTIVE_THEME_ID: RefCell<Option<String>> = const { RefCell::new(None) };

    /// Backup theme ID before an unconfirmed switch
    static SAVED_THEME_ID: RefCell<Option<String>> = const { RefCell::new(None) };

    /// Active custom reader.css payload for WebKit email bodies
    static ACTIVE_READER_CSS: RefCell<Option<String>> = const { RefCell::new(None) };

    /// Backup reader.css payload
    static SAVED_READER_CSS: RefCell<Option<String>> = const { RefCell::new(None) };

    /// Reference to the main application window for transparency toggling
    static MAIN_WINDOW: RefCell<Option<adw::ApplicationWindow>> = const { RefCell::new(None) };

    /// Backup transparency flag for rollback
    static SAVED_TRANSPARENCY: RefCell<bool> = const { RefCell::new(false) };

    /// Backup adw::ColorScheme for 20s rollback safety, fair dinkum!
    static SAVED_COLOR_SCHEME: RefCell<Option<adw::ColorScheme>> = const { RefCell::new(None) };

    /// Weak references to active WebKit UserContentManagers to update on theme switch
    static REGISTERED_UCMS: RefCell<Vec<glib::WeakRef<webkit6::UserContentManager>>> = const { RefCell::new(Vec::new()) };
}

/// Directory where user .theme files reside: `~/.local/share/hylki/themes`
pub fn themes_dir() -> PathBuf {
    dirs::data_local_dir()
        .map(|p| p.join("hylki").join("themes"))
        .unwrap_or_else(|| PathBuf::from(".local/share/hylki/themes"))
}

/// Directory where extracted theme assets are cached: `~/.cache/hylki/themes`
pub fn cache_dir() -> PathBuf {
    dirs::cache_dir()
        .map(|p| p.join("hylki").join("themes"))
        .unwrap_or_else(|| PathBuf::from(".cache/hylki/themes"))
}

/// Ensure local theme and cache directories exist and seed bundled example themes if available.
pub fn ensure_directories() {
    let t_dir = themes_dir();
    let c_dir = cache_dir();
    if let Err(e) = fs::create_dir_all(&t_dir) {
        warn!("[theme_manager] Failed to create themes dir {:?}: {}", t_dir, e);
    }
    if let Err(e) = fs::create_dir_all(&c_dir) {
        warn!("[theme_manager] Failed to create cache dir {:?}: {}", c_dir, e);
    }

    // Seed bundled themes from the root `themes/` directory into `~/.local/share/hylki/themes/`
    let bundled_root = Path::new("themes");
    if bundled_root.is_dir() {
        if let Ok(entries) = fs::read_dir(bundled_root) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() && p.join("theme.toml").exists() {
                    let dir_name = p.file_name().unwrap_or_default();
                    let target_dir = t_dir.join(dir_name);
                    if !target_dir.exists() {
                        let _ = copy_dir_all(&p, &target_dir);
                    }
                }
            }
        }
    }
}

fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &dst.join(entry.file_name()))?;
        } else {
            fs::copy(entry.path(), dst.join(entry.file_name()))?;
        }
    }
    Ok(())
}

/// Unpack a .theme (ZIP) archive into a destination cache directory.
pub fn extract_bundle(zip_path: &Path, dest_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let file = File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file)?;
    if dest_dir.exists() {
        let _ = fs::remove_dir_all(dest_dir);
    }
    fs::create_dir_all(dest_dir)?;
    archive.extract(dest_dir)?;
    debug!("[theme_manager] Extracted theme bundle {:?} to {:?}", zip_path, dest_dir);
    Ok(())
}

/// Scans `~/.local/share/hylki/themes/` for .theme bundles and parses their manifests.
pub fn discover_themes() -> Vec<ThemeInfo> {
    ensure_directories();
    let mut themes = Vec::new();
    let mut seen_ids = std::collections::HashSet::new();

    // Scan order:
    // 1. User local directory: ~/.local/share/hylki/themes/
    // 2. Bundled application themes: ./themes/
    let search_roots = [themes_dir(), PathBuf::from("themes")];

    for root_dir in &search_roots {
        let Ok(entries) = fs::read_dir(root_dir) else {
            continue;
        };

        for entry in entries.flatten() {
            let path = entry.path();

            // Scenario A: .theme ZIP file bundle
            if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("theme") {
                let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown");
                let target_cache = cache_dir().join(stem);

                // Extract if cache does not exist or if .theme archive is newer than cache
                let needs_extract = if !target_cache.exists() {
                    true
                } else {
                    let zip_mtime = entry.metadata().and_then(|m| m.modified()).ok();
                    let cache_mtime = fs::metadata(&target_cache).and_then(|m| m.modified()).ok();
                    match (zip_mtime, cache_mtime) {
                        (Some(zm), Some(cm)) => zm > cm,
                        _ => false,
                    }
                };

                if needs_extract {
                    if let Err(e) = extract_bundle(&path, &target_cache) {
                        error!("[theme_manager] Failed to extract bundle {:?}: {}", path, e);
                        continue;
                    }
                }

                let manifest_path = target_cache.join("theme.toml");
                let manifest: ThemeManifest = if manifest_path.exists() {
                    match fs::read_to_string(&manifest_path) {
                        Ok(toml_str) => toml::from_str(&toml_str).unwrap_or_else(|err| {
                            warn!("[theme_manager] Parse error in {:?}: {}", manifest_path, err);
                            fallback_manifest(stem)
                        }),
                        Err(_) => fallback_manifest(stem),
                    }
                } else {
                    fallback_manifest(stem)
                };

                if !seen_ids.contains(&manifest.theme.id) {
                    seen_ids.insert(manifest.theme.id.clone());
                    themes.push(ThemeInfo {
                        id: manifest.theme.id.clone(),
                        name: manifest.theme.name.clone(),
                        bundle_path: path,
                        extracted_dir: target_cache,
                        manifest,
                    });
                }
            }
            // Scenario B: Unpacked theme directory containing theme.toml
            else if path.is_dir() && path.join("theme.toml").exists() {
                let stem = path.file_name().and_then(|s| s.to_str()).unwrap_or("unknown");
                let manifest_path = path.join("theme.toml");
                let manifest: ThemeManifest = match fs::read_to_string(&manifest_path) {
                    Ok(toml_str) => toml::from_str(&toml_str).unwrap_or_else(|err| {
                        warn!("[theme_manager] Parse error in {:?}: {}", manifest_path, err);
                        fallback_manifest(stem)
                    }),
                    Err(_) => fallback_manifest(stem),
                };

                if !seen_ids.contains(&manifest.theme.id) {
                    seen_ids.insert(manifest.theme.id.clone());
                    themes.push(ThemeInfo {
                        id: manifest.theme.id.clone(),
                        name: manifest.theme.name.clone(),
                        bundle_path: path.clone(),
                        extracted_dir: path,
                        manifest,
                    });
                }
            }
        }
    }

    themes.sort_by(|a, b| a.name.cmp(&b.name));
    themes
}

fn fallback_manifest(stem: &str) -> ThemeManifest {
    ThemeManifest {
        theme: ThemeMeta {
            id: stem.to_string(),
            name: stem.replace('-', " ").replace('_', " "),
            author: None,
            version: None,
            style: Some("adaptive".to_string()),
        },
        window: WindowConfig::default(),
    }
}

/// Register the main application window for dynamic transparency toggling.
pub fn set_main_window(win: &adw::ApplicationWindow) {
    MAIN_WINDOW.with(|slot| {
        *slot.borrow_mut() = Some(win.clone());
    });
}

/// Returns the currently active custom theme ID, if any.
pub fn active_theme_id() -> Option<String> {
    ACTIVE_THEME_ID.with(|slot| slot.borrow().clone())
}

/// Back up the current style state prior to testing a new theme.
pub fn prepare_switch() {
    SAVED_PROVIDER.with(|p| {
        *p.borrow_mut() = ACTIVE_PROVIDER.with(|cur| cur.borrow().clone());
    });
    SAVED_THEME_ID.with(|t| {
        *t.borrow_mut() = ACTIVE_THEME_ID.with(|cur| cur.borrow().clone());
    });
    SAVED_READER_CSS.with(|r| {
        *r.borrow_mut() = ACTIVE_READER_CSS.with(|cur| cur.borrow().clone());
    });
    SAVED_TRANSPARENCY.with(|trans| {
        let is_trans = MAIN_WINDOW.with(|w| {
            w.borrow()
                .as_ref()
                .map(|win| win.has_css_class("transparent-window"))
                .unwrap_or(false)
        });
        *trans.borrow_mut() = is_trans;
    });
        let manager = adw::StyleManager::default();
    SAVED_COLOR_SCHEME.with(|cs| {
        *cs.borrow_mut() = Some(manager.color_scheme());
    });
    debug!("[theme_manager] Saved adw::StyleManager scheme {:?} for safety rollback.", manager.color_scheme());
    debug!("[theme_manager] Saved theme backup state for safety rollback, mate!");
}

/// Rollback the GTK CSS provider, window transparency, and WebKit CSS to the saved state.
pub fn rollback() {
    info!("[theme_manager] 20-Second rollback triggered! Reverting UI state, mate.");
    let display = gtk::gdk::Display::default();

    // 1. Revert GTK CSS Provider
    if let Some(disp) = display.as_ref() {
        ACTIVE_PROVIDER.with(|cur| {
            if let Some(prov) = cur.borrow_mut().take() {
                gtk::style_context_remove_provider_for_display(disp, &prov);
            }
        });
        SAVED_PROVIDER.with(|saved| {
            if let Some(prov) = saved.borrow().as_ref() {
                gtk::style_context_add_provider_for_display(
                    disp,
                    prov,
                    gtk::STYLE_PROVIDER_PRIORITY_USER,
                );
                ACTIVE_PROVIDER.with(|cur| *cur.borrow_mut() = Some(prov.clone()));
            }
        });
    }

    // 2. Revert Window Transparency
    let was_transparent = SAVED_TRANSPARENCY.with(|t| *t.borrow());
    MAIN_WINDOW.with(|w| {
        if let Some(win) = w.borrow().as_ref() {
            if was_transparent {
                win.add_css_class("transparent-window");
            } else {
                win.remove_css_class("transparent-window");
            }
        }
    });

    // 3. Revert Theme ID and Reader CSS
    SAVED_THEME_ID.with(|saved| {
        ACTIVE_THEME_ID.with(|cur| *cur.borrow_mut() = saved.borrow().clone());
    });
    SAVED_READER_CSS.with(|saved| {
        ACTIVE_READER_CSS.with(|cur| *cur.borrow_mut() = saved.borrow().clone());
    });
    sync_registered_ucms();

    // 4. Revert adw::StyleManager color scheme, fair dinkum!
    let manager = adw::StyleManager::default();
    SAVED_COLOR_SCHEME.with(|cs| {
        if let Some(scheme) = cs.borrow().as_ref() {
            manager.set_color_scheme(*scheme);
            info!("[theme_manager] Reverted adw::StyleManager color scheme to {:?}, mate.", scheme);
        }
    });
}

/// User confirmed the theme: commit and discard backup.
pub fn commit() {
    SAVED_PROVIDER.with(|p| *p.borrow_mut() = None);
    SAVED_THEME_ID.with(|t| *t.borrow_mut() = None);
    SAVED_READER_CSS.with(|r| *r.borrow_mut() = None);
    SAVED_COLOR_SCHEME.with(|cs| *cs.borrow_mut() = None);
    info!("[theme_manager] Theme switch committed and confirmed by user!");
}

/// Apply a custom theme by ID (or None to reset to default GNOME/stock styling).
pub fn apply_theme(target_id: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    let display = gtk::gdk::Display::default().ok_or("No default GDK display available")?;

    match target_id {
        None | Some("default") | Some("none") => {
            // Remove custom GTK CSS override provider
            ACTIVE_PROVIDER.with(|cur| {
                if let Some(prov) = cur.borrow_mut().take() {
                    gtk::style_context_remove_provider_for_display(&display, &prov);
                }
            });

            // Clear transparency
            MAIN_WINDOW.with(|w| {
                if let Some(win) = w.borrow().as_ref() {
                    win.remove_css_class("transparent-window");
                }
            });

            ACTIVE_THEME_ID.with(|t| *t.borrow_mut() = None);
            ACTIVE_READER_CSS.with(|r| *r.borrow_mut() = None);
            sync_registered_ucms();
                        let manager = adw::StyleManager::default();
            manager.set_color_scheme(adw::ColorScheme::Default);
            debug!("[theme_manager] Reset adw::StyleManager to default color scheme, mate.");
            info!("[theme_manager] Custom theme cleared, back to standard GNOME look.");
            Ok(())
        }
        Some(id) => {
            let themes = discover_themes();
            let theme = themes
                .into_iter()
                .find(|t| t.id == id || t.name == id)
                .ok_or_else(|| format!("Custom theme '{}' not found in ~/.local/share/hylki/themes", id))?;

            // CRITICAL GUARDRAIL: Strict State Isolation (No CSS Bleed)
            // On theme activation, purge the existing provider from gdk::Display::default()
            // prior to parsing and injecting the new CSS.
            ACTIVE_PROVIDER.with(|cur| {
                if let Some(old) = cur.borrow_mut().take() {
                    gtk::style_context_remove_provider_for_display(&display, &old);
                    debug!("[theme_manager] Purged existing CssProvider from display prior to loading new CSS, mate!");
                }
            });

            // 1. Load and inject gtk.css
            let gtk_css_path = theme.extracted_dir.join("gtk.css");
            if gtk_css_path.exists() {
                let provider = gtk::CssProvider::new();
                provider.load_from_path(&gtk_css_path);
                gtk::style_context_add_provider_for_display(
                    &display,
                    &provider,
                    gtk::STYLE_PROVIDER_PRIORITY_USER,
                );
                ACTIVE_PROVIDER.with(|cur| {
                    *cur.borrow_mut() = Some(provider);
                });
                debug!("[theme_manager] Injected gtk.css from {:?}", gtk_css_path);
            } else {
                warn!("[theme_manager] Theme bundle does not contain gtk.css: {:?}", gtk_css_path);
            }

            // 2. Handle window transparency capability:
            // Strictly isolated to "glass-vibrancy". All other themes remain 100% opaque at system window level.
            let transparent = theme.id == "glass-vibrancy" && theme.manifest.window.transparent;
            MAIN_WINDOW.with(|w| {
                if let Some(win) = w.borrow().as_ref() {
                    if transparent {
                        win.add_css_class("transparent-window");
                        debug!("[theme_manager] Enabled .transparent-window class on main window for glass-vibrancy.");
                    } else {
                        win.remove_css_class("transparent-window");
                        debug!("[theme_manager] Enforced 100% opaque window for theme '{}', mate.", theme.id);
                    }
                }
            });

            // 3. Handle WebKit reader.css
            let reader_css_path = theme.extracted_dir.join("reader.css");
            if reader_css_path.exists() {
                let css_content = fs::read_to_string(&reader_css_path).unwrap_or_default();
                ACTIVE_READER_CSS.with(|r| *r.borrow_mut() = Some(css_content));
            } else {
                ACTIVE_READER_CSS.with(|r| *r.borrow_mut() = None);
            }
            sync_registered_ucms();

                        // 4. Synchronize libadwaita StyleManager palette mode (ForceLight, ForceDark, Default)
            let manager = adw::StyleManager::default();
            match theme.manifest.theme.style.as_deref().map(|s| s.to_lowercase()).as_deref() {
                Some("light") => {
                    manager.set_color_scheme(adw::ColorScheme::ForceLight);
                    info!("[theme_manager] Synchronized adw::StyleManager to ForceLight for theme '{}'", theme.name);
                }
                Some("dark") => {
                    manager.set_color_scheme(adw::ColorScheme::ForceDark);
                    info!("[theme_manager] Synchronized adw::StyleManager to ForceDark for theme '{}'", theme.name);
                }
                _ => {
                    manager.set_color_scheme(adw::ColorScheme::Default);
                    info!("[theme_manager] Synchronized adw::StyleManager to Default for theme '{}'", theme.name);
                }
            }

            ACTIVE_THEME_ID.with(|t| *t.borrow_mut() = Some(theme.id.clone()));
            info!("[theme_manager] Successfully applied theme '{}'", theme.name);
            Ok(())
        }
    }
}

/// Inject active reader.css into a WebKit UserContentManager and register it for live updates.
pub fn inject_reader_css(ucm: &webkit6::UserContentManager) {
    REGISTERED_UCMS.with(|list| {
        list.borrow_mut().push(ucm.downgrade());
    });
    apply_reader_css_to_ucm(ucm);
}

fn apply_reader_css_to_ucm(ucm: &webkit6::UserContentManager) {
    ucm.remove_all_style_sheets();
    ACTIVE_READER_CSS.with(|slot| {
        if let Some(css) = slot.borrow().as_ref() {
            if !css.trim().is_empty() {
                let sheet = webkit6::UserStyleSheet::new(
                    css,
                    webkit6::UserContentInjectedFrames::AllFrames,
                    webkit6::UserStyleLevel::User,
                    &[],
                    &[],
                );
                ucm.add_style_sheet(&sheet);
                debug!("[theme_manager] Injected custom reader.css into WebKit UserContentManager.");
            }
        }
    });
}

fn sync_registered_ucms() {
    REGISTERED_UCMS.with(|list| {
        let mut alive = Vec::new();
        for weak in list.borrow().iter() {
            if let Some(ucm) = weak.upgrade() {
                apply_reader_css_to_ucm(&ucm);
                alive.push(weak.clone());
            }
        }
        *list.borrow_mut() = alive;
    });
}

/// Initialize theme manager on app startup.
pub fn init(window: &impl glib::IsA<gtk::Widget>) {
    ensure_directories();
    if let Some(app_win) = window.clone().upcast::<gtk::Widget>().downcast::<adw::ApplicationWindow>().ok() {
        set_main_window(&app_win);
    }
}
